//! Playback supervisor: audio ownership + the advance state machine, fully
//! off the UI frame loop.
//!
//! WHY: on some Wayland compositors (niri family) a redraw for an unfocused
//! window is withheld, which previously silenced every driver that lived
//! inside `update()` — auto-advance then waited for window focus. Two rounds
//! of repaint fixes did not fully cover it, so playback now lives entirely
//! outside the frame loop: the UI is a renderer + command sender, and this
//! thread runs the player. The UI can be hidden, frozen or never scheduled
//! again — playback, natural advance, play reporting and MPRIS keep working.
//!
//! OWNERSHIP (the rodio facts this is built on):
//! * `cpal::OutputStream` is `!Send` → created here, on this thread, and
//!   never leaves it.
//! * `OutputStreamHandle` is `Send + Sync + Clone`, and `Sink::try_new`
//!   works from any thread → the sink is created here too and simply stays.
//! * `PacketSource` is `Send` (play-workers already crossed threads) →
//!   prepared sources arrive as [`Command::Source`].
//!
//! SHARED STATE with the UI:
//! * `Arc<Mutex<Queue>>` — the queue MOVES behind a lock. The UI mutates list
//!   content (append / insert-after-current / reorder / remove / shuffle /
//!   pointer jump) under the lock; the supervisor advances the pointer on
//!   natural advance and on supervisor-side Next/Prev. Each side re-scans the
//!   prefetcher after its own mutations, so pointer consistency is airtight:
//!   every pointer move is paired with a playback action or a pure list op,
//!   never both from different threads at once.
//! * `Arc<Mutex<PbSnapshot>>` — ONE publication of playback state (now
//!   playing, position, duration, paused, repeat, shuffle, volume, …) read by
//!   the UI renderer, the MPRIS publisher and the tray.
//!
//! COMMANDS arrive over a `std::sync::mpsc` channel (same shape as the MPRIS
//! command channel it replaces). Play-workers send their prepared sources
//! through the SAME channel, so the loop blocks on one receiver and wakes for
//! exactly the things that matter.
//!
//! REPORT SEGMENTS (the PLAY_START/PLAY_END table) are emitted here, next to
//! the transitions that produce them; the pure pause/scrub decisions
//! (`applied_source_pause_state`, `seek_scrub_events`) moved with them,
//! unit-tested in this file. Decode-only mode (no audio device) runs the same
//! state machine on a fake clock — that is the MORDENT_SELFTEST path.
//!
//! The supervisor also drains the MPRIS command receiver, so media keys work
//! while the window is hidden or the frame loop is parked.

use crate::api::{PlayEvent, Track};
use crate::config::Config;
use crate::mpris;
use crate::player::PacketSource;
use crate::prefetch::Prefetcher;
use crate::queuemodel::{Advance, Queue, Repeat};
use crate::report::PlayReporter;
use crate::stream;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Frame-loop tick while a track is audibly running: fake-clock/sink-idle
/// detection granularity (decode-only tracks simulate ≤ 8 s, so 100 ms is
/// plenty precise and matches the old in-frame cadence).
const TICK_AUDIBLE: Duration = Duration::from_millis(100);
/// Tick while paused / preparing / stopped: only command latency (MPRIS Next
/// while stopped, unpause while paused) is bounded by this.
const TICK_IDLE: Duration = Duration::from_millis(250);

/// What kind of source application a play performs: it decides both the
/// report segment semantics and the pause state on arrival. (Moved from
/// ui.rs together with its decision fns and tests.)
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum PlayKind {
    /// User-initiated fresh segment (click-replace, album/queue/row play,
    /// ⏭/⏮ to another entry): PLAY_START at 0; audible unless the user had
    /// explicitly paused right before.
    NewSegment,
    /// Natural-end handover to the next queue entry: PLAY_START at 0;
    /// always audible.
    AutoAdvance,
    /// Reposition inside the current track (seek bar, ±5 s, ⏮ restart):
    /// no PLAY_START (the segment continues, or the resume opens it); the
    /// exact pre-seek pause state is restored.
    SeekRestart,
}

/// Pause state a freshly applied source must start with (pure; unit-tested).
///
/// * `pre_pause` — did the user explicitly pause right before this application
/// * `user_initiated` — click/shortcut/MPRIS replace or seek (vs programmatic)
/// * `auto_advance` — natural-end handover to the next queue entry
///
/// | transition                        | audible on arrival?    |
/// |-----------------------------------|------------------------|
/// | auto-advance (any)                | yes                    |
/// | replace/seek, user was playing    | yes                    |
/// | replace/seek, user was paused     | no (paused-on-arrival) |
///
/// The supervisor applies the decision both at application time (rodio 0.20's
/// `Sink::clear()` also pauses the sink — undo it unless `true` is returned)
/// and on source arrival.
fn applied_source_pause_state(pre_pause: bool, _user_initiated: bool, auto_advance: bool) -> bool {
    !auto_advance && pre_pause
}

/// Whether a user seek emits the scrub pair (pure; unit-tested): END(12) at
/// the old position + END(13) at the new one — only while a segment is open
/// AND audibly playing (Cider: the segment continues, no new START). A paused
/// seek repositions silently; the pause already closed the segment and the
/// resume START carries the seeked position. Returns the positions to report.
fn seek_scrub_events(
    report_open: bool,
    playing: bool,
    old_ms: u64,
    new_ms: u64,
) -> Option<(u64, u64)> {
    (report_open && playing).then_some((old_ms, new_ms))
}

/// The `(start, end)` each event of a scrub pair carries: END(12) keeps the
/// open segment's original start `[seg_start..old]`, END(13) chains from the
/// scrub-begin position `[old..new]` — "each event's start-position is the
/// previous event's end-position" (Cider `ws_204_controller.ts:17,368`). Without
/// the chain the pair reads as overlapping instead of contiguous.
fn scrub_pair_positions(seg_start_ms: u64, old_ms: u64, new_ms: u64) -> ((u64, u64), (u64, u64)) {
    ((seg_start_ms, old_ms), (old_ms, new_ms))
}

/// One publication of playback state; read by the UI renderer, the MPRIS
/// publisher and the tray. The supervisor re-publishes after every command,
/// every source arrival and every tick (the position moves).
#[derive(Clone)]
pub struct PbSnapshot {
    /// Now playing (None = stopped/idle).
    pub track: Option<Track>,
    /// Elapsed time of the current track (fake clock in decode-only mode).
    pub position: Duration,
    /// What the progress bar should treat as 100%: the real duration, or the
    /// decode-only simulation cap (≤ 8 s).
    pub duration: Duration,
    /// Paused flag; only meaningful while [`Self::has_track`].
    pub paused: bool,
    /// A worker is still building the source for the current track.
    pub preparing: bool,
    pub has_track: bool,
    /// Sticky session flag: at least one track has become current since launch
    /// (never cleared, even when playback stops). Drives the window-close
    /// policy in `ui::update`: no track ever played → quit; otherwise → close
    /// to tray where the platform can hide the window.
    pub ever_played: bool,
    pub volume: f32,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// No audio device: the fake clock drives everything (selftest path).
    pub decode_only: bool,
    /// Bumped on every explicit seek (MPRIS emits `Seeked` on change).
    pub seek_epoch: u64,
    /// Pure pointer bounds for MPRIS CanGoNext/CanGoPrevious.
    pub can_next: bool,
    pub can_prev: bool,
}

impl Default for PbSnapshot {
    fn default() -> Self {
        Self {
            track: None,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            paused: false,
            preparing: false,
            has_track: false,
            ever_played: false,
            volume: 0.8,
            repeat: Repeat::Off,
            shuffle: false,
            decode_only: true,
            seek_epoch: 0,
            can_next: false,
            can_prev: false,
        }
    }
}

impl PbSnapshot {
    /// The desktop-media-controls view of this publication. Lives here, next to
    /// the fields it reads, so the ONE publisher ([`Sup::publish`]) and the
    /// wire format cannot drift apart.
    fn mpris_snapshot(&self) -> mpris::Snapshot {
        let (title, artist, album, art_url, track_id, length) = match &self.track {
            Some(t) => (
                t.name.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.artwork_url(616, 616),
                t.id.clone(),
                Duration::from_millis(t.duration_ms),
            ),
            None => (
                String::new(),
                String::new(),
                String::new(),
                None,
                String::new(),
                Duration::ZERO,
            ),
        };
        mpris::Snapshot {
            status: if !self.has_track {
                mpris::PlaybackStatus::Stopped
            } else if self.paused {
                mpris::PlaybackStatus::Paused
            } else {
                mpris::PlaybackStatus::Playing
            },
            title,
            artist,
            album,
            art_url,
            track_id,
            length,
            position: self.position,
            volume: self.volume,
            shuffle: self.shuffle,
            loop_status: match self.repeat {
                Repeat::Off => mpris::LoopStatus::None,
                Repeat::One => mpris::LoopStatus::Track,
                Repeat::All => mpris::LoopStatus::Playlist,
            },
            has_track: self.has_track,
            // pure pointer bounds — repeat-all doesn't fake a next entry and
            // the >3 s restart isn't a "go previous"
            can_next: self.can_next,
            can_prev: self.can_prev,
            seek_epoch: self.seek_epoch,
        }
    }
}

/// UI → supervisor commands (plus the internal play-worker arrivals, which
/// ride the same channel so the loop blocks on exactly one receiver).
pub enum Command {
    /// Play `track` NOW as a fresh user segment. CONTRACT: the sender has
    /// already positioned the queue pointer at `track`'s row (play-now
    /// insert-after-current, queue-row jump, append auto-start, remove-current
    /// handover) and re-scanned the prefetcher. `close` is the END reason for
    /// the outgoing segment (0 = nothing to close: auto-start from idle;
    /// END_REPLACED for click/jump plays; END_SKIPPED_FORWARDS for the
    /// remove-current handover).
    Play {
        track: Track,
        close: u8,
    },
    Pause,
    Resume,
    Toggle,
    /// Manual ⏭ (keyboard, bar button, MPRIS): pointer+1 and play. At the end
    /// of the queue this is a deliberate no-op — manual skips never wrap,
    /// whatever the repeat mode.
    Next,
    /// Manual ⏮: restart current when >3 s elapsed, else pointer-1; on the
    /// first entry, restart.
    Prev,
    /// Reposition inside the current track (clamped to its duration).
    Seek(Duration),
    SetVolume(f32),
    SetRepeat(Repeat),
    /// Toggle the queue's shuffle state (a pure list op under the shared
    /// lock) and mirror the flag for snapshot/reporting.
    SetShuffle(bool),
    /// Adopt a new streaming `quality` for every source opened from now on
    /// (settings window). Already-cached tracks keep playing untouched.
    SetQuality(crate::config::Quality),
    /// MPRIS Stop / queue-panel "clear": close the segment, clear the queue,
    /// drop the current track, cancel any in-flight source.
    StopAll,
    /// Drop the current track audibly WITHOUT touching the queue (the
    /// remove-current handover ran out of successors).
    Halt,
    /// Album-context attribution for play reporting (the UI learns album ids
    /// when album/Queue-All track lists arrive).
    NoteAlbum {
        album_id: String,
        track_ids: Vec<String>,
    },
    /// Internal: a play-worker finished preparing the source for `generation`.
    Source(u64, Box<PacketSource>),
    /// Internal: a play-worker failed (open_track error or channel loss).
    WorkerFailed(String),
    /// Flush the open report segment (bounded) and exit the process — the
    /// same `std::process::exit` semantics every exit path has always used.
    Quit,
}

/// Supervisor side effects, injectable for tests. The real impl spawns the
/// same play-worker threads as before, wakes the egui frame loop and surfaces
/// errors as UI toasts; the test impl records calls.
trait Effects {
    fn spawn_worker(&mut self, generation: u64, track: Track, offset: Duration, keep: Vec<String>);
    /// Request one UI frame (state changed: new track, pause, advance…).
    fn wake_ui(&mut self);
    /// Adopt a new streaming quality for the play worker's config.
    fn set_quality(&mut self, quality: crate::config::Quality);
    /// Un-hide + focus the window (MPRIS Raise; also the trayless restore).
    fn show_window(&mut self);
    /// Non-fatal error for the UI toast.
    fn error(&mut self, msg: String);
}

/// Time source. The real clock is `Instant::now()`; tests inject a mutable
/// instant so advance/stop/repeat transitions are deterministic.
pub trait Clock: Send + Sync {
    fn now(&self) -> std::time::Instant;
}

struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// The state machine. Lives entirely on the supervisor thread (tests: the
/// test thread); nothing in here is shared except `queue`/`snap` behind
/// their mutexes.
struct Sup {
    queue: Arc<Mutex<Queue>>,
    snap: Arc<Mutex<PbSnapshot>>,
    clock: Arc<dyn Clock>,
    /// Process-start time, for readable `[sup 12.3s]` log lines.
    t0: std::time::Instant,
    // ---- audio (both None → decode-only fake clock) ----
    /// Kept only to hold the device open; dropping it stops playback.
    #[allow(dead_code)]
    stream: Option<rodio::OutputStream>,
    sink: Option<rodio::Sink>,
    decode_only: bool,
    // ---- playback state ----
    now: Option<Track>,
    /// Bumped on every play/stop; async sources carry it so stale ones are
    /// dropped instead of appended.
    generation: u64,
    preparing: bool,
    /// Sink-mode playback starts at this offset (pre-seek samples were
    /// discarded by the worker).
    play_offset: Duration,
    /// Pause state the next applied source must start with (see
    /// [`applied_source_pause_state`]). `None` once consumed or when the user
    /// acted in between (their action then wins over the pending decision).
    arrival_paused: Option<bool>,
    /// decode-only simulated playback window (clock start, duration at end).
    fake_play: Option<(std::time::Instant, Duration)>,
    /// decode-only pause point (frozen clock).
    fake_pause_at: Option<std::time::Instant>,
    repeat: Repeat,
    shuffle: bool,
    volume: f32,
    seek_epoch: u64,
    quit: bool,
    /// Sticky: a track has become current at least once this session (see
    /// [`PbSnapshot::ever_played`]).
    ever_played: bool,
    // ---- report segments ----
    reporter: Option<PlayReporter>,
    report_open: bool,
    /// start-position-in-milliseconds carried by the open segment's events
    /// (positions chain: each event starts where the previous one ended).
    report_seg_start_ms: u64,
    /// Container reported by the previous START, for the 0/1 change hint.
    report_last_container: Option<Option<String>>,
    /// track id → numeric catalog album adam id, learned via
    /// [`Command::NoteAlbum`] so STARTs keep their album attribution across
    /// queue auto-advance and shuffle.
    album_ids: HashMap<String, String>,
    // ---- neighbours ----
    prefetch: Prefetcher,
    mpris_rx: Option<Receiver<mpris::Command>>,
    /// The desktop-media-controls handle: `publish` (the single publisher, see
    /// [`Sup::publish`]) is the only thing the supervisor does with it.
    mpris: Option<mpris::Mpris>,
}

impl Sup {
    #[allow(clippy::too_many_arguments)]
    fn new(
        queue: Arc<Mutex<Queue>>,
        snap: Arc<Mutex<PbSnapshot>>,
        clock: Arc<dyn Clock>,
        stream: Option<rodio::OutputStream>,
        sink: Option<rodio::Sink>,
        repeat: Repeat,
        volume: f32,
        reporter: Option<PlayReporter>,
        prefetch: Prefetcher,
        mpris_rx: Option<Receiver<mpris::Command>>,
        mpris: Option<mpris::Mpris>,
    ) -> Self {
        let decode_only = sink.is_none();
        Self {
            queue,
            snap,
            clock,
            t0: std::time::Instant::now(),
            stream,
            sink,
            decode_only,
            now: None,
            generation: 0,
            preparing: false,
            play_offset: Duration::ZERO,
            arrival_paused: None,
            fake_play: None,
            fake_pause_at: None,
            repeat,
            shuffle: false,
            volume,
            seek_epoch: 0,
            quit: false,
            ever_played: false,
            reporter,
            report_open: false,
            report_seg_start_ms: 0,
            report_last_container: None,
            album_ids: HashMap::new(),
            prefetch,
            mpris_rx,
            mpris,
        }
    }

    fn log(&self, msg: &str) {
        eprintln!("[sup {:>6.1}s] {}", self.t0.elapsed().as_secs_f64(), msg);
    }

    // ---- state readers ----

    /// Elapsed time of the current track (fake clock in decode-only mode).
    fn position(&self) -> Duration {
        match (&self.fake_play, &self.fake_pause_at) {
            (Some((start, cap)), Some(p)) => p
                .checked_duration_since(*start)
                .unwrap_or_default()
                .min(*cap),
            (Some((start, cap)), None) => self.clock.now().duration_since(*start).min(*cap),
            _ => self
                .sink
                .as_ref()
                .map(|s| s.get_pos())
                .unwrap_or_default()
                + self.play_offset,
        }
    }

    fn is_paused(&self) -> bool {
        if self.now.is_none() {
            return true;
        }
        if self.decode_only {
            self.fake_play.is_some() && self.fake_pause_at.is_some()
        } else {
            self.sink.as_ref().is_some_and(|s| s.is_paused())
        }
    }

    /// True while the loop should tick at [`TICK_AUDIBLE`]: a track is
    /// loaded, not preparing and audibly running (the natural-end check).
    fn audible(&self) -> bool {
        self.now.is_some() && !self.preparing && !self.is_paused()
    }

    // ---- Apple play reporting (moved from App verbatim) ----

    /// play-mode triple from the current repeat/shuffle state (Mordent has no
    /// autoplay-after-queue feature → auto 1).
    fn play_mode(&self) -> crate::api::PlayMode {
        crate::api::PlayMode {
            auto: 1,
            repeat: match self.repeat {
                Repeat::Off => 1,
                Repeat::All => 2,
                Repeat::One => 3,
            },
            shuffle: if self.shuffle { 2 } else { 1 },
        }
    }

    /// Numeric catalog album adam id for `track` when it came from an album
    /// context; library (`i.`/`p.`) album ids are not adam ids → never sent.
    fn album_context(&self, track: &Track) -> Option<String> {
        self.album_ids.get(&track.id).cloned()
    }

    /// Per-send re-gate: any gate dropped mid-track silently discards the
    /// open segment.
    fn report_gates_open(&self) -> bool {
        self.reporter.is_some() && self.sink.is_some()
    }

    fn report_pos_ms(&self) -> u64 {
        self.position().as_millis() as u64
    }

    /// Open a segment for `track` at `pos_ms` (PLAY_START; hint 1 when the
    /// album container changed vs the previously reported one, else 0).
    fn report_start(&mut self, track: &Track, pos_ms: u64) {
        self.report_open = false; // never two open segments
        if !self.report_gates_open() {
            return;
        }
        let container = self.album_context(track);
        let hint = if self.report_last_container.as_ref() != Some(&container) {
            PlayEvent::HINT_CONTAINER_CHANGED
        } else {
            PlayEvent::HINT_NONE
        };
        self.report_last_container = Some(container.clone());
        self.report_seg_start_ms = pos_ms;
        self.report_open = true;
        if let Some(r) = &self.reporter {
            r.send(PlayEvent::start(
                &track.id,
                track.duration_ms,
                pos_ms,
                hint,
                self.play_mode(),
                container,
            ));
        }
    }

    /// Close `track`'s segment (PLAY_END) with `reason` at `pos_ms`.
    fn report_end_for(&mut self, track: &Track, reason: u8, pos_ms: u64) {
        if !self.report_open {
            return;
        }
        self.report_open = false;
        if !self.report_gates_open() {
            return;
        }
        let start_ms = self.report_seg_start_ms;
        let container = self.album_context(track);
        if let Some(r) = &self.reporter {
            r.send(PlayEvent::end(
                &track.id,
                track.duration_ms,
                start_ms,
                pos_ms,
                reason,
                self.play_mode(),
                container,
            ));
        }
    }

    /// Close the current track's segment.
    fn report_end(&mut self, reason: u8, pos_ms: u64) {
        match self.now.clone() {
            Some(t) => self.report_end_for(&t, reason, pos_ms),
            None => self.report_open = false,
        }
    }

    /// Seek while playing: END(12, old) + END(13, new); the segment stays
    /// open and continues at the new position (no new START).
    fn report_scrub(&mut self, old_ms: u64, new_ms: u64) {
        if !self.report_open {
            return;
        }
        self.report_end(PlayEvent::END_SCRUB_BEGIN, old_ms);
        self.report_open = true; // same segment continues across the scrub pair
        // positions chain: END(13) starts where END(12) ended (see the helper)
        let (_, (chained_start, _)) =
            scrub_pair_positions(self.report_seg_start_ms, old_ms, new_ms);
        self.report_seg_start_ms = chained_start;
        self.report_end(PlayEvent::END_SCRUB_END, new_ms);
        self.report_open = true;
        self.report_seg_start_ms = new_ms;
    }

    // ---- playback actions ----

    /// Play a track immediately at `offset`: cancels whatever is
    /// current/preparing by bumping the generation. Queue/pointer changes are
    /// the caller's job; this only owns audio, `now` and the START report.
    /// `kind` decides the report segment semantics and the pause state the
    /// source starts with on arrival.
    fn play_now_at(&mut self, fx: &mut dyn Effects, track: Track, offset: Duration, kind: PlayKind) {
        match kind {
            PlayKind::SeekRestart => {
                // a seek restart, not a new play — log it distinctly so a
                // seek is never mistaken for a second dispatch
                self.log(&format!(
                    "{} — {} (seek → {})",
                    track.name,
                    track.artist,
                    fmt_ms(offset.as_millis() as u64)
                ));
            }
            PlayKind::NewSegment | PlayKind::AutoAdvance => {
                self.log(&format!("{} — {}", track.name, track.artist));
                self.report_start(&track, 0);
            }
        }
        self.preparing = true;
        self.now = Some(track.clone());
        self.play_offset = offset;
        let pre_pause = if self.decode_only {
            self.fake_pause_at.is_some()
        } else {
            self.sink.as_ref().is_some_and(|s| s.is_paused())
        };
        let arrival_paused = applied_source_pause_state(
            pre_pause,
            kind != PlayKind::AutoAdvance,
            kind == PlayKind::AutoAdvance,
        );
        self.fake_play = None;
        self.fake_pause_at = arrival_paused.then(|| self.clock.now());
        self.arrival_paused = Some(arrival_paused);
        if let Some(s) = &self.sink {
            // rodio 0.20 `Sink::clear()` also pauses the sink; restore the
            // arrival state right away so the appended source is audible
            // unless paused-on-arrival is exactly what the user asked for.
            s.clear();
            if !arrival_paused {
                s.play();
            }
        }
        self.generation += 1;
        let generation = self.generation;
        // prune keep set for the play fetch: current + up-next stay cached
        let keep = self.queue.lock().eviction_keep();
        fx.spawn_worker(generation, track, offset, keep);
        // every play moved the pointer → re-target the up-next prefetch
        self.prefetch.scan(&self.queue.lock());
    }

    /// Natural end of the current track: move the pointer (repeat-one
    /// replays, repeat-all wraps) and play that entry; with no next entry,
    /// stop cleanly — the pointer stays on the finished entry so the session
    /// stays listed, and a later `+` resumes from the new append.
    fn advance(&mut self, fx: &mut dyn Effects) {
        let Some(finished) = self.now.clone() else {
            return;
        };
        // END 7 at the full duration. Manual skips report END 2 in Next.
        self.report_end_for(&finished, PlayEvent::END_NATURAL, finished.duration_ms);
        // NOTE: the lock guard must drop before the arms run — a `match
        // self.queue.lock().advance(..)` scrutinee would hold the temporary
        // guard across the arms and deadlock the Replay arm's re-lock.
        let adv = self.queue.lock().advance(self.repeat);
        let next = match adv {
            Advance::Replay => self.queue.lock().current_index(),
            Advance::Next(i) => Some(i),
            Advance::Stop => None,
        };
        match next {
            Some(i) => {
                self.log("natural advance");
                let t = self.queue.lock().tracks[i].clone();
                self.play_now_at(fx, t, Duration::ZERO, PlayKind::AutoAdvance);
            }
            None => {
                self.log("end of queue — stopped (pointer stays on the finished entry)");
                self.stop_playback();
            }
        }
        fx.wake_ui();
    }

    /// Drop the current track and stop audibly without reporting (the caller
    /// has already closed the segment); queue and pointer are left alone.
    fn stop_playback(&mut self) {
        self.now = None;
        self.generation += 1; // invalidate any in-flight source
        self.preparing = false;
        self.fake_play = None;
        self.fake_pause_at = None;
        self.play_offset = Duration::ZERO;
        if let Some(s) = &self.sink {
            s.clear();
            // rodio 0.20 `Sink::clear()` also pauses the sink; nothing is
            // playing here, so leave the flag clean for the next application
            s.play();
        }
        self.arrival_paused = None;
    }

    /// Apply the pause/resume transition for real (sink flag or fake clock)
    /// and report it. `resume` picks the direction; a no-op when the state
    /// already matches or nothing is loaded.
    fn set_paused(&mut self, resume: bool) {
        if self.now.is_none() || self.is_paused() == !resume {
            return;
        }
        if self.decode_only {
            if resume {
                // Continue from the frozen position: rebase the clock start to
                // (resume instant − frozen elapsed). The old `start += frozen`
                // drifted the position by the whole pause duration on resume.
                if let Some(p) = self.fake_pause_at.take()
                    && let Some((start, _)) = &mut self.fake_play
                    && let Some(frozen) = p.checked_duration_since(*start)
                {
                    *start = self.clock.now().checked_sub(frozen).unwrap_or(*start);
                }
            } else {
                self.fake_pause_at = Some(self.clock.now());
            }
        } else if let Some(s) = &self.sink {
            if resume {
                s.play();
            } else {
                s.pause();
            }
        }
        // the user just acted — a pending source must not override the new state
        self.arrival_paused = None;
        // pause closes the segment (3); resume opens one at the paused
        // position (hint 0 — same container)
        let pos = self.report_pos_ms();
        if resume {
            if let Some(t) = self.now.clone() {
                self.report_start(&t, pos);
            }
        } else {
            self.report_end(PlayEvent::END_PAUSED, pos);
        }
    }

    /// Click-to-seek / ±5s / ⏮ restart. Sink mode restarts the stream at the
    /// offset; decode-only shifts the fake clock.
    fn seek_to(&mut self, fx: &mut dyn Effects, target: Duration) {
        let Some(t) = self.now.as_ref().map(|t| t.duration_ms) else {
            return;
        };
        self.seek_epoch += 1; // MPRIS emits Seeked
        let total = Duration::from_millis(t);
        let target = target.min(total);
        // Scrub while PLAYING: END(12, old pos) then END(13, new pos) and the
        // segment continues at the new position without a new START. Seeking
        // while paused only repositions — the resume START lands there.
        let old = self.report_pos_ms();
        if let Some((o, n)) = seek_scrub_events(
            self.report_open,
            !self.is_paused(),
            old,
            target.as_millis() as u64,
        ) {
            self.report_scrub(o, n);
        }
        if self.decode_only {
            let now = self.clock.now();
            let end = Duration::from_millis(t.min(8_000));
            self.fake_play = Some((now.checked_sub(target).unwrap_or(now), end));
            if self.fake_pause_at.is_some() {
                self.fake_pause_at = Some(now);
            }
            self.play_offset = target;
        } else if self.now.is_some() {
            let t = self.now.clone().expect("checked above");
            self.play_now_at(fx, t, target, PlayKind::SeekRestart);
        }
        fx.wake_ui();
    }

    // ---- command dispatch ----

    fn handle(&mut self, fx: &mut dyn Effects, cmd: Command) {
        match cmd {
            Command::Play { track, close } => {
                if close != 0 {
                    let pos = self.report_pos_ms();
                    self.report_end(close, pos);
                }
                self.play_now_at(fx, track, Duration::ZERO, PlayKind::NewSegment);
                fx.wake_ui();
            }
            Command::Pause => {
                let was_audible = self.audible();
                self.set_paused(false);
                if was_audible {
                    fx.wake_ui();
                }
            }
            Command::Resume => {
                let was_paused = self.now.is_some() && self.is_paused();
                self.set_paused(true);
                if was_paused {
                    fx.wake_ui();
                }
            }
            Command::Toggle => {
                if self.now.is_some() {
                    let resume = self.is_paused();
                    self.set_paused(resume);
                    fx.wake_ui();
                }
            }
            Command::Next => {
                if self.now.is_none() {
                    return;
                }
                let Some(next) = self.queue.lock().peek_next() else {
                    return; // deliberate no-op: manual skips never wrap
                };
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SKIPPED_FORWARDS, pos);
                self.queue.lock().play_at(next);
                let t = self.queue.lock().tracks[next].clone();
                self.play_now_at(fx, t, Duration::ZERO, PlayKind::NewSegment);
                fx.wake_ui();
            }
            Command::Prev => {
                if self.now.is_some() && self.position().as_secs_f64() > 3.0 {
                    self.seek_to(fx, Duration::ZERO);
                    return;
                }
                let Some(prev) = self.queue.lock().peek_prev() else {
                    if self.now.is_some() {
                        self.seek_to(fx, Duration::ZERO);
                    }
                    return;
                };
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SKIPPED_BACKWARDS, pos);
                self.queue.lock().play_at(prev);
                let t = self.queue.lock().tracks[prev].clone();
                self.play_now_at(fx, t, Duration::ZERO, PlayKind::NewSegment);
                fx.wake_ui();
            }
            Command::Seek(target) => self.seek_to(fx, target),
            Command::SetVolume(v) => {
                let v = v.clamp(0.0, 1.0);
                let changed = (v - self.volume).abs() > f32::EPSILON;
                self.volume = v;
                if let Some(s) = &self.sink {
                    s.set_volume(v);
                }
                if changed {
                    fx.wake_ui();
                }
            }
            Command::SetRepeat(r) => {
                if self.repeat != r {
                    self.repeat = r;
                    fx.wake_ui();
                }
            }
            Command::SetShuffle(want) => {
                if self.shuffle != want {
                    self.shuffle = want;
                    self.queue.lock().toggle_shuffle();
                    self.prefetch.scan(&self.queue.lock());
                    fx.wake_ui();
                }
            }
            Command::SetQuality(q) => fx.set_quality(q),
            Command::StopAll => {
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SUSPENDED, pos);
                self.queue.lock().clear();
                self.stop_playback();
                self.prefetch.scan(&self.queue.lock());
                self.log("stop — queue cleared");
                fx.wake_ui();
            }
            Command::Halt => {
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SUSPENDED, pos);
                self.stop_playback();
                fx.wake_ui();
            }
            Command::NoteAlbum {
                album_id,
                track_ids,
            } => {
                if album_id.chars().all(|c| c.is_ascii_digit()) {
                    for id in track_ids {
                        self.album_ids.insert(id, album_id.clone());
                    }
                }
            }
            Command::Source(g, src) => {
                if g == self.generation {
                    if let Some(s) = &self.sink {
                        s.append(*src);
                    }
                    // decode-only (sink None): dropping src also cancels its
                    // download — the fake clock owns everything, as before.
                    self.preparing = false;
                    // enforce the planned arrival pause state; `None` means
                    // the user acted during preparation and their action
                    // (already applied to the sink) must not be overridden
                    if let Some(true) = self.arrival_paused
                        && let Some(s) = &self.sink
                    {
                        s.pause();
                    }
                    self.arrival_paused = None;
                    // start the decode-only fake clock (keep a seek target);
                    // tuple = (clock start, elapsed-at-end)
                    if self.decode_only
                        && self.fake_play.is_none()
                        && let Some(t) = &self.now
                    {
                        let end = Duration::from_millis(t.duration_ms.min(8_000));
                        let start = self
                            .clock
                            .now()
                            .checked_sub(self.play_offset)
                            .unwrap_or_else(|| self.clock.now());
                        self.fake_play = Some((start, end));
                    }
                    fx.wake_ui();
                } // stale: dropping src also cancels its download
            }
            Command::WorkerFailed(e) => {
                fx.error(e);
                self.preparing = false;
                // now/fake_play are left as play_now_at set them: the next
                // tick sees no running clock (or an empty sink) and advances
                // — the original error→skip-to-next behavior.
            }
            Command::Quit => self.quit = true,
        }
    }

    // ---- MPRIS drain (supervisor-side: works while the UI is frozen) ----

    fn drain_mpris(&mut self, fx: &mut dyn Effects) {
        let Some(rx) = &self.mpris_rx else { return };
        let mut cmds = Vec::new();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        // rx's borrow ends here; dispatch freely
        for c in cmds {
            match c {
                mpris::Command::Play => {
                    if self.now.is_some() && self.is_paused() {
                        self.handle(fx, Command::Resume);
                    }
                }
                mpris::Command::Pause => self.handle(fx, Command::Pause),
                mpris::Command::TogglePause => self.handle(fx, Command::Toggle),
                mpris::Command::Next => self.handle(fx, Command::Next),
                mpris::Command::Previous => self.handle(fx, Command::Prev),
                mpris::Command::Stop => self.handle(fx, Command::StopAll),
                mpris::Command::Seek(us) => {
                    let step = Duration::from_micros(us.unsigned_abs());
                    let target = if us >= 0 {
                        self.position() + step
                    } else {
                        self.position().saturating_sub(step)
                    };
                    self.handle(fx, Command::Seek(target));
                }
                mpris::Command::SetPosition(us) => {
                    self.handle(fx, Command::Seek(Duration::from_micros(us)))
                }
                mpris::Command::SetLoop(l) => {
                    let r = match l {
                        mpris::LoopStatus::None => Repeat::Off,
                        mpris::LoopStatus::Track => Repeat::One,
                        mpris::LoopStatus::Playlist => Repeat::All,
                    };
                    self.handle(fx, Command::SetRepeat(r));
                }
                mpris::Command::SetShuffle(want) => self.handle(fx, Command::SetShuffle(want)),
                mpris::Command::SetVolume(v) => self.handle(fx, Command::SetVolume(v)),
                mpris::Command::Raise => fx.show_window(),
                mpris::Command::Quit => self.handle(fx, Command::Quit),
            }
        }
    }

    // ---- frame-independent tick: natural advance ----

    /// Called every loop cycle. Advances when the current track is over:
    /// decode-only compares the fake clock against the simulated end (no
    /// clock running = the source never arrived → skip, the old error path);
    /// sink mode watches the sink drain. Paused tracks never auto-advance —
    /// the advance happens on resume instead (a deliberate tightening over
    /// the old in-frame check, which could un-pause a track paused in the
    /// tiny window at its very end).
    fn tick(&mut self, fx: &mut dyn Effects) {
        if !self.audible() {
            return;
        }
        let over = if self.decode_only {
            match &self.fake_play {
                Some((_, end)) => self.position() >= *end,
                None => true,
            }
        } else {
            self.sink.as_ref().is_some_and(|s| s.empty())
        };
        if over {
            self.advance(fx);
        }
    }

    // ---- publication ----

    fn publish(&mut self) {
        // Sticky: any track becoming current latches ever_played for the rest
        // of the session (drives the window-close policy).
        self.ever_played |= self.now.is_some();
        let (duration, paused) = match &self.now {
            Some(t) => {
                let d = if self.decode_only {
                    self.fake_play
                        .map(|(_, e)| e)
                        .unwrap_or(Duration::from_millis(t.duration_ms))
                } else {
                    Duration::from_millis(t.duration_ms)
                };
                (d, self.is_paused())
            }
            None => (Duration::ZERO, false),
        };
        let snap = {
            let q = self.queue.lock();
            PbSnapshot {
                track: self.now.clone(),
                position: self.position(),
                duration,
                paused,
                preparing: self.preparing,
                has_track: self.now.is_some(),
                ever_played: self.ever_played,
                volume: self.volume,
                repeat: self.repeat,
                shuffle: self.shuffle,
                decode_only: self.decode_only,
                seek_epoch: self.seek_epoch,
                can_next: q.peek_next().is_some(),
                can_prev: q.peek_prev().is_some(),
            }
        };
        // Single publisher for desktop media controls: the UI can be hidden
        // (destroyed window), frozen or never scheduled again, and MPRIS must
        // still follow playback — so the thread that owns the state machine
        // publishes, not the frame loop. `publish_mpris` used to run from
        // `App::update`; that froze Position/metadata on the D-Bus side for as
        // long as the window was hidden.
        if let Some(m) = &self.mpris {
            m.publish(snap.mpris_snapshot());
        }
        *self.snap.lock() = snap;
    }

    // ---- exit ----

    /// Best-effort END(4) at the current position + bounded flush; used by
    /// every exit path (tray Quit, ctrl+Q, MPRIS Quit).
    fn exit_flush(&mut self) {
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_SUSPENDED, pos);
        if let Some(r) = self.reporter.take() {
            r.shutdown(None);
        }
    }
}

/// Real side effects: play-worker threads, egui wake-ups, UI toasts.
struct RealFx {
    cfg: Config,
    tx: Sender<Command>,
    ui_tx: Sender<String>,
    /// The live window session. It owns the CURRENT `egui::Context` (a
    /// re-created session hands out a new one) and the mirrored visibility, so
    /// Raise and the wakes address whatever window exists right now — see
    /// `crate::session`.
    session: crate::session::Session,
    /// `MORDENT_NO_REPAINT=1` debug hook: suppress ALL repaint requests so the
    /// frame loop stays parked on an unfocused window — the decoupling proof
    /// that playback still advances with the UI effectively stopped.
    /// Playback/advance/reporting here do not depend on it at all.
    no_repaint: bool,
}

impl Effects for RealFx {
    fn spawn_worker(&mut self, generation: u64, track: Track, offset: Duration, keep: Vec<String>) {
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let _ = std::thread::Builder::new()
            .name("play-worker".into())
            .spawn(move || {
                let res = (|| -> anyhow::Result<()> {
                    let (prog, _mode) = stream::open_track(&cfg, &track.id, &keep)?;
                    let cancel = prog.cancel_flag();
                    let mut src = PacketSource::with_cancel(Box::new(prog), cancel)?;
                    // Seek = restart the stream and discard output before the
                    // offset (decode is ~100x realtime, so this is cheap).
                    if offset.as_nanos() > 0 {
                        let per_sec = src.sample_rate as u64 * src.channels.max(1) as u64;
                        let mut left = (offset.as_secs_f64() * per_sec as f64) as u64;
                        while left > 0 && src.next().is_some() {
                            left -= 1;
                        }
                    }
                    tx.send(Command::Source(generation, Box::new(src)))
                        .map_err(|_| anyhow::anyhow!("channel closed"))
                })();
                if let Err(e) = res {
                    let _ = tx.send(Command::WorkerFailed(format!("{e:#}")));
                }
            });
    }

    fn wake_ui(&mut self) {
        if !self.no_repaint {
            self.session.request_repaint();
        }
    }

    fn set_quality(&mut self, quality: crate::config::Quality) {
        // The play worker clones this config when it starts, so the next
        // source opened (any track not already cache-complete) uses it.
        self.cfg.quality = quality;
    }

    fn show_window(&mut self) {
        // Un-hides, raises, or re-creates the window — whichever this backend
        // needs (see `crate::session`); also clears the mirrored visibility so
        // the tray's Show/Hide label stays truthful.
        self.session.show();
    }

    fn error(&mut self, msg: String) {
        let _ = self.ui_tx.send(msg);
        if !self.no_repaint {
            self.session.request_repaint();
        }
    }
}

/// Spawn the playback supervisor thread. The OutputStream is created ON that
/// thread (it is `!Send`); everything the UI needs back is returned: the
/// command sender, the shared snapshot, and the supervisor→UI error receiver.
/// `mpris_rx` (when MPRIS started) is drained by the supervisor so media
/// keys work while the UI is hidden or frozen, and the same thread PUBLISHES
/// the MPRIS state (see [`Sup::publish`]) — the frame loop may be gone.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cfg: Config,
    queue: Arc<Mutex<Queue>>,
    repeat: Repeat,
    volume: f32,
    session: crate::session::Session,
    no_repaint: bool,
    mpris_rx: Option<Receiver<mpris::Command>>,
    mpris: Option<mpris::Mpris>,
    prefetch: Prefetcher,
) -> (Sender<Command>, Arc<Mutex<PbSnapshot>>, Receiver<String>) {
    let (tx, rx) = channel();
    let snap = Arc::new(Mutex::new(PbSnapshot {
        volume,
        repeat,
        decode_only: true,
        ..PbSnapshot::default()
    }));
    let (ui_tx, ui_rx) = channel();
    let snap_thread = snap.clone();
    let tx_thread = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("mordent-supervisor".into())
        .spawn(move || {
            run_thread(
                cfg, queue, snap_thread, repeat, volume, session, no_repaint, mpris_rx, mpris,
                prefetch, tx_thread, rx, ui_tx,
            );
        });
    if let Err(e) = spawned {
        // No supervisor = no playback at all; loud failure beats silence.
        panic!("spawn mordent-supervisor thread: {e}");
    }
    (tx, snap, ui_rx)
}

#[allow(clippy::too_many_arguments)]
fn run_thread(
    cfg: Config,
    queue: Arc<Mutex<Queue>>,
    snap: Arc<Mutex<PbSnapshot>>,
    repeat: Repeat,
    volume: f32,
    session: crate::session::Session,
    no_repaint: bool,
    mpris_rx: Option<Receiver<mpris::Command>>,
    mpris: Option<mpris::Mpris>,
    prefetch: Prefetcher,
    tx: Sender<Command>,
    rx: Receiver<Command>,
    ui_tx: Sender<String>,
) {
    // The OutputStream MUST be created here: it is `!Send` and stays on this
    // thread for the process lifetime. Without a device → decode-only mode
    // (headless VPS / CI): identical state machine on a fake clock.
    let (stream, sink) = match rodio::OutputStream::try_default() {
        Ok((s, h)) => (Some(s), rodio::Sink::try_new(&h).ok()),
        Err(_) => (None, None),
    };
    if let Some(s) = &sink {
        s.set_volume(volume);
    } else {
        eprintln!("[supervisor] no audio device — decode-only mode (fake clock drives advance)");
    }
    // Play reporting only with the config switch on, a media-user-token,
    // and a REAL audio sink — the decode-only fake clock never reports.
    let reporter = if PlayReporter::enabled(&cfg, sink.is_some()) {
        Some(PlayReporter::spawn(cfg.clone()))
    } else {
        None
    };
    let mut sup = Sup::new(
        queue,
        snap,
        Arc::new(RealClock),
        stream,
        sink,
        repeat,
        volume,
        reporter,
        prefetch,
        mpris_rx,
        mpris,
    );
    let mut fx = RealFx {
        cfg,
        tx,
        ui_tx,
        session,
        no_repaint,
    };
    loop {
        sup.drain_mpris(&mut fx);
        let timeout = if sup.audible() { TICK_AUDIBLE } else { TICK_IDLE };
        match rx.recv_timeout(timeout) {
            Ok(cmd) => {
                sup.handle(&mut fx, cmd);
                while let Ok(c) = rx.try_recv() {
                    sup.handle(&mut fx, c);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // UI gone (eframe event loop ended): flush best-effort, then let
            // the thread end — dropping the stream silences the device.
            Err(RecvTimeoutError::Disconnected) => {
                sup.log("UI channel closed — flushing and stopping");
                sup.exit_flush();
                return;
            }
        }
        sup.tick(&mut fx);
        sup.publish();
        if sup.quit {
            sup.log("quit — flushing report segment and exiting");
            sup.exit_flush();
            std::process::exit(0);
        }
    }
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms % 60000) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Track;

    fn trk(id: &str) -> Track {
        Track {
            id: id.to_string(),
            name: id.to_string(),
            artist: "a".into(),
            album: String::new(),
            duration_ms: 1_000,
            track_no: 1,
            disc_no: 1,
            artwork: None,
            lossless: false,
        }
    }

    /// Mutable instant: the injected clock for deterministic advance tests.
    struct TestClock(Mutex<std::time::Instant>);
    impl Clock for TestClock {
        fn now(&self) -> std::time::Instant {
            *self.0.lock()
        }
    }
    impl TestClock {
        fn advance(&self, d: Duration) {
            *self.0.lock() += d;
        }
    }

    /// Recorded side effects.
    #[derive(Default)]
    struct TestFx {
        spawned: Vec<(u64, String, Duration)>,
        wakes: usize,
        shows: usize,
        errors: Vec<String>,
    }
    impl Effects for TestFx {
        fn spawn_worker(
            &mut self,
            generation: u64,
            track: Track,
            offset: Duration,
            _keep: Vec<String>,
        ) {
            self.spawned.push((generation, track.id, offset));
        }
        fn wake_ui(&mut self) {
            self.wakes += 1;
        }
        fn set_quality(&mut self, _quality: crate::config::Quality) {}
        fn show_window(&mut self) {
            self.shows += 1;
        }
        fn error(&mut self, msg: String) {
            self.errors.push(msg);
        }
    }

    /// A decode-only supervisor (no sink → the fake clock path, exactly the
    /// selftest configuration) with an injected clock and an inert prefetcher
    /// (cache_size_gb 0 → no worker thread, no cache dir I/O).
    fn sup_with(seed: &[&str]) -> (Sup, Arc<TestClock>) {
        let cfg = Config {
            cache_size_gb: 0,
            ..Config::default()
        };
        let clock = Arc::new(TestClock(Mutex::new(std::time::Instant::now())));
        let mut queue = Queue::default();
        queue.tracks = seed.iter().map(|s| trk(s)).collect();
        let sup = Sup::new(
            Arc::new(Mutex::new(queue)),
            Arc::new(Mutex::new(PbSnapshot::default())),
            clock.clone(),
            None,
            None,
            Repeat::Off,
            0.8,
            None, // reporter off: decode-only never reports
            Prefetcher::new(&cfg),
            None,
            None,
        );
        (sup, clock)
    }

    /// Play `id` as the supervisor's current track (the Play command the UI
    /// sends after positioning the pointer).
    fn play(sup: &mut Sup, fx: &mut TestFx, id: &str, close: u8) {
        sup.handle(
            fx,
            Command::Play {
                track: trk(id),
                close,
            },
        );
    }

    /// Run the source-arrival step for the last spawned worker so `preparing`
    /// clears and the fake clock starts (a real run: play-worker → channel).
    fn arrive(sup: &mut Sup, fx: &mut TestFx) {
        let (arrival_gen, _, _) = *fx.spawned.last().expect("a worker was spawned");
        sup.handle(fx, Command::Source(arrival_gen, Box::new(PacketSource::for_tests())));
    }

    /// Advance the injected clock past the 1 s simulated track end and tick.
    fn tick_over_end(sup: &mut Sup, fx: &mut TestFx, clock: &Arc<TestClock>) {
        clock.advance(Duration::from_millis(1100));
        sup.tick(fx);
        sup.publish();
    }

    /// Tick once within the track (no end reached).
    fn tick_mid(sup: &mut Sup, fx: &mut TestFx, clock: &Arc<TestClock>) {
        clock.advance(TICK_AUDIBLE);
        sup.tick(fx);
        sup.publish();
    }

    fn pos(sup: &Sup) -> Option<usize> {
        sup.queue.lock().pos
    }

    // ---- natural advance / stop / repeat (injected clock) ----

    #[test]
    fn natural_advance_moves_pointer_and_starts_next() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        assert!(sup.audible());

        // before the simulated end nothing happens
        tick_mid(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(0));
        assert_eq!(fx.spawned.len(), 1);

        // at the end: pointer moves on, next track starts as an auto-advance
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(1));
        let (generation, id, offset) = fx.spawned.last().expect("advance spawned next");
        assert_eq!(id, "B");
        assert_eq!(*offset, Duration::ZERO);
        assert!(*generation > 0);
        // the new track is preparing, then audible after arrival
        assert!(sup.preparing);
        arrive(&mut sup, &mut fx);
        assert!(sup.audible());
    }

    #[test]
    fn natural_advance_at_end_of_queue_stops_and_keeps_pointer() {
        let (mut sup, clock) = sup_with(&["A"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        tick_over_end(&mut sup, &mut fx, &clock);
        // stopped: no current track, queue NOT consumed, pointer stays
        assert!(sup.now.is_none());
        assert_eq!(sup.queue.lock().len(), 1);
        assert_eq!(pos(&sup), Some(0));
        assert_eq!(fx.spawned.len(), 1, "no further play dispatched");
        // and the stopped state is quiet (further ticks do nothing)
        let wakes = fx.wakes;
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(fx.wakes, wakes);
    }

    #[test]
    fn repeat_one_replays_the_same_entry() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.repeat = Repeat::One;
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(0));
        assert_eq!(fx.spawned.last().expect("replay").1, "A");
    }

    #[test]
    fn repeat_all_wraps_to_the_first_entry() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.repeat = Repeat::All;
        sup.queue.lock().play_at(1); // start from the last entry
        play(&mut sup, &mut fx, "B", 0);
        arrive(&mut sup, &mut fx);
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(0));
        assert_eq!(fx.spawned.last().expect("wrap").1, "A");
    }

    #[test]
    fn worker_error_skips_to_the_next_entry() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        // the worker failed: preparing clears without a source arrival
        sup.handle(&mut fx, Command::WorkerFailed("wrapper down".into()));
        assert!(!sup.preparing);
        // the next tick sees no running clock → advance (old Msg::Error path)
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(1));
        assert_eq!(fx.errors.len(), 1);
        assert_eq!(fx.spawned.last().expect("skip").1, "B");
    }

    // ---- fake clock: pause / resume / seek arithmetic ----

    #[test]
    fn pause_freezes_the_fake_clock_and_resume_continues() {
        let (mut sup, clock) = sup_with(&["A"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        clock.advance(Duration::from_millis(400));
        assert_eq!(sup.position(), Duration::from_millis(400));

        sup.handle(&mut fx, Command::Pause);
        assert!(sup.is_paused());
        clock.advance(Duration::from_secs(60));
        assert_eq!(sup.position(), Duration::from_millis(400), "frozen while paused");

        sup.handle(&mut fx, Command::Resume);
        assert!(!sup.is_paused());
        clock.advance(Duration::from_millis(100));
        assert_eq!(
            sup.position(),
            Duration::from_millis(500),
            "resumes from the frozen point"
        );
    }

    #[test]
    fn seek_repositions_and_preserves_pause() {
        let (mut sup, clock) = sup_with(&["A"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        clock.advance(Duration::from_millis(500));
        sup.handle(&mut fx, Command::Pause);
        sup.handle(&mut fx, Command::Seek(Duration::from_millis(200)));
        assert!(sup.is_paused(), "a paused seek stays paused");
        clock.advance(Duration::from_millis(50));
        assert_eq!(sup.position(), Duration::from_millis(200), "frozen at the seek target");
        sup.handle(&mut fx, Command::Resume);
        clock.advance(Duration::from_millis(100));
        assert_eq!(sup.position(), Duration::from_millis(300));

        // a playing seek clamps to the duration
        sup.handle(&mut fx, Command::Seek(Duration::from_secs(3600)));
        assert_eq!(sup.position(), Duration::from_millis(1_000));
    }

    #[test]
    fn pause_on_arrival_is_decided_at_play_and_enforced_at_arrival() {
        let (mut sup, _clock) = sup_with(&["A"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        // replace while "paused" (fake_pause_at preset by a prior pause)
        sup.fake_pause_at = Some(std::time::Instant::now());
        play(&mut sup, &mut fx, "A", 0);
        assert_eq!(sup.arrival_paused, Some(true), "paused replace lands paused");
        arrive(&mut sup, &mut fx);
        assert!(sup.is_paused());
        assert!(!sup.audible());

        // resume, then a playing replace lands audible
        sup.handle(&mut fx, Command::Resume);
        assert!(!sup.is_paused());
        play(&mut sup, &mut fx, "A", 0);
        assert_eq!(sup.arrival_paused, Some(false));
        arrive(&mut sup, &mut fx);
        assert!(!sup.is_paused());
        assert!(sup.audible());
    }

    // ---- command handling ----

    #[test]
    fn play_replaces_current_and_invalidates_stale_sources() {
        let (mut sup, _clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        let old_gen = fx.spawned[0].0;

        play(&mut sup, &mut fx, "B", 0);
        // the stale source of the cancelled generation is dropped
        sup.handle(&mut fx, Command::Source(old_gen, Box::new(PacketSource::for_tests())));
        assert!(sup.preparing, "stale source must not clear the new prepare");
        arrive(&mut sup, &mut fx);
        assert_eq!(sup.now.as_ref().expect("now").id, "B");
        assert!(fx.spawned.last().expect("new gen").0 > old_gen);
    }

    #[test]
    fn next_at_end_of_queue_is_a_deliberate_noop() {
        let (mut sup, _clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(1);
        play(&mut sup, &mut fx, "B", 0);
        arrive(&mut sup, &mut fx);
        let before = fx.spawned.len();
        sup.handle(&mut fx, Command::Next);
        assert_eq!(pos(&sup), Some(1), "manual skips never wrap");
        assert_eq!(fx.spawned.len(), before, "no new play dispatched");
        assert_eq!(sup.now.as_ref().expect("kept").id, "B");
    }

    #[test]
    fn prev_after_three_seconds_restarts_current() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        // the >3 s restart rule needs a track long enough to get past 3 s
        // (the 1 s test tracks cap the fake clock at their duration)
        sup.queue.lock().tracks[1].duration_ms = 60_000;
        sup.queue.lock().play_at(1);
        // Play the queue's (modified) track, exactly what the UI sends after
        // positioning the pointer.
        let t = sup.queue.lock().tracks[1].clone();
        sup.handle(&mut fx, Command::Play { track: t, close: 0 });
        arrive(&mut sup, &mut fx);
        clock.advance(Duration::from_secs(4));
        sup.handle(&mut fx, Command::Prev);
        assert_eq!(pos(&sup), Some(1), "pointer untouched by the restart");
        // restart = SeekRestart: same entry, offset ZERO, no new dispatch id
        let (_, id, offset) = fx.spawned.last().expect("restart");
        assert_eq!(id, "B");
        assert_eq!(*offset, Duration::ZERO);
    }

    #[test]
    fn prev_soon_after_start_goes_to_the_previous_entry() {
        let (mut sup, clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(1);
        play(&mut sup, &mut fx, "B", 0);
        arrive(&mut sup, &mut fx);
        clock.advance(Duration::from_millis(300));
        sup.handle(&mut fx, Command::Prev);
        assert_eq!(pos(&sup), Some(0));
        assert_eq!(fx.spawned.last().expect("prev entry").1, "A");
    }

    #[test]
    fn halt_stops_playback_but_keeps_the_queue() {
        let (mut sup, _clock) = sup_with(&["A"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);
        sup.handle(&mut fx, Command::Halt);
        assert!(sup.now.is_none());
        assert_eq!(sup.queue.lock().len(), 1);
        assert_eq!(pos(&sup), Some(0));
    }

    #[test]
    fn stop_all_clears_queue_and_playback() {
        let (mut sup, _clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        sup.handle(&mut fx, Command::StopAll);
        assert!(sup.now.is_none());
        assert!(sup.queue.lock().is_empty());
        assert_eq!(pos(&sup), None);
    }

    #[test]
    fn set_shuffle_toggles_the_queue_and_mirrors_the_flag() {
        let (mut sup, _clock) = sup_with(&["A", "B", "C"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        sup.handle(&mut fx, Command::SetShuffle(true));
        assert!(sup.shuffle);
        sup.handle(&mut fx, Command::SetShuffle(false));
        assert!(!sup.shuffle);
        // toggle off restores the original order
        let ids: Vec<String> = sup.queue.lock().tracks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids, ["A", "B", "C"]);
        assert_eq!(pos(&sup), Some(0));
    }

    // ---- queue pointer consistency across user mutations + advances ----

    #[test]
    fn pointer_survives_ui_mutations_interleaved_with_natural_advances() {
        let (mut sup, clock) = sup_with(&[]);
        let mut fx = TestFx::default();

        // UI: play-now from idle (insert-after-current + Play command)
        let start = sup.queue.lock().insert_after_current(vec![trk("A"), trk("B")]);
        assert_eq!(start, 0);
        play(&mut sup, &mut fx, "A", 0);
        arrive(&mut sup, &mut fx);

        // UI: append while playing — never jumps
        sup.queue.lock().append(vec![trk("C")], false);
        assert_eq!(pos(&sup), Some(0));

        // UI: reorder keeps the pointer on the current row
        sup.queue.lock().move_row(1, 2);
        assert_eq!(pos(&sup), Some(0));

        // natural advance A → next entry
        tick_over_end(&mut sup, &mut fx, &clock);
        assert_eq!(pos(&sup), Some(1));
        arrive(&mut sup, &mut fx);

        // UI: remove an up-next row — pointer untouched
        assert_eq!(
            sup.queue.lock().remove(2),
            crate::queuemodel::RemoveEffect::Nothing
        );
        assert_eq!(pos(&sup), Some(1));

        // natural advance B → C is the last entry → stop, pointer stays
        tick_over_end(&mut sup, &mut fx, &clock);
        assert!(sup.now.is_none());
        assert_eq!(pos(&sup), Some(1), "pointer stays on the finished entry");
        assert_eq!(sup.queue.lock().len(), 2, "queue is never consumed");

        // UI: append after a finished session auto-starts the new track
        let started = sup.queue.lock().append(vec![trk("D")], true);
        assert_eq!(started, Some(2));
        play(&mut sup, &mut fx, "D", 0);
        arrive(&mut sup, &mut fx);
        assert_eq!(sup.now.as_ref().expect("resumed").id, "D");
    }

    #[test]
    fn publish_reflects_state_and_pointer_bounds() {
        let (mut sup, _clock) = sup_with(&["A", "B"]);
        let mut fx = TestFx::default();
        sup.queue.lock().play_at(0);
        play(&mut sup, &mut fx, "A", 0);
        sup.publish();
        let s = sup.snap.lock().clone();
        assert!(s.has_track);
        assert!(s.can_next);
        assert!(!s.can_prev);
        assert_eq!(s.repeat, Repeat::Off);
        assert!((s.volume - 0.8).abs() < f32::EPSILON);
        assert_eq!(s.duration, Duration::from_millis(1_000));
        sup.handle(&mut fx, Command::SetRepeat(Repeat::All));
        sup.handle(&mut fx, Command::SetVolume(0.5));
        sup.publish();
        let s = sup.snap.lock().clone();
        assert_eq!(s.repeat, Repeat::All);
        assert!((s.volume - 0.5).abs() < f32::EPSILON);
    }

    // ---- moved pure-fn tests (pause-arrival + scrub rules) ----

    #[test]
    fn auto_advance_always_lands_audible() {
        assert!(!applied_source_pause_state(false, true, true));
        assert!(!applied_source_pause_state(true, true, true));
        assert!(!applied_source_pause_state(true, false, true));
    }

    #[test]
    fn replace_and_seek_restore_the_pre_action_pause_state() {
        // user was playing → the new source is audible on arrival
        assert!(!applied_source_pause_state(false, true, false));
        // user had explicitly paused → paused-on-arrival is correct
        assert!(applied_source_pause_state(true, true, false));
    }

    #[test]
    fn seek_restart_is_distinct_from_fresh_segments() {
        // only SeekRestart skips the PLAY_START; a seek-to-zero restart must
        // not re-START the way an offset==0 fresh segment does
        assert_ne!(PlayKind::SeekRestart, PlayKind::NewSegment);
        assert_ne!(PlayKind::SeekRestart, PlayKind::AutoAdvance);
        assert_ne!(PlayKind::NewSegment, PlayKind::AutoAdvance);
    }

    #[test]
    fn scrub_pair_only_while_open_and_playing() {
        // seek while playing: END(12) at the old pos, END(13) at the new one,
        // and NO new START (the segment continues)
        assert_eq!(
            seek_scrub_events(true, true, 7_971, 146_956),
            Some((7_971, 146_956))
        );
        // seek while paused: the pause already closed the segment — emit
        // nothing; the resume START carries the seeked position
        assert_eq!(seek_scrub_events(true, false, 7_971, 146_956), None);
        // no open segment (gates closed, or nothing reported yet): nothing
        assert_eq!(seek_scrub_events(false, true, 0, 1_000), None);
        assert_eq!(seek_scrub_events(false, false, 0, 1_000), None);
    }

    #[test]
    fn scrub_pair_positions_chain_end_to_start() {
        // a scrub at 30 s→120 s in a track started at 0 (the forensics case):
        // END(12) spans [segStart..old], END(13) chains [old..new] — the two
        // events are contiguous, never overlapping
        let (begin, end) = scrub_pair_positions(0, 30_000, 120_000);
        assert_eq!(begin, (0, 30_000), "END(12): start=segStart, end=old");
        assert_eq!(end, (30_000, 120_000), "END(13): start=old, end=new");
        // a mid-segment scrub chains from the current segment start as well
        let (begin, end) = scrub_pair_positions(7_971, 20_000, 146_956);
        assert_eq!(begin, (7_971, 20_000));
        assert_eq!(end, (20_000, 146_956));
    }
}
