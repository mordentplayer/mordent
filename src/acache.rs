//! Track-granular audio cache under `{cache_dir}/audio/<adam_id>/`.
//!
//! Layout per track:
//! - `master.m3u8`  — master manifest text (only when the wrapper returned a master)
//! - `variant.m3u8` — the media (variant) manifest text
//! - `key_<hash>.json` — FairPlay license response per `skd://` key URI
//! - `media.bin`    — the CDN media file **exactly as served** (fMP4: init
//!   boxes + `moof`/`mdat` fragments), i.e. the ENCRYPTED segment bytes
//! - `meta.json`    — `{last_played, size_bytes, media_url, keys}`; written
//!   only after a full clean download, so it doubles as the completeness
//!   marker (a play is a full cache hit only when meta.json is present and
//!   `size_bytes` matches `media.bin`)
//!
//! DESIGN/PRIVACY: the cache stores ENCRYPTED (FairPlay `cbcs`) bytes only.
//! Decryption happens in memory at play time (~100x realtime). Never
//! persist decrypted FairPlay output here — the cached files are inert
//! without the per-device license material and keep the on-disk footprint
//! non-exploitable. (The pre-existing transient playback buffer
//! `tracks/<id>.m4a` that backs the seekable decoder source is unchanged
//! machinery and lives outside this cache and its budget.)
//!
//! Concurrency: only play-worker threads touch the audio cache; a global
//! mutex serializes the (fast) dir-walk prune, run once per play attempt
//! before any fetch — never per frame.

use crate::config::{fnv128, Config};
use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const GIB: u64 = 1024 * 1024 * 1024;

/// One track's fully cached material (complete entries only — see `lookup`).
pub struct TrackEntry {
    pub dir: PathBuf,
    /// Variant (media) manifest text.
    pub variant: String,
    /// CDN URL the media file was fetched from (for reference only).
    pub media_url: String,
    /// License JSON per key URI.
    pub keys: HashMap<String, String>,
    /// `media.bin` path (ENCRYPTED bytes).
    pub media: PathBuf,
}

/// Per-play payload handed to the stream fetch worker so it can tee raw
/// (encrypted) bytes and commit `meta.json` on clean EOF.
pub struct Populate {
    pub dir: PathBuf,
    pub last_played: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Meta {
    pub last_played: u64,
    pub size_bytes: u64,
    pub media_url: String,
    pub keys: Vec<String>,
}

/// First cache I/O failure logs once per process; later ones stay silent —
/// cache trouble must never break playback.
static WARNED: AtomicBool = AtomicBool::new(false);

pub fn once_warn(what: &str, e: impl std::fmt::Display) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("[cache] {what} failed (cache skipped this run): {e}");
    }
}

/// Serializes the prune dir-walk across play-worker threads.
static PRUNE_LOCK: Mutex<()> = Mutex::new(());

pub struct AudioCache {
    root: PathBuf,
    /// Byte budget; 0 disables the audio cache entirely.
    budget: u64,
}

impl AudioCache {
    pub fn new(cfg: &Config) -> Self {
        Self {
            root: cfg.cache_dir.join("audio"),
            budget: cfg.cache_size_gb.saturating_mul(GIB),
        }
    }

    pub fn enabled(&self) -> bool {
        self.budget != 0
    }

    /// Sanitized track dir; `None` for anything that could escape the root.
    fn dir_of(&self, adam_id: &str) -> Option<PathBuf> {
        if !self.enabled()
            || adam_id.is_empty()
            || adam_id.contains('/')
            || adam_id.contains('\\')
            || adam_id.contains("..")
        {
            return None;
        }
        Some(self.root.join(adam_id))
    }

    /// Complete cache entry for `adam_id`, or None (disabled / incomplete /
    /// corrupt). A truncated `media.bin` never validates against meta.json.
    pub fn lookup(&self, adam_id: &str) -> Option<TrackEntry> {
        let dir = self.dir_of(adam_id)?;
        let meta = read_meta(&dir)?;
        let media = dir.join("media.bin");
        if std::fs::metadata(&media).ok()?.len() != meta.size_bytes {
            return None;
        }
        let variant = std::fs::read_to_string(dir.join("variant.m3u8")).ok()?;
        let mut keys = HashMap::new();
        for uri in &meta.keys {
            let json = std::fs::read_to_string(key_path(&dir, uri)).ok()?;
            keys.insert(uri.clone(), json);
        }
        Some(TrackEntry {
            dir,
            variant,
            media_url: meta.media_url,
            media,
            keys,
        })
    }

    /// Populate target for a fresh download; None when disabled or the id
    /// is unusable — callers then run the plain network path.
    pub fn populate_target(&self, adam_id: &str, last_played: u64) -> Option<Populate> {
        let dir = self.dir_of(adam_id)?;
        Some(Populate { dir, last_played })
    }

    /// Refresh `last_played` on a complete entry (every play, including a
    /// play served from cache). Missing meta → no-op; the dir still evicts
    /// first (recency 0).
    pub fn touch(&self, adam_id: &str, now: u64) {
        let Some(dir) = self.dir_of(adam_id) else { return };
        if let Some(mut m) = read_meta(&dir) {
            m.last_played = now;
            if let Err(e) = commit_meta(&dir, &m) {
                once_warn("audio cache touch", e);
            }
        }
    }

    /// Drop the completeness marker so a failed cached playback self-heals
    /// to a fresh network fetch on the next attempt.
    pub fn invalidate(&self, adam_id: &str) {
        let Some(dir) = self.dir_of(adam_id) else { return };
        let _ = std::fs::remove_file(dir.join("meta.json"));
    }

    /// Whole-dir LRU prune, once per play attempt, before any fetch. `keep`
    /// (the track about to play) is exempt so an in-flight download can't
    /// be evicted from under itself.
    pub fn prune(&self, keep: &str) {
        if !self.enabled() {
            return;
        }
        let _g = PRUNE_LOCK.lock();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut items = Vec::new();
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let dir = e.path();
            // Pure dir walk: unresolvable ids still evict fine (recency 0).
            let last_played = read_meta(&dir).map(|m| m.last_played).unwrap_or(0);
            items.push(EvictItem {
                id: e.file_name().to_string_lossy().into_owned(),
                last_played,
                size: dir_size(&dir),
            });
        }
        let total: u64 = items.iter().map(|i| i.size).sum();
        if total <= self.budget {
            return;
        }
        for id in eviction_plan(&items, self.budget, keep) {
            if let Err(e) = std::fs::remove_dir_all(self.root.join(&id)) {
                once_warn("audio cache evict", e);
                return;
            }
        }
    }
}

// ---- helpers shared with stream.rs (the fetch worker) ----

pub fn key_path(dir: &Path, uri: &str) -> PathBuf {
    dir.join(format!("key_{}.json", fnv128(uri.as_bytes())))
}

/// Store master/variant manifest texts (best-effort).
pub fn store_manifests(dir: &Path, master: Option<&str>, variant: &str) {
    let r = (|| -> Result<()> {
        std::fs::create_dir_all(dir)?;
        if let Some(m) = master {
            write_atomic(&dir.join("master.m3u8"), m.as_bytes())?;
        }
        write_atomic(&dir.join("variant.m3u8"), variant.as_bytes())?;
        Ok(())
    })();
    if let Err(e) = r {
        once_warn("audio cache manifest store", e);
    }
}

/// Store a license response for a key URI (best-effort).
pub fn store_key(dir: &Path, uri: &str, json: &str) {
    let r = (|| -> Result<()> {
        std::fs::create_dir_all(dir)?;
        write_atomic(&key_path(dir, uri), json.as_bytes())
    })();
    if let Err(e) = r {
        once_warn("audio cache key store", e);
    }
}

/// Completeness marker: written only after a full clean download (or a
/// `touch`), atomically via rename.
pub fn commit_meta(dir: &Path, meta: &Meta) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec(meta)?;
    write_atomic(&dir.join("meta.json"), &json)
}

pub fn read_meta(dir: &Path) -> Option<Meta> {
    let raw = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_atomic(p: &Path, data: &[u8]) -> Result<()> {
    let tmp = p.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, p)?;
    Ok(())
}

// ---- eviction math (pure, injectable clock via stored recency) ----

pub struct EvictItem {
    pub id: String,
    pub last_played: u64,
    pub size: u64,
}

/// Pure eviction decision: delete whole track dirs, oldest `last_played`
/// first (unresolvable = 0 = first), until the total fits `budget`.
/// `keep` is never selected. Returns the ids to delete.
pub fn eviction_plan(items: &[EvictItem], budget: u64, keep: &str) -> Vec<String> {
    let mut order: Vec<&EvictItem> = items.iter().filter(|i| i.id != keep).collect();
    order.sort_by(|a, b| (a.last_played, &a.id).cmp(&(b.last_played, &b.id)));
    let mut total: u64 = items.iter().map(|i| i.size).sum();
    let mut out = Vec::new();
    for it in order {
        if total <= budget {
            break;
        }
        total = total.saturating_sub(it.size);
        out.push(it.id.clone());
    }
    out
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                n += dir_size(&e.path());
            } else {
                n += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("amc-acache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn item(id: &str, last: u64, size: u64) -> EvictItem {
        EvictItem { id: id.into(), last_played: last, size }
    }

    /// The user's exact scenario: play A, then B, then A — B is the first
    /// dir pruned once the budget overflows.
    #[test]
    fn aba_recency_prunes_b_first() {
        // A played at t=100, B at t=200, A replayed at t=300 → A is newest.
        let items = vec![item("a", 300, 6), item("b", 200, 6)];
        // Budget fits one 6-byte track only.
        let plan = eviction_plan(&items, 6, "");
        assert_eq!(plan, vec!["b".to_string()]);
    }

    #[test]
    fn budget_math_with_partial_fills() {
        // 3 + 4 + 5 = 12 > 8: oldest (x) goes, still 9 > 8, y goes, 5 ≤ 8.
        let items = vec![
            item("z", 3, 5),
            item("x", 1, 3),
            item("y", 2, 4),
        ];
        assert_eq!(eviction_plan(&items, 8, ""), vec!["x", "y"]);
        // Exactly at budget: nothing evicted.
        assert_eq!(eviction_plan(&items, 12, ""), Vec::<String>::new());
        // Oversized lone dir still evicts.
        let big = vec![item("big", 9, 100)];
        assert_eq!(eviction_plan(&big, 8, ""), vec!["big"]);
    }

    #[test]
    fn keep_exempts_current_track_from_eviction() {
        let items = vec![item("cur", 1, 6), item("old", 2, 6)];
        assert_eq!(eviction_plan(&items, 6, "cur"), vec!["old"]);
    }

    fn seed_track(root: &Path, id: &str, last: u64, size: usize) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("media.bin"), vec![0u8; size]).unwrap();
        std::fs::write(dir.join("variant.m3u8"), "#EXTM3U\n").unwrap();
        commit_meta(
            &dir,
            &Meta { last_played: last, size_bytes: size as u64, media_url: "https://cdn/x".into(), keys: vec![] },
        )
        .unwrap();
    }

    #[test]
    fn prune_on_disk_evicts_oldest_played_first() {
        let root = tmp("prune");
        seed_track(&root, "a", 100, 4096);
        seed_track(&root, "b", 200, 4096);
        seed_track(&root, "c", 300, 4096);
        // touch A (replay): A becomes newest; budget exactly fits the two
        // newest dirs (sizes include meta.json, as eviction accounts them).
        let cache = AudioCache { root: root.clone(), budget: 1 << 30 };
        cache.touch("a", 400);
        let fit = dir_size(&root.join("a")) + dir_size(&root.join("c"));
        let cache = AudioCache { root: root.clone(), budget: fit };
        cache.prune("");
        assert!(!root.join("b").exists(), "B (oldest replay) evicted first");
        assert!(root.join("a").exists() && root.join("c").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incomplete_dirs_fail_lookup_and_evict_first() {
        let root = tmp("partial");
        seed_track(&root, "full", 100, 512);
        // Partial dir: media without a completeness marker.
        let pd = root.join("part");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(pd.join("media.bin"), vec![0u8; 512]).unwrap();

        let cache = AudioCache { root: root.clone(), budget: 1024 };
        assert!(cache.lookup("full").is_some());
        assert!(cache.lookup("part").is_none(), "no meta.json → incomplete");

        // Partial dirs have recency 0 → evicted before everything.
        cache.prune("");
        assert!(!pd.exists());

        // Truncated media fails the size check.
        std::fs::write(root.join("full/media.bin"), vec![0u8; 100]).unwrap();
        assert!(cache.lookup("full").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disabled_cache_is_inert_and_writes_nothing() {
        let root = tmp("disabled");
        let cache = AudioCache { root: root.clone(), budget: 0 };
        assert!(!cache.enabled());
        assert!(cache.lookup("x").is_none());
        assert!(cache.populate_target("x", 1).is_none());
        cache.prune("");
        cache.touch("x", 1);
        cache.invalidate("x");
        assert!(!root.join("audio").exists(), "disabled cache must not create dirs");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn complete_entry_roundtrips_with_keys_and_manifests() {
        let root = tmp("roundtrip");
        let cache = AudioCache { root: root.clone(), budget: 1 << 30 };
        let dir = root.join("1440801913");
        std::fs::create_dir_all(&dir).unwrap();
        store_manifests(&dir, Some("master"), "variant");
        store_key(&dir, "skd://itunes.apple.com/P1440801913/s1/e1", "{\"k\":1}");
        std::fs::write(dir.join("media.bin"), b"ENCRYPTED").unwrap();
        commit_meta(
            &dir,
            &Meta {
                last_played: 7,
                size_bytes: 9,
                media_url: "https://cdn/media.m3u8".into(),
                keys: vec!["skd://itunes.apple.com/P1440801913/s1/e1".into()],
            },
        )
        .unwrap();

        let e = cache.lookup("1440801913").expect("complete entry");
        assert_eq!(e.variant, "variant");
        assert_eq!(
            std::fs::read_to_string(dir.join("master.m3u8")).unwrap(),
            "master",
            "master manifest stored alongside"
        );
        assert_eq!(e.keys.values().next().map(String::as_str), Some("{\"k\":1}"));
        assert_eq!(std::fs::read(&e.media).unwrap(), b"ENCRYPTED");

        cache.touch("1440801913", 99);
        assert_eq!(read_meta(&dir).unwrap().last_played, 99);
        cache.invalidate("1440801913");
        assert!(cache.lookup("1440801913").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_escape_rejected() {
        let root = tmp("escape");
        let cache = AudioCache { root: root.clone(), budget: 1 << 30 };
        assert!(cache.dir_of("../evil").is_none());
        assert!(cache.dir_of("a/b").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
