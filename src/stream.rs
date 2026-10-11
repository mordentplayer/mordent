//! Streaming pipeline: manifest resolution → variant pick → single-file
//! progressive download with in-line FairPlay `cbcs` decryption (or plain
//! passthrough for unencrypted previews) into a disk-backed seekable source.

use crate::acache;
use crate::config::{Config, Quality};
use crate::fairplay;
use crate::wrapper::Wrapper;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
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

/// Pick a variant URL from a master manifest for `quality`.
///
/// Scoring keeps the historical ordering: ALAC variants outrank every AAC one,
/// and AAC variants are ranked by BANDWIDTH. `quality` then selects from that
/// ranking — [`Quality::Lossless`] the best ALAC (falling back to the best AAC
/// when the track has none), [`Quality::High`] the best AAC, and
/// [`Quality::DataSaver`] the smallest variant. Variants whose CODECS match
/// nothing known are only used when nothing else exists.
pub fn pick_variant(master: &str, base_url: &str, quality: Quality) -> Result<String> {
    let mut groups: HashMap<String, (u32, u8)> = HashMap::new(); // group-id → (sample rate, bitrate-ish)
    let mut variants: Vec<(u64, bool, String)> = Vec::new(); // (score, is_alac, url)
    let mut pending: Option<(u64, bool)> = None;

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
            pending = Some(if codecs.contains("alac") {
                (1_000_000_000 + rate as u64, true)
            } else if codecs.contains("mp4a.") {
                (bandwidth.min(1_000_000), false)
            } else {
                (0, false)
            });
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some((score, alac)) = pending.take()
        {
            variants.push((score, alac, resolve_relative(base_url, line)));
        }
    }
    // Known-codec variants first; an all-unknown manifest still resolves.
    let mut pool: Vec<&(u64, bool, String)> =
        variants.iter().filter(|(s, _, _)| *s > 0).collect();
    if pool.is_empty() {
        pool = variants.iter().collect();
    }
    let pick = match quality {
        Quality::Lossless => pool
            .iter()
            .filter(|(_, alac, _)| *alac)
            .max_by_key(|(s, _, _)| *s)
            .or_else(|| pool.iter().max_by_key(|(s, _, _)| *s)),
        Quality::High => pool
            .iter()
            .filter(|(_, alac, _)| !*alac)
            .max_by_key(|(s, _, _)| *s)
            .or_else(|| pool.iter().max_by_key(|(s, _, _)| *s)),
        Quality::DataSaver => pool.iter().min_by_key(|(s, _, _)| *s),
    };
    pick.map(|(_, _, u)| u.clone())
        .ok_or_else(|| anyhow!("no variants in master manifest"))
}

fn attr(line: &str, name: &str) -> Option<String> {
    line.split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

/// Manifest texts + the URLs they came from — the material the audio cache
/// stores, and what `MediaPlan` is reduced from.
pub struct Manifests {
    /// Master manifest text, when the wrapper URL was a master.
    pub master: Option<String>,
    pub media_url: String,
    pub media: String,
}

/// Reduce fetched manifests to a [`MediaPlan`].
pub fn plan_of(m: &Manifests) -> Result<MediaPlan> {
    parse_media_manifest(&m.media, &m.media_url)
}

/// Fetch the manifest for a track and reduce it to a [`MediaPlan`].
///
/// Uses [`Quality::Lossless`] (the pre-`quality`-key behaviour); the player
/// path carries the configured quality through [`open_track`].
pub fn plan_for_track(wrapper: &Wrapper, agent: &ureq::Agent, adam_id: &str) -> Result<MediaPlan> {
    plan_of(&fetch_manifests(wrapper, agent, adam_id, Quality::Lossless)?)
}

/// Fetch master + variant manifests from the CDN (via the wrapper URL),
/// picking the variant for `quality`.
pub fn fetch_manifests(
    wrapper: &Wrapper,
    agent: &ureq::Agent,
    adam_id: &str,
    quality: Quality,
) -> Result<Manifests> {
    let manifest_url = wrapper.m3u8_url(adam_id)?;
    let text = fetch_text(agent, &manifest_url)?;
    if !text.contains("#EXT-X-STREAM-INF") {
        // The wrapper URL was already a media manifest.
        return Ok(Manifests {
            master: None,
            media_url: manifest_url,
            media: text,
        });
    }
    let media_url = pick_variant(&text, &manifest_url, quality)?;
    if media_url == manifest_url {
        bail!("variant resolution failed");
    }
    eprintln!("[stream] quality={quality} variant: {media_url}");
    let media = fetch_text(agent, &media_url)?;
    Ok(Manifests {
        master: Some(text),
        media_url,
        media,
    })
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
    /// Audio-cache track dir; license responses are stored/loaded here
    /// keyed by key URI when set.
    disk: Option<PathBuf>,
}

impl<'a> KeyResolver<'a> {
    pub fn new(wrapper: &'a Wrapper, adam_id: &str) -> Self {
        Self {
            wrapper,
            adam_id: adam_id.to_string(),
            cache: Mutex::new(HashMap::new()),
            disk: None,
        }
    }

    /// Like [`KeyResolver::new`] but persists license responses in the
    /// audio-cache track dir (best-effort, never fails playback).
    pub fn with_cache(wrapper: &'a Wrapper, adam_id: &str, dir: Option<PathBuf>) -> Self {
        Self { disk: dir, ..Self::new(wrapper, adam_id) }
    }

    fn disk_json(&self, uri: &str) -> Option<String> {
        let dir = self.disk.as_ref()?;
        let raw = std::fs::read_to_string(acache::key_path(dir, uri)).ok()?;
        let s = raw.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    pub fn template(&self, uri: &str) -> Result<Arc<temari::rounds::Template>> {
        if let Some(t) = self.cache.lock().get(uri) {
            return Ok(t.clone());
        }
        let make = |json: &str| -> Result<Arc<temari::rounds::Template>> {
            let tmpl = temari::template::template_from_json(json.trim())
                .map_err(|e| anyhow!("template from key json: {e}"))?;
            Ok(Arc::new(tmpl))
        };
        // disk (cache hit) first; a corrupt cached license falls back to the
        // wrapper below rather than failing playback.
        let cached = self.disk_json(uri).and_then(|j| make(&j).ok());
        let t = match cached {
            Some(t) => t,
            None => {
                let adam = if uri == PREFETCH_URI { "0" } else { &self.adam_id };
                let data = self.wrapper.key(adam, uri)?;
                let json = serde_json::to_string(&data).context("serializing key data")?;
                if let Some(dir) = &self.disk {
                    acache::store_key(dir, uri, &json);
                }
                make(&json)?
            }
        };
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
    cancel: Arc<std::sync::atomic::AtomicBool>,
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

    /// Backing file for populate-only fetches (audio-cache prefetch): an
    /// anonymous scratch file, unlinked right after open. The discarded
    /// decrypted output never occupies a visible path and the OS reclaims
    /// the space on close — even on a crash. (unlink-of-open-file is
    /// POSIX; the only platform this client targets.)
    fn shared_discard() -> Result<Arc<Mutex<Shared>>> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "mordent-prefetch-{}-{}.m4a",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&p)?;
        let _ = std::fs::remove_file(&p);
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
                if g.downloaded < need && let Some(e) = &g.err {
                    bail!("stream error: {e}");
                }
                return Ok(g.downloaded >= need);
            }
            self.cv.wait(&mut g);
        }
    }

    /// A shared flag that tells the fetch worker to stop early. Dropping a
    /// PacketSource wrapping this source sets it, so a skipped track stops
    /// downloading in the background.
    pub fn cancel_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.cancel)
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
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wsh = sh.clone();
        let wcv = cv.clone();
        let wcancel = Arc::clone(&cancel);
        let url = url.to_string();
        std::thread::Builder::new().name("mordent-fetch".into()).spawn(move || {
            let run = || -> Result<()> {
                let resp = agent.get(&url).call().with_context(|| format!("GET {url}"))?;
                let mut buf = [0u8; 64 * 1024];
                let mut r = resp.into_reader();
                loop {
                    if wcancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Ok(());
                    }
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
            cancel: Arc::clone(&cancel),
        };
        src.wait_available(1)?;
        Ok(src)
    }

    /// Shared box-pump: reads fMP4 boxes from `r` (network or the audio
    /// cache), appends DECRYPTED output to the playback buffer and tees raw
    /// ENCRYPTED bytes into `tee` when present. Returns on clean EOF (which
    /// commits the cache entry); a canceled download returns Ok without
    /// committing — an uncommitted entry simply misses next time.
    #[allow(clippy::too_many_arguments)]
    fn pump(
        r: &mut dyn Read,
        keys: &[Option<String>],
        first_key: Option<&str>,
        templates: &[(String, Arc<temari::rounds::Template>)],
        sh: &Arc<Mutex<Shared>>,
        cv: &Condvar,
        cancel: &std::sync::atomic::AtomicBool,
        tee: &mut Option<Tee>,
    ) -> Result<()> {
        // init: ftyp + moov
        let mut init_raw = Vec::new();
        let mut saw_moov = false;
        for _ in 0..2 {
            let (h, payload, hdr) = read_box(r)?;
            if let Some(t) = tee {
                t.write(&hdr, &payload);
            }
            let kind = kind_of(&h.kind);
            if kind != "ftyp" && kind != "moov" {
                bail!("unexpected init box {kind}");
            }
            saw_moov |= kind == "moov";
            init_raw.extend_from_slice(&hdr);
            init_raw.extend_from_slice(&payload);
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
        Self::append(sh, cv, &init.bytes)?;

        let mut frag_idx = 0usize;
        let mut cur_tmpl: Option<Arc<temari::rounds::Template>> = first_key
            .and_then(|k| templates.iter().find(|(u, _)| u == k))
            .map(|(_, t)| t.clone());

        loop {
            // read top-level boxes: moof then mdat form one fragment
            let mut pending_moof: Option<Vec<u8>> = None;
            let mdat;
            loop {
                let (h, payload, hdr) = match read_box(r) {
                    Ok(x) => x,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // clean end of stream → the cached entry is complete
                        if let Some(t) = tee {
                            t.commit();
                        }
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                };
                if let Some(t) = tee {
                    t.write(&hdr, &payload);
                }
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
                        Self::append(sh, cv, &h.assemble(&payload))?;
                    }
                }
            }
            let moof_buf = match pending_moof {
                Some(m) => m,
                None => bail!("mdat without moof"),
            };
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            let key_uri = keys.get(frag_idx).cloned().flatten();
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
            Self::append(sh, cv, &clear)?;
            frag_idx += 1;
        }
    }

    /// FairPlay stream: single-file download with in-line cbcs decryption.
    /// With `audio` set, the raw (ENCRYPTED) response bytes are teed into
    /// the audio cache — never the decrypted output.
    pub fn start_fairplay(
        plan: MediaPlan,
        resolver: &KeyResolver,
        cache_file: PathBuf,
        agent: ureq::Agent,
        audio: Option<acache::Populate>,
    ) -> Result<Self> {
        Self::spawn_fairplay(plan, resolver, Self::shared(&cache_file)?, agent, audio)
    }

    /// Populate-only variant (audio-cache prefetch): the IDENTICAL
    /// fetch/tee pipeline, but the in-line decrypted output is discarded
    /// to an anonymous scratch file nobody reads — the point is the
    /// ENCRYPTED tee, so no transient playback buffer is materialized.
    pub fn start_fairplay_populate(
        plan: MediaPlan,
        resolver: &KeyResolver,
        agent: ureq::Agent,
        audio: acache::Populate,
    ) -> Result<Self> {
        Self::spawn_fairplay(plan, resolver, Self::shared_discard()?, agent, Some(audio))
    }

    fn spawn_fairplay(
        plan: MediaPlan,
        resolver: &KeyResolver,
        sh: Arc<Mutex<Shared>>,
        agent: ureq::Agent,
        audio: Option<acache::Populate>,
    ) -> Result<Self> {
        let cv = Arc::new(Condvar::new());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wsh = sh.clone();
        let wcv = cv.clone();
        let wcancel = Arc::clone(&cancel);

        // The resolver borrows the wrapper; the worker thread needs 'static.
        // Templates are Arc'd and cloned out up front for every distinct key.
        let mut templates: Vec<(String, Arc<temari::rounds::Template>)> = Vec::new();
        for uri in plan.keys.iter().flatten() {
            if !templates.iter().any(|(u, _)| u == uri) {
                templates.push((uri.clone(), resolver.template(uri)?));
            }
        }
        let default_key = plan.keys.iter().flatten().next().cloned();
        let media_url = plan.file_url.clone();
        let keys = plan.keys.clone();
        let mut tee = match audio {
            Some(p) => match Tee::new(p, &media_url, &unique_keys(&keys)) {
                Ok(t) => Some(t),
                Err(e) => {
                    acache::once_warn("audio cache open", e);
                    None
                }
            },
            None => None,
        };

        std::thread::Builder::new().name("mordent-fetch".into()).spawn(move || {
            let mut run = || -> Result<()> {
                let resp = agent.get(&media_url).call().context("GET media file")?;
                let mut r = BufReader::new(resp.into_reader());
                Self::pump(
                    &mut r,
                    &keys,
                    default_key.as_deref(),
                    &templates,
                    &wsh,
                    &wcv,
                    &wcancel,
                    &mut tee,
                )
            };
            Self::finish(&wsh, &wcv, run().err().map(|e| format!("{e:#}")));
        })?;

        let src = Self {
            sh,
            cv,
            pos: Mutex::new(0),
            cancel,
        };
        // wait for the container header so probing can start
        src.wait_available(1)?;
        Ok(src)
    }

    /// FairPlay playback entirely from the audio cache: boxes come from the
    /// stored ENCRYPTED media file, keys from the stored license responses —
    /// zero network (no wrapper, no CDN). Decrypts at consumption speed,
    /// exactly like the network path.
    pub fn start_fairplay_cached(
        entry: &acache::TrackEntry,
        cache_file: PathBuf,
    ) -> Result<Self> {
        let plan = parse_media_manifest(&entry.variant, &entry.media_url)?;
        let mut templates: Vec<(String, Arc<temari::rounds::Template>)> = Vec::new();
        for uri in plan.keys.iter().flatten() {
            if templates.iter().any(|(u, _)| u == uri) {
                continue;
            }
            let json = entry
                .keys
                .get(uri)
                .ok_or_else(|| anyhow!("cached license for {uri} missing"))?;
            let tmpl = temari::template::template_from_json(json.trim())
                .map_err(|e| anyhow!("template from cached key json: {e}"))?;
            templates.push((uri.clone(), Arc::new(tmpl)));
        }
        let default_key = plan.keys.iter().flatten().next().cloned();
        let keys = plan.keys;
        let sh = Self::shared(&cache_file)?;
        let cv = Arc::new(Condvar::new());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wsh = sh.clone();
        let wcv = cv.clone();
        let wcancel = Arc::clone(&cancel);
        let media = entry.media.clone();
        let dir = entry.dir.clone();

        std::thread::Builder::new().name("mordent-cache".into()).spawn(move || {
            let run = || -> Result<()> {
                let f = std::fs::File::open(&media)?;
                let mut r = BufReader::new(f);
                Self::pump(
                    &mut r,
                    &keys,
                    default_key.as_deref(),
                    &templates,
                    &wsh,
                    &wcv,
                    &wcancel,
                    &mut None,
                )
            };
            match run() {
                Ok(()) => Self::finish(&wsh, &wcv, None),
                Err(e) => {
                    // corrupt cached material: drop the completeness marker
                    // so the next play attempt refetches from the network
                    acache::once_warn("cached playback", &e);
                    let _ = std::fs::remove_file(dir.join("meta.json"));
                    Self::finish(&wsh, &wcv, Some(format!("{e:#}")));
                }
            }
        })?;

        let src = Self {
            sh,
            cv,
            pos: Mutex::new(0),
            cancel,
        };
        src.wait_available(1)?;
        Ok(src)
    }
}

/// Destination for the encrypted-byte tee (audio cache `media.bin`).
struct Tee {
    file: std::io::BufWriter<std::fs::File>,
    dir: PathBuf,
    meta: acache::Meta,
    dead: bool,
}

impl Tee {
    fn new(p: acache::Populate, media_url: &str, keys: &[String]) -> Result<Self> {
        std::fs::create_dir_all(&p.dir)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(p.dir.join("media.bin"))?;
        Ok(Self {
            file: std::io::BufWriter::with_capacity(256 * 1024, file),
            dir: p.dir,
            meta: acache::Meta {
                last_played: p.last_played,
                size_bytes: 0,
                media_url: media_url.to_string(),
                keys: keys.to_vec(),
            },
            dead: false,
        })
    }

    /// `hdr` + `payload` are the exact bytes as served (8- or 16-byte box
    /// header preserved) — the cached file is byte-identical to the CDN
    /// response.
    fn write(&mut self, hdr: &[u8], payload: &[u8]) {
        if self.dead {
            return;
        }
        let r = self.file.write_all(hdr).and_then(|_| self.file.write_all(payload));
        match r {
            Ok(()) => self.meta.size_bytes += (hdr.len() + payload.len()) as u64,
            Err(e) => {
                self.dead = true;
                acache::once_warn("audio cache write", e);
            }
        }
    }

    /// Completeness marker — only on clean EOF of the full download.
    fn commit(&mut self) {
        if self.dead {
            return;
        }
        let r = self
            .file
            .flush()
            .map_err(anyhow::Error::from)
            .and_then(|()| acache::commit_meta(&self.dir, &self.meta));
        if let Err(e) = r {
            acache::once_warn("audio cache commit", e);
        }
    }
}

fn unique_keys(keys: &[Option<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keys.iter().flatten() {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

// ---- high-level: open a playable source with the audio cache in the loop ----

/// How the returned source is fed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    /// Served from the audio cache — zero network (no wrapper, no CDN).
    Cached,
    /// Wrapper-planned CDN stream (populating the cache when enabled).
    Stream,
    /// Unencrypted preview passthrough (wrapper unavailable / not logged in).
    Preview,
}

impl std::fmt::Display for SourceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceMode::Cached => write!(f, "cache (offline)"),
            SourceMode::Stream => write!(f, "CDN stream"),
            SourceMode::Preview => write!(f, "preview"),
        }
    }
}

/// Acquire a playable source for `adam_id`:
/// 1. complete audio-cache entry → decrypt locally, zero network;
/// 2. otherwise wrapper plan + CDN fetch, populating the cache;
/// 3. wrapper unreachable / not logged in → uncached preview fallback.
///
/// `keep` is the eviction-exempt id set (current + up-next — see
/// [`Queue::eviction_keep`]); prune never deletes those dirs.
///
/// Cache I/O problems never fail the call — they degrade to the network
/// path (logged once per process).
pub fn open_track(
    cfg: &Config,
    adam_id: &str,
    keep: &[String],
) -> Result<(ProgressiveSource, SourceMode)> {
    let started = std::time::Instant::now();
    let r = open_track_inner(cfg, adam_id, keep);
    if let Ok((_, mode)) = &r {
        eprintln!("[play] source: {mode} (ready in {:.1?})", started.elapsed());
    }
    r
}

fn open_track_inner(
    cfg: &Config,
    adam_id: &str,
    keep: &[String],
) -> Result<(ProgressiveSource, SourceMode)> {
    let agent = ureq::AgentBuilder::new().build();
    let playback_file = crate::config::cache_path(cfg, &format!("tracks/{adam_id}.m4a"))?;
    let audio = acache::AudioCache::new(cfg);
    let now = crate::config::unix_now();

    if audio.enabled() {
        // evict once per play attempt, before any fetch; the keep set
        // (current + up-next) protects near-term music and in-flight
        // downloads — theirs and any prefetch running alongside
        audio.prune(keep);
        if let Some(entry) = audio.lookup(adam_id) {
            match ProgressiveSource::start_fairplay_cached(&entry, playback_file.clone()) {
                Ok(src) => {
                    // every play refreshes recency (full cache hit included)
                    audio.touch(adam_id, now);
                    return Ok((src, SourceMode::Cached));
                }
                Err(e) => {
                    audio.invalidate(adam_id);
                    acache::once_warn("cached playback start", e);
                }
            }
        }
    }
    let populate = audio.populate_target(adam_id, now);

    let w = Wrapper::new(&cfg.wrapper_url);
    if w.regions().map(|r| !r.is_empty()).unwrap_or(false) {
        let src = start_miss(&w, agent, adam_id, cfg.quality, populate, Some(playback_file))?;
        return Ok((src, SourceMode::Stream));
    }
    let url = crate::api::Api::new(cfg.clone())?.preview_url(adam_id)?;
    let src = ProgressiveSource::start_plain(&url, playback_file, agent)?;
    Ok((src, SourceMode::Preview))
}

/// A cache miss: plan the track via the wrapper (`quality` selects the
/// variant) and start the CDN fetch, teeing raw ENCRYPTED bytes into
/// `populate` when set. `playback` decides where the decrypted output goes —
/// the transient seekable playback buffer (a path), or an anonymous discard
/// file (`None`: populate-only).
fn start_miss(
    w: &Wrapper,
    agent: ureq::Agent,
    adam_id: &str,
    quality: Quality,
    populate: Option<acache::Populate>,
    playback: Option<PathBuf>,
) -> Result<ProgressiveSource> {
    let m = fetch_manifests(w, &agent, adam_id, quality)?;
    if let Some(p) = &populate {
        acache::store_manifests(&p.dir, m.master.as_deref(), &m.media);
    }
    let plan = plan_of(&m)?;
    let resolver = KeyResolver::with_cache(w, adam_id, populate.as_ref().map(|p| p.dir.clone()));
    match playback {
        Some(file) => ProgressiveSource::start_fairplay(plan, &resolver, file, agent, populate),
        None => ProgressiveSource::start_fairplay_populate(
            plan,
            &resolver,
            agent,
            populate.ok_or_else(|| anyhow!("populate-only fetch without a cache target"))?,
        ),
    }
}

/// Populate the audio cache for `adam_id` WITHOUT playing: the same
/// wrapper plan + CDN fetch + ENCRYPTED tee as a cache-miss playback, but
/// the decrypted output is discarded. The returned source's
/// `cancel_flag()` aborts the fetch at the next fragment boundary (an
/// aborted fetch never commits its cache entry), `is_done()` reports the
/// outcome; the entry is complete once `AudioCache::lookup` finds it.
///
/// Fails when the cache is disabled or the wrapper is unusable — previews
/// are not cacheable and are never prefetched.
pub fn populate_track(cfg: &Config, adam_id: &str, last_played: u64) -> Result<ProgressiveSource> {
    let audio = acache::AudioCache::new(cfg);
    let populate = audio
        .populate_target(adam_id, last_played)
        .ok_or_else(|| anyhow!("audio cache disabled"))?;
    let w = Wrapper::new(&cfg.wrapper_url);
    if !w.regions().map(|r| !r.is_empty()).unwrap_or(false) {
        bail!("wrapper has no regions — nothing cacheable to fetch");
    }
    start_miss(&w, ureq::AgentBuilder::new().build(), adam_id, cfg.quality, Some(populate), None)
}

/// Read one top-level box: `(head, payload, exact header bytes as read)`
/// — the header is kept so callers can tee byte-identical raw data.
fn read_box(r: &mut dyn std::io::Read) -> std::io::Result<(BoxHead, Vec<u8>, Vec<u8>)> {
    let mut b8 = [0u8; 8];
    r.read_exact(&mut b8)?;
    let mut size = u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]) as usize;
    let mut hdr = 8usize;
    let mut head = b8.to_vec();
    if size == 1 {
        let mut b16 = [0u8; 8];
        r.read_exact(&mut b16)?;
        size = u64::from_be_bytes(b16) as usize;
        hdr = 16;
        head.extend_from_slice(&b16);
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
        head,
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

#[cfg(test)]
mod variant_tests {
    use super::*;

    /// The variant set the wrapper serves for every track observed so far
    /// (ALAC + AAC 256/128 + HE-AAC 64), as relative URIs.
    const MASTER: &str = "\
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a256\",AUTOSELECT=YES,NAME=\"songEnhanced\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aalac\",AUTOSELECT=YES,NAME=\"songEnhanced\",SAMPLE-RATE=44100,BIT-DEPTH=24
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a128\",AUTOSELECT=YES,NAME=\"songEnhanced\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a64\",AUTOSELECT=YES,NAME=\"songEnhanced\"
#EXT-X-STREAM-INF:BANDWIDTH=270699,CODECS=\"mp4a.40.2\",AUDIO=\"a256\"
v256.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1808512,CODECS=\"alac\",AUDIO=\"aalac\"
valac.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=134985,CODECS=\"mp4a.40.2\",AUDIO=\"a128\"
v128.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=77899,CODECS=\"mp4a.40.5\",AUDIO=\"a64\"
v64.m3u8
";
    const BASE: &str = "https://cdn.example/x/master.m3u8";

    fn pick(master: &str, q: Quality) -> String {
        pick_variant(master, BASE, q).unwrap()
    }

    #[test]
    fn each_quality_picks_its_own_variant() {
        assert_eq!(pick(MASTER, Quality::Lossless), "https://cdn.example/x/valac.m3u8");
        assert_eq!(pick(MASTER, Quality::High), "https://cdn.example/x/v256.m3u8");
        assert_eq!(pick(MASTER, Quality::DataSaver), "https://cdn.example/x/v64.m3u8");
    }

    #[test]
    fn lossless_falls_back_to_aac_without_alac() {
        let no_alac = MASTER
            .lines()
            .filter(|l| !l.contains("alac") && !l.contains("valac"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(pick(&no_alac, Quality::Lossless), "https://cdn.example/x/v256.m3u8");
        assert_eq!(pick(&no_alac, Quality::High), "https://cdn.example/x/v256.m3u8");
        assert_eq!(pick(&no_alac, Quality::DataSaver), "https://cdn.example/x/v64.m3u8");
    }

    #[test]
    fn absolute_variant_urls_are_kept() {
        let abs = "#EXTM3U\n\
                   #EXT-X-STREAM-INF:BANDWIDTH=100,CODECS=\"alac\"\n\
                   https://other.example/audio.m3u8\n";
        assert_eq!(pick(abs, Quality::Lossless), "https://other.example/audio.m3u8");
    }

    #[test]
    fn empty_manifest_is_an_error() {
        assert!(pick_variant("#EXTM3U\n", BASE, Quality::Lossless).is_err());
    }

    /// The lossless pick must not drift from the historical behaviour: best
    /// ALAC by sample rate, never an AAC variant when ALAC exists.
    #[test]
    fn lossless_ignores_aac_even_when_aac_has_more_bandwidth() {
        let master = "\
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"hi\",NAME=\"h\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"l\",NAME=\"l\",SAMPLE-RATE=192000
#EXT-X-STREAM-INF:BANDWIDTH=990000,CODECS=\"mp4a.40.2\",AUDIO=\"hi\"
aac.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1200000,CODECS=\"alac\",AUDIO=\"l\"
alac192.m3u8
";
        assert_eq!(pick(master, Quality::Lossless), "https://cdn.example/x/alac192.m3u8");
        assert_eq!(pick(master, Quality::High), "https://cdn.example/x/aac.m3u8");
    }
}
