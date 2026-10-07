//! Streaming pipeline: manifest resolution → variant pick → single-file
//! progressive download with in-line FairPlay `cbcs` decryption (or plain
//! passthrough for unencrypted previews) into a disk-backed seekable source.

use crate::fairplay;
use crate::wrapper::Wrapper;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

// ---- manifest model ----

/// A resolved media manifest: one MP4 file fetched sequentially, with the
/// key URI that guards each fragment (byte ranges are ignored — fragments
/// are re-parsed from the byte stream).
#[derive(Clone, Debug)]
pub struct MediaPlan {
    pub file_url: String,
    /// Per-fragment skd URIs (index-aligned). `None` inherits the previous.
    pub keys: Vec<Option<String>>,
}

fn resolve_relative(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    let end = base.rfind('/').unwrap_or(0);
    format!("{}/{}", &base[..end], url)
}

/// Highest-quality variant URL from a master manifest: prefer ALAC (highest
/// sample rate), then AAC 256, then anything else.
pub fn pick_variant(master: &str, base_url: &str) -> Result<String> {
    let mut groups: HashMap<String, (u32, u8)> = HashMap::new(); // group-id → (sample rate, bitrate-ish)
    let mut best: Option<(u64, String)> = None; // (score, url)
    let mut pending_score = 0u64;

    for line in master.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let gid = attr(rest, "GROUP-ID").unwrap_or_default();
            let rate = attr(rest, "SAMPLE-RATE")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let bitrate = attr(rest, "BANDWIDTH")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            groups.insert(gid, (rate, bitrate));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let codecs = attr(rest, "CODECS").unwrap_or_default();
            let bandwidth: u64 = attr(rest, "BANDWIDTH")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let audio = attr(rest, "AUDIO").unwrap_or_default();
            let (rate, _) = groups.get(&audio).copied().unwrap_or((0, 0));
            pending_score = if codecs.contains("alac") {
                1_000_000_000 + rate as u64
            } else if codecs.contains("mp4a.40.2") {
                bandwidth.min(1_000_000)
            } else {
                0
            };
        } else if !line.is_empty() && !line.starts_with('#') {
            if best.as_ref().map_or(true, |(s, _)| pending_score > *s) {
                best = Some((pending_score, resolve_relative(base_url, line)));
            }
        }
    }
    best.take().map(|(_, u)| u).ok_or_else(|| anyhow!("no variants in master manifest"))
}

fn attr(line: &str, name: &str) -> Option<String> {
    line.split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

/// Fetch the manifest for a track and reduce it to a [`MediaPlan`].
pub fn plan_for_track(wrapper: &Wrapper, agent: &ureq::Agent, adam_id: &str) -> Result<MediaPlan> {
    let manifest_url = wrapper.m3u8_url(adam_id)?;
    let text = fetch_text(agent, &manifest_url)?;
    let media_url = if text.contains("#EXT-X-STREAM-INF") {
        pick_variant(&text, &manifest_url)?
    } else {
        manifest_url.clone()
    };
    if media_url == manifest_url && text.contains("#EXT-X-STREAM-INF") {
        bail!("variant resolution failed");
    }
    let media_text = if text.contains("#EXT-X-STREAM-INF") {
        fetch_text(agent, &media_url)?
    } else {
        text
    };
    parse_media_manifest(&media_text, &media_url)
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .with_context(|| format!("body of {url}"))
}

fn parse_media_manifest(text: &str, manifest_url: &str) -> Result<MediaPlan> {
    if !text.contains("#EXTM3U") {
        bail!("not an m3u8 manifest");
    }
    if text.contains("#EXT-X-STREAM-INF") {
        bail!("got master manifest where media expected");
    }
    let mut file_url = None;
    let mut keys: Vec<Option<String>> = Vec::new();
    let mut cur_key: Option<String> = None;
    let mut expect_url = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            cur_key = attr(rest, "URI");
            continue;
        }
        if line.starts_with("#EXTINF") || line.starts_with("#EXT-X-BYTERANGE") {
            expect_url = expect_url || line.starts_with("#EXTINF");
            continue;
        }
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if expect_url {
            if file_url.is_none() {
                file_url = Some(resolve_relative(manifest_url, line));
            }
            keys.push(cur_key.clone());
            expect_url = false;
        }
    }
    let file_url = file_url.ok_or_else(|| anyhow!("media manifest has no segments"))?;
    Ok(MediaPlan { file_url, keys })
}

// ---- templates from wrapper /key ----

const PREFETCH_URI: &str = "skd://itunes.apple.com/P000000000/s1/e1";

pub struct KeyResolver<'a> {
    wrapper: &'a Wrapper,
    adam_id: String,
    cache: Mutex<HashMap<String, Arc<temari::rounds::Template>>>,
}

impl<'a> KeyResolver<'a> {
    pub fn new(wrapper: &'a Wrapper, adam_id: &str) -> Self {
        Self {
            wrapper,
            adam_id: adam_id.to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn template(&self, uri: &str) -> Result<Arc<temari::rounds::Template>> {
        if let Some(t) = self.cache.lock().get(uri) {
            return Ok(t.clone());
        }
        let adam = if uri == PREFETCH_URI { "0" } else { &self.adam_id };
        let data = self.wrapper.key(adam, uri)?;
        let json = serde_json::to_string(&data).context("serializing key data")?;
        let tmpl = temari::template::template_from_json(&json)
            .map_err(|e| anyhow!("template from key json: {e}"))?;
        let t = Arc::new(tmpl);
        self.cache.lock().insert(uri.to_string(), t.clone());
        Ok(t)
    }
}

// ---- disk-backed progressive source ----

struct Shared {
    file: std::fs::File,
    downloaded: u64,
    eof: bool,
    err: Option<String>,
}

/// `Read + Seek` over a growing on-disk buffer produced by the fetch worker.
pub struct ProgressiveSource {
    sh: Arc<Mutex<Shared>>,
    cv: Arc<Condvar>,
    pos: Mutex<u64>,
}

impl ProgressiveSource {
    fn shared(cache_file: &PathBuf) -> Result<Arc<Mutex<Shared>>> {
        if let Some(parent) = cache_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(cache_file)?;
        Ok(Arc::new(Mutex::new(Shared {
            file,
            downloaded: 0,
            eof: false,
            err: None,
        })))
    }

    fn append(sh: &Mutex<Shared>, cv: &Condvar, data: &[u8]) -> Result<()> {
        let mut s = sh.lock();
        use std::io::Write;
        let pos = s.downloaded;
        s.file.seek(SeekFrom::Start(pos))?;
        s.file.write_all(data)?;
        s.downloaded += data.len() as u64;
        drop(s);
        cv.notify_all();
        Ok(())
    }

    fn finish(sh: &Mutex<Shared>, cv: &Condvar, err: Option<String>) {
        let mut s = sh.lock();
        s.eof = true;
        s.err = err;
        drop(s);
        cv.notify_all();
    }

    fn wait_available(&self, need: u64) -> Result<bool> {
        let mut g = self.sh.lock();
        loop {
            if g.downloaded >= need || g.eof {
                if g.downloaded < need {
                    if let Some(e) = &g.err {
                        bail!("stream error: {e}");
                    }
                }
                return Ok(g.downloaded >= need);
            }
            self.cv.wait(&mut g);
        }
    }

    /// True when the fetch worker finished (clean EOF) or failed (Err).
    pub fn is_done(&self) -> Result<bool> {
        let g = self.sh.lock();
        if let Some(e) = &g.err {
            bail!("stream error: {e}");
        }
        Ok(g.eof)
    }

    /// Bytes written to the backing file so far.
    pub fn downloaded(&self) -> u64 {
        self.sh.lock().downloaded
    }

    /// Unencrypted single-file stream (previews): byte passthrough.
    pub fn start_plain(url: &str, cache_file: PathBuf, agent: ureq::Agent) -> Result<Self> {
        let sh = Self::shared(&cache_file)?;
        let cv = Arc::new(Condvar::new());
        let wsh = sh.clone();
        let wcv = cv.clone();
        let url = url.to_string();
        std::thread::Builder::new().name("amc-fetch".into()).spawn(move || {
            let run = || -> Result<()> {
                let mut resp = agent.get(&url).call().with_context(|| format!("GET {url}"))?;
                let mut buf = [0u8; 64 * 1024];
                let mut r = resp.into_reader();
                loop {
                    let n = r.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    Self::append(&wsh, &wcv, &buf[..n])?;
                }
                Ok(())
            };
            Self::finish(&wsh, &wcv, run().err().map(|e| format!("{e:#}")));
        })?;
        let src = Self {
            sh,
            cv,
            pos: Mutex::new(0),
        };
        src.wait_available(1)?;
        Ok(src)
    }

    /// FairPlay stream: single-file download with in-line cbcs decryption.
    pub fn start_fairplay(
        plan: MediaPlan,
        resolver: &KeyResolver,
        cache_file: PathBuf,
        agent: ureq::Agent,
    ) -> Result<Self> {
        let sh = Self::shared(&cache_file)?;
        let cv = Arc::new(Condvar::new());
        let wsh = sh.clone();
        let wcv = cv.clone();

        // pre-resolve the first key's template off-thread is unnecessary;
        // worker resolves lazily per key change.
        let adam_urls = plan.file_url.clone();
        // The resolver borrows the wrapper; the worker thread needs 'static.
        // Templates are Arc'd and cloned out up front for every distinct key.
        let mut templates: Vec<(String, Arc<temari::rounds::Template>)> = Vec::new();
        for k in &plan.keys {
            if let Some(uri) = k {
                if !templates.iter().any(|(u, _)| u == uri) {
                    templates.push((uri.clone(), resolver.template(uri)?));
                }
            }
        }
        let default_key = plan.keys.iter().flatten().next().cloned();

        std::thread::Builder::new().name("amc-fetch".into()).spawn(move || {
            let run = || -> Result<()> {
                let resp = agent.get(&adam_urls).call().context("GET media file")?;
                let mut r = BufReader::new(resp.into_reader());

                // init: ftyp + moov
                let mut init_raw = Vec::new();
                let mut saw_moov = false;
                for _ in 0..2 {
                    let (h, payload) = read_box(&mut r)?;
                    let kind = kind_of(&h.kind);
                    if kind != "ftyp" && kind != "moov" {
                        bail!("unexpected init box {kind}");
                    }
                    saw_moov |= kind == "moov";
                    init_raw.extend_from_slice(&h.assemble(&payload));
                }
                if !saw_moov {
                    bail!("no moov in init");
                }
                let init = fairplay::parse_init(&init_raw)?;
                let info = init
                    .tracks
                    .values()
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow!("no encrypted track in init"))?;
                Self::append(&wsh, &wcv, &init.bytes)?;

                let mut frag_idx = 0usize;
                let mut cur_tmpl: Option<Arc<temari::rounds::Template>> = None;
                if let Some(k) = plan.keys.first().and_then(|k| k.as_ref()).or(default_key.as_ref()) {
                    cur_tmpl = templates
                        .iter()
                        .find(|(u, _)| u == k)
                        .map(|(_, t)| t.clone());
                }

                loop {
                    // read top-level boxes: moof then mdat form one fragment
                    let mut pending_moof: Option<Vec<u8>> = None;
                    let mdat;
                    loop {
                        let (h, payload) = match read_box(&mut r) {
                            Ok(x) => x,
                            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                                // clean end
                                return Ok(());
                            }
                            Err(e) => return Err(e.into()),
                        };
                        match kind_of(&h.kind).as_str() {
                            "mdat" => {
                                mdat = h.assemble(&payload);
                                break;
                            }
                            "moof" => pending_moof = Some(h.assemble(&payload)),
                            "prft" | "emsg" | "free" | "skip" => {}
                            other => {
                                // trailing mfra etc — copy verbatim
                                if pending_moof.is_some() {
                                    bail!("unexpected box {other} mid-fragment");
                                }
                                Self::append(&wsh, &wcv, &h.assemble(&payload))?;
                            }
                        }
                    }
                    let moof_buf = match pending_moof {
                        Some(m) => m,
                        None => bail!("mdat without moof"),
                    };
                    let key_uri = plan.keys.get(frag_idx).cloned().flatten();
                    if let Some(uri) = key_uri {
                        cur_tmpl = templates
                            .iter()
                            .find(|(u, _)| *u == uri)
                            .map(|(_, t)| t.clone());
                    }
                    let tmpl = cur_tmpl
                        .as_ref()
                        .ok_or_else(|| anyhow!("encrypted fragment with no key"))?;
                    let clear =
                        fairplay::decrypt_fragment(&moof_buf, &mdat, &info, tmpl.as_ref())
                            .with_context(|| format!("fragment {frag_idx}"))?;
                    Self::append(&wsh, &wcv, &clear)?;
                    frag_idx += 1;
                }
            };
            Self::finish(&wsh, &wcv, run().err().map(|e| format!("{e:#}")));
        })?;

        let src = Self {
            sh,
            cv,
            pos: Mutex::new(0),
        };
        // wait for the container header so probing can start
        src.wait_available(1)?;
        Ok(src)
    }
}

fn read_box(r: &mut impl std::io::Read) -> std::io::Result<(BoxHead, Vec<u8>)> {
    let mut b8 = [0u8; 8];
    r.read_exact(&mut b8)?;
    let mut size = u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]) as usize;
    let mut hdr = 8usize;
    if size == 1 {
        let mut b16 = [0u8; 8];
        r.read_exact(&mut b16)?;
        size = u64::from_be_bytes(b16) as usize;
        hdr = 16;
    }
    if size < hdr {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "box smaller than header",
        ));
    }
    let mut payload = vec![0u8; size - hdr];
    r.read_exact(&mut payload)?;
    Ok((
        BoxHead {
            kind: [b8[4], b8[5], b8[6], b8[7]],
        },
        payload,
    ))
}

struct BoxHead {
    kind: [u8; 4],
}

impl BoxHead {
    fn assemble(&self, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(payload.len() + 8);
        v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(&self.kind);
        v.extend_from_slice(payload);
        v
    }
}

fn kind_of(k: &[u8; 4]) -> String {
    k.iter().map(|&c| c as char).collect()
}

impl Read for ProgressiveSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let pos = *self.pos.lock();
        let avail = self
            .wait_available(pos + 1)
            .map_err(std::io::Error::other)?;
        let mut sh = self.sh.lock();
        let end = sh.downloaded;
        if pos >= end {
            if sh.eof {
                return Ok(0);
            }
            return Err(std::io::Error::other("stream frontier stalled"));
        }
        let take = buf.len().min((end - pos) as usize);
        sh.file.seek(SeekFrom::Start(pos))?;
        sh.file.read_exact(&mut buf[..take])?;
        drop(sh);
        *self.pos.lock() = pos + take as u64;
        let _ = avail;
        Ok(take)
    }
}

impl Seek for ProgressiveSource {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let cur = *self.pos.lock();
        let target = match to {
            SeekFrom::Start(o) => o as i128,
            // total unknown until EOF; frontier is the honest answer for
            // mp4 size tables
            SeekFrom::End(_) => self.sh.lock().downloaded as i128,
            SeekFrom::Current(d) => cur as i128 + d as i128,
        };
        let target = target.max(0) as u64;
        let ok = self.wait_available(target).map_err(std::io::Error::other)?;
        let clamped = if ok { target } else { self.sh.lock().downloaded };
        *self.pos.lock() = clamped;
        Ok(clamped)
    }
}

impl symphonia::core::io::MediaSource for ProgressiveSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let g = self.sh.lock();
        if g.eof {
            Some(g.downloaded)
        } else {
            None
        }
    }
}
