//! Best-effort play reporting to Apple's Universal Activity Service, so plays
//! land in the user's Recently Played / play counts.
//!
//! Design: `App` emits segment events (PLAY_START / PLAY_END, one per user
//! action) into a bounded-order channel; a single OS thread drains them and
//! POSTs sequentially (order matters: START must precede END), one event per
//! POST, no retry — failures are swallowed. Auth (bearer, media-user-token,
//! storefront) is resolved lazily on that thread; the UI thread never blocks.
//!
//! Reporting is gated at creation (config `report_plays`, a media-user-token,
//! a real audio sink) and re-gated per send; gates dropped mid-track silently
//! discard the open segment. The decode-only fallback (VPS, CI) has no sink
//! and therefore never reports.
//!
//! Logging: one line per successfully POSTed event (`[report] …`), nothing on
//! failures.

use crate::api::{Api, PlayEvent};
use crate::config::Config;
use std::sync::mpsc::{channel, Sender};

pub struct PlayReporter {
    tx: Option<Sender<PlayEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PlayReporter {
    /// The full reporting gate: config switch, a media-user-token, and a real
    /// audio sink (the decode-only fake clock must never report).
    pub fn enabled(cfg: &Config, has_sink: bool) -> bool {
        cfg.report_plays
            && cfg
                .media_user_token
                .as_deref()
                .is_some_and(|t| !t.is_empty())
            && has_sink
    }

    /// Spawn the drain thread. Call only when [`Self::enabled`] returned true.
    pub fn spawn(cfg: Config) -> Self {
        let (tx, rx) = channel::<PlayEvent>();
        let worker = std::thread::Builder::new()
            .name("report".into())
            .spawn(move || {
                // Same pattern as the play-worker threads: the thread owns its
                // Api. Shorter total timeout so app exit never blocks long on
                // the final POST.
                let Ok(api) = Api::with_timeouts(
                    cfg,
                    std::time::Duration::from_secs(5),
                    std::time::Duration::from_secs(8),
                ) else {
                    return;
                };
                while let Ok(mut ev) = rx.recv() {
                    stamp(&mut ev, &api);
                    if api.report_activity(std::slice::from_ref(&ev)).is_ok() {
                        log_event(&ev);
                    }
                    // best-effort: failures are dropped without retry
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            worker,
        }
    }

    /// Queue one event. Never blocks the UI thread; silently dropped when the
    /// reporter is off.
    pub fn send(&self, ev: PlayEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }

    /// Exit flush: drain everything queued (including `last`), then wait for
    /// the thread — bounded by its 8 s request timeout.
    pub fn shutdown(mut self, last: Option<PlayEvent>) {
        if let (Some(tx), Some(ev)) = (&self.tx, last) {
            let _ = tx.send(ev);
        }
        self.tx = None; // closing the channel ends the loop once drained
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Fill the per-send fields the UI cannot know: live tokens, storefront,
/// local UTC offset, fresh persistent-id.
fn stamp(ev: &mut PlayEvent, api: &Api) {
    ev.developer_token = api.bearer().unwrap_or_default();
    ev.user_token = api
        .cfg
        .media_user_token
        .clone()
        .unwrap_or_default();
    ev.store_front = api.cfg.storefront.to_lowercase();
    ev.utc_offset_seconds = utc_offset_seconds();
    ev.milliseconds_since_play = 1;
    ev.persistent_id = fresh_event_id();
}

/// Seconds EAST of UTC (Apple's convention; `getTimezoneOffset` is minutes
/// west, so US timezones send negative values).
fn utc_offset_seconds() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}

/// Fresh random 8-byte (16-hex) event id. Apple rotates this per event; a
/// thread-local xorshift keeps consecutive events (a scrub pair lands in the
/// same millisecond) distinct without a rand dependency.
fn fresh_event_id() -> String {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|st| {
        if st.get() == 0 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                ^ ((std::process::id() as u64) << 32);
            st.set(seed | 1);
        }
        let mut s = st.get();
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        st.set(s);
        format!("{s:016x}")
    })
}

fn log_event(ev: &PlayEvent) {
    if ev.event_type == PlayEvent::EVENT_TYPE_PLAY_START {
        eprintln!(
            "[report] START adam={} pos={}ms",
            ev.ids.subscription_adam_id, ev.start_position_ms
        );
    } else {
        eprintln!(
            "[report] END adam={} reason={} pos={}ms",
            ev.ids.subscription_adam_id,
            ev.end_reason_type.unwrap_or(0),
            ev.end_position_ms.unwrap_or(0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_are_16_lowercase_hex_and_unique() {
        let a = fresh_event_id();
        let b = fresh_event_id();
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(b.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "consecutive events must rotate the persistent-id");
    }

    #[test]
    fn gating_requires_all_three_conditions() {
        let mut cfg = Config::default();
        assert!(!PlayReporter::enabled(&cfg, true), "no media-user-token");
        cfg.media_user_token = Some("tok".into());
        assert!(!PlayReporter::enabled(&cfg, false), "no sink (decode-only)");
        assert!(PlayReporter::enabled(&cfg, true));
        cfg.report_plays = false;
        assert!(!PlayReporter::enabled(&cfg, true), "config switch off");
    }
}

/// One-shot live proof: sends exactly ONE complete play pair (START hint 1,
/// immediate natural END at full duration) for a public Daft Punk catalog
/// track and asserts both POSTs succeed. User-approved single library-visible
/// test play; never run in CI.
#[cfg(test)]
mod live_tests {
    use crate::api::{Api, PlayEvent, PlayMode};

    #[test]
    #[ignore = "sends one real play report to Apple; run explicitly"]
    fn report_live_smoke() {
        let cfg = crate::config::Config::load().expect("config");
        assert!(
            cfg.media_user_token
                .as_deref()
                .is_some_and(|t| !t.is_empty()),
            "media-user-token must be configured"
        );
        let api = Api::new(cfg).expect("api");
        let t = api
            .search("daft punk one more time", 1)
            .expect("catalog search")
            .tracks
            .into_iter()
            .next()
            .expect("a Daft Punk catalog track");
        eprintln!(
            "live smoke: reporting {} — {} [adam {}] ({} ms)",
            t.name, t.artist, t.id, t.duration_ms
        );
        let mode = PlayMode { auto: 1, repeat: 1, shuffle: 1 };
        let start = PlayEvent::start(
            &t.id,
            t.duration_ms,
            0,
            PlayEvent::HINT_CONTAINER_CHANGED,
            mode.clone(),
            None,
        );
        let end = PlayEvent::end(
            &t.id,
            t.duration_ms,
            0,
            t.duration_ms,
            PlayEvent::END_NATURAL,
            mode,
            None,
        );
        api.report_activity(&[start])
            .expect("PLAY_START POST should succeed");
        eprintln!("live smoke: START accepted");
        api.report_activity(&[end])
            .expect("PLAY_END POST should succeed");
        eprintln!("live smoke: END accepted — pair complete");
    }
}
