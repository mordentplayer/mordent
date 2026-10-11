//! Dense egui UI: search (Songs/Albums sub-views) / library (albums) /
//! favorites / playlists tabs, album & playlist detail modals, explicit
//! queue, upgraded player bar.
//!
//! Favorites are a set of track ids persisted in state.toml; the favorites
//! view names them from an in-memory id→Track map filled as tracks are seen.
//!
//! Playback model: clicking a row plays it NOW (replacing current; the stale
//! async source is dropped, which also cancels its download). `+` appends to
//! the queue. Album rows open a detail modal; album `+` enqueues the whole
//! album (tracks fetched on click).
//!
//! DECOUPLED PLAYBACK: the UI thread owns NO audio. A playback supervisor
//! thread (`supervisor.rs`) owns the rodio output, the pending-arrival pause
//! decisions, natural-end advance and play reporting; the UI mutates the
//! shared queue under its mutex, sends [`Command`]s, and renders from the
//! shared [`PbSnapshot`]. Hiding, freezing or never scheduling `update()`
//! again cannot stop playback — that is the point (see supervisor.rs).
//!
//! In decode-only mode (no audio device) the supervisor's fake clock
//! simulates the running track so queue/auto-advance behaves like a real
//! player.

use crate::api::{AlbumHit, Api, LibraryPlaylist, PlayEvent, SearchResults, Track, META_TTL_LIBRARY};
use crate::config::Config;
use crate::fonts::FontChoice;
use crate::lyrics::{self, LyricTimeline};
use crate::mpris;
use crate::prefetch::Prefetcher;
use crate::supervisor::{self, Command, PbSnapshot};
use crate::theme::Theme;
use crate::update::{self, Check};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use crate::queuemodel::{Queue, RemoveEffect, Repeat};

enum Msg {
    Results(Box<SearchResults>),
    /// One page of the user's library albums, tagged with the sort it was
    /// fetched under (mismatched pages are dropped, never mixed).
    Library(Vec<AlbumHit>, Option<u32>, LibrarySort),
    /// Tracks of an album, in order, tagged with the album they belong to.
    AlbumTracks {
        id: String,
        #[allow(dead_code)]
        name: String,
        tracks: Vec<Track>,
    },
    Error(String),
    /// Non-fatal warning from the cache layer (e.g. a stale copy was served
    /// because the network is down); shown through the same toast.
    Notice(String),
    /// One page of the user's library playlists (appended), next offset.
    Playlists(Vec<LibraryPlaylist>, Option<u32>),
    /// Tracks of a library playlist: first page (replaces) or a later page
    /// (appended when the modal already holds tracks).
    PlaylistTracks {
        id: String,
        tracks: Vec<Track>,
        next: Option<u32>,
    },
    /// Lyrics for `song` (background fetch, tagged so a late arrival for a
    /// previous track is dropped).
    Lyrics {
        song: String,
        result: Result<lyrics::Fetched, String>,
    },
    /// Update check finished, with the newer release's parsed `SHA256SUMS`
    /// (already fetched, so the self-updater needs no second API call).
    Update(Box<Check>, Option<Vec<(String, String)>>),
    /// One line of the running update download's log.
    UpdateLog(String),
    /// A self-update finished downloading + verifying + staging.
    Staged(Result<Box<update::Pending>, String>),
    /// A download for a platform that cannot swap its own files: `(tag, dir)`.
    Downloaded(Result<Box<(String, std::path::PathBuf)>, String>),
    /// Installed-vs-bundled helper payload comparison, tagged with the memo key
    /// it describes.
    HelperState(Box<update::HelperCheck>, String),
    /// Progress from the pkexec helper reinstall.
    Helper(update::HelperEvent),
}

/// A helper ("wrapper-lite" payload) reinstall driven from the ? panel: its
/// log lines and, once the pkexec child exits, its result.
struct HelperInstall {
    log: Vec<String>,
    done: Option<Result<(), String>>,
}

/// What the update panel last started: a staged self-update, or a verified
/// download for a platform that installs by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateJob {
    Stage,
    Download,
}

/// Lyrics panel state for the current track (`/lyrics` is a wrapper endpoint,
/// so it has its own in-memory cache: the last track's lyrics are kept and
/// refetched on track change — see `App::ensure_lyrics`).
enum LyricsState {
    /// No track has been started (or playback stopped).
    Idle,
    /// Background fetch in flight.
    Loading,
    Ready(LyricTimeline),
    /// The wrapper answered 404: this track has no lyrics.
    NoLyrics,
    /// Wrapper unreachable / upstream error — the offline state.
    Unavailable(String),
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Search,
    Library,
    Favorites,
    Playlists,
}

/// Sub-view of the Search tab (segmented toggle; switching does not
/// re-query).
#[derive(PartialEq, Clone, Copy)]
enum SearchSub {
    /// The track results grid.
    Songs,
    /// The album results for the current/last search.
    Albums,
}

/// Library view sort order.
#[derive(PartialEq, Clone, Copy)]
enum LibrarySort {
    /// Newest first by the API's dateAdded.
    Added,
    /// Case-insensitive by album name.
    Name,
}
/// Why an album is being fetched: to open the detail modal or to enqueue.
enum AlbumJob {
    Modal { id: String },
    Queue { id: String },
}

/// Open album detail modal.
struct AlbumModal {
    album: AlbumHit,
    tracks: Option<Vec<Track>>,
}

/// Open playlist detail modal. `tracks` is None until the first page lands;
/// later pages are appended and tracked by `next`.
struct PlaylistModal {
    playlist: LibraryPlaylist,
    tracks: Option<Vec<Track>>,
    next: Option<u32>,
    loading_more: bool,
}

/// Clicks gathered during widget rendering, applied after borrow ends.
enum CentralAction {
    Play(Track),
    Enqueue(Track),
    OpenAlbum(AlbumHit),
    QueueAlbum(AlbumHit),
    OpenPlaylist(LibraryPlaylist),
    /// Toggle favorite state of a known track.
    ToggleFav(Track),
    /// Remove a favorite we only know by id (metadata never seen).
    Unfav(String),
}

struct SelftestState {
    stage: u8,
    /// Sub-step within a stage: every supervisor-mediated action lands
    /// asynchronously, so each step awaits its observable state before
    /// firing the next action (deterministic pass/fail, bounded waits).
    step: u8,
    /// stage1 / stage2 / stage4 audible targets (now-playing names).
    expect: [String; 3],
    started: std::time::Instant,
    /// Snapshot seek_epoch when the stage-6 seek was sent; the seek has
    /// landed once the published epoch passes this.
    seek_epoch: u64,
    /// Wrapper can serve cacheable CDN streams (regions non-empty) — gates
    /// the stage1 up-next-prefetch assertion; preview-only environments
    /// have nothing to cache.
    wrapper_cacheable: bool,
}

/// Auto-dismiss timeout of the collapsed error toast.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(8);
/// How long the detail view shows its "Copied" confirmation.
const COPIED_FEEDBACK: std::time::Duration = std::time::Duration::from_secs(1);

/// Error toast. The detail view (`expanded`) pauses auto-expiry: closing it
/// resumes with the *remaining* time (not a full reset), so a toast that was
/// about to expire still goes away promptly after a quick look.
struct ErrorToast {
    /// Full error text (HTTP status lines can be long).
    msg: String,
    /// Instant the toast auto-dismisses at; frozen while `paused_at` is set.
    expires_at: std::time::Instant,
    /// When the detail view was opened, or `None` while collapsing/collapsed.
    paused_at: Option<std::time::Instant>,
    /// Detail view open (also freezes expiry via `paused_at`).
    expanded: bool,
    /// When "Copy" was last pressed (drives the transient "Copied ✓" label).
    copied_at: Option<std::time::Instant>,
}

impl ErrorToast {
    fn new(msg: String) -> Self {
        let now = std::time::Instant::now();
        Self {
            msg,
            expires_at: now + TOAST_TTL,
            paused_at: None,
            expanded: false,
            copied_at: None,
        }
    }

    /// Open/close the detail view. Opening freezes the countdown; closing
    /// rebases `expires_at` so the remaining time is preserved.
    fn set_expanded(&mut self, expanded: bool, now: std::time::Instant) {
        match (expanded, self.paused_at) {
            (true, None) => self.paused_at = Some(now),
            (false, Some(t)) => {
                self.expires_at = now + self.expires_at.saturating_duration_since(t);
                self.paused_at = None;
            }
            _ => {}
        }
        self.expanded = expanded;
    }

    /// Time until auto-dismiss (frozen while paused).
    fn remaining(&self, now: std::time::Instant) -> std::time::Duration {
        match self.paused_at {
            Some(t) => self.expires_at.saturating_duration_since(t),
            None => self.expires_at.saturating_duration_since(now),
        }
    }

    fn is_expired(&self, now: std::time::Instant) -> bool {
        self.paused_at.is_none() && now >= self.expires_at
    }

    /// True while the "Copied ✓" confirmation should be shown.
    fn copied_feedback(&self, now: std::time::Instant) -> bool {
        matches!(self.copied_at, Some(t) if now.saturating_duration_since(t) < COPIED_FEEDBACK)
    }

    /// Time until this toast next needs a frame, or `None` once it is static:
    /// the auto-dismiss countdown (frozen while the detail view is open) or the
    /// transient "Copied ✓" label, whichever expires first. Feeds the single
    /// repaint driver so both keep ticking while the window is unfocused.
    fn next_repaint(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        let ttl = (!self.expanded).then(|| self.remaining(now));
        let copied = self.copied_at.and_then(|t| {
            let left = COPIED_FEEDBACK.saturating_sub(now.saturating_duration_since(t));
            (!left.is_zero()).then_some(left)
        });
        match (ttl, copied) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// UI settings persisted to ~/.config/mordent/state.toml (atomic write).
#[derive(Serialize, Deserialize, Default)]
struct UiState {
    volume: Option<f32>,
    last_view: Option<String>,
    /// Search sub-view: "songs" | "albums" (optional; older builds lack it).
    search_sub: Option<String>,
    repeat: Option<String>,
    /// Favorited track ids, sorted for a stable file (in memory it is a set).
    favorites: Option<Vec<String>>,
    /// Unix seconds of the last update check (the 24 h gate, see `update.rs`).
    last_update_check: Option<u64>,
    /// Release tag the last check saw.
    last_seen_tag: Option<String>,
    /// Tag whose player-bar "newer version" hint the user dismissed (stored by
    /// tag, so a *later* release shows the hint again).
    update_notice_dismissed: Option<String>,
    /// Memoised identity of this build's bundled helper payload, and what it
    /// describes (`"<version>|<bundle dir>"`) — hashing the payload is not free.
    payload_hash: Option<String>,
    payload_hash_key: Option<String>,
}

fn state_path() -> std::path::PathBuf {
    crate::config::home().join(".config/mordent/state.toml")
}

fn load_state() -> UiState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(st: &UiState) {
    let Ok(text) = toml::to_string_pretty(st) else {
        return;
    };
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("toml.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn sleep_label(sleep_at: &Option<(std::time::Instant, std::time::Duration)>) -> String {
    match sleep_at {
        None => "sleep off".to_string(),
        Some((t0, d)) => format!("sleep {}", fmt_pos(d.saturating_sub(t0.elapsed()))),
    }
}

/// Bounded delay until `App::update` must run again, or `None` when nothing is
/// pending and the event loop may park indefinitely.
///
/// Auto-advance, the MPRIS command drain and the toast countdown are all
/// evaluated inside `update()`, and egui/winit parks the frame loop when no
/// repaint is requested and no window events arrive (an unfocused window gets
/// no events at all). So a frame is guaranteed in every state that could still
/// produce work: playing ticks fast (100 ms) to drive progress and the
/// natural-end check; a loaded/preparing track, a non-empty queue or a
/// debounced write ticks slower (250 ms); a live toast or sleep-timer deadline
/// wakes exactly when it elapses (whichever comes first). With none of those
/// there is nothing left to do and the loop parks.
fn next_repaint_delay(
    playing: bool,
    work_pending: bool,
    toast: Option<std::time::Duration>,
    sleep: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    let mut delay = if playing {
        Some(std::time::Duration::from_millis(100))
    } else if work_pending {
        Some(std::time::Duration::from_millis(250))
    } else {
        None
    };
    for extra in toast.into_iter().chain(sleep) {
        delay = Some(delay.map_or(extra, |d| d.min(extra)));
    }
    delay
}

/// Value published to the wake thread meaning "fully idle — request nothing".
const WAKE_PARKED: u64 = u64::MAX;

/// Millis handed to the off-frame wake thread for one cycle: the desired
/// delay, or the `WAKE_PARKED` sentinel when nothing is pending (pure;
/// unit-tested).
fn wake_publish_ms(delay: Option<std::time::Duration>) -> u64 {
    delay.map_or(WAKE_PARKED, |d| {
        d.as_millis().min((WAKE_PARKED - 1) as u128) as u64
    })
}

/// How long the wake thread sleeps for a published value: the delay itself,
/// or `None` when parked (idle cycles poll slowly and never repaint — no busy
/// loop). A 1 ms floor keeps a zero published delay from spinning the thread
/// hot (pure; unit-tested).
fn wake_thread_sleep(ms: u64) -> Option<std::time::Duration> {
    (ms != WAKE_PARKED).then(|| std::time::Duration::from_millis(ms.max(1)))
}

/// Permanent off-frame wake thread ("mordent-wake"). `update`'s in-frame
/// `request_repaint_after` is kept (correct and zero-cost when frames flow),
/// but it can only speak from INSIDE a frame: on Wayland a redraw withheld
/// for an unfocused window (niri-family compositors stop delivering them)
/// would silence the frame loop, and with it the driver that could revive it
/// — auto-advance then waits for focus. This thread re-issues the wake from
/// off-frame every cycle via `Context::request_repaint`, which eframe wires
/// through its `EventLoopProxy` (an event-loop user event, not a compositor
/// frame callback), so playback keeps advancing while the window is
/// unfocused. Lives until process exit, like the mpris/prefetch threads.
fn spawn_wake_thread(ctx: egui::Context, wake_ms: Arc<AtomicU64>) {
    std::thread::Builder::new()
        .name("mordent-wake".into())
        .spawn(move || loop {
            match wake_thread_sleep(wake_ms.load(Ordering::Relaxed)) {
                None => std::thread::sleep(std::time::Duration::from_secs(1)),
                Some(d) => {
                    // Sleep exactly as long as the in-frame driver intended:
                    // never more often than the driver requests.
                    std::thread::sleep(d);
                    ctx.request_repaint();
                }
            }
        })
        .expect("spawn mordent-wake thread");
}

pub struct App {
    cfg: Config,
    search: String,
    /// Last executed search term (for empty-state copy).
    last_term: String,
    results: Option<SearchResults>,
    searching: bool,
    view: View,
    /// Active Search sub-view (Songs | Albums).
    search_sub: SearchSub,
    /// User library albums (paginated).
    library: Option<Vec<AlbumHit>>,
    library_loading: bool,
    library_next: Option<u32>,
    /// When the current library page(s) were loaded; past META_TTL_LIBRARY
    /// the view refetches on entry instead of pinning session-stale rows.
    library_loaded: Option<std::time::Instant>,
    /// Library sort order (not persisted).
    library_sort: LibrarySort,
    /// Favorited track ids (persisted; sorted on save).
    favorites: HashSet<String>,
    /// id → Track seen so far, for the favorites view.
    ///
    /// LIMITATION: only ids are persisted in state.toml, so a favorite whose
    /// track has not been observed in this session (search / album / playlist
    /// / queue / now-playing) has no name/artist to render and shows as a bare
    /// id. We deliberately do not re-fetch metadata for such entries.
    fav_tracks: HashMap<String, Track>,
    /// Pending debounced state.toml write (favorite toggles).
    state_dirty: Option<std::time::Instant>,
    /// User library playlists (paginated).
    playlists: Option<Vec<LibraryPlaylist>>,
    playlists_loading: bool,
    playlists_next: Option<u32>,
    playlist_modal: Option<PlaylistModal>,
    /// MORDENT_DEMO_PLAYLIST=<n>: open the n-th playlist once the list loads
    /// (headless screenshot hook, alongside MORDENT_DEMO_VIEW).
    demo_playlist: Option<usize>,
    /// Non-consuming queue + pointer to the current entry (see queuemodel),
    /// SHARED with the playback supervisor: the UI mutates list content under
    /// the lock; the supervisor advances the pointer on natural advance.
    queue: Arc<parking_lot::Mutex<Queue>>,
    /// RENDER MIRROR of `PbSnapshot::track`, refreshed from the snapshot at
    /// the top of every frame. Rendering and labels only — every playback
    /// decision reads the snapshot (or sends a Command), never this field.
    now: Option<Track>,
    show_queue: bool,
    /// Lyrics side panel visibility (`L` / the bar's "lyrics" button).
    show_lyrics: bool,
    /// Song id `lyrics` belongs to: the fetch tag, and the refetch trigger
    /// (a different id ⇒ fetch again). The lyrics themselves are a per-track
    /// in-memory cache — `/lyrics` is a wrapper endpoint, not an amp-api URL,
    /// so the api.rs metadata cache cannot hold them.
    lyrics_song: Option<String>,
    lyrics: LyricsState,
    /// Line index the karaoke view auto-scrolled to; scrolling is applied only
    /// when the active line CHANGES so a manual scroll is never fought.
    lyrics_follow: Option<usize>,
    /// Queue row currently being dragged by its ⠿ handle, and the slot it
    /// would land in (index among the other rows) — both cleared on release.
    queue_drag: Option<usize>,
    queue_drop: Option<usize>,
    /// Album being fetched, and what to do with its tracks.
    album_job: Option<AlbumJob>,
    /// Album id whose `+` is pending (shows "adding…").
    album_busy: Option<String>,
    album_modal: Option<AlbumModal>,
    cheatsheet: bool,
    /// Sleep timer deadline (pauses playback when it fires).
    sleep_at: Option<(std::time::Instant, std::time::Duration)>,
    /// MORDENT_SELFTEST=1: drive play_now/queue/auto-advance deterministically
    /// and print SELFTEST PASS/FAIL (headless CI aid).
    selftest: Option<SelftestState>,
    volume: f32,
    /// Active theme (built-in defaults + `~/.config/mordent/theme.toml`).
    theme: Theme,
    /// (mtime, size) of theme.toml at last load; `None` when the file is absent.
    theme_stamp: Option<(std::time::SystemTime, u64)>,
    /// Throttles the once-per-second hot-reload poll; `None` = not polled yet.
    theme_poll: Option<std::time::Instant>,
    /// Font set currently installed on the egui context; starts as egui's
    /// built-ins (which is what eframe sets up) and follows `theme.font`.
    font: FontChoice,
    /// Error toast (full text + expiry state); auto-dismisses after TOAST_TTL.
    error: Option<ErrorToast>,
    last_title: String,
    /// Desktop media controls (MPRIS); `None` if D-Bus is unusable. The UI
    /// only PUBLISHES here; commands are drained by the supervisor thread.
    mpris: Option<mpris::Mpris>,
    /// Background sequential prefetch of the up-next queue (inert when the
    /// audio cache is disabled). Re-scanned by the UI after its own queue
    /// mutations; the supervisor scans after its own pointer moves.
    prefetch: Prefetcher,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    // ---- supervisor handles ----
    /// Commands to the playback supervisor (all play/pause/seek/stop/...).
    cmd: Sender<Command>,
    /// ONE playback publication read by UI, MPRIS and tray.
    pb: Arc<parking_lot::Mutex<PbSnapshot>>,
    /// Supervisor → UI errors (play-worker failures) for the toast.
    sup_rx: Receiver<String>,
    /// Desired next-wake delay in millis, published by `update` every frame
    /// and read by the "mordent-wake" thread (`WAKE_PARKED` = fully idle).
    wake_ms: Arc<AtomicU64>,
    /// `MORDENT_NO_REPAINT=1` debug hook: suppress ALL repaint requests (in-frame
    /// driver + wake thread + supervisor wakes) so `update` effectively stops
    /// being scheduled — the decoupling proof that playback still advances.
    no_repaint: bool,
    /// Mirrored window visibility, kept in sync by the ✕ handler (hide),
    /// the tray's Show/Hide + Activate, and MPRIS Raise (show).
    hidden: Arc<std::sync::atomic::AtomicBool>,
    /// Last volume/repeat values seen in the snapshot: external changes
    /// (MPRIS / tray) are adopted into the local mirrors + state.toml.
    vol_seen: f32,
    repeat_seen: Repeat,
    /// Optimistic local shuffle mirror: `toggle_shuffle` flips THIS and sends
    /// the explicit target — deriving it from the snapshot would read the
    /// pre-command value and send the same `SetShuffle` twice (the supervisor
    /// dedupes equal targets, so the queue would stay shuffled).
    shuffle: bool,
    shuffle_seen: bool,
    /// One-shot "decode-only" log flag (the supervisor owns the real state).
    decode_logged: bool,
    /// MORDENT_SELFTEST: does the wrapper serve cacheable CDN streams? Probed
    /// once on a background thread (blocking HTTP must never run on the UI
    /// thread — under the supervisor's parallel fetches it can stall for
    /// seconds and freeze the frame loop).
    wrapper_cacheable: Arc<std::sync::atomic::AtomicBool>,
    /// First-run setup wizard; `Some` while the wizard owns the window (see
    /// setup.rs). Started by `mordent setup`, or by `mordent ui` when no config.toml
    /// existed at launch. Never under MORDENT_SELFTEST; `MORDENT_DEMO_SETUP=<1..7>`
    /// lands directly on a step (headless screenshot hook).
    setup: Option<crate::setup::SetupState>,
    /// egui context, kept only so off-frame update threads can request a frame
    /// when their message lands (the loop parks when nothing is playing).
    ctx: egui::Context,
    // ---- updates (see update.rs) ----
    /// Result of the last completed check; `None` until one finishes.
    update_check: Option<Check>,
    /// An update check/download is running (spinner, buttons disabled).
    update_busy: bool,
    /// Which job `update_busy` refers to (button label/status line).
    update_job: Option<UpdateJob>,
    /// Log lines of the running job (shown in the ? panel).
    update_log: Vec<String>,
    /// The newer release's parsed SHA256SUMS, kept so the panel's download
    /// button needs no second API call.
    update_sums: Option<Vec<(String, String)>>,
    /// Update downloaded, verified and staged for the next launch.
    update_pending: Option<update::Pending>,
    /// Verified folder downloaded for a platform that installs by hand.
    update_downloaded: Option<(String, std::path::PathBuf)>,
    /// Tag this launch installed (`update::apply_pending`), toasted once.
    update_applied: Option<String>,
    /// Tag whose hint the user dismissed (mirrored from state.toml).
    update_notice_dismissed: Option<String>,
    /// Last check time / tag seen (mirrored from state.toml; the 24 h gate).
    last_update_check: Option<u64>,
    last_seen_tag: Option<String>,
    /// Memoised bundled-payload identity + the key it is valid for.
    payload_hash: Option<String>,
    payload_hash_key: Option<String>,
    /// Installed helper vs this build's payload; `None` until checked.
    helper_state: Option<update::HelperState>,
    helper_busy: bool,
    /// The configured helper URL is not on this machine: nothing here to update.
    helper_remote: bool,
    /// A pkexec helper reinstall started from the panel.
    helper_install: Option<HelperInstall>,
}

impl App {
    fn new(cfg: Config, egui_ctx: egui::Context, start_wizard: bool) -> Self {
        let state = load_state();
        let (theme, theme_warns) = Theme::load();
        for w in &theme_warns {
            eprintln!("[theme] {w}");
        }
        let volume = state.volume.unwrap_or(0.8).clamp(0.0, 1.0);
        let repeat = match state.repeat.as_deref() {
            Some("all") => Repeat::All,
            Some("one") => Repeat::One,
            _ => Repeat::Off,
        };
        // MORDENT_NO_REPAINT=1 debug hook: suppress ALL repaint requests (see the
        // field doc). Meaningless together with MORDENT_SELFTEST (whose stage
        // machine needs frames) — the selftest wins, this flag is ignored.
        let no_repaint = std::env::var("MORDENT_NO_REPAINT").is_ok()
            && std::env::var("MORDENT_SELFTEST").is_err();
        if no_repaint {
            eprintln!(
                "[no-repaint] MORDENT_NO_REPAINT=1: every repaint request is suppressed \
                 (in-frame driver, mordent-wake thread, supervisor wakes) — the UI stops \
                 being scheduled after the initial frames; playback must not care"
            );
        }
        // MPRIS: spawn() only starts a thread (the connection is established
        // there), so this is as safe as the old first-frame lazy start. The
        // UI keeps the publish handle; the SUPERVISOR drains the commands.
        let (mpris, mpris_rx) = match mpris::Mpris::spawn() {
            Some((m, rx)) => (Some(m), Some(rx)),
            None => (None, None),
        };
        let (tx, rx) = channel();
        let prefetch = Prefetcher::new(&cfg);
        let queue = Arc::new(parking_lot::Mutex::new(Queue::default()));
        // Mirrored window visibility (✕/tray/MPRIS Raise keep it in sync).
        let hidden = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // MORDENT_SELFTEST wrapper probe result (filled on a background thread —
        // blocking HTTP must never run on the UI thread).
        let wrapper_cacheable = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cfg_probe = cfg.clone();
        let wc_flag = wrapper_cacheable.clone();
        // The playback supervisor owns audio + the advance state machine.
        // The selftest pins repeat-off for its deterministic assertions (the
        // queue-model stage relies on natural advance moving on).
        let selftest = std::env::var("MORDENT_SELFTEST").is_ok();
        // The selftest stage machine asserts on REAL search results, so it is
        // meaningless without a query. Fail loudly and immediately instead of
        // sitting idle until the (minutes-later) per-stage timeout.
        if selftest
            && std::env::var("MORDENT_DEMO_TERM")
                .map(|t| t.trim().is_empty())
                .unwrap_or(true)
        {
            eprintln!(
                "[selftest] no MORDENT_DEMO_TERM set — selftest needs a search query \
                 (e.g. MORDENT_DEMO_TERM=\"daft punk\"); exiting"
            );
            std::process::exit(2);
        }
        let (cmd, pb, sup_rx) = supervisor::spawn(
            cfg.clone(),
            queue.clone(),
            if selftest { Repeat::Off } else { repeat },
            volume,
            egui_ctx.clone(),
            no_repaint,
            mpris_rx,
            prefetch.clone(),
            hidden.clone(),
        );
        // Tray (StatusNotifierItem; stubbed no-op off Linux — see main.rs).
        // Best-effort by design: any D-Bus/watcher failure degrades to no
        // icon, and ✕ still hides the window either way.
        crate::tray::spawn(
            pb.clone(),
            cmd.clone(),
            hidden.clone(),
            egui_ctx.clone(),
        );
        // Parked sentinel until the first frame publishes a real delay.
        let wake_ms = Arc::new(AtomicU64::new(WAKE_PARKED));
        // First-run wizard. MORDENT_SELFTEST never enters it; MORDENT_DEMO_SETUP=<1..7>
        // lands straight on a step (headless screenshot hook).
        let setup = if selftest {
            None
        } else if let Ok(n) = std::env::var("MORDENT_DEMO_SETUP") {
            Some(crate::setup::SetupState::demo(
                n.trim().parse().unwrap_or(1),
                &cfg,
            ))
        } else if start_wizard {
            let mut wizard = crate::setup::SetupState::new(&cfg);
            wizard.seed_update_cache(state.last_update_check, state.last_seen_tag.clone());
            // One automatic check, so the wizard's Preferences step can show the
            // newest release without the user pressing anything. Never under
            // the demo/screenshot hook (see `auto_check_updates`).
            wizard.auto_check_updates();
            Some(wizard)
        } else {
            None
        };
        let mut app = Self {
            cfg,
            search: String::new(),
            last_term: String::new(),
            results: None,
            searching: false,
            view: View::Search,
            search_sub: SearchSub::Songs,
            library: None,
            library_loading: false,
            library_next: None,
            library_loaded: None,
            library_sort: LibrarySort::Added,
            favorites: state.favorites.clone().unwrap_or_default().into_iter().collect(),
            fav_tracks: HashMap::new(),
            state_dirty: None,
            playlists: None,
            playlists_loading: false,
            playlists_next: None,
            playlist_modal: None,
            demo_playlist: std::env::var("MORDENT_DEMO_PLAYLIST")
                .ok()
                .and_then(|s| s.parse().ok()),
            queue,
            now: None,
            show_queue: false,
            show_lyrics: false,
            lyrics_song: None,
            lyrics: LyricsState::Idle,
            lyrics_follow: None,
            queue_drag: None,
            queue_drop: None,
            album_job: None,
            album_busy: None,
            album_modal: None,
            cheatsheet: false,
            sleep_at: None,
            selftest: selftest.then(|| SelftestState {
                stage: 0,
                step: 0,
                expect: Default::default(),
                started: std::time::Instant::now(),
                seek_epoch: 0,
                wrapper_cacheable: false,
            }),
            volume,
            theme,
            theme_stamp: Theme::stamp(&crate::theme::theme_path()),
            theme_poll: None,
            // eframe starts every app on egui's built-in fonts; the first
            // poll_theme installs the configured set.
            font: FontChoice::Default,
            // invalid theme keys are surfaced once through the toast
            error: theme_warns.first().map(|w| ErrorToast::new(w.clone())),
            last_title: "Mordent".into(),
            mpris,
            prefetch,
            rx,
            tx,
            cmd,
            pb,
            sup_rx,
            wake_ms: wake_ms.clone(),
            no_repaint,
            hidden,
            vol_seen: volume,
            repeat_seen: repeat,
            shuffle: false,
            shuffle_seen: false,
            decode_logged: false,
            wrapper_cacheable,
            setup,
            ctx: egui_ctx.clone(),
            update_check: None,
            update_busy: false,
            update_job: None,
            update_log: Vec::new(),
            update_sums: None,
            update_pending: update::read_pending(),
            update_downloaded: None,
            update_applied: None,
            update_notice_dismissed: state.update_notice_dismissed.clone(),
            last_update_check: state.last_update_check,
            last_seen_tag: state.last_seen_tag.clone(),
            payload_hash: state.payload_hash.clone(),
            payload_hash_key: state.payload_hash_key.clone(),
            helper_state: None,
            helper_busy: false,
            helper_remote: false,
            helper_install: None,
        };
        app.helper_remote = !is_loopback_url(&app.cfg.wrapper_url);
        // Startup wrapper reachability hint (once, off-thread — see the fn).
        app.spawn_wrapper_check();
        // Updates: read what the previous launch installed (toast once), then
        // check GitHub at most once a day and compare the installed helper
        // payload. Both are off-thread; neither runs under MORDENT_SELFTEST
        // (deterministic) or while the wizard owns the window (the wizard has
        // its own check on its Preferences step).
        if !selftest {
            app.update_applied = update::take_applied();
            if let Some(tag) = app.update_applied.clone() {
                app.fail(format!(
                    "Mordent {tag} was installed — restart to run the new version."
                ));
            }
            if app.setup.is_none() {
                app.check_updates_now(false);
                app.start_helper_check();
            }
        }
        if selftest {
            std::thread::Builder::new()
                .name("mordent-wrapper-probe".into())
                .spawn(move || {
                    let cacheable = crate::wrapper::Wrapper::new(&cfg_probe.wrapper_url)
                        .regions()
                        .map(|r| !r.is_empty())
                        .unwrap_or(false);
                    wc_flag.store(cacheable, std::sync::atomic::Ordering::Relaxed);
                })
                .ok();
        }
        if !no_repaint {
            spawn_wake_thread(egui_ctx.clone(), wake_ms);
        }
        match state.last_view.as_deref() {
            // older builds had Albums as a main tab; map it to the Search
            // sub-view. Unknown values fall back to Search (the default).
            Some("albums") => app.search_sub = SearchSub::Albums,
            Some("library") => app.switch_view(View::Library),
            Some("favorites") => app.switch_view(View::Favorites),
            Some("playlists") => app.switch_view(View::Playlists),
            _ => {}
        }
        // persisted Search sub-view; unknown values → Songs (the default)
        match state.search_sub.as_deref() {
            Some("albums") => app.search_sub = SearchSub::Albums,
            Some("songs") => app.search_sub = SearchSub::Songs,
            _ => {}
        }
        if app.selftest.is_some() {
            // The auto-advance stage asserts queue promotion; a persisted
            // repeat-one would restart the same track instead, so pin the
            // input the deterministic test depends on (the supervisor was
            // spawned with Repeat::Off above).
            eprintln!("[selftest] armed (repeat forced off)");
        }
        // headless verification hooks: MORDENT_DEMO_TERM=<query> auto-searches,
        // MORDENT_DEMO_VIEW=library|albums|favorites|playlists opens that view
        // ("albums" = the Search view's Albums sub-view), MORDENT_DEMO_QUEUE=1
        // opens the queue panel.
        if let Ok(term) = std::env::var("MORDENT_DEMO_TERM") {
            app.search = term;
            app.spawn_search();
        }
        if std::env::var("MORDENT_DEMO_QUEUE").is_ok() {
            app.show_queue = true;
        }
        // MORDENT_DEMO_LYRICS=1: open the lyrics panel (headless screenshot hook;
        // pairs with MORDENT_DEMO_PLAY so the karaoke highlight has a position).
        if std::env::var("MORDENT_DEMO_LYRICS").is_ok() {
            app.show_lyrics = true;
        }
        // MORDENT_DEMO_PLAY=<n>: frame-INDEPENDENT headless play hook. Searches
        // MORDENT_DEMO_TERM (default "daft punk"), queues tracks n..n+3 and plays
        // track n via the supervisor command channel — deliberately not tied
        // to poll_messages, so MORDENT_NO_REPAINT runs (whose frame loop stops
        // being scheduled) still start playback and exercise natural advance.
        if let Some(n) = std::env::var("MORDENT_DEMO_PLAY")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            let cfg = app.cfg.clone();
            let cmd = app.cmd.clone();
            let queue = app.queue.clone();
            let term = std::env::var("MORDENT_DEMO_TERM").unwrap_or_else(|_| "daft punk".into());
            std::thread::Builder::new()
                .name("mordent-demo-play".into())
                .spawn(move || {
                    let Ok(api) = Api::new(cfg) else { return };
                    let Ok(res) = api.search(&term, 30) else { return };
                    let tracks: Vec<Track> =
                        res.tracks.into_iter().skip(n).take(4).collect();
                    if tracks.is_empty() {
                        eprintln!("[demo-play] no search results for {term:?}");
                        return;
                    }
                    let first = tracks[0].clone();
                    queue.lock().insert_after_current(tracks);
                    eprintln!("[demo-play] queued 4 from {term:?}, playing #{n}");
                    let _ = cmd.send(Command::Play {
                        track: first,
                        close: 0,
                    });
                })
                .expect("spawn mordent-demo-play thread");
        }
        // MORDENT_DEMO_CLOSE=1: simulate a WM close request
        // (`ViewportCommand::Close` → `close_requested()` on the next frame) so
        // the window-close policy is testable headlessly. Together with
        // MORDENT_DEMO_PLAY the request waits until a track is current — otherwise
        // playback would race the close and the idle branch would win; alone it
        // fires after a short delay that lets the window map.
        if std::env::var("MORDENT_DEMO_CLOSE").is_ok() {
            let ctx = egui_ctx.clone();
            let pb = app.pb.clone();
            let wait_for_play = std::env::var("MORDENT_DEMO_PLAY").is_ok();
            std::thread::Builder::new()
                .name("mordent-demo-close".into())
                .spawn(move || {
                    if wait_for_play {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(30);
                        while !pb.lock().has_track && std::time::Instant::now() < deadline {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        // Let the window map and a couple of frames run, so a
                        // headless observer can see the visible state before
                        // the close (the real user closes well after this).
                        std::thread::sleep(std::time::Duration::from_secs(3));
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                    eprintln!(
                        "[demo-close] sending ViewportCommand::Close (simulated WM close; \
                         ever_played={})",
                        pb.lock().ever_played
                    );
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                })
                .expect("spawn mordent-demo-close thread");
        }
        match std::env::var("MORDENT_DEMO_VIEW").as_deref() {
            Ok("library") => app.switch_view(View::Library),
            Ok("albums") => app.search_sub = SearchSub::Albums,
            Ok("favorites") => app.switch_view(View::Favorites),
            Ok("playlists") => app.switch_view(View::Playlists),
            _ => {}
        }
        app
    }

    // ---- actions ----

    fn spawn_search(&mut self) {
        let term = self.search.trim().to_string();
        if term.is_empty() || self.searching {
            return;
        }
        self.searching = true;
        self.last_term = term.clone();
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let api = match Api::new(cfg) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
            match api.search(&term, 30) {
                Ok(r) => {
                    let _ = tx.send(Msg::Results(Box::new(r)));
                    if let Some(note) = api.take_notice() {
                        let _ = tx.send(Msg::Notice(note));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    /// Restart the library fetch from page 0 (sort switch): drop all pages
    /// held under the old server order. An in-flight page of the old sort is
    /// discarded on arrival by the sort tag on `Msg::Library`.
    fn restart_library(&mut self) {
        self.library = None;
        self.library_next = None;
        self.library_loaded = None;
        self.spawn_library(0);
    }

    fn spawn_library(&mut self, offset: u32) {
        self.library_loading = true;
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let sort = self.library_sort;
        std::thread::spawn(move || {
            let api = match Api::new(cfg) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
            // Added: server-side newest-first so page 0 contains the recent
            // additions (the default alphabetical order puts them outside
            // page 0, which made the view look frozen at an old snapshot).
            // Name: no server support (400); the client sorts the fetched
            // pages case-insensitively.
            let param = match sort {
                LibrarySort::Added => Some("-dateAdded"),
                LibrarySort::Name => None,
            };
            match api.library_albums_page(100, offset, param) {
                Ok((albums, next)) => {
                    let _ = tx.send(Msg::Library(albums, next, sort));
                    if let Some(note) = api.take_notice() {
                        let _ = tx.send(Msg::Notice(note));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_playlists(&mut self, offset: u32) {
        self.playlists_loading = true;
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let api = match Api::new(cfg) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
            match api.library_playlists(50, offset) {
                Ok((lists, next)) => {
                    let _ = tx.send(Msg::Playlists(lists, next));
                    if let Some(note) = api.take_notice() {
                        let _ = tx.send(Msg::Notice(note));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_playlist_tracks(&mut self, id: String, offset: u32) {
        if let Some(m) = &mut self.playlist_modal
            && m.playlist.id == id
        {
            m.loading_more = offset > 0;
        }
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let api = match Api::new(cfg) {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
            match api.playlist_tracks_page(&id, 100, offset) {
                Ok((tracks, next)) => {
                    let _ = tx.send(Msg::PlaylistTracks { id, tracks, next });
                    if let Some(note) = api.take_notice() {
                        let _ = tx.send(Msg::Notice(note));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn open_playlist(&mut self, p: LibraryPlaylist) {
        let id = p.id.clone();
        self.playlist_modal = Some(PlaylistModal {
            playlist: p,
            tracks: None,
            next: None,
            loading_more: false,
        });
        self.spawn_playlist_tracks(id, 0);
    }

    fn close_playlist_modal(&mut self) {
        self.playlist_modal = None;
    }

    fn switch_view(&mut self, v: View) {
        self.view = v;
        self.save_ui_state();
        if v == View::Library && !self.library_loading {
            // within the metadata TTL the in-memory page is served as-is;
            // past it, a re-entry refetches so a long session never pins
            // stale rows (the disk cache may serve its own fresh copy)
            let expired = self
                .library_loaded
                .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(META_TTL_LIBRARY));
            if expired {
                self.library = None;
                self.library_next = None;
                self.library_loaded = None;
            }
            if self.library.is_none() {
                self.spawn_library(0);
            }
        }
        if v == View::Playlists && self.playlists.is_none() && !self.playlists_loading {
            self.spawn_playlists(0);
        }
    }

    /// Toggle the Search sub-view (no re-query); persisted like last_view.
    fn set_search_sub(&mut self, s: SearchSub) {
        if self.search_sub != s {
            self.search_sub = s;
            self.save_ui_state();
        }
    }

    fn fail(&mut self, e: String) {
        self.error = Some(ErrorToast::new(e));
    }

    /// One-shot startup reachability probe for the wrapper. Off-thread by
    /// design: blocking HTTP must never run on the UI thread (a slow remote/
    /// tunnel `/status` would freeze the first frame). Silent when the wrapper
    /// answers; on failure it sends ONE actionable hint through the normal
    /// notice/toast path (`Msg::Notice`), once per session.
    fn spawn_wrapper_check(&self) {
        let url = self.cfg.wrapper_url.clone();
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("mordent-wrapper-check".into())
            .spawn(move || {
                if !probe_wrapper_reachable(&url) {
                    let _ = tx.send(Msg::Notice(wrapper_down_hint(&url)));
                }
            })
            .ok();
    }

    // ---- updates (checker + self-updater; see update.rs) ----

    /// Spawn one update-check thread. `force` is the user's "Check now": it
    /// bypasses both the 24 h gate and the `check_updates` preference. The
    /// automatic path is silent — a rate limit, an offline machine or a missing
    /// release is logged, never toasted.
    fn check_updates_now(&mut self, force: bool) {
        if self.update_busy {
            return;
        }
        let now = crate::config::unix_now();
        let cached = update::Cached {
            last_check: self.last_update_check,
            last_seen_tag: self.last_seen_tag.clone(),
        };
        if !update::should_check(&cached, self.cfg.check_updates, now, force) {
            eprintln!(
                "[update] not checking ({}; {})",
                if self.cfg.check_updates {
                    "inside the 24 h window"
                } else {
                    "check_updates = false"
                },
                update::last_check_label(self.last_update_check, now)
            );
            return;
        }
        self.update_busy = true;
        self.update_job = None;
        let tx = self.tx.clone();
        let ctx = (!self.no_repaint).then(|| self.ctx.clone());
        std::thread::Builder::new()
            .name("mordent-update-check".into())
            .spawn(move || {
                let (check, sums) = update::check_with_sums(&update::api_agent());
                match &check {
                    Check::Newer { latest, .. } => {
                        eprintln!("[update] {latest} is available (running {})", update::version().display())
                    }
                    Check::UpToDate { latest } => eprintln!("[update] up to date ({latest})"),
                    Check::Uncomparable { latest } => {
                        eprintln!("[update] newest release {latest}; this build has no comparable tag")
                    }
                    Check::Failed { reason } => eprintln!("[update] check failed: {reason}"),
                }
                if let Some(ctx) = &ctx {
                    ctx.request_repaint();
                }
                let _ = tx.send(Msg::Update(Box::new(check), sums));
            })
            .ok();
    }

    /// Compare the installed helper payload with the one bundled next to this
    /// binary (off-thread: it may hash ~100 MB of payload).
    fn start_helper_check(&mut self) {
        if self.helper_busy {
            return;
        }
        let scripts = crate::setup::resolve_scripts_dir();
        let bundle = scripts.as_deref().map(crate::setup::bundle_dir);
        let key = match &bundle {
            Some(b) => format!("{}|{}", update::version().display(), b.display()),
            None => String::new(),
        };
        let memo = if self.payload_hash_key.as_deref() == Some(key.as_str()) {
            self.payload_hash.clone()
        } else {
            None
        };
        let prefix = std::path::PathBuf::from(crate::setup::helper_prefix());
        self.helper_busy = true;
        let tx = self.tx.clone();
        let ctx = (!self.no_repaint).then(|| self.ctx.clone());
        std::thread::Builder::new()
            .name("mordent-helper-check".into())
            .spawn(move || {
                let report = update::helper_check(bundle.as_deref(), &prefix, memo.as_deref());
                eprintln!(
                    "[update] helper payload: {:?} (installed vs this build)",
                    report.state
                );
                if let Some(ctx) = &ctx {
                    ctx.request_repaint();
                }
                let _ = tx.send(Msg::HelperState(Box::new(report), key));
            })
            .ok();
    }

    /// Download + verify + stage the newer release. The swap itself happens at
    /// the next launch (`update::apply_pending`, called by `main`), so an update
    /// can never interrupt playback.
    fn start_self_update(&mut self, latest: update::Tag, assets: Vec<update::Asset>, sums: Option<Vec<(String, String)>>) {
        if self.update_busy {
            return;
        }
        let kind = update::kind();
        let plan = match update::plan(
            kind,
            std::env::consts::OS,
            std::env::consts::ARCH,
            None,
            latest,
            &assets,
            &sums.unwrap_or_default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                self.fail(format!("Cannot update automatically: {e:#}"));
                return;
            }
        };
        self.start_update_job(plan, UpdateJob::Stage);
    }

    /// The manual path (Windows zip, unrecognised layout): download, verify and
    /// unpack so the user can put the files in place themselves.
    fn start_manual_download(&mut self, latest: update::Tag, assets: Vec<update::Asset>, sums: Option<Vec<(String, String)>>) {
        if self.update_busy {
            return;
        }
        let plan = match update::plan(
            update::kind(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            None,
            latest,
            &assets,
            &sums.unwrap_or_default(),
        ) {
            Ok(p) => p,
            Err(e) => {
                self.fail(format!("Cannot download the release: {e:#}"));
                return;
            }
        };
        self.start_update_job(plan, UpdateJob::Download);
    }

    fn start_update_job(&mut self, plan: update::Plan, job: UpdateJob) {
        eprintln!(
            "[update] {} {} ({} → {})",
            match job {
                UpdateJob::Stage => "staging",
                UpdateJob::Download => "downloading",
            },
            plan.asset.name,
            plan.asset.browser_download_url,
            plan.target.display()
        );
        self.update_busy = true;
        self.update_job = Some(job);
        self.update_log.clear();
        let tx = self.tx.clone();
        let ctx = (!self.no_repaint).then(|| self.ctx.clone());
        std::thread::Builder::new()
            .name("mordent-update-download".into())
            .spawn(move || {
                let mut sink = |line: String| {
                    if line.starts_with("downloading") {
                        eprintln!("[update] {line}");
                    }
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                    let _ = tx.send(Msg::UpdateLog(line));
                };
                let msg = match job {
                    UpdateJob::Stage => Msg::Staged(
                        update::stage(&plan, &mut sink)
                            .map(Box::new)
                            .map_err(|e| format!("{e:#}")),
                    ),
                    UpdateJob::Download => {
                        let tag = plan.tag.to_string();
                        Msg::Downloaded(
                            update::fetch_for_manual(&plan, &mut sink)
                                .map(|dir| Box::new((tag, dir)))
                                .map_err(|e| format!("{e:#}")),
                        )
                    }
                };
                if let Some(ctx) = &ctx {
                    ctx.request_repaint();
                }
                let _ = tx.send(msg);
            })
            .ok();
    }

    /// Re-run the shipped installer (pkexec → the desktop's auth dialog) so
    /// `/opt/mordent-wrapper` gets this build's payload. The script is
    /// idempotent: it stops the service, replaces the payload, restarts it and
    /// waits for `/status`.
    fn start_helper_install(&mut self) {
        if self.helper_install.as_ref().is_some_and(|h| h.done.is_none()) {
            return;
        }
        let Some(scripts) = crate::setup::resolve_scripts_dir() else {
            self.fail("The helper scripts are not next to this build — run `mordent setup`.".into());
            return;
        };
        let port = helper_port(&self.cfg.wrapper_url);
        let (htx, hrx) = channel();
        let tx = self.tx.clone();
        let ctx = (!self.no_repaint).then(|| self.ctx.clone());
        std::thread::Builder::new()
            .name("mordent-helper-forward".into())
            .spawn(move || {
                for ev in hrx {
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                    let _ = tx.send(Msg::Helper(ev));
                }
            })
            .ok();
        match update::spawn_helper_install(&scripts, port, htx) {
            Ok(()) => {
                eprintln!(
                    "[update] reinstalling the helper from {} (port {port})",
                    scripts.display()
                );
                self.helper_install = Some(HelperInstall {
                    log: vec![format!("running {}/install-wrapper.sh --port {port}", scripts.display())],
                    done: None,
                });
            }
            Err(e) => self.fail(format!("Could not start the helper update: {e}")),
        }
    }

    /// The player-bar "a newer version exists" hint, unless the user dismissed
    /// that tag. One short button, no modal, nothing that steals focus.
    fn update_hint(&self) -> Option<String> {
        match &self.update_check {
            Some(Check::Newer { latest, .. }) => {
                let tag = latest.to_string();
                (self.update_notice_dismissed.as_deref() != Some(tag.as_str())).then_some(tag)
            }
            _ => None,
        }
    }

    /// Persist the update bookkeeping into state.toml (debounced write).
    fn note_update_state_dirty(&mut self) {
        self.state_dirty = Some(std::time::Instant::now());
    }

    /// The "updates" section of the ? panel: this build's version, the newest
    /// release, the 24 h-gated check, the self-update button for this install
    /// and the installed-helper payload comparison.
    fn updates_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let now = crate::config::unix_now();
        let version = update::version();
        let kind = update::kind();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Mordent {}", version.display())).strong());
            ui.weak(format!("({})", kind.label()));
        });
        if let Some(tag) = &self.update_applied {
            ui.label(
                egui::RichText::new(format!(
                    "{tag} was installed at startup — restart Mordent to run it."
                ))
                .color(theme.lossless),
            );
        }
        match &self.update_check {
            Some(Check::Newer { latest, .. }) => {
                ui.label(
                    egui::RichText::new(format!("{latest} is available"))
                        .strong()
                        .color(theme.accent),
                );
            }
            Some(Check::UpToDate { latest }) => {
                ui.weak(format!("up to date — {latest} is the newest release"));
            }
            Some(Check::Uncomparable { latest }) => {
                ui.weak(format!(
                    "newest release: {latest} (this build carries no comparable version)"
                ));
            }
            Some(Check::Failed { reason }) => {
                ui.weak(format!("check failed: {reason}"));
            }
            None => {
                ui.weak("not checked yet");
            }
        }
        ui.horizontal(|ui| {
            ui.weak(update::last_check_label(self.last_update_check, now));
            if self.update_busy {
                ui.spinner();
            } else {
                if ui.small_button("Check now").clicked() {
                    self.check_updates_now(true);
                }
                let mut on = self.cfg.check_updates;
                if ui
                    .checkbox(&mut on, "check daily")
                    .on_hover_text(
                        "One unauthenticated request to api.github.com per day; nothing is \
                         sent about you or your library.",
                    )
                    .changed()
                {
                    self.cfg.check_updates = on;
                    if let Err(e) = crate::config::write_config(&self.cfg) {
                        self.fail(format!("Could not save the preference: {e:#}"));
                    } else {
                        eprintln!("[update] check_updates = {on}");
                        if on {
                            self.check_updates_now(false);
                        }
                    }
                }
            }
        });

        // ---- this install's update path ----
        let newer = match &self.update_check {
            Some(Check::Newer { latest, assets, .. }) => Some((*latest, assets.clone())),
            _ => None,
        };
        if let Some(p) = self.update_pending.clone() {
            ui.label(
                egui::RichText::new(format!(
                    "{} is downloaded and verified — it will be installed the next time you start Mordent.",
                    p.tag
                ))
                .color(theme.lossless),
            );
        } else if let Some((tag, dir)) = self.update_downloaded.clone() {
            ui.label(
                egui::RichText::new(format!("{tag} downloaded, checksum verified and unpacked."))
                    .color(theme.lossless),
            );
            ui.weak(format!("in {}", dir.display()));
            if ui.small_button("Open the folder").clicked()
                && let Err(e) = update::reveal(&dir)
            {
                self.fail(format!("Could not open the folder: {e:#}"));
            }
            ui.weak("Close Mordent, then copy these files over your install folder.");
        } else if let Some((latest, assets)) = newer {
            // Nothing is downloaded without a published checksum for *this*
            // asset — a release that predates SHA256SUMS gets no button, with
            // the reason spelled out instead.
            let asset = kind.asset_name(std::env::consts::OS, std::env::consts::ARCH, &latest.to_string());
            let verified = asset.as_deref().is_some_and(|name| {
                self.update_sums
                    .as_ref()
                    .is_some_and(|sums| update::sum_for(sums, name).is_some())
            });
            if asset.is_none() {
                ui.weak(format!(
                    "This install layout has no release asset — download {latest} from the release page."
                ));
            } else if !verified {
                ui.weak(format!(
                    "Release {latest} ships no SHA256SUMS for {} — Mordent never installs an \
                     unverified download; grab it from the release page instead.",
                    asset.unwrap_or_default()
                ));
            } else if kind.supports_self_update() {
                if ui
                    .add_enabled(!self.update_busy, egui::Button::new(format!("Download & install {latest}")))
                    .on_hover_text(
                        "Verified against the release's SHA256SUMS (integrity only — releases are \
                         not signed), then swapped in when you next start Mordent.",
                    )
                    .clicked()
                {
                    let sums = self.update_sums.clone();
                    self.start_self_update(latest, assets, sums);
                }
                ui.weak("Downloaded in the background; the swap happens at the next start, never mid-playback.");
            } else {
                let why = match kind {
                    update::Kind::WindowsZip => {
                        "Windows cannot replace a running mordent.exe — Mordent downloads and \
                         verifies the update, and you copy it over the install folder."
                    }
                    _ => {
                        "This copy of Mordent cannot update itself (unrecognised layout) — \
                         Mordent can still download and verify the release for you."
                    }
                };
                ui.weak(why);
                if ui
                    .add_enabled(!self.update_busy, egui::Button::new(format!("Download {latest}")))
                    .clicked()
                {
                    let sums = self.update_sums.clone();
                    self.start_manual_download(latest, assets, sums);
                }
            }
        }
        if let Some(job) = self.update_job {
            ui.weak(match job {
                UpdateJob::Stage => "downloading the update…",
                UpdateJob::Download => "downloading the release…",
            });
        }
        if !self.update_log.is_empty() {
            let last = self.update_log.last().cloned().unwrap_or_default();
            ui.weak(last);
        }
        let hint = self.update_hint();
        if let Some(tag) = &hint
            && ui
                .small_button("Hide the update hint")
                .on_hover_text("Hides the player-bar hint for this release; ? still shows it.")
                .clicked()
        {
            self.update_notice_dismissed = Some(tag.clone());
            self.note_update_state_dirty();
        }
        ui.weak(format!("Release page: {}", update::RELEASE_PAGE));

        // ---- the installed helper ----
        ui.add_space(6.0);
        ui.separator();
        ui.label(egui::RichText::new("helper (wrapper-lite)").strong());
        if self.helper_remote {
            ui.weak(
                "This helper runs on another machine — update it there. Mordent cannot reach \
                 into a remote helper, and wrapper-lite reports no version (only /status regions).",
            );
            return;
        }
        match &self.helper_state {
            Some(update::HelperState::NotInstalled) => {
                ui.weak("no local helper installed (previews only)");
            }
            Some(update::HelperState::Current) => {
                ui.weak("the installed helper payload matches this build");
            }
            Some(update::HelperState::Unknown(why)) => {
                ui.weak(format!("helper payload: unknown — {why}"));
            }
            Some(update::HelperState::Outdated { installed, bundled }) => {
                ui.label(
                    egui::RichText::new("Helper update available")
                        .strong()
                        .color(theme.accent),
                );
                ui.weak(format!(
                    "installed {}…, this build ships {}…",
                    &installed[..8.min(installed.len())],
                    &bundled[..8.min(bundled.len())]
                ));
                let busy = self.helper_install.as_ref().is_some_and(|h| h.done.is_none());
                if ui
                    .add_enabled(!busy, egui::Button::new("Update helper…"))
                    .on_hover_text(
                        "Re-runs the installer through pkexec (your desktop asks for the admin \
                         password): the helper is stopped, its payload replaced and the service \
                         restarted. Apple login state is preserved.",
                    )
                    .clicked()
                {
                    self.start_helper_install();
                }
            }
            None => {
                ui.weak("checking the installed helper…");
            }
        }
        if let Some(h) = &self.helper_install {
            for line in h.log.iter().rev().take(4).rev() {
                ui.weak(egui::RichText::new(line).size(11.0).monospace());
            }
            match &h.done {
                Some(Ok(())) => {
                    ui.label(
                        egui::RichText::new("Helper updated.").color(theme.lossless),
                    );
                }
                Some(Err(e)) => {
                    ui.label(
                        egui::RichText::new(format!("Helper update failed: {e}"))
                            .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
                    );
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("installing (the password prompt may be behind this window)");
                    });
                }
            }
        }
    }

    /// Render the first-run wizard and act on its outcome. Called INSTEAD of
    /// the normal UI while `self.setup` is `Some` (see `update`).
    fn setup_screen(&mut self, ctx: &egui::Context) {
        if self.last_title != "Mordent — setup" {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title("Mordent — setup".into()));
            self.last_title = "Mordent — setup".into();
        }
        let theme = self.theme;
        let mut action = crate::setup::Action::None;
        if let Some(setup) = self.setup.as_mut() {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = setup.ui(ui, &theme);
            });
        }
        match action {
            crate::setup::Action::None => {}
            crate::setup::Action::Finish => {
                let Some(new_cfg) = self.setup.as_ref().map(|s| s.new_config(&self.cfg)) else {
                    return;
                };
                match crate::config::write_config(&new_cfg) {
                    Ok(()) => {
                        eprintln!(
                            "[setup] wrote {} (wrapper {}, storefront {})",
                            crate::config::config_file().display(),
                            new_cfg.wrapper_url,
                            new_cfg.storefront
                        );
                        self.cfg = new_cfg;
                        // The wizard's Preferences step may have checked for
                        // updates (and toggled `check_updates`); adopt what it
                        // learned so the app does not repeat the request.
                        let adoption = self.setup.as_ref().map(|s| s.update_adoption());
                        self.setup = None;
                        if let Some(a) = adoption {
                            if a.last_check.is_some() {
                                self.last_update_check = a.last_check;
                            }
                            if a.seen_tag.is_some() {
                                self.last_seen_tag = a.seen_tag;
                            }
                            if let Some(check) = a.check {
                                self.update_check = Some(check);
                            }
                            self.note_update_state_dirty();
                        }
                        self.helper_remote = !is_loopback_url(&self.cfg.wrapper_url);
                        self.check_updates_now(false);
                        self.start_helper_check();
                    }
                    Err(e) => {
                        if let Some(s) = self.setup.as_mut() {
                            s.set_error(format!("could not write the config: {e:#}"));
                        }
                    }
                }
            }
        }
    }

    // ---- lyrics (wrapper endpoint; own per-track in-memory cache) ----
    /// Show/hide the lyrics panel. Hiding drops the follow anchor so reopening
    /// mid-track scrolls back to the active line; showing after a failed fetch
    /// is the user's retry (the wrapper may be back) — the attempt is
    /// forgotten so the next frame refetches this track's lyrics.
    fn toggle_lyrics(&mut self) {
        self.show_lyrics = !self.show_lyrics;
        if self.show_lyrics {
            if matches!(self.lyrics, LyricsState::Unavailable(_)) {
                self.lyrics_song = None;
            }
        } else {
            self.lyrics_follow = None;
        }
    }

    /// Make sure `lyrics`/`lyrics_song` describe the CURRENT track: clears them
    /// when playback stopped and spawns the background fetch when the track
    /// changed (never on the UI thread — the `/lyrics` call does real HTTP).
    /// Cheap per frame: the id comparison short-circuits the common case.
    fn ensure_lyrics(&mut self, pb: &PbSnapshot) {
        let song = match &pb.track {
            Some(t) => t.id.clone(),
            None => {
                if self.lyrics_song.take().is_some() {
                    self.lyrics = LyricsState::Idle;
                    self.lyrics_follow = None;
                }
                return;
            }
        };
        if self.lyrics_song.as_deref() == Some(song.as_str()) {
            return;
        }
        self.lyrics_song = Some(song.clone());
        self.lyrics = LyricsState::Loading;
        self.lyrics_follow = None;
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("mordent-lyrics".into())
            .spawn(move || {
                let result = lyrics::fetch(&cfg, &song, true).map_err(|e| format!("{e:#}"));
                // one `[lyrics]` line per track: the headless evidence that the
                // fetch ran, how many lines came back and whether words timed
                match &result {
                    Ok(lyrics::Fetched::Timeline(tl)) => eprintln!(
                        "[lyrics] {song}: {} lines, word timing {}",
                        tl.lines.len(),
                        if tl.lines.iter().any(|l| !l.words.is_empty()) {
                            "yes"
                        } else {
                            "no"
                        }
                    ),
                    Ok(lyrics::Fetched::NoLyrics) => eprintln!("[lyrics] {song}: none"),
                    Err(e) => eprintln!("[lyrics] {song}: unavailable ({e})"),
                }
                let _ = tx.send(Msg::Lyrics { song, result });
            })
            .ok();
    }

    // ---- supervisor plumbing ----

    /// Read-only clone of the ONE playback publication (UI/MPRIS/tray).
    fn pb(&self) -> PbSnapshot {
        self.pb.lock().clone()
    }

    fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }

    /// Best-effort exit: the supervisor closes the open report segment and
    /// flushes the reporter (bounded by its request timeout), then exits the
    /// process — the single exit authority every path (ctrl+Q, MPRIS Quit,
    /// tray Quit, this) shares.
    fn exit_flush(&self) {
        self.send(Command::Quit);
    }

    /// Play a track immediately: the queue pointer is positioned here (under
    /// the shared lock), the up-next prefetch re-targeted, and the whole
    /// audio/report side happens in the supervisor. `close` is the END reason
    /// for the outgoing segment (see [`Command::Play`]).
    fn play_now_reasoned(&mut self, track: Track, close: u8) {
        self.remember_tracks(std::slice::from_ref(&track));
        let t = {
            let mut q = self.queue.lock();
            q.insert_after_current(vec![track.clone()]);
            self.prefetch.scan(&q);
            track
        };
        self.send(Command::Play { track: t, close });
    }

    /// Click-replacing the current track closes its segment (reason 5); the
    /// outgoing current stays listed in the queue (pointer moves on).
    fn play_now(&mut self, track: Track) {
        self.play_now_reasoned(track, PlayEvent::END_REPLACED);
    }

    // ---- favorites ----

    /// Keep the metadata of already-favorited tracks so the favorites view can
    /// render names (only ids are persisted — see `fav_tracks`).
    fn remember_tracks(&mut self, tracks: &[Track]) {
        if self.favorites.is_empty() {
            return;
        }
        for t in tracks {
            if self.favorites.contains(&t.id) {
                self.fav_tracks.insert(t.id.clone(), t.clone());
            }
        }
    }

    /// Toggle a favorite instantly (optimistic); the state file is written on
    /// a ≤1 s debounce by `update`.
    fn toggle_favorite(&mut self, track: &Track) {
        if self.favorites.remove(&track.id) {
            self.fav_tracks.remove(&track.id);
        } else {
            self.favorites.insert(track.id.clone());
            self.fav_tracks.insert(track.id.clone(), track.clone());
        }
        self.state_dirty = Some(std::time::Instant::now());
    }

    /// Drop a favorite known only by id (no metadata to store).
    fn remove_favorite(&mut self, id: &str) {
        if self.favorites.remove(id) {
            self.fav_tracks.remove(id);
            self.state_dirty = Some(std::time::Instant::now());
        }
    }

    /// Favorites view rows: resolvable tracks (name/artist known) sorted by
    /// name, then the ids we have never seen the metadata for.
    fn favorites_entries(&self) -> (Vec<Track>, Vec<String>) {
        let mut ids: Vec<&String> = self.favorites.iter().collect();
        ids.sort();
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for id in ids {
            match self.fav_tracks.get(id) {
                Some(t) => known.push(t.clone()),
                None => unknown.push(id.clone()),
            }
        }
        known.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        (known, unknown)
    }

    /// `+` (single row, album `+`, Queue All): always appends at the end, in
    /// order. Auto-starts the first newly-added track only when the player is
    /// empty — never started (pointer None) or finished on the last entry —
    /// otherwise the append leaves playback untouched.
    fn enqueue_tracks(&mut self, new: Vec<Track>) {
        if new.is_empty() {
            return;
        }
        self.remember_tracks(&new);
        let idle_finished = !self.pb().has_track;
        let start = {
            let mut q = self.queue.lock();
            let start = q.append(new, idle_finished);
            self.prefetch.scan(&q);
            start.map(|idx| q.tracks[idx].clone())
        };
        // append-only path: nothing to play; the up-next set grew (scanned)
        if let Some(t) = start {
            self.send(Command::Play { track: t, close: 0 });
        }
    }

    /// Album modal "Play": the whole album is inserted right after the
    /// current entry and the pointer jumps to its first track; the rest of
    /// the album is up-next.
    fn play_album_tracks(&mut self, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        self.remember_tracks(&tracks);
        let first = tracks[0].clone();
        {
            let mut q = self.queue.lock();
            q.insert_after_current(tracks);
            self.prefetch.scan(&q);
        }
        self.send(Command::Play {
            track: first,
            close: PlayEvent::END_REPLACED,
        });
    }

    fn spawn_album_fetch(&mut self, id: String, name: String) {
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let res = Api::new(cfg).and_then(|api| api.album_tracks_any(&id));
            match res {
                Ok(tracks) => {
                    let _ = tx.send(Msg::AlbumTracks { id, name, tracks });
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    /// Open the album detail modal (fetches tracks).
    fn open_album(&mut self, album: AlbumHit) {
        self.cheatsheet = false;
        self.album_modal = Some(AlbumModal {
            tracks: None,
            album: album.clone(),
        });
        self.album_job = Some(AlbumJob::Modal { id: album.id.clone() });
        self.spawn_album_fetch(album.id, album.name);
    }

    /// Enqueue a whole album (fetches tracks; `+` shows "adding…" meanwhile).
    fn queue_album(&mut self, album: AlbumHit) {
        if self.album_busy.is_some() {
            return;
        }
        self.album_busy = Some(album.id.clone());
        self.album_job = Some(AlbumJob::Queue { id: album.id.clone() });
        self.spawn_album_fetch(album.id, album.name);
    }

    fn close_album_modal(&mut self) {
        self.album_modal = None;
        self.album_job = None;
    }

    /// Swap queue row `i` with its neighbour (`delta` -1 = up, +1 = down);
    /// the pointer follows the row / compensates for crossings (queuemodel).
    fn queue_move(&mut self, i: usize, delta: isize) {
        let Some(j) = (i as isize).checked_add(delta) else {
            return;
        };
        let mut q = self.queue.lock();
        if j < 0 || j as usize >= q.len() {
            return;
        }
        q.move_row(i, j as usize);
        self.prefetch.scan(&q);
    }

    /// Move queue row `from` to final index `to` (drag-and-drop drop handler).
    fn queue_move_to(&mut self, from: usize, to: usize) {
        let mut q = self.queue.lock();
        q.move_row(from, to);
        self.prefetch.scan(&q);
    }

    /// Shuffle keeps the played region + current intact and reorders only the
    /// up-next region; toggling off restores the saved order (queuemodel).
    /// The queue list op runs in the supervisor (shared lock); this sends the
    /// EXPLICIT target from the optimistic mirror — the snapshot lags a tick
    /// and would hand back the pre-command value.
    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        let want = self.shuffle;
        self.send(Command::SetShuffle(want));
    }

    fn is_paused(&self) -> bool {
        let pb = self.pb();
        !pb.has_track || pb.paused
    }

    fn toggle_pause_now(&mut self) {
        self.send(Command::Toggle);
    }

    /// Skip to the next queue entry (manual ⏭ / N / MPRIS Next): the
    /// supervisor moves the pointer and plays. At the end of the list this is
    /// a deliberate no-op — manual skips never wrap, whatever the repeat mode.
    fn skip_current(&mut self) {
        self.send(Command::Next);
    }

    /// ⏮: the supervisor restarts current if >3s elapsed, else goes to the
    /// previous entry (pointer-1); on the first entry, restarts it.
    fn prev_track(&mut self) {
        self.send(Command::Prev);
    }

    /// Click-to-seek / ±5s. The supervisor restarts the stream at the offset
    /// (sink mode) or shifts its fake clock (decode-only).
    fn seek_to(&mut self, target: std::time::Duration) {
        self.send(Command::Seek(target));
    }

    fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.send(Command::SetVolume(self.volume));
    }

    /// Pause without toggling (sleep timer).
    fn pause_now(&mut self) {
        self.send(Command::Pause);
    }

    /// MPRIS Stop / queue panel "clear": the supervisor closes the segment,
    /// clears the queue (under the shared lock) and drops the current track.
    fn stop_all(&mut self) {
        self.send(Command::StopAll);
    }

    /// Publish the supervisor's snapshot to the MPRIS thread (per frame; the
    /// D-Bus side diffs and emits only on change).
    fn publish_mpris(&self, pb: &PbSnapshot) {
        let Some(m) = self.mpris.as_ref() else {
            return;
        };
        let (title, artist, album, art_url, track_id, length) = match &pb.track {
            Some(t) => (
                t.name.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.artwork_url(616, 616),
                t.id.clone(),
                std::time::Duration::from_millis(t.duration_ms),
            ),
            None => (
                String::new(),
                String::new(),
                String::new(),
                None,
                String::new(),
                std::time::Duration::ZERO,
            ),
        };
        m.publish(mpris::Snapshot {
            status: if !pb.has_track {
                mpris::PlaybackStatus::Stopped
            } else if pb.paused {
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
            position: pb.position,
            volume: pb.volume,
            shuffle: pb.shuffle,
            loop_status: match pb.repeat {
                Repeat::Off => mpris::LoopStatus::None,
                Repeat::One => mpris::LoopStatus::Track,
                Repeat::All => mpris::LoopStatus::Playlist,
            },
            has_track: pb.has_track,
            // pure pointer bounds — repeat-all doesn't fake a next entry and
            // the >3 s restart isn't a "go previous"
            can_next: pb.can_next,
            can_prev: pb.can_prev,
            seek_epoch: pb.seek_epoch,
        });
    }

    /// Write the whole UI state (favorites included). Also cancels any pending
    /// debounced write.
    fn save_ui_state(&mut self) {
        let mut favs: Vec<String> = self.favorites.iter().cloned().collect();
        favs.sort();
        save_state(&UiState {
                volume: Some(self.volume),
                last_view: Some(
                    match self.view {
                        View::Search => "search",
                        View::Library => "library",
                        View::Favorites => "favorites",
                        View::Playlists => "playlists",
                    }
                    .to_string(),
                ),
                search_sub: Some(
                    match self.search_sub {
                        SearchSub::Songs => "songs",
                        SearchSub::Albums => "albums",
                    }
                    .to_string(),
                ),
                repeat: Some(
                    match self.repeat_seen {
                        Repeat::Off => "off",
                        Repeat::All => "all",
                        Repeat::One => "one",
                    }
                    .to_string(),
                ),
                favorites: Some(favs),
                last_update_check: self.last_update_check,
                last_seen_tag: self.last_seen_tag.clone(),
                update_notice_dismissed: self.update_notice_dismissed.clone(),
                payload_hash: self.payload_hash.clone(),
                payload_hash_key: self.payload_hash_key.clone(),
            },
        );
        self.state_dirty = None;
    }

    // ---- keyboard ----

    fn handle_keys(&mut self, ctx: &egui::Context) -> bool {
        let mut focus_search = false;
        let typing = ctx.wants_keyboard_input();
        let pb = self.pb();
        ctx.input(|i| {
            // Esc works even while a text field has focus
            if i.key_pressed(egui::Key::Escape) {
                if self.error.as_ref().is_some_and(|e| e.expanded) {
                    self.close_error_detail();
                } else if self.cheatsheet {
                    self.cheatsheet = false;
                } else if self.album_modal.is_some() {
                    self.close_album_modal();
                } else if self.playlist_modal.is_some() {
                    self.close_playlist_modal();
                } else if self.show_queue {
                    self.show_queue = false;
                }
            }
            if typing {
                return;
            }
            if i.key_pressed(egui::Key::Space) {
                self.toggle_pause_now();
            }
            if i.key_pressed(egui::Key::N) {
                self.skip_current();
            }
            if i.key_pressed(egui::Key::P) {
                self.prev_track();
            }
            if i.key_pressed(egui::Key::Q) {
                self.show_queue = !self.show_queue;
            }
            if i.key_pressed(egui::Key::L) {
                self.toggle_lyrics();
            }
            if i.key_pressed(egui::Key::Slash) && i.modifiers.shift {
                self.cheatsheet = !self.cheatsheet;
            }
            if i.key_pressed(egui::Key::Slash) && !i.modifiers.shift {
                focus_search = true;
            }
            if pb.has_track {
                let step = std::time::Duration::from_secs(5);
                if i.key_pressed(egui::Key::ArrowRight) {
                    let t = pb.position + step;
                    self.seek_to(t);
                }
                if i.key_pressed(egui::Key::ArrowLeft) {
                    self.seek_to(pb.position.saturating_sub(step));
                }
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.change_volume(0.05);
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                self.change_volume(-0.05);
            }
            if i.modifiers.command {
                if i.key_pressed(egui::Key::Num1) {
                    self.switch_view(View::Search);
                }
                if i.key_pressed(egui::Key::Num2) {
                    self.switch_view(View::Library);
                }
                if i.key_pressed(egui::Key::Num3) {
                    self.switch_view(View::Favorites);
                }
                if i.key_pressed(egui::Key::Num4) {
                    self.switch_view(View::Playlists);
                }
                if i.key_pressed(egui::Key::Q) {
                    // the supervisor flushes the report segment and exits
                    self.exit_flush();
                }
                if i.key_pressed(egui::Key::F) {
                    focus_search = true;
                }
            }
        });
        focus_search
    }

    // ---- worker messages ----

    fn poll_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Results(r) => {
                    self.remember_tracks(&r.tracks);
                    self.results = Some(*r);
                    self.searching = false;
                }
                Msg::Library(_, _, sort) if sort != self.library_sort => {
                    // a page fetched under the previously selected sort:
                    // drop it so pages from different server orders never mix
                }
                Msg::Library(albums, next, _) => {
                    self.library.get_or_insert_with(Vec::new).extend(albums);
                    self.library_next = next;
                    // stamp on the first page; "load more" keeps the
                    // original load time
                    self.library_loaded.get_or_insert_with(std::time::Instant::now);
                    self.library_loading = false;
                }
                Msg::Playlists(lists, next) => {
                    self.playlists.get_or_insert_with(Vec::new).extend(lists);
                    self.playlists_next = next;
                    self.playlists_loading = false;
                    if let Some(n) = self.demo_playlist.take()
                        && let Some(p) = self.playlists.as_ref().and_then(|v| v.get(n)).cloned()
                    {
                        self.open_playlist(p);
                    }
                }
                Msg::PlaylistTracks { id, tracks, next } => {
                    self.remember_tracks(&tracks);
                    if let Some(m) = &mut self.playlist_modal
                        && m.playlist.id == id
                    {
                        if m.loading_more {
                            m.tracks.get_or_insert_with(Vec::new).extend(tracks);
                        } else {
                            m.tracks = Some(tracks);
                        }
                        m.next = next;
                        m.loading_more = false;
                    }
                }
                Msg::AlbumTracks { id, tracks, .. } => {
                    // album-context attribution: hand the album adam id to the
                    // supervisor (library `i.` ids are not adam ids and are
                    // never reported) so STARTs stay attributed through queue
                    // auto-advance / shuffle.
                    self.send(Command::NoteAlbum {
                        album_id: id.clone(),
                        track_ids: tracks.iter().map(|t| t.id.clone()).collect(),
                    });
                    let modal_want =
                        matches!(&self.album_job, Some(AlbumJob::Modal { id: w }) if w.as_str() == id.as_str());
                    let queue_want =
                        matches!(&self.album_job, Some(AlbumJob::Queue { id: w }) if w.as_str() == id.as_str());
                    if modal_want {
                        self.album_job = None;
                        self.remember_tracks(&tracks);
                        if let Some(m) = &mut self.album_modal
                            && m.album.id == id
                        {
                            m.tracks = Some(tracks);
                        }
                    } else if queue_want {
                        self.album_job = None;
                        self.album_busy = None;
                        self.remember_tracks(&tracks);
                        self.enqueue_tracks(tracks);
                    }
                }
                Msg::Error(e) => {
                    self.fail(e);
                    self.searching = false;
                    self.album_job = None;
                    self.album_busy = None;
                    self.library_loading = false;
                    self.playlists_loading = false;
                    if let Some(m) = &mut self.playlist_modal {
                        m.loading_more = false;
                    }
                }
                // a stale-cache hint: same toast slot, non-fatal (no state
                // reset — the fetch it belongs to succeeded)
                Msg::Notice(note) => self.fail(note),
                Msg::Update(check, sums) => {
                    self.update_busy = false;
                    self.update_job = None;
                    self.last_update_check = Some(crate::config::unix_now());
                    match &*check {
                        Check::Newer { latest, .. }
                        | Check::UpToDate { latest }
                        | Check::Uncomparable { latest } => {
                            self.last_seen_tag = Some(latest.to_string());
                        }
                        // A failure is not a sighting of a release.
                        Check::Failed { .. } => {}
                    }
                    self.update_sums = sums;
                    self.update_check = Some(*check);
                    self.note_update_state_dirty();
                }
                Msg::UpdateLog(line) => {
                    self.update_log.push(line);
                    if self.update_log.len() > 20 {
                        let drop = self.update_log.len() - 20;
                        self.update_log.drain(0..drop);
                    }
                }
                Msg::Staged(Ok(pending)) => {
                    self.update_busy = false;
                    self.update_job = None;
                    eprintln!("[update] staged {} for the next launch", pending.tag);
                    self.fail(format!(
                        "Mordent {} downloaded and verified — it will be installed the next \
                         time you start Mordent.",
                        pending.tag
                    ));
                    self.update_pending = Some(*pending);
                    self.update_downloaded = None;
                }
                Msg::Staged(Err(e)) => {
                    self.update_busy = false;
                    self.update_job = None;
                    self.fail(format!("Update download failed: {e}"));
                }
                Msg::Downloaded(Ok(pair)) => {
                    self.update_busy = false;
                    self.update_job = None;
                    eprintln!("[update] {} downloaded and verified into {}", pair.0, pair.1.display());
                    self.fail(format!(
                        "Mordent {} downloaded and checksum-verified — open ? to install it.",
                        pair.0
                    ));
                    self.update_downloaded = Some(*pair);
                }
                Msg::Downloaded(Err(e)) => {
                    self.update_busy = false;
                    self.update_job = None;
                    self.fail(format!("Update download failed: {e}"));
                }
                Msg::HelperState(report, key) => {
                    self.helper_busy = false;
                    if !report.bundled.is_empty() {
                        self.payload_hash = Some(report.bundled.clone());
                        self.payload_hash_key = Some(key);
                        self.note_update_state_dirty();
                    }
                    self.helper_state = Some(report.state.clone());
                }
                Msg::Helper(ev) => {
                    let done = match ev {
                        update::HelperEvent::Line(line) => {
                            let h = self.helper_install.get_or_insert_with(|| HelperInstall {
                                log: Vec::new(),
                                done: None,
                            });
                            h.log.push(line);
                            if h.log.len() > 200 {
                                let drop = h.log.len() - 200;
                                h.log.drain(0..drop);
                            }
                            None
                        }
                        update::HelperEvent::Done(res) => Some(res),
                    };
                    if let Some(res) = done {
                        let h = self.helper_install.get_or_insert_with(|| HelperInstall {
                            log: Vec::new(),
                            done: None,
                        });
                        h.done = Some(res.clone());
                        match res {
                            Ok(()) => {
                                // Re-compare: the payload on disk just changed.
                                self.payload_hash_key = None;
                                self.helper_state = None;
                                self.fail("Helper updated to this build's payload.".into());
                                self.start_helper_check();
                            }
                            Err(e) => self.fail(format!("Helper update failed: {e}")),
                        }
                    }
                }
                Msg::Lyrics { song, result } => {
                    // drop a late arrival for a track we already left
                    if self.lyrics_song.as_deref() == Some(song.as_str()) {
                        self.lyrics = match result {
                            Ok(lyrics::Fetched::Timeline(tl)) => LyricsState::Ready(tl),
                            Ok(lyrics::Fetched::NoLyrics) => LyricsState::NoLyrics,
                            Err(e) => LyricsState::Unavailable(e),
                        };
                    }
                }
            }
        }
        // supervisor → UI (play-worker failures): same toast, no fetch-state
        // resets — the supervisor already cleared its own preparing flag.
        while let Ok(e) = self.sup_rx.try_recv() {
            self.fail(e);
        }
    }

    // ---- deterministic selftest (MORDENT_SELFTEST=1) ----

    // Selftest observation helpers (snapshot/queue reads that interleave
    // with &mut self actions inside the stage machine).
    fn st_now_is(&self, want: &str) -> bool {
        self.pb
            .lock()
            .track
            .as_ref()
            .is_some_and(|t| t.name == want)
    }
    fn st_q_len(&self) -> usize {
        self.queue.lock().len()
    }
    fn st_q_pos(&self) -> Option<usize> {
        self.queue.lock().pos
    }
    fn st_get_is(&self, i: usize, id: &str) -> bool {
        self.queue.lock().tracks.get(i).is_some_and(|t| t.id == id)
    }
    fn st_front_is(&self, id: &str) -> bool {
        self.queue.lock().tracks.first().is_some_and(|t| t.id == id)
    }
    fn st_ids(&self) -> Vec<String> {
        self.queue.lock().tracks.iter().map(|t| t.id.clone()).collect()
    }

    /// Advance the stage machine one step and reset its per-step budget.
    fn st_advance(&mut self, stage: u8, step: u8) {
        if let Some(st) = &mut self.selftest {
            st.stage = stage;
            st.step = step;
            st.started = std::time::Instant::now();
        }
    }

    fn selftest_step(&mut self) {
        // stage 0 — queue-model invariants end-to-end (pointer non-
        // consumption, +/auto-start rules, reorder/drag/shuffle bookkeeping,
        // prev/next, ✕ handling). Supervisor-mediated actions land
        // asynchronously now, so each sub-step awaits its observable state
        // (≤ 15 s, normally the next frame) before firing the next action;
        // pure list ops still assert in the same frame. No audio needed;
        // decode-only safe.
        if self
            .selftest
            .as_ref()
            .is_some_and(|st| st.stage == 0 && self.results.as_ref().is_some_and(|r| r.tracks.len() >= 5))
        {
            let (step, elapsed) = {
                let st = self.selftest.as_ref().expect("checked");
                (st.step, st.started.elapsed())
            };
            if elapsed > std::time::Duration::from_secs(15) {
                eprintln!("[selftest] stage0 FAIL (step {step} timed out)");
                eprintln!("[selftest] RESULT: FAIL");
                std::process::exit(1);
            }
            let seed: Vec<Track> = self
                .results
                .as_ref()
                .expect("checked")
                .tracks
                .iter()
                .take(5)
                .cloned()
                .collect();
            let (n0, n1, n2, n4) = (
                seed[0].name.clone(),
                seed[1].name.clone(),
                seed[2].name.clone(),
                seed[4].name.clone(),
            );
            let (id0, id1) = (seed[0].id.clone(), seed[1].id.clone());

            // Each arm first checks the state the previous step's action must
            // have produced (`false` = keep waiting); only then does it fire
            // the next action.
            let done = match step {
                // (0→1) "play now" from idle: pointer lands on the track
                0 => {
                    self.play_now(seed[0].clone());
                    true
                }
                1 => {
                    let ok = self.st_now_is(&n0)
                        && self.st_q_len() == 1
                        && self.st_q_pos() == Some(0);
                    if ok {
                        // (1→2) `+` while playing: append at the end, never jump
                        self.enqueue_tracks(vec![seed[1].clone(), seed[2].clone()]);
                    }
                    ok
                }
                2 => {
                    let ok = self.st_q_len() == 3
                        && self.st_q_pos() == Some(0)
                        && self.st_get_is(1, &id1)
                        && self.st_now_is(&n0);
                    if ok {
                        // (2→3) reorder / drag / shuffle keep the pointer on
                        // the current row — pure list ops, asserted in-frame
                        self.queue_move(1, -1);
                        let ok_up = self.st_front_is(&id1) && self.st_q_pos() == Some(1);
                        self.queue_move(0, 1);
                        let ok_down = self.st_get_is(1, &id1) && self.st_q_pos() == Some(0);
                        // drag-and-drop path: the exact method the drop
                        // handler calls
                        self.queue_move_to(0, 2);
                        let ok_drag_down = self.st_front_is(&id1) && self.st_q_pos() == Some(2);
                        self.queue_move_to(2, 0);
                        let ok_drag_up = self.st_front_is(&id0) && self.st_q_pos() == Some(0);
                        // shuffle toggle must restore the original order
                        let before = self.st_ids();
                        self.toggle_shuffle();
                        self.toggle_shuffle();
                        let after = self.st_ids();
                        let ok_shuffle = after == before && self.st_q_pos() == Some(0);
                        let ok = ok_up && ok_down && ok_drag_down && ok_drag_up && ok_shuffle;
                        if !ok {
                            eprintln!(
                                "[selftest] stage0 FAIL (up={ok_up} down={ok_down} \
                                 drag_down={ok_drag_down} drag_up={ok_drag_up} \
                                 shuffle={ok_shuffle})"
                            );
                            eprintln!("[selftest] RESULT: FAIL");
                            std::process::exit(1);
                        }
                        // (3→4) manual ⏭ moves the pointer without consuming
                        self.skip_current();
                    }
                    ok
                }
                3 => {
                    let ok = self.st_q_pos() == Some(1)
                        && self.st_q_len() == 3
                        && self.st_now_is(&n1);
                    if ok {
                        // (4→5) ⏮ (track just started, <3 s) goes to the
                        // previous entry
                        self.prev_track();
                    }
                    ok
                }
                4 => {
                    let ok = self.st_q_pos() == Some(0)
                        && self.st_q_len() == 3
                        && self.st_now_is(&n0);
                    if ok {
                        // (5→6) pause for the paused-`+` check
                        self.pause_now();
                    }
                    ok
                }
                5 => {
                    let ok = self.is_paused();
                    if ok {
                        // (5→6) paused + `+`: append only, no auto-jump —
                        // asserted in-frame (pure list op)
                        self.enqueue_tracks(vec![seed[3].clone()]);
                        let okp = self.st_q_len() == 4
                            && self.st_q_pos() == Some(0)
                            && self.st_now_is(&n0);
                        if !okp {
                            eprintln!("[selftest] stage0 FAIL (paused_plus={okp})");
                            eprintln!("[selftest] RESULT: FAIL");
                            std::process::exit(1);
                        }
                        self.toggle_pause_now();
                    }
                    ok
                }
                6 => {
                    let ok = !self.is_paused() && self.st_q_len() == 4;
                    if ok {
                        // (6→7) ✕ of a later row leaves playback alone
                        self.remove_queue_row(3);
                        let okr = self.st_q_len() == 3 && self.st_q_pos() == Some(0);
                        if !okr {
                            eprintln!("[selftest] stage0 FAIL (remove_later={okr})");
                            eprintln!("[selftest] RESULT: FAIL");
                            std::process::exit(1);
                        }
                    }
                    ok
                }
                // (7→8) ✕ of the current row hands over to its successor
                7 => {
                    self.remove_queue_row(0);
                    true
                }
                8 => {
                    let ok = self.st_now_is(&n1)
                        && self.st_q_pos() == Some(0)
                        && self.st_q_len() == 2;
                    if ok {
                        // (8→9) "play now" inserts right after the current entry
                        self.play_now(seed[4].clone());
                    }
                    ok
                }
                9 => {
                    let ok = self.st_q_len() == 3
                        && self.st_q_pos() == Some(1)
                        && self.st_get_is(1, &seed[4].id)
                        && self.st_now_is(&n4);
                    if ok {
                        eprintln!("[selftest] stage0 queue-model PASS");
                        if let Some(st) = &mut self.selftest {
                            st.expect = [n4, n2, n0];
                            // probed OFF-frame at startup (blocking HTTP — the
                            // wrapper is busy with the supervisor's fetches)
                            st.wrapper_cacheable = self
                                .wrapper_cacheable
                                .load(std::sync::atomic::Ordering::Relaxed);
                        }
                        self.st_advance(1, 0);
                    }
                    ok
                }
                _ => false,
            };
            if done && self.selftest.as_ref().is_some_and(|st| st.stage == 0) {
                self.st_advance(0, step + 1);
            }
        }
        if let Some(st) = &mut self.selftest {
            let elapsed = st.started.elapsed();
            let pb = self.pb.lock().clone();
            let now_name = pb.track.as_ref().map(|t| t.name.clone()).unwrap_or_default();
            let matches = |want: &str| now_name == *want;
            match st.stage {
                // stage 1: the play_now target must eventually play (≤ 15 s);
                // on a wrapper-backed run the up-next entry must also be
                // cache-complete by then (sequential prefetch proof).
                1 => {
                    if matches(&st.expect[0]) && !pb.preparing {
                        // NOTE: one lock scope for the whole queue read — a
                        // `self.queue.lock()...and_then(|p| self.queue.lock()…)`
                        // would self-deadlock (the scrutinee guard outlives
                        // the closure).
                        let next_cached = {
                            let q = self.queue.lock();
                            match q
                                .current_index()
                                .and_then(|p| q.tracks.get(p + 1).cloned())
                            {
                                Some(t) if st.wrapper_cacheable => {
                                    crate::acache::AudioCache::new(&self.cfg)
                                        .lookup(&t.id)
                                        .is_some()
                                }
                                _ => true,
                            }
                        };
                        if next_cached {
                            eprintln!("[selftest] stage1 PASS (now={now_name:?}, queue={})", { self.queue.lock().len() });
                            st.stage = 2;
                            st.started = std::time::Instant::now();
                        } else if elapsed > std::time::Duration::from_secs(30) {
                            eprintln!(
                                "[selftest] stage1 FAIL (prefetch: up-next not cache-complete, now={now_name:?}, queue={})",
                                { self.queue.lock().len() }
                            );
                            eprintln!("[selftest] RESULT: FAIL");
                            std::process::exit(1);
                        }
                    } else if elapsed > std::time::Duration::from_secs(30) {
                        eprintln!(
                            "[selftest] stage1 FAIL (now={now_name:?}, want {:?}, queue={})",
                            st.expect[0],
                            { self.queue.lock().len() }
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stage 2: natural end auto-advances to the next entry; the
                // queue is NOT consumed (still 3 rows, pointer moved on)
                2 => {
                    if matches(&st.expect[1]) && { self.queue.lock().len() } == 3 && { self.queue.lock().pos } == Some(2) {
                        eprintln!("[selftest] stage2 PASS (now={now_name:?}, queue={})", { self.queue.lock().len() });
                        st.stage = 3;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage2 FAIL (now={now_name:?}, want {:?}, queue={} pos={:?})",
                            st.expect[1],
                            { self.queue.lock().len() },
                            { self.queue.lock().pos }
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stage 3: natural end on the LAST entry stops cleanly — the
                // pointer stays on the finished entry, history stays listed.
                // (5 = same check passed; the `+` fires outside this borrow.)
                3 | 5 => {
                    let done =
                        !pb.has_track && { self.queue.lock().pos } == Some(2) && { self.queue.lock().len() } == 3;
                    if st.stage == 3 && done {
                        st.stage = 5;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage3 FAIL (now={now_name:?}, queue={} pos={:?})",
                            { self.queue.lock().len() },
                            { self.queue.lock().pos }
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stage 4: the post-finish `+` auto-started the appended track
                4 => {
                    if matches(&st.expect[2]) && { self.queue.lock().len() } == 4 && { self.queue.lock().pos } == Some(3) {
                        eprintln!("[selftest] stage4 PASS (now={now_name:?}, queue={})", { self.queue.lock().len() });
                        // stages 6-8 verify the pause mirror (seek restarts
                        // restore the pre-seek pause state; a paused replace
                        // lands paused) and then finish the run
                        st.stage = 6;
                        st.step = 0;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage4 FAIL (now={now_name:?}, want {:?}, queue={} pos={:?})",
                            st.expect[2],
                            { self.queue.lock().len() },
                            { self.queue.lock().pos }
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stages 6-8: per-step waits happen outside this borrow
                // (below), like trigger (b); only the per-step timeout is
                // handled here.
                6..=8 if elapsed > std::time::Duration::from_secs(15) => {
                    eprintln!(
                        "[selftest] stage{} FAIL (now={now_name:?}, preparing={}, step={})",
                        st.stage,
                        pb.preparing,
                        st.step
                    );
                    eprintln!("[selftest] RESULT: FAIL");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        // trigger (b), outside the selftest borrow: a `+` after the queue ran
        // out must resume playback from the appended track
        if self.selftest.as_ref().is_some_and(|st| st.stage == 5) {
            let again = self.results.as_ref().expect("checked").tracks[0].clone();
            self.enqueue_tracks(vec![again]);
            if let Some(st) = &mut self.selftest {
                st.stage = 4;
                st.started = std::time::Instant::now();
            }
        }
        // triggers (c)-(e), outside the selftest borrow: the pause-mirror
        // stages. In decode-only mode the mirror is the fake clock; on a real
        // sink it is the sink's pause flag — the same assertions cover both.
        // Every action lands via the supervisor, so each step awaits its
        // observable state (the 6..=8 timeout arm above bounds each wait).
        match self.selftest.as_ref().map(|st| (st.stage, st.step)) {
            // (c) stage 6: pause, then a seek restart — the exact pre-seek
            // pause state must be restored; resuming must be audible.
            Some((6, 0)) if !self.pb.lock().preparing => {
                self.pause_now();
                self.st_advance(6, 1);
            }
            Some((6, 1)) if self.pb.lock().has_track && self.pb.lock().paused => {
                let epoch = self.pb.lock().seek_epoch;
                self.seek_to(std::time::Duration::ZERO);
                if let Some(st) = &mut self.selftest {
                    st.seek_epoch = epoch;
                }
                self.st_advance(6, 2);
            }
            Some((6, 2)) => {
                let pb = self.pb.lock().clone();
                let seek_landed = pb.seek_epoch > self
                    .selftest
                    .as_ref()
                    .map(|st| st.seek_epoch)
                    .unwrap_or(0);
                let ok_seek = pb.has_track && pb.paused && !pb.preparing && seek_landed;
                if ok_seek {
                    // resume must be audible again
                    self.toggle_pause_now();
                    self.st_advance(6, 3);
                }
            }
            Some((6, 3)) if !self.is_paused() => {
                eprintln!("[selftest] stage6 PASS (seek restores pause; resume plays)");
                self.st_advance(7, 0);
            }
            // (d) stage 7: replace the current track while PAUSED.
            Some((7, 0)) if !self.pb.lock().preparing => {
                self.pause_now();
                self.st_advance(7, 1);
            }
            Some((7, 1)) if self.pb.lock().paused => {
                let t = self.results.as_ref().expect("checked").tracks[2].clone();
                self.play_now(t);
                self.st_advance(8, 0);
            }
            // (e) stage 8: the replacement must land PAUSED (paused-on-arrival
            // is correct when the user had explicitly paused); resuming plays.
            Some((8, 0)) => {
                let pb = self.pb.lock().clone();
                if pb.has_track && pb.paused && !pb.preparing {
                    self.toggle_pause_now();
                    self.st_advance(8, 1);
                }
            }
            Some((8, 1)) if !self.is_paused() => {
                eprintln!("[selftest] stage8 PASS (paused replace lands paused; resume plays)");
                eprintln!("[selftest] RESULT: PASS");
                std::process::exit(0);
            }
            _ => {}
        }
    }

    /// Re-apply the theme when `~/.config/mordent/theme.toml` changes. The poll is
    /// throttled to about once a second (a stat, not a parse) so it stays cheap
    /// even though `update` runs every frame; the theme is (re)applied on the
    /// first frame and whenever the file's identity changes.
    fn poll_theme(&mut self, ctx: &egui::Context) {
        // First call happens on the first frame: apply even if the file is
        // absent, so the built-in defaults are the ones actually in use.
        let first_frame = self.theme_poll.is_none();
        if let Some(t) = self.theme_poll
            && t.elapsed() < std::time::Duration::from_millis(1000)
        {
            return;
        }
        self.theme_poll = Some(std::time::Instant::now());
        let path = crate::theme::theme_path();
        let stamp = Theme::stamp(&path);
        if !first_frame && stamp == self.theme_stamp {
            return;
        }
        self.theme_stamp = stamp;
        let (theme, warns) = Theme::load();
        for w in &warns {
            eprintln!("[theme] {w}");
        }
        if let Some(w) = warns.first() {
            // warn once per edit via the existing toast
            self.error = Some(ErrorToast::new(w.clone()));
        }
        self.theme = theme;
        self.theme.apply(ctx);
        // Fonts are hot-reloadable the same way: egui picks up new
        // definitions at the start of the next pass, so editing the `font`
        // key switches the typeface without a restart. Only resubmit when the
        // choice actually changed (set_fonts compares the TTF data).
        if self.font != self.theme.font {
            crate::fonts::apply(ctx, self.theme.font);
            self.font = self.theme.font;
        }
        ctx.request_repaint();
    }
}

/// What the first-run wizard's Preferences step learned about updates, adopted
/// by the app when the wizard finishes (see `setup::SetupState`).
#[derive(Debug, Clone, Default)]
pub struct UpdateAdoption {
    pub last_check: Option<u64>,
    pub seen_tag: Option<String>,
    pub check: Option<Check>,
}

/// What a window-close request (✕ / WM close, e.g. niri's Win+Q) must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    /// Real exit: flush the report segment and terminate (the supervisor's
    /// `Command::Quit` path — the same single exit authority as Ctrl+Q).
    Quit,
    /// Close to tray: veto the close and hide the window.
    Hide,
}

/// The user's policy: a session that has not played anything yet exits on
/// close; once a track has played, close goes to the tray — but only where the
/// platform can actually hide the window (X11). On Wayland hiding is
/// impossible (see `wm`), and silently doing nothing is the exact bug this
/// replaces, so the played branch quits there too (cleanly, like the idle
/// branch). Pure so it is unit tested.
fn close_action(ever_played: bool, hide_supported: bool) -> CloseAction {
    if ever_played && hide_supported {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

#[cfg(test)]
mod close_policy_tests {
    use super::*;

    #[test]
    fn idle_session_quits_on_close() {
        // Nothing played yet → real exit, whatever the platform.
        assert_eq!(close_action(false, true), CloseAction::Quit);
        assert_eq!(close_action(false, false), CloseAction::Quit);
    }

    #[test]
    fn played_session_hides_where_supported() {
        assert_eq!(close_action(true, true), CloseAction::Hide);
    }

    #[test]
    fn played_session_quits_on_wayland() {
        // Hiding is impossible on Wayland; doing nothing is the bug we are
        // fixing, so the played branch quits cleanly there too.
        assert_eq!(close_action(true, false), CloseAction::Quit);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_theme(ctx);
        // Refresh the render mirror from the ONE playback publication.
        let pb = self.pb();
        self.now = pb.track.clone();

        // Window close (✕ / WM close — niri's Win+Q arrives here too).
        // Policy (see [`close_action`]): no track played yet → quit for real;
        // a track HAS played → close to tray where the platform can hide the
        // window (X11), else quit as well (Wayland cannot unmap — `wm`).
        //
        // CancelClose is sent on BOTH branches in THIS pass: eframe vetoes the
        // close (see its `close_requested` handling), which keeps the event
        // loop alive so the supervisor's flush-then-`process::exit` stays the
        // single exit authority (letting eframe close would race `main`
        // returning against the report flush).
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            match close_action(pb.ever_played, crate::wm::hide_supported()) {
                CloseAction::Quit => {
                    if pb.ever_played {
                        eprintln!(
                            "[wm] close requested: a track has played but Wayland \
                             cannot hide the window → quitting (same flush path as Ctrl+Q)"
                        );
                    } else {
                        eprintln!("[wm] close requested: nothing played yet → quitting");
                    }
                    self.exit_flush();
                }
                CloseAction::Hide => {
                    eprintln!("[wm] close requested: closing to tray");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    self.hidden.store(true, Ordering::Relaxed);
                }
            }
        }

        // First-run wizard owns the window: render it and skip the normal app
        // (the wizard's own threads keep working; frames are kept flowing).
        if self.setup.is_some() {
            self.poll_messages();
            self.setup_screen(ctx);
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
            return;
        }

        let mut focus_search = self.handle_keys(ctx);
        self.poll_messages();
        self.selftest_step();
        // ---- debounced state.toml write (favorite toggles) ----
        if let Some(t) = self.state_dirty
            && t.elapsed() >= std::time::Duration::from_millis(1000)
        {
            self.save_ui_state();
        }

        // ---- sleep timer ----
        if let Some((t0, d)) = self.sleep_at
            && t0.elapsed() >= d
        {
            self.sleep_at = None;
            self.pause_now();
            self.fail("sleep timer expired — playback paused".into());
        }

        // ---- adopt external volume/repeat changes (MPRIS / tray) ----
        if (pb.volume - self.vol_seen).abs() > f32::EPSILON {
            self.vol_seen = pb.volume;
            if (pb.volume - self.volume).abs() > f32::EPSILON {
                self.volume = pb.volume;
                self.save_ui_state();
            }
        }
        if pb.repeat != self.repeat_seen {
            self.repeat_seen = pb.repeat;
            self.save_ui_state();
        }
        if pb.shuffle != self.shuffle_seen {
            self.shuffle_seen = pb.shuffle;
            self.shuffle = pb.shuffle;
        }
        if pb.decode_only && !self.decode_logged {
            self.decode_logged = true;
            eprintln!("[ui] decode-only mode — the supervisor's fake clock drives playback");
        }

        // ---- window title ----
        let title = match &self.now {
            Some(t) => format!("Mordent — {} — {}", t.name, t.artist),
            None => "Mordent".to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        // ---- top: tabs + search ----
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Mordent").strong());
                ui.add_space(4.0);
                if ui
                    .selectable_label(self.view == View::Search, "Search")
                    .clicked()
                {
                    self.switch_view(View::Search);
                }
                if ui
                    .selectable_label(self.view == View::Library, "Library")
                    .clicked()
                {
                    self.switch_view(View::Library);
                }
                if ui
                    .selectable_label(self.view == View::Favorites, "Favorites")
                    .clicked()
                {
                    self.switch_view(View::Favorites);
                }
                if ui
                    .selectable_label(self.view == View::Playlists, "Playlists")
                    .clicked()
                {
                    self.switch_view(View::Playlists);
                }
                ui.separator();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search  ( / )")
                        .desired_width(340.0)
                        .font(egui::TextStyle::Body),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.view = View::Search;
                    self.spawn_search();
                    resp.request_focus();
                }
                if focus_search {
                    resp.request_focus();
                }
                if ui.small_button("go").clicked() {
                    self.view = View::Search;
                    self.spawn_search();
                }
                if ui.small_button("/").clicked() {
                    focus_search = true;
                }
                if ui.small_button("?").clicked() {
                    self.cheatsheet = !self.cheatsheet;
                }
                let sl = sleep_label(&self.sleep_at);
                if hand(ui.small_button(egui::RichText::new(sl).size(11.0))).clicked() {
                    self.sleep_at = match &self.sleep_at {
                        None => Some((
                            std::time::Instant::now(),
                            std::time::Duration::from_secs(15 * 60),
                        )),
                        Some((_, d)) if *d <= std::time::Duration::from_secs(15 * 60) => {
                            Some((std::time::Instant::now(), std::time::Duration::from_secs(30 * 60)))
                        }
                        Some((_, d)) if *d <= std::time::Duration::from_secs(30 * 60) => {
                            Some((std::time::Instant::now(), std::time::Duration::from_secs(60 * 60)))
                        }
                        Some(_) => None,
                    };
                }
                if self.searching {
                    ui.spinner();
                }
            });
            ui.add_space(2.0);
        });

        // ---- bottom: player bar ----
        self.player_bar(ctx);

        // ---- right: queue panel ----
        if self.show_queue {
            self.queue_panel(ctx);
        }

        // ---- left: lyrics panel (own side, so it never stacks with the
        // right-hand queue). Fetching is (re)triggered on track change.
        if self.show_lyrics {
            self.ensure_lyrics(&pb);
            self.lyrics_panel(ctx, &pb);
        }

        // ---- central: rows ----
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut act: Option<CentralAction> = None;
            match self.view {
                View::Search => self.search_central(ui, &mut act),
                View::Library => self.library_central(ui, &mut act),
                View::Favorites => self.favorites_central(ui, &mut act),
                View::Playlists => self.playlists_central(ui, &mut act),
            }
            match act {
                Some(CentralAction::Play(t)) => self.play_now(t),
                Some(CentralAction::Enqueue(t)) => self.enqueue_tracks(vec![t]),
                Some(CentralAction::OpenAlbum(a)) => self.open_album(a),
                Some(CentralAction::QueueAlbum(a)) => self.queue_album(a),
                Some(CentralAction::OpenPlaylist(p)) => self.open_playlist(p),
                Some(CentralAction::ToggleFav(t)) => self.toggle_favorite(&t),
                Some(CentralAction::Unfav(id)) => self.remove_favorite(&id),
                None => {}
            }
        });

        // ---- overlays ----
        if self.album_modal.is_some() {
            self.album_modal_ui(ctx);
        }
        if self.playlist_modal.is_some() {
            self.playlist_modal_ui(ctx);
        }
        if self.cheatsheet {
            self.cheatsheet_ui(ctx);
        }
        self.toast_ui(ctx);

        self.publish_mpris(&pb);

        // Single repaint driver: one bounded wakeup covers every state where
        // `update` may still have work (progress ticking, toasts), so the
        // render keeps up while the window is unfocused; truly idle states
        // park. The ADVANCE no longer depends on any of this — the supervisor
        // runs off-frame and wakes us when something changes. See
        // `next_repaint_delay`.
        let now = std::time::Instant::now();
        let playing = pb.has_track && !pb.preparing && !pb.paused;
        let work_pending =
            pb.has_track || pb.preparing || !self.queue.lock().is_empty() || self.state_dirty.is_some();
        let toast = self.error.as_ref().and_then(|e| e.next_repaint(now));
        let sleep = self.sleep_at.map(|(t0, d)| d.saturating_sub(t0.elapsed()));
        let next_delay = next_repaint_delay(playing, work_pending, toast, sleep);
        if let Some(delay) = next_delay {
            ctx.request_repaint_after(delay);
        }
        // Mirror the delay to the off-frame wake thread ("mordent-wake"): the
        // in-frame request above only fires while frames are flowing, so a
        // compositor that stops redrawing an unfocused window (Wayland /
        // niri-family frame-callback throttle) would silence it permanently.
        // The thread re-issues the wake off-frame; when parked it sleeps ~1s
        // and requests nothing. Under MORDENT_NO_REPAINT the thread does not
        // exist and this request is skipped — the frame loop genuinely parks.
        if !self.no_repaint {
            self.wake_ms
                .store(wake_publish_ms(next_delay), Ordering::Relaxed);
        }
    }

    /// Transparent-viewport clear color: the theme background at the theme's
    /// opacity. See `theme.rs` for the compositor caveat.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.clear_color()
    }

    /// Best-effort play-report flush on window close: END(4) at the current
    /// position, then the reporter thread drains and exits (bounded by its
    /// 8 s request timeout).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.exit_flush();
    }
}

// ---- central views (all gather clicks into `act`; applied by caller) ----

impl App {
    fn search_central(&mut self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        // segmented sub-bar: Songs = the track results grid, Albums = the
        // album results for the current/last search (no re-query on switch).
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if ui
                .selectable_label(self.search_sub == SearchSub::Songs, "Songs")
                .clicked()
            {
                self.set_search_sub(SearchSub::Songs);
            }
            if ui
                .selectable_label(self.search_sub == SearchSub::Albums, "Albums")
                .clicked()
            {
                self.set_search_sub(SearchSub::Albums);
            }
        });
        ui.add_space(2.0);
        match self.search_sub {
            SearchSub::Songs => match self.results.as_ref() {
                None => self.empty_state(
                    ui,
                    if self.searching {
                        "searching…".to_string()
                    } else {
                        "type to search · enter plays · + queues · Q queue · ? shortcuts"
                            .to_string()
                    },
                    self.searching,
                ),
                Some(r) if r.tracks.is_empty() => {
                    if self.searching {
                        self.empty_state(ui, "searching…".to_string(), true);
                    } else {
                        self.empty_state(
                            ui,
                            format!("no results for \u{201c}{}\u{201d}", self.last_term),
                            false,
                        );
                    }
                }
                Some(r) => self.track_grid(ui, &r.tracks, act),
            },
            SearchSub::Albums => {
                let albums: &[AlbumHit] = match self.results.as_ref() {
                    Some(r) => &r.albums,
                    None => &[],
                };
                if albums.is_empty() {
                    let msg = if self.searching {
                        "searching…".to_string()
                    } else if self.results.is_some() {
                        "no albums in the last search — try another term".to_string()
                    } else {
                        "albums from your last search appear here — run a search first"
                            .to_string()
                    };
                    self.empty_state(ui, msg, self.searching);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    self.album_rows_grid(ui, "search_albums", albums, act);
                });
            }
        }
    }

    /// Library albums with a sort toggle (add date newest first, or name).
    fn library_central(&mut self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        let loading = self.library_loading;
        // sort control; switching refetches page 0 under the new server sort
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.weak("sort");
            if ui
                .selectable_label(self.library_sort == LibrarySort::Added, "add date")
                .clicked()
                && self.library_sort != LibrarySort::Added
            {
                self.library_sort = LibrarySort::Added;
                self.restart_library();
            }
            if ui
                .selectable_label(self.library_sort == LibrarySort::Name, "name")
                .clicked()
                && self.library_sort != LibrarySort::Name
            {
                self.library_sort = LibrarySort::Name;
                self.restart_library();
            }
        });
        ui.add_space(2.0);
        // sort in place; idempotent across frames
        if let Some(v) = self.library.as_mut() {
            match self.library_sort {
                LibrarySort::Name => v.sort_by(|a, b| cmp_ignore_case(&a.name, &b.name)),
                LibrarySort::Added => v.sort_by(|a, b| match (&a.date_added, &b.date_added) {
                    // ISO-8601 strings compare chronologically; newest first,
                    // undated rows last (stable sort keeps ties in API order)
                    (Some(x), Some(y)) => y.cmp(x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }),
            }
        }
        match self.library.as_ref() {
            None => self.empty_state(ui, "loading library…".to_string(), loading),
            Some(v) if v.is_empty() => {
                if loading {
                    self.empty_state(ui, "loading library…".to_string(), true);
                } else {
                    self.empty_state(
                        ui,
                        "library is empty (or not signed in)".to_string(),
                        false,
                    );
                }
            }
            Some(v) => {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    self.album_rows_grid(ui, "library_albums", v, act);
                });
                if self.library_next.is_some() && !loading {
                    ui.add_space(4.0);
                    if ui.small_button("load more").clicked()
                        && let Some(next) = self.library_next
                    {
                        self.spawn_library(next);
                    }
                }
            }
        }
    }

    /// Album rows shared by the Search/Albums sub-view and the Library view;
    /// callers own the surrounding ScrollArea and any header controls. Click
    /// opens the album detail modal, `+` queues the whole album.
    fn album_rows_grid(
        &self,
        ui: &mut egui::Ui,
        id: &str,
        albums: &[AlbumHit],
        act: &mut Option<CentralAction>,
    ) {
        egui::Grid::new(id)
            .num_columns(5)
            .spacing([10.0, 2.0])
            .min_col_width(0.0)
            .show(ui, |ui| {
                for a in albums {
                    let name = egui::RichText::new(&a.name).size(13.0).strong();
                    if cell(ui, 340.0, 20.0, name, true).clicked() {
                        *act = Some(CentralAction::OpenAlbum(a.clone()));
                    }
                    cell(
                        ui,
                        220.0,
                        20.0,
                        egui::RichText::new(&a.artist).weak().size(12.0),
                        false,
                    );
                    ui.weak(format!("{} tracks", a.track_count));
                    if !a.release_date.is_empty() {
                        ui.weak(&a.release_date);
                    } else {
                        ui.weak("");
                    }
                    if self.album_busy.as_deref() == Some(a.id.as_str()) {
                        ui.weak("adding…");
                    } else if hand(ui.small_button("+")).clicked() {
                        *act = Some(CentralAction::QueueAlbum(a.clone()));
                    }
                    ui.end_row();
                }
            });
    }

    fn track_grid(&self, ui: &mut egui::Ui, rows: &[Track], act: &mut Option<CentralAction>) {
        ui.add_space(2.0);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            self.track_rows_grid(ui, rows, act);
        });
    }

    /// The track rows themselves; callers own the surrounding ScrollArea so
    /// extra sections can share one scroll region.
    fn track_rows_grid(&self, ui: &mut egui::Ui, rows: &[Track], act: &mut Option<CentralAction>) {
        egui::Grid::new("rows")
            .num_columns(7)
            .spacing([10.0, 2.0])
            .min_col_width(0.0)
            .show(ui, |ui| {
                for (i, t) in rows.iter().enumerate() {
                    let playing = self.now.as_ref().is_some_and(|n| n.id == t.id);
                    let mark = if playing {
                        "▶".to_string()
                    } else {
                        format!("{:>2}", i + 1)
                    };
                    ui.monospace(mark);
                    let name = egui::RichText::new(&t.name).size(13.0).strong();
                    if cell(ui, 280.0, 18.0, name, true).clicked() {
                        *act = Some(CentralAction::Play(t.clone()));
                    }
                    cell(
                        ui,
                        250.0,
                        18.0,
                        egui::RichText::new(&t.artist).weak().size(12.0),
                        false,
                    );
                    cell(
                        ui,
                        230.0,
                        18.0,
                        egui::RichText::new(if t.album.is_empty() {
                            ""
                        } else {
                            &t.album
                        })
                        .weak()
                        .size(11.0),
                        false,
                    );
                    if hand(ui.small_button("+")).clicked() {
                        *act = Some(CentralAction::Enqueue(t.clone()));
                    }
                    if fav_button(ui, self.favorites.contains(&t.id), self.theme.accent).clicked() {
                        *act = Some(CentralAction::ToggleFav(t.clone()));
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.monospace(fmt_ms(t.duration_ms));
                            if t.lossless {
                                ui.colored_label(
                                    self.theme.lossless,
                                    egui::RichText::new("lossless").size(10.0),
                                );
                            }
                        },
                    );
                    ui.end_row();
                }
            });
    }

    /// Favorited tracks: rows we can name (seen this session) first, then the
    /// ids we only know from state.toml (no metadata — see `fav_tracks`).
    fn favorites_central(&self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        if self.favorites.is_empty() {
            self.empty_state(
                ui,
                "no favorites yet — click ♡ on any track row".to_string(),
                false,
            );
            return;
        }
        let (known, unknown) = self.favorites_entries();
        ui.add_space(2.0);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if !known.is_empty() {
                self.track_rows_grid(ui, &known, act);
            }
            if !unknown.is_empty() {
                ui.add_space(6.0);
                ui.separator();
                ui.weak(format!(
                    "{} favorite(s) not seen this session — play or search them once to resolve a name",
                    unknown.len()
                ));
                ui.add_space(2.0);
                egui::Grid::new("fav_unknown")
                    .num_columns(3)
                    .spacing([10.0, 2.0])
                    .min_col_width(0.0)
                    .show(ui, |ui| {
                        for id in &unknown {
                            ui.monospace("?");
                            ui.weak(egui::RichText::new(id).size(12.0).monospace());
                            if fav_button(ui, true, self.theme.accent).clicked() {
                                *act = Some(CentralAction::Unfav(id.clone()));
                            }
                            ui.end_row();
                        }
                    });
            }
        });
    }

    /// User library playlists (text rows; a small swatch from the artwork's
    /// background colour stands in for the cover — no image loading).
    fn playlists_central(&mut self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        let loading = self.playlists_loading;
        match self.playlists.as_ref() {
            None => self.empty_state(ui, "loading playlists…".to_string(), loading),
            Some(v) if v.is_empty() => {
                if loading {
                    self.empty_state(ui, "loading playlists…".to_string(), true);
                } else {
                    self.empty_state(
                        ui,
                        "no library playlists (or no media-user-token set)".to_string(),
                        false,
                    );
                }
            }
            Some(v) => {
                ui.add_space(2.0);
                let weak = ui.visuals().weak_text_color();
                let mut load_more = false;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    egui::Grid::new("playlists")
                        .num_columns(5)
                        .spacing([10.0, 2.0])
                        .min_col_width(0.0)
                        .show(ui, |ui| {
                            for p in v {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(14.0, 14.0),
                                    egui::Sense::hover(),
                                );
                                let col = p
                                    .artwork
                                    .as_ref()
                                    .and_then(|a| a.bg_color.as_deref())
                                    .and_then(hex_color)
                                    .unwrap_or(weak);
                                ui.painter().rect_filled(rect, 2.0, col);
                                let name = egui::RichText::new(&p.name).size(13.0).strong();
                                if cell(ui, 300.0, 20.0, name, true).clicked() {
                                    *act = Some(CentralAction::OpenPlaylist(p.clone()));
                                }
                                ui.weak(format!("{} tracks", p.track_count));
                                cell(
                                    ui,
                                    380.0,
                                    20.0,
                                    egui::RichText::new(&p.description).weak().size(11.0),
                                    false,
                                );
                                ui.monospace("›");
                                ui.end_row();
                            }
                        });
                    // keep the pager inside the scroll region so it stays
                    // reachable when the list fills the panel
                    if self.playlists_next.is_some() && !loading {
                        ui.add_space(4.0);
                        if hand(ui.small_button("load more")).clicked() {
                            load_more = true;
                        }
                    }
                });
                if load_more
                    && let Some(next) = self.playlists_next
                {
                    self.spawn_playlists(next);
                }
            }
        }
    }

    fn empty_state(&self, ui: &mut egui::Ui, msg: String, busy: bool) {
        ui.add_space(24.0);
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                if busy {
                    ui.spinner();
                }
                ui.add_space(6.0);
                ui.weak(msg);
            });
        });
    }
}

// ---- panels & overlays ----

/// Click intents gathered while drawing the player bar; applied after
/// rendering (the bar borrows `self.now` while drawing).
enum BarIntent {
    Toggle,
    Prev,
    Next,
    Queue,
    Lyrics,
    CycleRepeat,
    ToggleShuffle,
    Seek(std::time::Duration),
}

impl App {
    fn player_bar(&mut self, ctx: &egui::Context) {
        let pb = self.pb();
        let mut act: Option<BarIntent> = None;
        let mut vol = self.volume;
        egui::TopBottomPanel::bottom("player").show(ctx, |ui| {
            let paused = !pb.has_track || pb.paused;
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                if hand(ui.button(if paused { "▶" } else { "⏸" })).clicked() {
                    act = Some(BarIntent::Toggle);
                }
                if hand(ui.button("⏮")).clicked() {
                    act = Some(BarIntent::Prev);
                }
                if hand(ui.button("⏭")).clicked() {
                    act = Some(BarIntent::Next);
                }
                ui.add_space(6.0);
                if let Some(t) = &pb.track {
                    ui.vertical(|ui| {
                        ui.add_sized(
                            [280.0, 14.0],
                            egui::Label::new(
                                egui::RichText::new(&t.name).strong().size(13.0),
                            )
                            .halign(egui::Align::LEFT)
                            .truncate(),
                        );
                        let pos = pb.position;
                        // `duration` is the decode-only simulation cap (≤ 8 s)
                        // when there is no audio device — the bar and the
                        // remaining label match the simulation
                        let remain = pb.duration.saturating_sub(pos);
                        ui.horizontal(|ui| {
                            ui.monospace(fmt_pos(pos));
                            seek_bar_ui(ui, 300.0, pos, pb.duration, self.theme.accent, &mut act);
                            ui.monospace(fmt_pos(remain));
                        });
                    });
                } else {
                    ui.weak("nothing playing");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hand(ui.small_button("queue")).clicked() {
                        act = Some(BarIntent::Queue);
                    }
                    let lyr_col = if self.show_lyrics {
                        self.theme.accent
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    if hand(ui.small_button(egui::RichText::new("lyrics").size(11.0).color(lyr_col)))
                        .clicked()
                    {
                        act = Some(BarIntent::Lyrics);
                    }
                    ui.add_sized(
                        [90.0, 16.0],
                        egui::Slider::new(&mut vol, 0.0..=1.0).show_value(false),
                    );
                    self.repeat_shuffle_buttons(ui, &pb, &mut act);
                    // Subtle, one-line "a newer Mordent exists" hint: no modal,
                    // no focus stealing, and it disappears once the user
                    // dismisses it or updates (? panel → updates).
                    if let Some(tag) = self.update_hint()
                        && hand(ui.small_button(
                            egui::RichText::new(format!("↓ {tag}"))
                                .size(11.0)
                                .color(self.theme.accent),
                        ))
                        .on_hover_text("A newer Mordent is available — details in ?")
                        .clicked()
                    {
                        self.cheatsheet = true;
                    }
                });
                ui.add_space(8.0);
            });
            ui.add_space(5.0);
        });
        if vol != self.volume {
            self.volume = vol;
            self.send(Command::SetVolume(vol));
            self.save_ui_state();
        }
        match act {
            Some(BarIntent::Toggle) => self.toggle_pause_now(),
            Some(BarIntent::Prev) => self.prev_track(),
            Some(BarIntent::Next) => self.skip_current(),
            Some(BarIntent::Queue) => self.show_queue = !self.show_queue,
            Some(BarIntent::Lyrics) => self.toggle_lyrics(),
            Some(BarIntent::CycleRepeat) => {
                let next = match pb.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                // optimistic mirror (the snapshot catches up within a tick)
                self.repeat_seen = next;
                self.send(Command::SetRepeat(next));
                self.save_ui_state();
            }
            Some(BarIntent::ToggleShuffle) => self.toggle_shuffle(),
            Some(BarIntent::Seek(target)) => self.seek_to(target),
            None => {}
        }
    }

    fn repeat_shuffle_buttons(&self, ui: &mut egui::Ui, pb: &PbSnapshot, act: &mut Option<BarIntent>) {
        let repeat_txt = match pb.repeat {
            Repeat::Off => "repeat",
            Repeat::All => "repeat all",
            Repeat::One => "repeat 1",
        };
        let col = if pb.repeat == Repeat::Off {
            ui.visuals().weak_text_color()
        } else {
            self.theme.accent
        };
        if hand(ui.small_button(egui::RichText::new(repeat_txt).size(11.0).color(col)))
            .clicked()
        {
            *act = Some(BarIntent::CycleRepeat);
        }
        let col = if pb.shuffle {
            self.theme.accent
        } else {
            ui.visuals().weak_text_color()
        };
        if hand(ui.small_button(egui::RichText::new("shuffle").size(11.0).color(col)))
            .clicked()
        {
            *act = Some(BarIntent::ToggleShuffle);
        }
    }

    /// Left-hand lyrics panel: the line-timed karaoke view, with per-word
    /// highlighting when the document carries word timing. Position comes from
    /// the same [`PbSnapshot`] the player bar uses, so a paused track freezes
    /// both the bar and the highlight with no extra state. A left panel (the
    /// queue is the right one) keeps the two from stacking.
    fn lyrics_panel(&mut self, ctx: &egui::Context, pb: &PbSnapshot) {
        // Nominal row height for the auto-scroll math. Lines are laid out at
        // (at most) this height so the offset tracks the rendered position.
        const ROW_H: f32 = 22.0;
        let pos_ms = pb.position.as_millis() as u64;
        let accent = self.theme.accent;
        let mut close = false;
        // Applied after the closure, which only reads `self`.
        let mut follow: Option<usize> = None;
        egui::SidePanel::left("lyrics")
            .exact_width(300.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Lyrics");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if hand(ui.small_button("✕")).clicked() {
                            close = true;
                        }
                        if let Some(t) = &pb.track {
                            ui.weak(egui::RichText::new(&t.name).size(11.0));
                        }
                    });
                });
                ui.separator();
                match &self.lyrics {
                    LyricsState::Idle => {
                        self.empty_state(ui, "start a track for lyrics".to_string(), false)
                    }
                    LyricsState::Loading => self.empty_state(ui, "loading lyrics…".to_string(), true),
                    LyricsState::NoLyrics => {
                        self.empty_state(ui, "no lyrics for this track".to_string(), false)
                    }
                    LyricsState::Unavailable(e) => {
                        self.empty_state(ui, "lyrics unavailable offline".to_string(), false);
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(egui::RichText::new(e).size(10.0));
                        });
                    }
                    LyricsState::Ready(tl) if tl.is_empty() => {
                        self.empty_state(ui, "no lyrics for this track".to_string(), false)
                    }
                    LyricsState::Ready(tl) => {
                        let active = tl.line_at(pos_ms);
                        let view_h = ui.available_height().max(1.0);
                        let mut sa = egui::ScrollArea::vertical().auto_shrink(false);
                        // Auto-scroll ONLY when the active line changes: the
                        // offset is re-applied per line, so a manual scroll in
                        // between is never fought back.
                        if let Some(i) = active.filter(|i| Some(*i) != self.lyrics_follow) {
                            sa = sa.vertical_scroll_offset(
                                ((i as f32) * ROW_H - view_h * 0.35).max(0.0),
                            );
                            follow = Some(i);
                        }
                        sa.show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            for (i, line) in tl.lines.iter().enumerate() {
                                let is_active = Some(i) == active;
                                // Word-timed active line: highlight the active
                                // word in place (word spans carry the words'
                                // own spacing; background vocals dim + italic).
                                if is_active && !line.words.is_empty() {
                                    let aw = tl.word_at(pos_ms, i);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        for (wi, w) in line.words.iter().enumerate() {
                                            let mut rt = egui::RichText::new(&w.text).size(14.0);
                                            if Some(wi) == aw {
                                                rt = rt.color(accent).background_color(
                                                    accent.gamma_multiply(0.22),
                                                );
                                            } else if w.bg {
                                                rt = rt.weak().italics();
                                            }
                                            ui.label(rt);
                                        }
                                    });
                                } else {
                                    let mut rt = egui::RichText::new(&line.text).size(13.0);
                                    if is_active {
                                        rt = rt.strong().color(accent);
                                        ui.label(rt);
                                    } else {
                                        // fixed height keeps the auto-scroll
                                        // offset in step with the rows
                                        rt = rt.weak();
                                        ui.add_sized(
                                            [ui.available_width(), ROW_H],
                                            egui::Label::new(rt).truncate(),
                                        );
                                    }
                                }
                            }
                        });
                    }
                }
            });
        if let Some(i) = follow {
            self.lyrics_follow = Some(i);
        }
        if close {
            self.toggle_lyrics();
        }
    }

    fn queue_panel(&mut self, ctx: &egui::Context) {
        let mut clear = false;
        let mut shuf = false;
        // Per-frame snapshot of the shared queue for rendering; mutations go
        // through &mut self helpers that lock afresh (the supervisor only
        // ever holds the lock for microsecond ops).
        let (tracks, queue_pos) = {
            let q = self.queue.lock();
            (q.tracks.clone(), q.pos)
        };
        let queue_len = tracks.len();
        egui::SidePanel::right("queue")
            .exact_width(290.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("Queue · {queue_len}"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if hand(ui.small_button("clear")).clicked() {
                            clear = true;
                        }
                        if hand(ui.small_button("shuf")).clicked() {
                            shuf = true;
                        }
                    });
                });
                ui.separator();
                if tracks.is_empty() {
                    self.empty_state(ui, "queue empty — add with +".to_string(), false);
                }
                let mut remove: Option<usize> = None;
                let mut mv: Option<(usize, isize)> = None;
                let mut fav: Option<Track> = None;
                let mut jump: Option<usize> = None;
                // drag-and-drop state collected this frame (applied after the loop)
                let mut rows: Vec<(f32, f32)> = Vec::new();
                let mut drag_from: Option<usize> = None;
                let mut drag_y: Option<f32> = None;
                let mut drag_end = false;
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    // Fixed columns so the ↑ ↓ ✕ cluster sits in the same x
                    // on every row, whatever the title length: drag | marker |
                    // heart | name (capped, truncated) | right-aligned cluster.
                    egui::Grid::new("queue_rows")
                        .num_columns(5)
                        .spacing([8.0, 2.0])
                        .min_col_width(0.0)
                        .show(ui, |ui| {
                            for (i, t) in tracks.iter().enumerate() {
                                // current entry: ▶ marker + accent; played entries dim
                                let is_cur = queue_pos == Some(i);
                                let played = queue_pos.is_some_and(|p| i < p);
                                if self.queue_drag == Some(i) {
                                    ui.visuals_mut().override_text_color =
                                        Some(ui.visuals().weak_text_color());
                                }
                                // ⠿ drag handle — its own rect with a pure drag
                                // sense, so it never conflicts with the buttons'
                                // click sense (and vice versa).
                                let (hrect, hresp) = ui.allocate_exact_size(
                                    egui::vec2(11.0, 15.0),
                                    egui::Sense::drag(),
                                );
                                let hresp = if hresp.dragged() {
                                    hresp.on_hover_cursor(egui::CursorIcon::Grabbing)
                                } else {
                                    hresp.on_hover_cursor(egui::CursorIcon::Grab)
                                };
                                let hcol = if hresp.dragged() || self.queue_drag == Some(i) {
                                    self.theme.accent
                                } else {
                                    ui.visuals().weak_text_color()
                                };
                                ui.painter().text(
                                    hrect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "⠿",
                                    egui::FontId::proportional(11.0),
                                    hcol,
                                );
                                if hresp.dragged() || hresp.drag_stopped() {
                                    drag_from = Some(i);
                                    drag_y = hresp.interact_pointer_pos().map(|p| p.y);
                                    drag_end |= hresp.drag_stopped();
                                }
                                // fixed-width marker column: the ▶ pointer or the
                                // row number, so the columns after it never shift
                                ui.allocate_ui_with_layout(
                                    egui::vec2(15.0, 16.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        if is_cur {
                                            ui.monospace(
                                                egui::RichText::new("▶")
                                                    .size(11.0)
                                                    .color(self.theme.accent),
                                            );
                                        } else {
                                            ui.monospace(
                                                egui::RichText::new(format!("{:>2}", i + 1))
                                                    .size(11.0)
                                                    .weak(),
                                            );
                                        }
                                    },
                                );
                                if fav_button(ui, self.favorites.contains(&t.id), self.theme.accent)
                                    .clicked()
                                {
                                    fav = Some(t.clone());
                                }
                                let mut name = egui::RichText::new(&t.name).size(12.0);
                                if is_cur {
                                    name = name.strong().color(self.theme.accent);
                                } else if played {
                                    name = name.weak();
                                }
                                let name_resp = cell(ui, 110.0, 16.0, name, true);
                                rows.push((name_resp.rect.top(), name_resp.rect.bottom()));
                                if name_resp.clicked() {
                                    jump = Some(i);
                                }
                                // right-anchored fixed cluster: identical x per row
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if hand(ui.small_button("×")).clicked() {
                                            remove = Some(i);
                                        }
                                        if hand(ui.small_button("↓")).clicked() {
                                            mv = Some((i, 1));
                                        }
                                        if hand(ui.small_button("↑")).clicked() {
                                            mv = Some((i, -1));
                                        }
                                    },
                                );
                                ui.end_row();
                            }
                        });
                    if let Some(i) = drag_from {
                        self.queue_drag = Some(i);
                        if let Some(y) = drag_y {
                            self.queue_drop = Some(drop_slot(&rows, Some(i), y));
                        }
                    }
                    // drop indicator: 2.5 px accent line at the landing gap
                    if let (Some(i), Some(slot)) = (self.queue_drag, self.queue_drop) {
                        let others: Vec<(f32, f32)> = rows
                            .iter()
                            .enumerate()
                            .filter(|(k, _)| *k != i)
                            .map(|(_, r)| *r)
                            .collect();
                        let line_y = match others.get(slot) {
                            Some(r) => r.0,
                            None => others.last().map_or(0.0, |r| r.1),
                        };
                        ui.painter().hline(
                            ui.max_rect().x_range(),
                            line_y,
                            egui::Stroke::new(2.5_f32, self.theme.accent),
                        );
                    }
                });
                if drag_end {
                    if let (Some(i), Some(slot)) = (drag_from, self.queue_drop) {
                        self.queue_move_to(i, slot);
                    }
                    self.queue_drag = None;
                    self.queue_drop = None;
                }
                if let Some(t) = fav {
                    self.toggle_favorite(&t);
                }
                if let Some((i, d)) = mv {
                    self.queue_move(i, d);
                }
                if let Some(i) = jump {
                    self.play_queue_row(i);
                }
                if let Some(i) = remove {
                    self.remove_queue_row(i);
                }
                if clear {
                    // the supervisor closes the segment, clears the queue and
                    // drops the current track (re-scans the prefetcher too)
                    self.stop_all();
                }
                if shuf {
                    self.toggle_shuffle();
                }
            });
    }

    /// Click a queue row: move the pointer there and play it (the outgoing
    /// current stays listed, its segment closes with END_REPLACED).
    fn play_queue_row(&mut self, i: usize) {
        let t = {
            let mut q = self.queue.lock();
            if q.play_at(i).is_none() {
                return;
            }
            let t = q.tracks[i].clone();
            self.prefetch.scan(&q);
            t
        };
        self.send(Command::Play {
            track: t,
            close: PlayEvent::END_REPLACED,
        });
    }

    /// Queue-row ✕: the pointer-aware removal decides what happens to
    /// playback — a removed current track hands over to its successor (kept
    /// playing) or, with no successor, stops cleanly.
    fn remove_queue_row(&mut self, i: usize) {
        let handover = {
            let mut q = self.queue.lock();
            let effect = q.remove(i);
            self.prefetch.scan(&q);
            match effect {
                RemoveEffect::PlayNext(j) => Some(q.tracks[j].clone()),
                RemoveEffect::Stopped => None,
                RemoveEffect::Nothing => return,
            }
        };
        match handover {
            Some(t) => self.send(Command::Play {
                track: t,
                close: PlayEvent::END_SKIPPED_FORWARDS,
            }),
            None => self.send(Command::Halt),
        }
    }

    fn album_modal_ui(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("album_modal"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                // floating Areas don't reserve height for ScrollArea —
                // reserve an explicit box for the detail page
                ui.allocate_ui_with_layout(
                    egui::vec2(560.0, 430.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| self.album_modal_body(ui),
                );
            });
        if resp.should_close() {
            self.close_album_modal();
        }
    }

    fn album_modal_body(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &mut self.album_modal else { return };
        let mut close = false;
        let mut act: Option<CentralAction> = None;
        let mut play_all: Option<Vec<Track>> = None;
        let mut queue_all: Option<Vec<Track>> = None;
        {
            let album = &m.album;
            ui.horizontal(|ui| {
                ui.strong(egui::RichText::new(&album.name).size(18.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hand(ui.small_button("×")).clicked() {
                        close = true;
                    }
                });
            });
            ui.weak(format!(
                "{} · {} tracks · {}",
                album.artist, album.track_count, album.release_date
            ));
            match m.tracks.as_deref() {
                None => {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("loading tracks…");
                    });
                }
                Some(tracks) => {
                    let total_min = tracks.iter().map(|t| t.duration_ms).sum::<u64>() / 60_000;
                    ui.weak(format!("{total_min} min"));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if hand(ui.button("Play")).clicked() {
                            play_all = Some(tracks.to_vec());
                        }
                        if hand(ui.button("Queue All")).clicked() {
                            queue_all = Some(tracks.to_vec());
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(340.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            egui::Grid::new("album_tracks")
                                .num_columns(4)
                                .spacing([10.0, 2.0])
                                .min_col_width(0.0)
                                .show(ui, |ui| {
                                    for t in tracks {
                                        ui.monospace(format!("{:>2}", t.track_no));
                                        let name = egui::RichText::new(&t.name).size(13.0);
                                        if cell(ui, 380.0, 18.0, name, true).clicked() {
                                            act = Some(CentralAction::Play(t.clone()));
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.monospace(fmt_ms(t.duration_ms));
                                                if t.lossless {
                                                    ui.colored_label(
                                                        self.theme.lossless,
                                                        egui::RichText::new("lossless").size(10.0),
                                                    );
                                                }
                                            },
                                        );
                                        if hand(ui.small_button("+")).clicked() {
                                            act = Some(CentralAction::Enqueue(t.clone()));
                                        }
                                        if fav_button(ui, self.favorites.contains(&t.id), self.theme.accent).clicked() {
                                            act = Some(CentralAction::ToggleFav(t.clone()));
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
        }
        if let Some(tracks) = play_all {
            self.play_album_tracks(tracks);
            close = true;
        }
        if let Some(tracks) = queue_all {
            self.enqueue_tracks(tracks);
        }
        match act {
            Some(CentralAction::Play(t)) => self.play_now(t),
            Some(CentralAction::Enqueue(t)) => self.enqueue_tracks(vec![t]),
            _ => {}
        }
        if close {
            self.close_album_modal();
        }
    }

    fn playlist_modal_ui(&mut self, ctx: &egui::Context) {
        let resp = egui::Modal::new(egui::Id::new("playlist_modal"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(620.0, 460.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| self.playlist_modal_body(ui),
                );
            });
        if resp.should_close() {
            self.close_playlist_modal();
        }
    }

    fn playlist_modal_body(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &mut self.playlist_modal else { return };
        let mut close = false;
        let mut act: Option<CentralAction> = None;
        let mut play_all: Option<Vec<Track>> = None;
        let mut queue_all: Option<Vec<Track>> = None;
        let mut load_more = false;
        {
            let pl = &m.playlist;
            ui.horizontal(|ui| {
                ui.strong(egui::RichText::new(&pl.name).size(18.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hand(ui.small_button("×")).clicked() {
                        close = true;
                    }
                });
            });
            let mut sub = format!("{} tracks", pl.track_count);
            if !pl.description.is_empty() {
                sub.push_str(" · ");
                sub.push_str(&pl.description);
            }
            ui.add_sized(
                [600.0, 16.0],
                egui::Label::new(egui::RichText::new(sub).weak().size(12.0)).truncate(),
            );
            match m.tracks.as_deref() {
                None => {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("loading tracks…");
                    });
                }
                Some(tracks) => {
                    let total_min = tracks.iter().map(|t| t.duration_ms).sum::<u64>() / 60_000;
                    ui.weak(format!("{total_min} min"));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if hand(ui.button("Play")).clicked() {
                            play_all = Some(tracks.to_vec());
                        }
                        if hand(ui.button("Queue All")).clicked() {
                            queue_all = Some(tracks.to_vec());
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            egui::Grid::new("playlist_tracks")
                                .num_columns(4)
                                .spacing([10.0, 2.0])
                                .min_col_width(0.0)
                                .show(ui, |ui| {
                                    for t in tracks {
                                        ui.monospace(format!("{:>2}", t.track_no));
                                        let name = egui::RichText::new(&t.name).size(13.0);
                                        if cell(ui, 400.0, 18.0, name, true).clicked() {
                                            act = Some(CentralAction::Play(t.clone()));
                                        }
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.monospace(fmt_ms(t.duration_ms));
                                                if t.lossless {
                                                    ui.colored_label(
                                                        self.theme.lossless,
                                                        egui::RichText::new("lossless").size(10.0),
                                                    );
                                                }
                                            },
                                        );
                                        if hand(ui.small_button("+")).clicked() {
                                            act = Some(CentralAction::Enqueue(t.clone()));
                                        }
                                        if fav_button(ui, self.favorites.contains(&t.id), self.theme.accent).clicked() {
                                            act = Some(CentralAction::ToggleFav(t.clone()));
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                    if m.next.is_some() || m.loading_more {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if m.loading_more {
                                ui.spinner();
                                ui.weak("loading…");
                            } else if hand(ui.small_button("load more")).clicked() {
                                load_more = true;
                            }
                        });
                    }
                }
            }
        }
        if let Some(tracks) = play_all {
            self.play_album_tracks(tracks);
            close = true;
        }
        if let Some(tracks) = queue_all {
            self.enqueue_tracks(tracks);
        }
        match act {
            Some(CentralAction::Play(t)) => self.play_now(t),
            Some(CentralAction::Enqueue(t)) => self.enqueue_tracks(vec![t]),
            Some(CentralAction::ToggleFav(t)) => self.toggle_favorite(&t),
            _ => {}
        }
        if load_more {
            let (id, next) = match &self.playlist_modal {
                Some(m) => (m.playlist.id.clone(), m.next),
                None => (String::new(), None),
            };
            if let Some(next) = next {
                self.spawn_playlist_tracks(id, next);
            }
        }
        if close {
            self.close_playlist_modal();
        }
    }

    fn cheatsheet_ui(&mut self, ctx: &egui::Context) {
        let theme = self.theme;
        let resp = egui::Modal::new(egui::Id::new("cheatsheet"))
            .backdrop_color(egui::Color32::from_black_alpha(120))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.strong(egui::RichText::new("keyboard shortcuts").size(15.0));
                ui.separator();
                egui::Grid::new("cheats")
                    .num_columns(2)
                    .spacing([18.0, 3.0])
                    .show(ui, |ui| {
                        for (k, v) in [
                            ("space", "play / pause"),
                            ("→ / ←", "seek ±5 s"),
                            ("↑ / ↓", "volume"),
                            ("N / P", "next / previous track"),
                            ("Q", "toggle queue panel"),
                            ("L", "toggle lyrics panel"),
                            ("/ or ctrl+F", "focus search"),
                            ("ctrl+1 / 2 / 3", "search / library / favorites"),
                            ("ctrl+4", "playlists"),
                            ("♥ / ♡", "toggle favorite on a row"),
                            ("esc", "close modal / panel"),
                            ("?", "this cheatsheet"),
                            ("click progress bar", "seek"),
                        ] {
                            ui.monospace(egui::RichText::new(k).color(theme.accent));
                            ui.weak(v);
                            ui.end_row();
                        }
                    });
                ui.add_space(10.0);
                ui.separator();
                ui.strong(egui::RichText::new("updates").size(15.0));
                // Bounded height: the section grows with the update state and
                // the window may be as short as 420 px.
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        self.updates_ui(ui, &theme);
                    });
            });
        if resp.should_close() {
            self.cheatsheet = false;
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let now = std::time::Instant::now();
        if self.error.as_ref().is_some_and(|e| e.is_expired(now)) {
            self.error = None;
        }
        let Some(expanded) = self.error.as_ref().map(|e| e.expanded) else {
            return;
        };
        if expanded {
            self.error_detail_ui(ctx);
            return;
        }
        // The collapsed countdown's repaint is owned by the single driver at
        // the end of `update` (see `next_repaint_delay`), which covers the
        // idle case too (e.g. an error raised from a fired search).
        let mut dismiss = false;
        let mut expand = false;
        egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -60.0])
            .show(ctx, |ui| {
                let frame = egui::Frame::default()
                    .fill(egui::Color32::from_rgb(0x3a, 0x1a, 0x1e))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xa0, 0x40, 0x50)))
                    .inner_margin(8.0)
                    .corner_radius(6.0_f32)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(e) = &self.error {
                                ui.add_sized(
                                    [420.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new(&e.msg)
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text("click to expand");
                            }
                            if hand(ui.small_button("×")).clicked() {
                                dismiss = true;
                            }
                        });
                    });
                // The whole toast (except the ✕, which wins) opens the detail view.
                if ui
                    .interact(
                        frame.response.rect,
                        egui::Id::new("toast_click"),
                        egui::Sense::click(),
                    )
                    .clicked()
                {
                    expand = true;
                }
                if frame.response.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            });
        if dismiss {
            self.error = None;
        } else if expand && let Some(t) = &mut self.error {
            t.set_expanded(true, std::time::Instant::now());
        }
    }

    /// Expanded error view: full text, selectable, with a Copy button.
    fn error_detail_ui(&mut self, ctx: &egui::Context) {
        let now = std::time::Instant::now();
        let (msg, copied) = match &self.error {
            Some(e) => (e.msg.clone(), e.copied_feedback(now)),
            None => return,
        };
        let mut close = false;
        let mut copy = false;
        let resp = egui::Modal::new(egui::Id::new("error_detail"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(560.0);
                ui.horizontal(|ui| {
                    ui.strong(egui::RichText::new("Error").size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if hand(ui.small_button("×")).clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // selectable → the text can also be dragged over and
                        // copied with Ctrl+C; the button below copies it whole.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&msg)
                                    .size(12.5)
                                    .color(self.theme.text)
                                    .monospace(),
                            )
                            .wrap()
                            .selectable(true),
                        );
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let label = if copied { "Copied ✓" } else { "Copy" };
                    if hand(ui.button(egui::RichText::new(label).color(self.theme.accent))).clicked() {
                        copy = true;
                    }
                    ui.weak("Esc closes");
                });
            });
        if resp.should_close() {
            close = true;
        }
        if copy {
            ctx.copy_text(msg);
            if let Some(e) = &mut self.error {
                e.copied_at = Some(std::time::Instant::now());
            }
            // The "Copied ✓" label's repaint is owned by the single driver at
            // the end of `update` (via `ErrorToast::next_repaint`).
        }
        if close {
            self.close_error_detail();
        }
    }

    /// Collapse the detail view, resuming auto-expiry with the remaining time.
    fn close_error_detail(&mut self) {
        if let Some(e) = &mut self.error {
            e.set_expanded(false, std::time::Instant::now());
        }
    }
}

/// Left-aligned fixed-width text cell (add_sized centres content, which made
/// columns look ragged).
fn cell(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    text: egui::RichText,
    clickable: bool,
) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(w, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let mut label = egui::Label::new(text).truncate();
            if clickable {
                label = label.sense(egui::Sense::click());
            }
            hand(ui.add(label))
        },
    ).inner
}

fn hand(r: egui::Response) -> egui::Response {
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Drop slot for a queue drag: with `rows` being the `(top, bottom)` y-bounds of
/// every rendered row in visual order and `dragged` the row being moved (skipped),
/// return the index the pointer at `pointer_y` would land on *among the other
/// rows* — i.e. the final index after the dragged row is lifted out.
///
/// A pointer above a row's midpoint inserts before it, so the result is the
/// count of non-dragged rows whose midpoint is above the pointer.
fn drop_slot(rows: &[(f32, f32)], dragged: Option<usize>, pointer_y: f32) -> usize {
    let mut slot = 0;
    for (i, (top, bottom)) in rows.iter().enumerate() {
        if dragged == Some(i) {
            continue;
        }
        if pointer_y < (top + bottom) / 2.0 {
            return slot;
        }
        slot += 1;
    }
    slot
}

/// Small ♥/♡ favorite toggle (both glyphs are covered by egui's built-in
/// emoji fonts and the bundled DejaVu Sans fallback).
fn fav_button(ui: &mut egui::Ui, fav: bool, accent: egui::Color32) -> egui::Response {
    let (txt, col) = if fav {
        ("♥", accent)
    } else {
        ("♡", ui.visuals().weak_text_color())
    };
    hand(ui.small_button(egui::RichText::new(txt).size(13.0).color(col)))
}

/// `RRGGBB` (optionally `#`-prefixed) → colour, for artwork palette swatches.
fn hex_color(hex: &str) -> Option<egui::Color32> {
    let h = hex.trim_start_matches('#');
    if h.len() < 6 || !h.is_char_boundary(6) {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// `GET {wrapper_url}/status` with a generous 5 s budget — a remote/tunnel
/// wrapper can be legitimately slow, so the startup hint only fires on a real
/// failure (connect/HTTP/timeout error), never a slow-but-alive one. Runs on
/// the `mordent-wrapper-check` startup thread, never the UI thread.
pub(crate) fn probe_wrapper_reachable(url: &str) -> bool {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let status_url = format!("{}/status", url.trim_end_matches('/'));
    agent.get(&status_url).call().is_ok()
}

/// Actionable startup hint when the wrapper's `/status` does not answer.
/// Loopback URLs get the local start command + the config pointer; remote/
/// tunnel URLs get the connectivity question instead (the local command would
/// be wrong there).
pub(crate) fn wrapper_down_hint(url: &str) -> String {
    let url = url.trim_end_matches('/');
    if is_loopback_url(url) {
        format!(
            "wrapper not reachable at {url} — start it \
             (sudo ./wrapper-lite --host 127.0.0.1 --port 12340), set a remote \
             URL in ~/.config/mordent/config.toml, or re-run `mordent setup`"
        )
    } else {
        format!(
            "wrapper at {url} isn't responding — is the host up / tunnel connected? \
             (re-point Mordent with `mordent setup`)"
        )
    }
}

/// Whether `url`'s host is a loopback address (`localhost`, `127.0.0.0/8`,
/// `[::1]`). Deliberately simple string parsing — the config value is a bare
/// `scheme://host[:port]`, not worth a url crate.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        // drop any userinfo, then the :port
        authority
            .rsplit('@')
            .next()
            .unwrap_or(authority)
            .split(':')
            .next()
            .unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host == "::1" || host == "127.0.0.1" || host.starts_with("127.")
}

/// The helper port named by `url` (its authority's `:port`), or the packaged
/// default. Used when re-running the installer for a *local* helper, where the
/// service must keep listening on the port the config points at.
fn helper_port(url: &str) -> u16 {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    // An IPv6 authority without a port (`[::1]`) has no last-colon port; a
    // trailing bracket means the colon inside it is not a separator.
    if let Some(rest) = hostport.strip_prefix('[')
        && let Some((_, after)) = rest.split_once(']')
        && let Some(port) = after.strip_prefix(':')
        && let Ok(port) = port.parse()
    {
        return port;
    }
    hostport
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(crate::setup::DEFAULT_PORT)
}

/// Click-to-seek progress bar; reports the clicked fraction (0..1) via `act`.
fn seek_bar_ui(
    ui: &mut egui::Ui,
    width: f32,
    pos: std::time::Duration,
    total: std::time::Duration,
    accent: egui::Color32,
    act: &mut Option<BarIntent>,
) {
    let frac = if total.as_secs_f64() > 0.0 {
        (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let h = 8.0;
    // generous hit target; the visual bar is centred inside it
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 18.0), egui::Sense::click());
    let bar = egui::Rect::from_center_size(rect.center(), egui::vec2(width, h));
    ui.painter()
        .rect_filled(bar, 4.0, egui::Color32::from_gray(64));
    let w = (rect.width() as f64 * frac) as f32;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(bar.min, egui::vec2(w, h)),
        4.0,
        accent,
    );
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let f = ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0) as f64;
        *act = Some(BarIntent::Seek(std::time::Duration::from_secs_f64(
            f * total.as_secs_f64(),
        )));
    }
}

/// Allocation-free case-insensitive ordering for names (Iterator::cmp walks
/// both char streams lazily).
fn cmp_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms % 60000) / 1000)
}

fn fmt_pos(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn launch(cfg: Config) -> anyhow::Result<()> {
    launch_with(cfg, false)
}

/// `mordent setup`: open the UI already inside the first-run wizard, whatever the
/// config situation (this is the entry point for re-running setup).
pub fn launch_setup(cfg: Config) -> anyhow::Result<()> {
    launch_with(cfg, true)
}

fn launch_with(cfg: Config, start_wizard: bool) -> anyhow::Result<()> {
    // Transparent viewport: the window background comes from the theme's
    // `bg` color at `opacity` (see `App::clear_color` / theme.rs).
    //
    // Caveat: see-through only works when a compositor is running (X11
    // compositing manager / Wayland). On a bare X server (or a headless Xvfb
    // like the selftest uses) there is nothing to blend against, so the window
    // just renders solid at full alpha. That is a silent degradation — no
    // error, no panic, and playback/UI behave identically either way.
    //
    // Window icon: the app mark, rasterised by `brand` — the taskbar/dock shows
    // the logo instead of winit's default. Wayland has no protocol for this,
    // so the compositor there uses the .desktop entry's icon instead.
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 420.0])
            .with_icon(egui::IconData {
                rgba: crate::brand::app_icon_rgba(crate::brand::WINDOW_ICON_SIZE),
                width: crate::brand::WINDOW_ICON_SIZE,
                height: crate::brand::WINDOW_ICON_SIZE,
            })
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "mordent",
        native,
        // Fonts are bundled (`src/fonts.rs`, `include_bytes!`) and installed
        // by the first `App::poll_theme`: nothing is read from the filesystem,
        // so the UI renders identically on a bare system.
        Box::new(move |cc| Ok(Box::new(App::new(cfg, cc.egui_ctx.clone(), start_wizard)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod dnd_tests {
    use super::*;

    /// Three stacked rows: y 0-10, 10-20, 20-30 (midpoints 5, 15, 25).
    const ROWS: [(f32, f32); 3] = [(0.0, 10.0), (10.0, 20.0), (20.0, 30.0)];

    fn q(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn s(v: &[char]) -> String {
        v.iter().collect()
    }

    /// Mirrors `queuemodel::Queue::move_row`'s order effect (remove+insert,
    /// destination clamped) so these tests verify the drop_slot → move
    /// composition; the pointer bookkeeping itself is unit-tested in
    /// queuemodel.
    fn move_to(v: &mut Vec<char>, from: usize, to: usize) {
        if from >= v.len() {
            return;
        }
        let to = to.min(v.len() - 1);
        if from == to {
            return;
        }
        let c = v.remove(from);
        v.insert(to, c);
    }

    #[test]
    fn drop_slot_without_dragged_row() {
        assert_eq!(drop_slot(&ROWS, None, 3.0), 0); // above first midpoint
        assert_eq!(drop_slot(&ROWS, None, 12.0), 1); // between mid 5 and 15
        assert_eq!(drop_slot(&ROWS, None, 22.0), 2);
        assert_eq!(drop_slot(&ROWS, None, 99.0), 3); // past the end
        assert_eq!(drop_slot(&ROWS, None, 5.0), 1); // exactly on a midpoint goes below
    }

    #[test]
    fn drop_slot_skips_the_dragged_row() {
        // dragging row 0: only rows 1 (mid 15) and 2 (mid 25) count
        assert_eq!(drop_slot(&ROWS, Some(0), 3.0), 0);
        assert_eq!(drop_slot(&ROWS, Some(0), 20.0), 1);
        assert_eq!(drop_slot(&ROWS, Some(0), 99.0), 2);
        // dragging row 1: rows 0 (mid 5) and 2 (mid 25) count
        assert_eq!(drop_slot(&ROWS, Some(1), 3.0), 0);
        assert_eq!(drop_slot(&ROWS, Some(1), 22.0), 1);
        assert_eq!(drop_slot(&ROWS, Some(1), 99.0), 2);
        // dragging row 2: rows 0 and 1 count
        assert_eq!(drop_slot(&ROWS, Some(2), 3.0), 0);
        assert_eq!(drop_slot(&ROWS, Some(2), 12.0), 1);
        assert_eq!(drop_slot(&ROWS, Some(2), 99.0), 2);
    }

    #[test]
    fn move_to_matches_queue_move_row_order_effect() {
        let mut v = q("abcde");
        move_to(&mut v, 0, 2); // a down by two
        assert_eq!(s(&v), "bcade");
        move_to(&mut v, 2, 0); // a (now index 2) back to the top
        assert_eq!(s(&v), "abcde");
        move_to(&mut v, 4, 1); // multi-step upward move == remove+insert
        assert_eq!(s(&v), "aebcd");
        // unchanged idempotent cases
        let mut w = q("abcde");
        move_to(&mut w, 2, 2); // identity
        assert_eq!(s(&w), "abcde");
        move_to(&mut w, 1, 9); // clamped to the last index
        assert_eq!(s(&w), "acdeb");
        move_to(&mut w, 9, 0); // out of range: ignored
        assert_eq!(s(&w), "acdeb");
    }

    #[test]
    fn drag_down_reindexes_after_removal() {
        // queue a,b,c; the user drags row 0 (a) below row 1's midpoint:
        // drop_slot already reports the index among the *remaining* rows, so
        // the drop handler can feed it straight to the row move.
        let mut v = q("abc");
        let slot = drop_slot(&ROWS, Some(0), 22.0);
        assert_eq!(slot, 1);
        move_to(&mut v, 0, slot);
        assert_eq!(s(&v), "bac");
    }

    #[test]
    fn drag_up_reindexes_after_removal() {
        // queue a,b,c; the user drags row 2 (c) above the first midpoint.
        let mut v = q("abc");
        let slot = drop_slot(&ROWS, Some(2), 3.0);
        assert_eq!(slot, 0);
        move_to(&mut v, 2, slot);
        assert_eq!(s(&v), "cab");
        // and a downward drag of a middle row: b lands below c (and d)
        let mut v = q("abcd");
        let slot = drop_slot(&ROWS, Some(1), 99.0);
        assert_eq!(slot, 2); // among a, c → after them
        move_to(&mut v, 1, slot);
        assert_eq!(s(&v), "acbd");
    }
}

// (pause_state_tests moved to supervisor.rs with the fns they cover.)

#[cfg(test)]
mod wrapper_hint_tests {
    use super::{helper_port, is_loopback_url, wrapper_down_hint};
    use crate::setup::DEFAULT_PORT;

    /// The helper reinstall must keep the port the config points at: the
    /// service unit is rendered with it, so getting this wrong would move the
    /// helper's port out from under the running app.
    #[test]
    fn helper_port_reads_the_configured_port() {
        assert_eq!(helper_port("http://127.0.0.1:12340"), 12340);
        assert_eq!(helper_port("http://127.0.0.1:12340/"), 12340);
        assert_eq!(helper_port("https://music-box.local:8443"), 8443);
        assert_eq!(helper_port("http://user@host:9999/base"), 9999);
        assert_eq!(helper_port("http://[::1]:12000"), 12000);
        // no port anywhere → the packaged default
        assert_eq!(helper_port("http://localhost"), DEFAULT_PORT);
        assert_eq!(helper_port("http://[::1]"), DEFAULT_PORT);
        assert_eq!(helper_port("localhost"), DEFAULT_PORT);
        assert_eq!(helper_port(""), DEFAULT_PORT);
    }

    #[test]
    fn loopback_urls_get_the_local_start_hint() {
        for url in [
            "http://127.0.0.1:12340",
            "http://127.0.0.1:12340/",
            "http://localhost:12340",
            "http://[::1]:12340",
            "127.0.0.1:12340",
        ] {
            assert!(is_loopback_url(url), "{url} is loopback");
            let hint = wrapper_down_hint(url);
            assert!(hint.contains("not reachable at"), "{hint}");
            assert!(hint.contains(url.trim_end_matches('/')), "{hint}");
            assert!(hint.contains("start it"), "{hint}");
            assert!(hint.contains("~/.config/mordent/config.toml"), "{hint}");
            assert!(hint.contains("mordent setup"), "{hint}");
        }
    }

    #[test]
    fn remote_urls_get_the_connectivity_hint() {
        for url in [
            "https://wrapper.example.com",
            "http://10.0.0.5:12340",
            "http://192.168.1.9:8080/wrapper",
        ] {
            assert!(!is_loopback_url(url), "{url} is remote");
            let hint = wrapper_down_hint(url);
            assert!(hint.contains("isn't responding"), "{hint}");
            assert!(hint.contains("tunnel connected"), "{hint}");
            assert!(hint.contains("mordent setup"), "{hint}");
            assert!(!hint.contains("start it"), "{hint}");
        }
    }
}

#[cfg(test)]
mod toast_state_tests {
    use super::{COPIED_FEEDBACK, ErrorToast, TOAST_TTL};
    use std::time::{Duration, Instant};

    /// Synthetic instant `secs` after `base` (keeps the tests deterministic).
    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    fn toast(base: Instant) -> ErrorToast {
        ErrorToast {
            msg: "boom".into(),
            expires_at: base + TOAST_TTL,
            paused_at: None,
            expanded: false,
            copied_at: None,
        }
    }

    #[test]
    fn collapsed_toast_expires_after_ttl() {
        let t0 = Instant::now();
        let t = toast(t0);
        assert!(!t.is_expired(at(t0, 7.9)));
        assert_eq!(t.remaining(at(t0, 3.0)), Duration::from_secs(5));
        assert!(t.is_expired(at(t0, 8.0)));
        assert_eq!(t.remaining(at(t0, 9.0)), Duration::ZERO);
    }

    #[test]
    fn expanded_toast_pauses_expiry() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        t.set_expanded(true, at(t0, 2.0));
        // far past the original deadline: still alive, remaining frozen
        assert!(!t.is_expired(at(t0, 100.0)));
        assert_eq!(t.remaining(at(t0, 100.0)), Duration::from_secs(6));
        assert!(t.expanded);
    }

    #[test]
    fn closing_detail_resumes_with_remaining_time() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        t.set_expanded(true, at(t0, 2.0));
        assert!(!t.is_expired(at(t0, 100.0)));
        t.set_expanded(false, at(t0, 100.0));
        // 2 s were consumed before the pause → 6 s remain from t0+100
        assert_eq!(t.remaining(at(t0, 100.0)), Duration::from_secs(6));
        assert!(!t.is_expired(at(t0, 105.9)));
        assert!(t.is_expired(at(t0, 106.0)));
    }

    #[test]
    fn repeated_open_close_only_consumes_unpaused_time() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        t.set_expanded(true, at(t0, 1.0));
        t.set_expanded(false, at(t0, 3.0)); // consumed 1 s → 7 s left from t0+3
        t.set_expanded(true, at(t0, 5.0)); // paused again; 2 more s consumed → 5 s left
        t.set_expanded(false, at(t0, 50.0));
        assert_eq!(t.remaining(at(t0, 50.0)), Duration::from_secs(5));
        assert!(!t.is_expired(at(t0, 54.9)));
        assert!(t.is_expired(at(t0, 55.0)));
    }

    #[test]
    fn copied_feedback_lasts_one_second() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        assert!(!t.copied_feedback(t0));
        t.copied_at = Some(t0);
        assert!(t.copied_feedback(at(t0, 0.5)));
        assert!(!t.copied_feedback(t0 + COPIED_FEEDBACK));
    }

    #[test]
    fn collapsed_toast_next_repaint_is_remaining_ttl() {
        let t0 = Instant::now();
        let t = toast(t0);
        assert_eq!(t.next_repaint(at(t0, 3.0)), Some(Duration::from_secs(5)));
    }

    #[test]
    fn expanded_toast_next_repaint_is_none_without_copied_feedback() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        t.set_expanded(true, at(t0, 2.0));
        // expiry is frozen while the detail is open → nothing to wake for
        assert_eq!(t.next_repaint(at(t0, 100.0)), None);
    }

    #[test]
    fn copied_feedback_wins_while_it_shows() {
        let t0 = Instant::now();
        let mut t = toast(t0);
        t.set_expanded(true, at(t0, 1.0));
        t.copied_at = Some(at(t0, 1.0));
        assert_eq!(t.next_repaint(at(t0, 1.2)), Some(Duration::from_millis(800)));
        // the label has cleared and expiry is still frozen → static
        assert_eq!(t.next_repaint(at(t0, 3.0)), None);
    }
}

#[cfg(test)]
mod repaint_tests {
    use super::next_repaint_delay;
    use std::time::Duration;

    #[test]
    fn idle_parks() {
        assert_eq!(next_repaint_delay(false, false, None, None), None);
    }

    #[test]
    fn playing_ticks_fast() {
        assert_eq!(
            next_repaint_delay(true, true, None, None),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn loaded_track_or_queue_ticks_slow() {
        assert_eq!(
            next_repaint_delay(false, true, None, None),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn countdowns_shorten_the_sleep() {
        // a live countdown alone still wakes the otherwise-parked loop
        assert_eq!(
            next_repaint_delay(false, false, Some(Duration::from_millis(40)), None),
            Some(Duration::from_millis(40))
        );
        // the nearest of toast / sleep deadline wins
        assert_eq!(
            next_repaint_delay(
                false,
                false,
                Some(Duration::from_secs(8)),
                Some(Duration::from_secs(3))
            ),
            Some(Duration::from_secs(3))
        );
        // a countdown never slows a playing tick
        assert_eq!(
            next_repaint_delay(true, true, Some(Duration::from_secs(5)), None),
            Some(Duration::from_millis(100))
        );
    }
}

/// The wake-thread shared-value surface is deliberately thin: publication is
/// `update` storing the already-unit-tested `next_repaint_delay` output, and
/// the thread side is `sleep` + `request_repaint` (std + egui calls with no
/// logic of their own). Only the pure encode/decode of the published value is
/// testable — covered here.
#[cfg(test)]
mod wake_thread_tests {
    use super::{wake_publish_ms, wake_thread_sleep, WAKE_PARKED};
    use std::time::Duration;

    #[test]
    fn idle_publishes_parked_sentinel_and_thread_parks() {
        assert_eq!(wake_publish_ms(None), WAKE_PARKED);
        assert_eq!(wake_thread_sleep(WAKE_PARKED), None);
    }

    #[test]
    fn delay_roundtrips_through_the_published_value() {
        for ms in [100u64, 250, 40, 1000 * 60 * 30] {
            let published = wake_publish_ms(Some(Duration::from_millis(ms)));
            assert_eq!(wake_thread_sleep(published), Some(Duration::from_millis(ms)));
        }
    }

    #[test]
    fn zero_delay_never_spins_the_thread() {
        assert_eq!(wake_thread_sleep(0), Some(Duration::from_millis(1)));
    }

    #[test]
    fn oversized_delay_clamps_below_the_sentinel() {
        let clamped = wake_publish_ms(Some(Duration::MAX));
        assert_eq!(clamped, WAKE_PARKED - 1);
        // and it still means "wake after a very long sleep", not "park"
        assert_ne!(wake_thread_sleep(clamped), None);
    }
}
