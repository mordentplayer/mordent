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
//! In decode-only mode (no audio device) a fake playback clock simulates the
//! running track so queue/auto-advance behaves like a real player.

use crate::api::{
    AlbumHit, Api, LibraryPlaylist, PlayEvent, PlayMode, SearchResults, Track, META_TTL_LIBRARY,
};
use crate::config::Config;
use crate::fonts::FontChoice;
use crate::mpris;
use crate::player::{PacketSource, Player};
use crate::report;
use crate::theme::Theme;
use crate::stream;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::queuemodel::{Advance, Queue, RemoveEffect, Repeat};

enum Msg {
    Results(Box<SearchResults>),
    /// One page of the user's library albums (appended), next offset.
    Library(Vec<AlbumHit>, Option<u32>),
    /// Fully prepared decoder for the given playback generation.
    Source(u64, Box<PacketSource>),
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

/// What kind of source application [`App::play_now_at`] performs: it decides
/// both the report segment semantics and the pause state on arrival.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum PlayKind {
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
    /// stage1 / stage2 / stage4 audible targets (now-playing names).
    expect: [String; 3],
    started: std::time::Instant,
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
}

/// UI settings persisted to ~/.config/amc/state.toml (atomic write).
#[derive(Serialize, Deserialize, Default)]
struct UiState {
    volume: Option<f32>,
    last_view: Option<String>,
    /// Search sub-view: "songs" | "albums" (optional; older builds lack it).
    search_sub: Option<String>,
    repeat: Option<String>,
    /// Favorited track ids, sorted for a stable file (in memory it is a set).
    favorites: Option<Vec<String>>,
}

fn state_path() -> std::path::PathBuf {
    crate::config::home().join(".config/amc/state.toml")
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

pub struct App {
    cfg: Config,
    player: Player,
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
    /// AMC_DEMO_PLAYLIST=<n>: open the n-th playlist once the list loads
    /// (headless screenshot hook, alongside AMC_DEMO_VIEW).
    demo_playlist: Option<usize>,
    /// Non-consuming queue + pointer to the current entry (see queuemodel).
    queue: Queue,
    now: Option<Track>,
    /// Bumped on every play request; async sources carry the generation so
    /// stale ones are dropped instead of appended.
    generation: u64,
    /// A worker is building the source for the current generation.
    preparing: bool,
    show_queue: bool,
    /// Queue row currently being dragged by its ⠿ handle, and the slot it
    /// would land in (index among the other rows) — both cleared on release.
    queue_drag: Option<usize>,
    queue_drop: Option<usize>,
    /// decode-only (no audio device): simulated playback window
    /// (clock start, remaining duration).
    fake_play: Option<(std::time::Instant, std::time::Duration)>,
    /// decode-only pause point (frozen clock).
    fake_pause_at: Option<std::time::Instant>,
    /// Sink-mode playback starts at this offset (pre-seek samples were
    /// discarded by the worker).
    play_offset: std::time::Duration,
    /// Pause state the next applied source must start with (see
    /// [`applied_source_pause_state`]). `None` once consumed or when the user
    /// acted in between (their action then wins over the pending decision).
    arrival_paused: Option<bool>,
    repeat: Repeat,
    shuffle: bool,
    /// Album being fetched, and what to do with its tracks.
    album_job: Option<AlbumJob>,
    /// Album id whose `+` is pending (shows "adding…").
    album_busy: Option<String>,
    album_modal: Option<AlbumModal>,
    cheatsheet: bool,
    /// Sleep timer deadline (paused playback when it fires).
    sleep_at: Option<(std::time::Instant, std::time::Duration)>,
    /// AMC_SELFTEST=1: drive play_now/queue/auto-advance deterministically
    /// and print SELFTEST PASS/FAIL (headless CI aid).
    selftest: Option<SelftestState>,
    volume: f32,
    /// Active theme (built-in defaults + `~/.config/amc/theme.toml`).
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
    /// Desktop media controls (MPRIS); `None` if D-Bus is unusable.
    mpris: Option<mpris::Mpris>,
    /// Bumped on every explicit seek so MPRIS emits `Seeked`.
    seek_epoch: u64,
    /// Best-effort Apple play reporting; None = gated off (no token / no
    /// sink / disabled), so nothing is ever sent in decode-only mode.
    reporter: Option<report::PlayReporter>,
    /// A segment is open for `now` (START went out; END pending).
    report_open: bool,
    /// start-position-in-milliseconds carried by the open segment's events
    /// (positions chain: each event starts where the previous one ended).
    report_seg_start_ms: u64,
    /// Container reported by the previous START, for the 0/1 change hint.
    report_last_container: Option<Option<String>>,
    /// track id → numeric catalog album adam id, filled when tracks arrive
    /// from an album context (album modal fetch / Queue All) so STARTs keep
    /// their album attribution across queue auto-advance and shuffle.
    album_ids: HashMap<String, String>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let player = Player::new();
        let state = load_state();
        let (theme, theme_warns) = Theme::load();
        for w in &theme_warns {
            eprintln!("[theme] {w}");
        }
        let volume = state.volume.unwrap_or(0.8).clamp(0.0, 1.0);
        player.volume(volume);
        // Play reporting only with the config switch on, a media-user-token,
        // and a REAL audio sink — the decode-only fake clock never reports.
        let reporter = if report::PlayReporter::enabled(&cfg, player.sink().is_some()) {
            Some(report::PlayReporter::spawn(cfg.clone()))
        } else {
            None
        };
        let (tx, rx) = channel();
        let mut app = Self {
            cfg,
            player,
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
            demo_playlist: std::env::var("AMC_DEMO_PLAYLIST")
                .ok()
                .and_then(|s| s.parse().ok()),
            queue: Queue::default(),
            now: None,
            generation: 0,
            preparing: false,
            show_queue: false,
            queue_drag: None,
            queue_drop: None,
            fake_play: None,
            fake_pause_at: None,
            play_offset: std::time::Duration::ZERO,
            arrival_paused: None,
            repeat: match state.repeat.as_deref() {
                Some("all") => Repeat::All,
                Some("one") => Repeat::One,
                _ => Repeat::Off,
            },
            shuffle: false,
            album_job: None,
            album_busy: None,
            album_modal: None,
            cheatsheet: false,
            sleep_at: None,
            selftest: std::env::var("AMC_SELFTEST").is_ok().then(|| SelftestState {
                stage: 0,
                expect: Default::default(),
                started: std::time::Instant::now(),
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
            last_title: "amc".into(),
            mpris: None,
            seek_epoch: 0,
            reporter,
            report_open: false,
            report_seg_start_ms: 0,
            report_last_container: None,
            album_ids: HashMap::new(),
            rx,
            tx,
        };
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
            // input the deterministic test depends on.
            app.repeat = Repeat::Off;
            eprintln!("[selftest] armed (repeat forced off)");
        }
        // headless verification hooks: AMC_DEMO_TERM=<query> auto-searches,
        // AMC_DEMO_VIEW=library|albums|favorites|playlists opens that view
        // ("albums" = the Search view's Albums sub-view), AMC_DEMO_QUEUE=1
        // opens the queue panel.
        if let Ok(term) = std::env::var("AMC_DEMO_TERM") {
            app.search = term;
            app.spawn_search();
        }
        if std::env::var("AMC_DEMO_QUEUE").is_ok() {
            app.show_queue = true;
        }
        match std::env::var("AMC_DEMO_VIEW").as_deref() {
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

    fn spawn_library(&mut self, offset: u32) {
        self.library_loading = true;
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
            match api.library_albums_page(100, offset) {
                Ok((albums, next)) => {
                    let _ = tx.send(Msg::Library(albums, next));
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

    // ---- Apple play reporting ----

    /// play-mode triple from the current repeat/shuffle state (amc has no
    /// autoplay-after-queue feature → auto 1).
    fn play_mode(&self) -> PlayMode {
        PlayMode {
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
        self.reporter.is_some() && self.player.sink().is_some()
    }

    fn report_pos_ms(&self) -> u64 {
        self.playback_position().as_millis() as u64
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
        self.report_end(PlayEvent::END_SCRUB_END, new_ms);
        self.report_open = true;
        self.report_seg_start_ms = new_ms;
    }

    /// Best-effort END(4) at the current position + bounded flush; used by
    /// every exit path (window close, ctrl+Q, MPRIS Quit).
    fn exit_flush(&mut self) {
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_SUSPENDED, pos);
        if let Some(r) = self.reporter.take() {
            r.shutdown(None);
        }
    }

    /// Play a track immediately at `offset`: cancels whatever is
    /// current/preparing by bumping the generation. Queue/pointer changes are
    /// the caller's job; this only owns audio, `now` and the START report.
    /// `kind` decides the report segment semantics and the pause state the
    /// source starts with on arrival.
    fn play_now_at(&mut self, track: Track, offset: std::time::Duration, kind: PlayKind) {
        match kind {
            PlayKind::SeekRestart => {
                // a seek restart, not a new play — log it distinctly so a
                // seek is never mistaken for a second dispatch
                eprintln!(
                    "[play] {} — {} (seek → {})",
                    track.name,
                    track.artist,
                    fmt_ms(offset.as_millis() as u64)
                );
            }
            PlayKind::NewSegment | PlayKind::AutoAdvance => {
                eprintln!("[play] {} — {}", track.name, track.artist);
                self.report_start(&track, 0);
            }
        }
        self.preparing = true;
        self.now = Some(track.clone());
        self.play_offset = offset;
        let pre_pause = if self.player.decode_only {
            self.fake_pause_at.is_some()
        } else {
            self.player.sink().is_some_and(|s| s.is_paused())
        };
        let arrival_paused =
            applied_source_pause_state(pre_pause, kind != PlayKind::AutoAdvance, kind == PlayKind::AutoAdvance);
        self.fake_play = None;
        self.fake_pause_at = arrival_paused.then(std::time::Instant::now);
        self.arrival_paused = Some(arrival_paused);
        if let Some(s) = self.player.sink() {
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
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let t = track;
        let _ = std::thread::Builder::new().name("play-worker".into()).spawn(move || {
            let res = (|| -> anyhow::Result<()> {
                let (prog, _mode) = stream::open_track(&cfg, &t.id)?;
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
                tx.send(Msg::Source(generation, Box::new(src)))
                    .map_err(|_| anyhow::anyhow!("channel closed"))
            })();
            if let Err(e) = res {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        });
    }

    fn play_now(&mut self, track: Track) {
        self.remember_tracks(std::slice::from_ref(&track));
        // click-replacing the current track closes its segment (reason 5);
        // the outgoing current stays listed in the queue (pointer moves on)
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_REPLACED, pos);
        self.queue.insert_after_current(vec![track.clone()]);
        self.play_now_at(track, std::time::Duration::ZERO, PlayKind::NewSegment);
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
        let idle_finished = self.now.is_none();
        if let Some(idx) = self.queue.append(new, idle_finished) {
            let t = self.queue.tracks[idx].clone();
            self.play_now_at(t, std::time::Duration::ZERO, PlayKind::NewSegment);
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
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_REPLACED, pos);
        let first = tracks[0].clone();
        self.queue.insert_after_current(tracks);
        self.play_now_at(first, std::time::Duration::ZERO, PlayKind::NewSegment);
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
        if j < 0 || j as usize >= self.queue.len() {
            return;
        }
        self.queue.move_row(i, j as usize);
    }

    /// Move queue row `from` to final index `to` (drag-and-drop drop handler).
    fn queue_move_to(&mut self, from: usize, to: usize) {
        self.queue.move_row(from, to);
    }

    /// Shuffle keeps the played region + current intact and reorders only the
    /// up-next region; toggling off restores the saved order (queuemodel).
    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.queue.toggle_shuffle();
    }

    /// Elapsed time of the current track (fake clock in decode-only mode).
    fn playback_position(&self) -> std::time::Duration {
        match (&self.fake_play, &self.fake_pause_at) {
            (Some((start, cap)), Some(p)) => p
                .checked_duration_since(*start)
                .unwrap_or_default()
                .min(*cap),
            (Some((start, cap)), None) => start.elapsed().min(*cap),
            _ => self.player.position().unwrap_or_default() + self.play_offset,
        }
    }

    fn is_paused(&self) -> bool {
        if self.now.is_none() {
            return true;
        }
        if self.player.decode_only {
            self.fake_play.is_some() && self.fake_pause_at.is_some()
        } else {
            self.player.sink().is_some_and(|s| s.is_paused())
        }
    }

    fn toggle_pause_now(&mut self) {
        if self.now.is_none() {
            return;
        }
        let was_paused = self.is_paused();
        if self.player.decode_only {
            if let Some((start, _)) = &mut self.fake_play {
                if let Some(p) = self.fake_pause_at.take() {
                    if let Some(frozen) = p.checked_duration_since(*start) {
                        *start += frozen;
                    }
                } else {
                    self.fake_pause_at = Some(std::time::Instant::now());
                }
            }
        } else {
            self.player.toggle_pause();
        }
        // the user just acted — a pending source must not override the new state
        self.arrival_paused = None;
        // pause closes the segment (3); resume opens one at the paused
        // position (hint 0 — same container)
        let pos = self.report_pos_ms();
        if was_paused {
            if let Some(t) = self.now.clone() {
                self.report_start(&t, pos);
            }
        } else {
            self.report_end(PlayEvent::END_PAUSED, pos);
        }
    }

    /// Skip to the next queue entry (manual ⏭ / N / MPRIS Next): pointer+1
    /// and play. At the end of the list this is a deliberate no-op — the
    /// current track keeps playing (manual skips never wrap, whatever the
    /// repeat mode).
    fn skip_current(&mut self) {
        if self.now.is_none() {
            return;
        }
        let Some(next) = self.queue.peek_next() else {
            return;
        };
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_SKIPPED_FORWARDS, pos);
        self.player.skip();
        self.queue.play_at(next);
        let t = self.queue.tracks[next].clone();
        self.play_now_at(t, std::time::Duration::ZERO, PlayKind::NewSegment);
    }

    /// Natural end of the current track: move the pointer (repeat-one
    /// replays, repeat-all wraps) and play that entry; with no next entry,
    /// stop cleanly — the pointer stays on the finished entry so the session
    /// stays listed, and a later `+` resumes from the new append.
    fn advance(&mut self) {
        let Some(finished) = self.now.clone() else {
            return;
        };
        // END 7 at the full duration. Manual skips report END 2 in
        // skip_current instead.
        self.report_end_for(&finished, PlayEvent::END_NATURAL, finished.duration_ms);
        let next = match self.queue.advance(self.repeat) {
            Advance::Replay => self.queue.current_index(),
            Advance::Next(i) => Some(i),
            Advance::Stop => None,
        };
        if let Some(i) = next {
            let t = self.queue.tracks[i].clone();
            self.play_now_at(t, std::time::Duration::ZERO, PlayKind::AutoAdvance);
        } else {
            self.stop_playback();
        }
    }

    /// Drop the current track and stop audibly without reporting (the caller
    /// has already closed the segment); queue and pointer are left alone.
    fn stop_playback(&mut self) {
        self.now = None;
        self.generation += 1; // invalidate any in-flight source
        self.preparing = false;
        self.fake_play = None;
        self.fake_pause_at = None;
        self.play_offset = std::time::Duration::ZERO;
        if let Some(s) = self.player.sink() {
            s.clear();
            // rodio 0.20 `Sink::clear()` also pauses the sink; nothing is
            // playing here, so leave the flag clean for the next application
            s.play();
        }
        self.arrival_paused = None;
    }

    /// ⏮: restart current if >3s elapsed, else the previous entry (pointer-1);
    /// on the first entry, restart it.
    fn prev_track(&mut self) {
        if self.now.is_some() && self.playback_position().as_secs_f64() > 3.0 {
            self.seek_to(std::time::Duration::ZERO);
            return;
        }
        let Some(prev) = self.queue.peek_prev() else {
            if self.now.is_some() {
                self.seek_to(std::time::Duration::ZERO);
            }
            return;
        };
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_SKIPPED_BACKWARDS, pos);
        self.queue.play_at(prev);
        let t = self.queue.tracks[prev].clone();
        self.play_now_at(t, std::time::Duration::ZERO, PlayKind::NewSegment);
    }

    /// Click-to-seek / ±5s. Sink mode restarts the stream at the offset;
    /// decode-only shifts the fake clock.
    fn seek_to(&mut self, target: std::time::Duration) {
        let Some(t) = self.now.as_ref().map(|t| t.duration_ms) else {
            return;
        };
        self.seek_epoch += 1; // MPRIS emits Seeked
        let total = std::time::Duration::from_millis(t);
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
        if self.player.decode_only {
            let now = std::time::Instant::now();
            let end = std::time::Duration::from_millis(t.min(8_000));
            self.fake_play = Some((now.checked_sub(target).unwrap_or(now), end));
            if self.fake_pause_at.is_some() {
                self.fake_pause_at = Some(now);
            }
            self.play_offset = target;
        } else if self.now.is_some() {
            let t = self.now.clone().expect("checked above");
            self.play_now_at(t, target, PlayKind::SeekRestart);
        }
    }

    fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.player.volume(self.volume);
    }

    /// Pause without toggling (sleep timer).
    fn pause_now(&mut self) {
        let playing = if self.player.decode_only {
            self.fake_play.is_some() && self.fake_pause_at.is_none()
        } else {
            self.player.sink().is_some_and(|s| !s.is_paused())
        };
        if self.player.decode_only {
            if self.fake_play.is_some() && self.fake_pause_at.is_none() {
                self.fake_pause_at = Some(std::time::Instant::now());
            }
        } else {
            if let Some(s) = self.player.sink()
                && !s.is_paused()
            {
                s.pause();
            }
        }
        // the user just acted — a pending source must not override the new state
        self.arrival_paused = None;
        if playing {
            let pos = self.report_pos_ms();
            self.report_end(PlayEvent::END_PAUSED, pos);
        }
    }

    /// MPRIS Stop: clear the queue (the queue panel's "clear") and drop the
    /// current track, cancelling any in-flight source.
    fn stop_all(&mut self) {
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_SUSPENDED, pos);
        self.queue.clear();
        self.stop_playback();
    }

    /// Drain remote MPRIS commands and dispatch them onto the same code
    /// paths as clicks/shortcuts.
    fn drain_mpris(&mut self, ctx: &egui::Context) {
        let cmds: Vec<mpris::Command> = match self.mpris.as_ref() {
            Some(m) => {
                let mut v = Vec::new();
                while let Ok(c) = m.rx.try_recv() {
                    v.push(c);
                }
                v
            }
            None => return,
        };
        for cmd in cmds {
            match cmd {
                mpris::Command::Play => {
                    if self.now.is_some() && self.is_paused() {
                        self.toggle_pause_now();
                    }
                }
                mpris::Command::Pause => self.pause_now(),
                mpris::Command::TogglePause => self.toggle_pause_now(),
                mpris::Command::Next => self.skip_current(),
                mpris::Command::Previous => self.prev_track(),
                mpris::Command::Stop => self.stop_all(),
                mpris::Command::Seek(us) => {
                    let step = std::time::Duration::from_micros(us.unsigned_abs());
                    let target = if us >= 0 {
                        self.playback_position() + step
                    } else {
                        self.playback_position().saturating_sub(step)
                    };
                    self.seek_to(target);
                }
                mpris::Command::SetPosition(us) => {
                    self.seek_to(std::time::Duration::from_micros(us))
                }
                mpris::Command::SetLoop(l) => {
                    self.repeat = match l {
                        mpris::LoopStatus::None => Repeat::Off,
                        mpris::LoopStatus::Track => Repeat::One,
                        mpris::LoopStatus::Playlist => Repeat::All,
                    };
                    self.save_ui_state();
                }
                mpris::Command::SetShuffle(want) => {
                    if self.shuffle != want {
                        self.toggle_shuffle();
                    }
                }
                mpris::Command::SetVolume(v) => {
                    self.volume = v.clamp(0.0, 1.0);
                    self.player.volume(self.volume);
                    self.save_ui_state();
                }
                mpris::Command::Raise => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                mpris::Command::Quit => {
                    self.exit_flush();
                    std::process::exit(0);
                } // same as ctrl+Q
            }
        }
    }

    /// Publish a playback snapshot for the MPRIS thread (per frame; the
    /// D-Bus side diffs and emits only on change).
    fn publish_mpris(&self) {
        let Some(m) = self.mpris.as_ref() else {
            return;
        };
        let (title, artist, album, art_url, track_id, length) = match &self.now {
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
        let position = self.playback_position();
        m.publish(mpris::Snapshot {
            status: if self.now.is_none() {
                mpris::PlaybackStatus::Stopped
            } else if self.is_paused() {
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
            position,
            volume: self.volume,
            shuffle: self.shuffle,
            loop_status: match self.repeat {
                Repeat::Off => mpris::LoopStatus::None,
                Repeat::One => mpris::LoopStatus::Track,
                Repeat::All => mpris::LoopStatus::Playlist,
            },
            has_track: self.now.is_some(),
            // pure pointer bounds — repeat-all doesn't fake a next entry and
            // the >3 s restart isn't a "go previous"
            can_next: self.queue.peek_next().is_some(),
            can_prev: self.queue.peek_prev().is_some(),
            seek_epoch: self.seek_epoch,
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
                    match self.repeat {
                        Repeat::Off => "off",
                        Repeat::All => "all",
                        Repeat::One => "one",
                    }
                    .to_string(),
                ),
                favorites: Some(favs),
            },
        );
        self.state_dirty = None;
    }

    // ---- keyboard ----

    fn handle_keys(&mut self, ctx: &egui::Context) -> bool {
        let mut focus_search = false;
        let typing = ctx.wants_keyboard_input();
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
            if i.key_pressed(egui::Key::Slash) && i.modifiers.shift {
                self.cheatsheet = !self.cheatsheet;
            }
            if i.key_pressed(egui::Key::Slash) && !i.modifiers.shift {
                focus_search = true;
            }
            let total_ms = self.now.as_ref().map(|t| t.duration_ms).unwrap_or(0);
            if total_ms > 0 {
                let step = std::time::Duration::from_secs(5);
                if i.key_pressed(egui::Key::ArrowRight) {
                    let t = self.playback_position() + step;
                    self.seek_to(t);
                }
                if i.key_pressed(egui::Key::ArrowLeft) {
                    self.seek_to(self.playback_position().saturating_sub(step));
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
                    self.exit_flush();
                    std::process::exit(0);
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
                Msg::Library(albums, next) => {
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
                Msg::Source(g, src) => {
                    if g == self.generation {
                        self.player.enqueue(*src);
                        self.preparing = false;
                        // enforce the planned arrival pause state; `None` means
                        // the user acted during preparation and their action
                        // (already applied to the sink) must not be overridden
                        if let Some(true) = self.arrival_paused
                            && let Some(s) = self.player.sink()
                        {
                            s.pause();
                        }
                        self.arrival_paused = None;
                        // start the decode-only fake clock (keep a seek target);
                        // tuple = (clock start, elapsed-at-end)
                        if self.player.decode_only
                            && self.fake_play.is_none()
                            && let Some(t) = &self.now
                        {
                            let end =
                                std::time::Duration::from_millis(t.duration_ms.min(8_000));
                            let start = std::time::Instant::now()
                                .checked_sub(self.play_offset)
                                .unwrap_or_else(std::time::Instant::now);
                            self.fake_play = Some((start, end));
                        }
                    } // stale: dropping src also cancels its download
                }
                Msg::AlbumTracks { id, tracks, .. } => {
                    // album-context attribution: keep the album adam id for
                    // these tracks so STARTs stay attributed through queue
                    // auto-advance / shuffle (library `i.` ids are not adam
                    // ids and are never reported).
                    if id.chars().all(|c| c.is_ascii_digit()) {
                        for t in &tracks {
                            self.album_ids.insert(t.id.clone(), id.clone());
                        }
                    }
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
                    self.preparing = false;
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
            }
        }
    }

    // ---- deterministic selftest (AMC_SELFTEST=1) ----

    fn selftest_step(&mut self) {
        // stage 0 — synchronous queue-model invariants (pointer non-
        // consumption, +/auto-start rules, reorder/drag/shuffle bookkeeping,
        // prev/next, ✕ handling). No audio needed; decode-only safe.
        if self
            .selftest
            .as_ref()
            .is_some_and(|st| st.stage == 0 && self.results.as_ref().is_some_and(|r| r.tracks.len() >= 5))
        {
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
            let now_is = |app: &Self, want: &str| {
                app.now.as_ref().map(|t| t.name.as_str()) == Some(want)
            };
            let front_is =
                |q: &Queue, id: &str| q.tracks.first().map(|t| t.id.as_str()) == Some(id);
            let get_is = |q: &Queue, i: usize, id: &str| {
                q.tracks.get(i).map(|t| t.id.as_str()) == Some(id)
            };

            // (1) "play now" from idle: pointer lands on the track
            self.play_now(seed[0].clone());
            let ok_start =
                now_is(self, &n0) && self.queue.len() == 1 && self.queue.pos == Some(0);

            // (2) `+` while playing: append at the end, never jump
            self.enqueue_tracks(vec![seed[1].clone(), seed[2].clone()]);
            let ok_plus = self.queue.len() == 3
                && self.queue.pos == Some(0)
                && get_is(&self.queue, 1, &id1)
                && now_is(self, &n0);

            // (3) reorder / drag / shuffle keep the pointer on the current row
            self.queue_move(1, -1);
            let ok_up = front_is(&self.queue, &id1) && self.queue.pos == Some(1);
            self.queue_move(0, 1);
            let ok_down = get_is(&self.queue, 1, &id1) && self.queue.pos == Some(0);
            // drag-and-drop path: the exact method the drop handler calls
            self.queue_move_to(0, 2);
            let ok_drag_down = front_is(&self.queue, &id1) && self.queue.pos == Some(2);
            self.queue_move_to(2, 0);
            let ok_drag_up = front_is(&self.queue, &id0) && self.queue.pos == Some(0);
            // shuffle toggle must restore the original order deterministically
            let before: Vec<String> = self.queue.tracks.iter().map(|t| t.id.clone()).collect();
            self.toggle_shuffle();
            self.toggle_shuffle();
            let after: Vec<String> = self.queue.tracks.iter().map(|t| t.id.clone()).collect();
            let ok_shuffle = after == before && self.queue.pos == Some(0);

            // (4) manual ⏭ / ⏮ move the pointer without consuming
            self.skip_current();
            let ok_next = self.queue.pos == Some(1) && self.queue.len() == 3;
            self.prev_track();
            let ok_prev = self.queue.pos == Some(0) && self.queue.len() == 3;

            // (5) paused + `+`: append only, no auto-jump
            self.pause_now();
            self.enqueue_tracks(vec![seed[3].clone()]);
            let ok_paused_plus = self.queue.len() == 4
                && self.queue.pos == Some(0)
                && now_is(self, &n0);
            self.toggle_pause_now();

            // (6) ✕ of a later row leaves playback alone
            self.remove_queue_row(3);
            let ok_remove_later = self.queue.len() == 3 && self.queue.pos == Some(0);

            // (7) ✕ of the current row hands over to its successor
            self.remove_queue_row(0);
            let ok_remove_cur = self.queue.pos == Some(0)
                && self.queue.len() == 2
                && now_is(self, &n1);

            // (8) "play now" inserts right after the current entry
            self.play_now(seed[4].clone());
            let ok_insert = self.queue.len() == 3
                && self.queue.pos == Some(1)
                && get_is(&self.queue, 1, &seed[4].id);

            let ok = ok_start
                && ok_plus
                && ok_up
                && ok_down
                && ok_drag_down
                && ok_drag_up
                && ok_shuffle
                && ok_next
                && ok_prev
                && ok_paused_plus
                && ok_remove_later
                && ok_remove_cur
                && ok_insert;
            if !ok {
                eprintln!(
                    "[selftest] stage0 FAIL (start={ok_start} plus={ok_plus} up={ok_up} \
                     down={ok_down} drag_down={ok_drag_down} drag_up={ok_drag_up} \
                     shuffle={ok_shuffle} next={ok_next} prev={ok_prev} \
                     paused_plus={ok_paused_plus} remove_later={ok_remove_later} \
                     remove_cur={ok_remove_cur} insert={ok_insert})"
                );
                eprintln!("[selftest] RESULT: FAIL");
                std::process::exit(1);
            }
            eprintln!("[selftest] stage0 queue-model PASS");
            if let Some(st) = &mut self.selftest {
                st.expect = [n4, n2, n0];
                st.stage = 1;
                st.started = std::time::Instant::now();
            }
        }
        if let Some(st) = &mut self.selftest {
            let elapsed = st.started.elapsed();
            let now_name = self.now.as_ref().map(|t| t.name.clone()).unwrap_or_default();
            let matches = |want: &str| now_name == *want;
            match st.stage {
                // stage 1: the play_now target must eventually play (≤ 15 s)
                1 => {
                    if matches(&st.expect[0]) && !self.preparing {
                        eprintln!("[selftest] stage1 PASS (now={now_name:?}, queue={})", self.queue.len());
                        st.stage = 2;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage1 FAIL (now={now_name:?}, want {:?}, queue={})",
                            st.expect[0],
                            self.queue.len()
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stage 2: natural end auto-advances to the next entry; the
                // queue is NOT consumed (still 3 rows, pointer moved on)
                2 => {
                    if matches(&st.expect[1]) && self.queue.len() == 3 && self.queue.pos == Some(2) {
                        eprintln!("[selftest] stage2 PASS (now={now_name:?}, queue={})", self.queue.len());
                        st.stage = 3;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage2 FAIL (now={now_name:?}, want {:?}, queue={} pos={:?})",
                            st.expect[1],
                            self.queue.len(),
                            self.queue.pos
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
                        self.now.is_none() && self.queue.pos == Some(2) && self.queue.len() == 3;
                    if st.stage == 3 && done {
                        st.stage = 5;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage3 FAIL (now={now_name:?}, queue={} pos={:?})",
                            self.queue.len(),
                            self.queue.pos
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stage 4: the post-finish `+` auto-started the appended track
                4 => {
                    if matches(&st.expect[2]) && self.queue.len() == 4 && self.queue.pos == Some(3) {
                        eprintln!("[selftest] stage4 PASS (now={now_name:?}, queue={})", self.queue.len());
                        // stages 6-8 verify the pause mirror (seek restarts
                        // restore the pre-seek pause state; a paused replace
                        // lands paused) and then finish the run
                        st.stage = 6;
                        st.started = std::time::Instant::now();
                    } else if elapsed > std::time::Duration::from_secs(15) {
                        eprintln!(
                            "[selftest] stage4 FAIL (now={now_name:?}, want {:?}, queue={} pos={:?})",
                            st.expect[2],
                            self.queue.len(),
                            self.queue.pos
                        );
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
                // stages 6-8: wait for the current source to land; the actual
                // steps run outside this borrow (below), like trigger (b).
                // Only the timeout is handled here.
                6..=8 if elapsed > std::time::Duration::from_secs(15) => {
                    eprintln!(
                        "[selftest] stage{} FAIL (now={now_name:?}, preparing={})",
                        st.stage,
                        self.preparing
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
        match self.selftest.as_ref().map(|st| st.stage) {
            // (c) stage 6: pause, then a seek restart — the exact pre-seek
            // pause state must be restored; resuming must be audible.
            Some(6) if !self.preparing => {
                self.pause_now();
                self.seek_to(std::time::Duration::ZERO);
                let ok_seek = self.is_paused() && self.now.is_some();
                self.toggle_pause_now();
                let ok_resume = !self.is_paused();
                if let Some(st) = &mut self.selftest {
                    st.started = std::time::Instant::now();
                    if ok_seek && ok_resume {
                        eprintln!("[selftest] stage6 PASS (seek restores pause; resume plays)");
                        st.stage = 7;
                    } else {
                        eprintln!("[selftest] stage6 FAIL (seek_paused={ok_seek} resume_playing={ok_resume})");
                        eprintln!("[selftest] RESULT: FAIL");
                        std::process::exit(1);
                    }
                }
            }
            // (d) stage 7: replace the current track while PAUSED.
            Some(7) if !self.preparing => {
                self.pause_now();
                let t = self.results.as_ref().expect("checked").tracks[2].clone();
                self.play_now(t);
                if let Some(st) = &mut self.selftest {
                    st.stage = 8;
                    st.started = std::time::Instant::now();
                }
            }
            // (e) stage 8: the replacement must land PAUSED (paused-on-arrival
            // is correct when the user had explicitly paused); resuming plays.
            Some(8) if !self.preparing => {
                let landed_paused = self.is_paused();
                self.toggle_pause_now();
                let resume_plays = !self.is_paused();
                if landed_paused && resume_plays {
                    eprintln!("[selftest] stage8 PASS (paused replace lands paused; resume plays)");
                    eprintln!("[selftest] RESULT: PASS");
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    }

    /// Re-apply the theme when `~/.config/amc/theme.toml` changes. The poll is
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_theme(ctx);
        // Lazy MPRIS startup: after the window is up, never blocking the
        // first frame (spawn() only starts a thread).
        if self.mpris.is_none() {
            self.mpris = mpris::Mpris::spawn();
        }
        let mut focus_search = self.handle_keys(ctx);
        self.poll_messages();
        self.selftest_step();
        self.drain_mpris(ctx);

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

        // ---- auto-advance ----
        let idle = if self.player.decode_only {
            match &self.fake_play {
                // playback_position freezes while paused
                Some((_, end)) => self.playback_position() >= *end,
                None => true,
            }
        } else {
            self.player.is_idle()
        };
        if self.now.is_some() && !self.preparing && idle {
            self.advance();
        }

        // ---- window title ----
        let title = match &self.now {
            Some(t) => format!("amc — {} — {}", t.name, t.artist),
            None => "amc".to_string(),
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
                ui.label(egui::RichText::new("amc").strong());
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

        // ---- central: rows ----
        egui::CentralPanel::default().show(ctx, |ui| {
            self.player.volume(self.volume);
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

        self.publish_mpris();

        let repaint = if self.now.is_some() { 100 } else { 250 };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint));
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
        // sort control (client-side; switching does not re-query)
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.weak("sort");
            if ui
                .selectable_label(self.library_sort == LibrarySort::Added, "add date")
                .clicked()
            {
                self.library_sort = LibrarySort::Added;
            }
            if ui
                .selectable_label(self.library_sort == LibrarySort::Name, "name")
                .clicked()
            {
                self.library_sort = LibrarySort::Name;
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
            .num_columns(6)
            .spacing([10.0, 2.0])
            .min_col_width(0.0)
            .show(ui, |ui| {
                for a in albums {
                    ui.monospace(" ♫ ");
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
    CycleRepeat,
    ToggleShuffle,
    Seek(std::time::Duration),
}

impl App {
    fn player_bar(&mut self, ctx: &egui::Context) {
        let mut act: Option<BarIntent> = None;
        let mut vol = self.volume;
        egui::TopBottomPanel::bottom("player").show(ctx, |ui| {
            let paused = self.is_paused();
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
                if self.now.is_some() {
                    ui.vertical(|ui| {
                        if let Some(t) = &self.now {
                            ui.add_sized(
                                [280.0, 14.0],
                                egui::Label::new(
                                    egui::RichText::new(&t.name).strong().size(13.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                        }
                        let pos = self.playback_position();
                        let real_ms =
                            self.now.as_ref().map(|t| t.duration_ms).unwrap_or(0);
                        // decode-only simulates an 8 s track — make the bar
                        // and remaining label match the simulation
                        let total = if self.player.decode_only {
                            self.fake_play
                                .map(|(_, e)| e)
                                .unwrap_or(std::time::Duration::from_millis(real_ms))
                        } else {
                            std::time::Duration::from_millis(real_ms)
                        };
                        let remain = total.saturating_sub(pos);
                        ui.horizontal(|ui| {
                            ui.monospace(fmt_pos(pos));
                            seek_bar_ui(ui, 300.0, pos, total, self.theme.accent, &mut act);
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
                    ui.add_sized(
                        [90.0, 16.0],
                        egui::Slider::new(&mut vol, 0.0..=1.0).show_value(false),
                    );
                    self.repeat_shuffle_buttons(ui, &mut act);
                });
                ui.add_space(8.0);
            });
            ui.add_space(5.0);
        });
        if vol != self.volume {
            self.volume = vol;
            self.player.volume(vol);
            self.save_ui_state();
        }
        match act {
            Some(BarIntent::Toggle) => self.toggle_pause_now(),
            Some(BarIntent::Prev) => self.prev_track(),
            Some(BarIntent::Next) => self.skip_current(),
            Some(BarIntent::Queue) => self.show_queue = !self.show_queue,
            Some(BarIntent::CycleRepeat) => {
                self.repeat = match self.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                self.save_ui_state();
            }
            Some(BarIntent::ToggleShuffle) => self.toggle_shuffle(),
            Some(BarIntent::Seek(target)) => self.seek_to(target),
            None => {}
        }
    }

    fn repeat_shuffle_buttons(&self, ui: &mut egui::Ui, act: &mut Option<BarIntent>) {
        let repeat_txt = match self.repeat {
            Repeat::Off => "repeat",
            Repeat::All => "repeat all",
            Repeat::One => "repeat 1",
        };
        let col = if self.repeat == Repeat::Off {
            ui.visuals().weak_text_color()
        } else {
            self.theme.accent
        };
        if hand(ui.small_button(egui::RichText::new(repeat_txt).size(11.0).color(col)))
            .clicked()
        {
            *act = Some(BarIntent::CycleRepeat);
        }
        let col = if self.shuffle {
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

    fn queue_panel(&mut self, ctx: &egui::Context) {
        let mut clear = false;
        let mut shuf = false;
        egui::SidePanel::right("queue")
            .exact_width(290.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("Queue · {}", self.queue.len()));
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
                if self.queue.is_empty() {
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
                    for (i, t) in self.queue.tracks.iter().enumerate() {
                        // current entry: ▶ marker + accent; played entries dim
                        let is_cur = self.queue.pos == Some(i);
                        let played = self.queue.pos.is_some_and(|p| i < p);
                        let row = ui.horizontal(|ui| {
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
                            if fav_button(ui, self.favorites.contains(&t.id), self.theme.accent).clicked() {
                                fav = Some(t.clone());
                            }
                            let mut name = egui::RichText::new(&t.name).size(12.0);
                            if is_cur {
                                name = name.strong().color(self.theme.accent);
                            } else if played {
                                name = name.weak();
                            }
                            if cell(ui, 130.0, 16.0, name, true).clicked() {
                                jump = Some(i);
                            }
                            if hand(ui.small_button("↑")).clicked() {
                                mv = Some((i, -1));
                            }
                            if hand(ui.small_button("↓")).clicked() {
                                mv = Some((i, 1));
                            }
                            if hand(ui.small_button("×")).clicked() {
                                remove = Some(i);
                            }
                        });
                        rows.push((row.response.rect.top(), row.response.rect.bottom()));
                    }
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
                    let pos = self.report_pos_ms();
                    self.report_end(PlayEvent::END_SUSPENDED, pos);
                    self.queue.clear();
                    self.stop_playback();
                }
                if shuf {
                    self.toggle_shuffle();
                }
            });
    }

    /// Click a queue row: move the pointer there and play it (the outgoing
    /// current stays listed, its segment closes with END_REPLACED).
    fn play_queue_row(&mut self, i: usize) {
        if self.queue.play_at(i).is_none() {
            return;
        }
        let pos = self.report_pos_ms();
        self.report_end(PlayEvent::END_REPLACED, pos);
        let t = self.queue.tracks[i].clone();
        self.play_now_at(t, std::time::Duration::ZERO, PlayKind::NewSegment);
    }

    /// Queue-row ✕: the pointer-aware removal decides what happens to
    /// playback — a removed current track hands over to its successor (kept
    /// playing) or, with no successor, stops cleanly.
    fn remove_queue_row(&mut self, i: usize) {
        match self.queue.remove(i) {
            RemoveEffect::PlayNext(j) => {
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SKIPPED_FORWARDS, pos);
                self.player.skip();
                let t = self.queue.tracks[j].clone();
                self.play_now_at(t, std::time::Duration::ZERO, PlayKind::NewSegment);
            }
            RemoveEffect::Stopped => {
                let pos = self.report_pos_ms();
                self.report_end(PlayEvent::END_SUSPENDED, pos);
                self.stop_playback();
            }
            RemoveEffect::Nothing => {}
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
                            ("/ or ctrl+F", "focus search"),
                            ("ctrl+1 / 2 / 3", "search / library / favorites"),
                            ("ctrl+4", "playlists"),
                            ("♥ / ♡", "toggle favorite on a row"),
                            ("esc", "close modal / panel"),
                            ("?", "this cheatsheet"),
                            ("click progress bar", "seek"),
                        ] {
                            ui.monospace(egui::RichText::new(k).color(self.theme.accent));
                            ui.weak(v);
                            ui.end_row();
                        }
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
        // Keep the collapsed countdown ticking (playback usually repaints, but
        // not while idle, e.g. an error raised from a fired search).
        if let Some(t) = &self.error {
            ctx.request_repaint_after(t.remaining(now));
        }
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
            ctx.request_repaint_after(COPIED_FEEDBACK);
        } else if copied {
            // repaint so "Copied ✓" clears even without further input
            ctx.request_repaint_after(COPIED_FEEDBACK);
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
/// Callers must apply the decision both at application time (rodio 0.20's
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

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms % 60000) / 1000)
}

fn fmt_pos(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn launch(cfg: Config) -> anyhow::Result<()> {
    // Transparent viewport: the window background comes from the theme's
    // `bg` color at `opacity` (see `App::clear_color` / theme.rs).
    //
    // Caveat: see-through only works when a compositor is running (X11
    // compositing manager / Wayland). On a bare X server (or a headless Xvfb
    // like the selftest uses) there is nothing to blend against, so the window
    // just renders solid at full alpha. That is a silent degradation — no
    // error, no panic, and playback/UI behave identically either way.
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 420.0])
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "amc",
        native,
        // Fonts are bundled (`src/fonts.rs`, `include_bytes!`) and installed
        // by the first `App::poll_theme`: nothing is read from the filesystem,
        // so the UI renders identically on a bare system.
        Box::new(move |_cc| Ok(Box::new(App::new(cfg)))),
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

#[cfg(test)]
mod pause_state_tests {
    use super::{applied_source_pause_state, seek_scrub_events, PlayKind};

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
}
