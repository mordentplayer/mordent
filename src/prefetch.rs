//! Background sequential prefetch of the up-next queue.
//!
//! One worker thread (no fan-out — polite to the wrapper/CDN) walks the
//! queue entries after the pointer, nearest first, fetching one track fully
//! at a time into the audio cache via the regular fetch pipeline
//! ([`crate::stream::populate_track`]). Tracks that are already
//! cache-complete are skipped, so the second play of a queue costs nothing.
//!
//! LIFECYCLE: the thread is spawned once with the app and lives until
//! process exit (every exit path is `std::process::exit`, so no graceful
//! shutdown exists). It parks on a condvar whenever there is nothing to
//! fetch.
//!
//! RE-TARGETING: the UI thread replaces the up-next snapshot on every
//! queue/pointer/playback change (`scan`). The worker re-checks the epoch
//! before each track and every ~`POLL` while a fetch runs. A track that
//! left the queue is abandoned — the cancel flag stops its fetch at the
//! next fragment boundary and an aborted fetch never commits a cache
//! entry, so the partial dir just evicts first. An in-flight fetch is
//! kept alive only while its track is still the FIRST one the fresh
//! snapshot needs (the common natural-advance case: the next track's
//! prefetch survives the pointer move instead of restarting); anything
//! else is aborted so the new nearest-first track gets the bandwidth.
//!
//! EVICTION: before each fetch the worker prunes with the keep set
//! current + up-next (never evicts near-term music), and hands the fetch
//! a strictly increasing `last_played` stamp so that once prefetched
//! entries leave the protected set they still evict in queue order even
//! when the whole queue was prefetched within one wall-clock second.
//!
//! FAILURES: a track whose fetch fails is put on a short cooldown so one
//! bad/unavailable id cannot stall the rest of the queue; the cooldown
//! expiry also wakes the parked worker to retry (wrapper came back).

use crate::acache::AudioCache;
use crate::config::Config;
use crate::queuemodel::Queue;
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often the worker checks for scan changes / fetch completion while
/// a fetch runs. The fetch itself is aborted at fragment granularity by
/// the cancel flag; this only bounds the decision latency.
const POLL: Duration = Duration::from_millis(100);

/// Cool-down for a track whose prefetch failed; retries then re-walk the
/// same snapshot (also covers the wrapper being down entirely).
const COOLDOWN: Duration = Duration::from_secs(60);

struct Scan {
    /// `(adam id, track name)` after the pointer, nearest first.
    up_next: Vec<(String, String)>,
    /// Current entry's id (protected from eviction; never prefetched).
    current: Option<String>,
    /// Bumped by every [`Prefetcher::scan`].
    epoch: u64,
}

struct Sh {
    st: Mutex<Scan>,
    cv: Condvar,
}

/// Handle the UI thread holds; cheap to store, no-op when cache is off.
pub struct Prefetcher {
    sh: Arc<Sh>,
    enabled: bool,
}

impl Prefetcher {
    /// Spawns the single worker when the audio cache is enabled; with
    /// `cache_size_gb = 0` this is an inert stub — no thread, `scan` is a
    /// no-op, the feature has zero overhead.
    pub fn new(cfg: &Config) -> Self {
        let enabled = AudioCache::new(cfg).enabled();
        let sh = Arc::new(Sh {
            st: Mutex::new(Scan { up_next: Vec::new(), current: None, epoch: 0 }),
            cv: Condvar::new(),
        });
        if enabled {
            let cfg = cfg.clone();
            let sh = Arc::clone(&sh);
            let _ = std::thread::Builder::new()
                .name("amc-prefetch".into())
                .spawn(move || worker(cfg, sh));
        }
        Self { sh, enabled }
    }

    /// Queue/pointer/playback changed: replace the up-next snapshot. The
    /// worker notices on its next check and re-targets (see module docs).
    pub fn scan(&self, queue: &Queue) {
        if !self.enabled {
            return;
        }
        let mut st = self.sh.st.lock();
        st.up_next = queue.prefetch_list();
        st.current = queue.current().map(|t| t.id.clone());
        st.epoch += 1;
        drop(st);
        self.sh.cv.notify_one();
    }
}

/// One populate-only fetch in progress.
struct InFlight {
    id: String,
    name: String,
    src: crate::stream::ProgressiveSource,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

fn worker(cfg: Config, sh: Arc<Sh>) {
    let audio = AudioCache::new(&cfg);
    let mut consumed = 0u64; // epoch of the snapshot being walked
    let mut last_stamp = 0u64;
    let mut cooldown: HashMap<String, Instant> = HashMap::new();
    let mut inflight: Option<InFlight> = None;
    loop {
        // ---- resolve the in-flight fetch, if any ----
        if let Some(f) = inflight.as_ref() {
            // A newer snapshot supersedes the walk: keep the fetch only
            // while its track is still the first one the fresh snapshot
            // needs; otherwise abort it (never commits → evicts first).
            let superseded = sh.st.lock().epoch != consumed;
            if superseded {
                let st = sh.st.lock();
                let still_first = st
                    .up_next
                    .iter()
                    .find(|(nid, _)| needs_fetch(&audio, nid, &cooldown))
                    .is_some_and(|(nid, _)| nid == &f.id);
                if !still_first {
                    f.cancel.store(true, Ordering::Relaxed);
                }
            }
            match f.src.is_done() {
                Ok(true) => {
                    if let Some(e) = audio.lookup(&f.id) {
                        let size = std::fs::metadata(&e.media).map(|m| m.len()).unwrap_or(0);
                        eprintln!("[cache] prefetched {} ({})", f.name, fmt_size(size));
                    }
                    inflight = None;
                }
                Err(_) => {
                    // failed fetch: no marker was committed; back off so one
                    // bad track cannot stall the rest of the queue
                    cooldown.insert(f.id.clone(), Instant::now());
                    inflight = None;
                }
                Ok(false) => std::thread::sleep(POLL),
            }
            continue;
        }
        // ---- adopt the newest snapshot (parks until there is work) ----
        let (list, current) = adopt(&sh, &mut consumed, &cooldown);
        cooldown.retain(|_, t| t.elapsed() < COOLDOWN);
        // prune keep set: current + the whole up-next list
        let mut keep = Vec::with_capacity(list.len() + 1);
        if let Some(c) = &current {
            keep.push(c.clone());
        }
        keep.extend(list.iter().map(|(id, _)| id.clone()));
        // ---- walk nearest-first; any scan restarts from the new list ----
        for (id, name) in &list {
            if sh.st.lock().epoch != consumed {
                break;
            }
            if audio.lookup(id).is_some() {
                continue; // already cache-complete
            }
            if cooldown.contains_key(id) {
                continue; // failed recently
            }
            audio.prune(&keep);
            let stamp = next_stamp(&mut last_stamp, crate::config::unix_now());
            match crate::stream::populate_track(&cfg, id, stamp) {
                Err(_) => {
                    // disabled cache / wrapper down / bad manifest — retry
                    // after the cooldown, silently (cache trouble must never
                    // make noise beyond the completed-prefetch lines)
                    cooldown.insert(id.clone(), Instant::now());
                }
                Ok(src) => {
                    inflight = Some(InFlight {
                        id: id.clone(),
                        name: name.clone(),
                        cancel: src.cancel_flag(),
                        src,
                    });
                    break; // back to the top: wait for it
                }
            }
        }
    }
}

/// Block until there is a walk to run: a scan newer than `consumed`, or a
/// failed track's retry cooldown expiring (re-walk the same snapshot).
fn adopt(
    sh: &Sh,
    consumed: &mut u64,
    cooldown: &HashMap<String, Instant>,
) -> (Vec<(String, String)>, Option<String>) {
    let mut st = sh.st.lock();
    loop {
        if st.epoch != *consumed {
            *consumed = st.epoch;
            return (st.up_next.clone(), st.current.clone());
        }
        match next_retry(cooldown) {
            None => sh.cv.wait(&mut st),
            Some(d) => {
                let t = sh.cv.wait_for(&mut st, d);
                if t.timed_out() {
                    // a cooldown ran out with no newer scan: retry
                    return (st.up_next.clone(), st.current.clone());
                }
            }
        }
    }
}

/// Time until the earliest failed track becomes retryable.
fn next_retry(cooldown: &HashMap<String, Instant>) -> Option<Duration> {
    cooldown
        .values()
        .map(|t| COOLDOWN.saturating_sub(t.elapsed()).max(Duration::from_millis(1)))
        .min()
}

/// A track needs a fetch when it is neither cache-complete nor cooling down.
fn needs_fetch(
    audio: &AudioCache,
    id: &str,
    cooldown: &HashMap<String, Instant>,
) -> bool {
    !cooldown.contains_key(id) && audio.lookup(id).is_none()
}

/// Strictly increasing completion stamp. Wall-clock seconds alone would tie
/// when a queue prefetches within one second, and eviction then falls back
/// to id order instead of queue order.
fn next_stamp(last: &mut u64, now: u64) -> u64 {
    let s = now.max(last.saturating_add(1));
    *last = s;
    s
}

fn fmt_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acache::Meta;
    use crate::config::Config;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("amc-prefetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stamps_increase_even_within_one_clock_second() {
        let mut last = 0;
        assert_eq!(next_stamp(&mut last, 100), 100);
        assert_eq!(next_stamp(&mut last, 100), 101, "same-second fetch must not tie");
        assert_eq!(next_stamp(&mut last, 100), 102);
        assert_eq!(next_stamp(&mut last, 500), 500, "later wall clock wins");
        assert_eq!(next_stamp(&mut last, 500), 501);
    }

    #[test]
    fn size_formatting() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(40 * 1024), "40 KiB");
        assert_eq!(fmt_size(36 * 1024 * 1024), "36.0 MiB");
    }

    #[test]
    fn needs_fetch_skips_complete_and_cooldown_entries() {
        let root = tmp("needs");
        let cfg = Config { cache_dir: root.clone(), cache_size_gb: 1, ..Config::default() };
        let audio = AudioCache::new(&cfg);
        // complete entry (marker + matching media size) under <cache>/audio/
        let dir = root.join("audio/1440801913");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("media.bin"), b"ENCRYPTED").unwrap();
        std::fs::write(dir.join("variant.m3u8"), "#EXTM3U\n").unwrap();
        crate::acache::commit_meta(
            &dir,
            &Meta { last_played: 1, size_bytes: 9, media_url: "https://cdn/x".into(), keys: vec![] },
        )
        .unwrap();

        let cooling: HashMap<String, Instant> =
            [("777".to_string(), Instant::now())].into_iter().collect();

        assert!(!needs_fetch(&audio, "1440801913", &HashMap::new()), "complete → skip");
        assert!(needs_fetch(&audio, "42", &HashMap::new()), "missing → fetch");
        assert!(!needs_fetch(&audio, "777", &cooling), "cooldown → skip");
        assert!(needs_fetch(&audio, "777", &HashMap::new()));
        // (expired cooldowns are GC'd by the worker before the re-walk, so
        // needs_fetch only ever sees live cooldowns — expiry wakeup is
        // adopt()/next_retry's job, covered by retry_timer test)
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retry_timer_is_the_earliest_cooldown_expiry() {
        let mut cd: HashMap<String, Instant> = HashMap::new();
        assert_eq!(next_retry(&cd), None, "nothing failed → park forever");
        cd.insert("a".into(), Instant::now() - Duration::from_secs(59));
        cd.insert("b".into(), Instant::now() - Duration::from_secs(10));
        let d = next_retry(&cd).unwrap();
        assert!(d <= Duration::from_secs(1) && d > Duration::ZERO, "a expires first: {d:?}");
    }

    #[test]
    fn disabled_cache_never_spawns_a_worker_and_scan_is_inert() {
        let cfg = Config {
            cache_dir: tmp("disabled"),
            cache_size_gb: 0,
            ..Config::default()
        };
        let p = Prefetcher::new(&cfg);
        assert!(!p.enabled);
        // must be a cheap no-op, not a panic — and the worker's snapshot
        // stays untouched
        let q = Queue::default();
        p.scan(&q);
        assert!(p.sh.st.lock().up_next.is_empty());
        let _ = std::fs::remove_dir_all(tmp("disabled"));
    }

    /// Live proof (never run in CI): takes three Daft Punk catalog tracks as
    /// a stand-in up-next list and drives the worker's populate path
    /// directly — each track must end up a COMPLETE cache entry (lookup
    /// finds it), with strictly increasing `last_played` stamps so eviction
    /// keeps queue order. Requires the wrapper to be up.
    #[test]
    #[ignore = "downloads real tracks from the Apple CDN via the wrapper; run explicitly"]
    fn live_sequential_prefetch_populates_up_next() {
        let cfg = Config::load().expect("config");
        let api = crate::api::Api::new(cfg.clone()).expect("api");
        let tracks = api.search("daft punk", 10).expect("catalog search").tracks;
        assert!(tracks.len() >= 3, "need 3 catalog tracks");
        let audio = AudioCache::new(&cfg);
        let mut last = 0u64;
        let mut fresh_stamps: Vec<u64> = Vec::new();
        for t in &tracks[..3] {
            if audio.lookup(&t.id).is_some() {
                eprintln!("live: already cached — {} — {}", t.name, t.artist);
                continue;
            }
            let stamp = next_stamp(&mut last, crate::config::unix_now());
            let src = crate::stream::populate_track(&cfg, &t.id, stamp)
                .expect("populate fetch (wrapper must be up)");
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                match src.is_done() {
                    Ok(true) => break,
                    Err(e) => panic!("prefetch of {} failed: {e:#}", t.name),
                    Ok(false) => {
                        assert!(
                            Instant::now() < deadline,
                            "prefetch of {} timed out",
                            t.name
                        );
                        std::thread::sleep(POLL);
                    }
                }
            }
            let entry = audio
                .lookup(&t.id)
                .unwrap_or_else(|| panic!("{} completed without committing", t.name));
            let size = std::fs::metadata(&entry.media).unwrap().len();
            let stamp_back = crate::acache::read_meta(&cfg.cache_dir.join("audio").join(&t.id))
                .expect("meta readable")
                .last_played;
            fresh_stamps.push(stamp_back);
            eprintln!("live: prefetched {} — {} ({})", t.name, t.artist, fmt_size(size));
        }
        let inc = fresh_stamps.windows(2).all(|w| w[0] < w[1]);
        assert!(inc, "stamps must preserve queue order: {fresh_stamps:?}");
        // all three tracks are complete cache entries now
        for t in &tracks[..3] {
            assert!(audio.lookup(&t.id).is_some(), "{} not cache-complete", t.name);
        }
    }
}
