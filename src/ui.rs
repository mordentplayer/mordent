//! Dense egui UI: search / albums / library tabs, album detail modal,
//! explicit queue, upgraded player bar.
//!
//! Playback model: clicking a row plays it NOW (replacing current; the stale
//! async source is dropped, which also cancels its download). `+` appends to
//! the queue. Album rows open a detail modal; album `+` enqueues the whole
//! album (tracks fetched on click).
//!
//! In decode-only mode (no audio device) a fake playback clock simulates the
//! running track so queue/auto-advance behaves like a real player.

use crate::api::{AlbumHit, Api, SearchResults, Track};
use crate::config::Config;
use crate::player::{PacketSource, Player};
use crate::theme::Theme;
use crate::{stream, wrapper};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};

enum Msg {
    Results(Box<SearchResults>),
    Library(Vec<Track>, Option<u32>),
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
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Search,
    Albums,
    Library,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Repeat {
    Off,
    All,
    One,
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

/// Clicks gathered during widget rendering, applied after borrow ends.
enum CentralAction {
    Play(Track),
    Enqueue(Track),
    OpenAlbum(AlbumHit),
    QueueAlbum(AlbumHit),
}

struct SelftestState {
    stage: u8,
    expect: [String; 2],
    started: std::time::Instant,
}

/// UI settings persisted to ~/.config/amc/state.toml (atomic write).
#[derive(Serialize, Deserialize, Default)]
struct UiState {
    volume: Option<f32>,
    last_view: Option<String>,
    repeat: Option<String>,
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
    library: Option<Vec<Track>>,
    library_loading: bool,
    library_next: Option<u32>,
    queue: VecDeque<Track>,
    now: Option<Track>,
    /// Bumped on every play request; async sources carry the generation so
    /// stale ones are dropped instead of appended.
    generation: u64,
    /// A worker is building the source for the current generation.
    preparing: bool,
    show_queue: bool,
    /// decode-only (no audio device): simulated playback window
    /// (clock start, remaining duration).
    fake_play: Option<(std::time::Instant, std::time::Duration)>,
    /// decode-only pause point (frozen clock).
    fake_pause_at: Option<std::time::Instant>,
    /// Sink-mode playback starts at this offset (pre-seek samples were
    /// discarded by the worker).
    play_offset: std::time::Duration,
    /// Re-pause the sink when a seek restart lands.
    resume_paused: bool,
    /// Tracks finished or replaced (for ⏮), newest last.
    history: VecDeque<Track>,
    repeat: Repeat,
    shuffle: bool,
    /// Queue order before shuffle was enabled.
    shuffle_saved: Option<VecDeque<Track>>,
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
    error: Option<(String, std::time::Instant)>,
    last_title: String,
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
        let (tx, rx) = channel();
        let mut app = Self {
            cfg,
            player,
            search: String::new(),
            last_term: String::new(),
            results: None,
            searching: false,
            view: View::Search,
            library: None,
            library_loading: false,
            library_next: None,
            queue: VecDeque::new(),
            now: None,
            generation: 0,
            preparing: false,
            show_queue: false,
            fake_play: None,
            fake_pause_at: None,
            play_offset: std::time::Duration::ZERO,
            resume_paused: false,
            history: VecDeque::new(),
            repeat: match state.repeat.as_deref() {
                Some("all") => Repeat::All,
                Some("one") => Repeat::One,
                _ => Repeat::Off,
            },
            shuffle: false,
            shuffle_saved: None,
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
            // invalid theme keys are surfaced once through the toast
            error: theme_warns
                .first()
                .map(|w| (w.clone(), std::time::Instant::now())),
            last_title: "amc".into(),
            rx,
            tx,
        };
        match state.last_view.as_deref() {
            Some("albums") => app.view = View::Albums,
            Some("library") => app.switch_view(View::Library),
            _ => {}
        }
        if app.selftest.is_some() {
            eprintln!("[selftest] armed");
        }
        // headless verification hooks: AMC_DEMO_TERM=<query> auto-searches,
        // AMC_DEMO_VIEW=library|albums opens that view
        if let Ok(term) = std::env::var("AMC_DEMO_TERM") {
            app.search = term;
            app.spawn_search();
        }
        match std::env::var("AMC_DEMO_VIEW").as_deref() {
            Ok("library") => app.switch_view(View::Library),
            Ok("albums") => app.view = View::Albums,
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
            match Api::new(cfg).and_then(|api| api.library_songs(100, offset)) {
                Ok((songs, next)) => {
                    let _ = tx.send(Msg::Library(songs, next));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn switch_view(&mut self, v: View) {
        self.view = v;
        self.save_ui_state();
        if v == View::Library && self.library.is_none() && !self.library_loading {
            self.spawn_library(0);
        }
    }

    fn fail(&mut self, e: String) {
        self.error = Some((e, std::time::Instant::now()));
    }

    /// Play a track immediately at `offset`: cancels whatever is
    /// current/preparing by bumping the generation.
    fn play_now_at(&mut self, track: Track, offset: std::time::Duration, record: bool) {
        eprintln!("[play] {} — {}", track.name, track.artist);
        if record {
            if let Some(old) = self.now.take() {
                self.push_history(old);
            }
        } else {
            self.now = None;
        }
        self.preparing = true;
        self.now = Some(track.clone());
        self.play_offset = offset;
        self.fake_play = None;
        self.fake_pause_at = None;
        self.resume_paused = self.player.sink().is_some_and(|s| s.is_paused());
        self.player.sink().map(|s| s.clear());
        self.generation += 1;
        let generation = self.generation;
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let t = track;
        let _ = std::thread::Builder::new().name("play-worker".into()).spawn(move || {
            let res = (|| -> anyhow::Result<()> {
                let w = wrapper::Wrapper::new(&cfg.wrapper_url);
                let logged_in = w.regions().map(|r| !r.is_empty()).unwrap_or(false);
                let agent = ureq::AgentBuilder::new().build();
                let cache = crate::config::cache_path(&cfg, &format!("tracks/{}.m4a", t.id))?;
                let prog = if logged_in {
                    let plan = stream::plan_for_track(&w, &agent, &t.id)?;
                    let resolver = stream::KeyResolver::new(&w, &t.id);
                    stream::ProgressiveSource::start_fairplay(plan, &resolver, cache, agent)?
                } else {
                    let url = Api::new(cfg.clone())?.preview_url(&t.id)?;
                    stream::ProgressiveSource::start_plain(&url, cache, agent)?
                };
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
        self.play_now_at(track, std::time::Duration::ZERO, true);
    }

    fn push_history(&mut self, t: Track) {
        self.history.push_back(t);
        while self.history.len() > 100 {
            self.history.pop_front();
        }
    }

    fn enqueue(&mut self, track: Track) {
        self.queue.push_back(track);
    }

    /// Play album tracks: first plays now, the rest are appended to the
    /// queue in order.
    fn play_album_tracks(&mut self, tracks: Vec<Track>) {
        let mut it = tracks.into_iter();
        if let Some(first) = it.next() {
            self.queue.extend(it);
            self.play_now(first);
        }
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

    /// Swap queue row `i` with its neighbour (`delta` -1 = up, +1 = down).
    fn queue_move(&mut self, i: usize, delta: isize) {
        let Some(j) = (i as isize).checked_add(delta) else {
            return;
        };
        if j < 0 || j as usize >= self.queue.len() {
            return;
        }
        self.queue.swap(i, j as usize);
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.shuffle_saved = Some(self.queue.drain(..).collect());
            let mut v: Vec<Track> = self
                .shuffle_saved
                .as_ref()
                .expect("just saved")
                .iter()
                .cloned()
                .collect();
            let mut seed = std::time::Instant::now().elapsed().as_nanos() as u64 | 1;
            let mut rnd = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            for i in (1..v.len()).rev() {
                let j = (rnd() % (i as u64 + 1)) as usize;
                v.swap(i, j);
            }
            self.queue = v.into();
        } else if let Some(saved) = self.shuffle_saved.take() {
            // restore the pre-shuffle order minus anything since removed
            let keep: HashSet<String> = self.queue.iter().map(|t| t.id.clone()).collect();
            self.queue = saved.into_iter().filter(|t| keep.contains(&t.id)).collect();
        }
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
    }

    /// Skip to the next queued track (manual).
    fn skip_current(&mut self) {
        if self.now.is_none() {
            return;
        }
        self.player.skip();
        self.advance(true);
    }

    /// Advance to the next track; `manual` = user-initiated (⏭ / N), which
    /// ignores repeat-one/repeat-all wrapping.
    fn advance(&mut self, manual: bool) {
        let Some(finished) = self.now.take() else {
            return;
        };
        if !manual && self.repeat == Repeat::One {
            self.push_history(finished.clone());
            self.play_now_at(finished, std::time::Duration::ZERO, false);
            return;
        }
        if let Some(next) = self.queue.pop_front() {
            self.push_history(finished);
            self.play_now_at(next, std::time::Duration::ZERO, true);
            return;
        }
        if !manual && self.repeat == Repeat::All {
            // replay the session history in order
            let hist: Vec<Track> = self.history.drain(..).collect();
            if let Some(first) = hist.first() {
                let first = first.clone();
                self.queue.extend(hist.into_iter().skip(1));
                self.play_now_at(first, std::time::Duration::ZERO, true);
                return;
            }
        }
        // stop
        self.push_history(finished);
        self.generation += 1; // invalidate any in-flight source
        self.preparing = false;
        self.fake_play = None;
        self.fake_pause_at = None;
        self.play_offset = std::time::Duration::ZERO;
    }

    /// ⏮: restart current if >3s elapsed, else previous track.
    fn prev_track(&mut self) {
        if self.now.is_some() && self.playback_position().as_secs_f64() > 3.0 {
            self.seek_to(std::time::Duration::ZERO);
        } else if let Some(t) = self.history.pop_back() {
            self.play_now_at(t, std::time::Duration::ZERO, false);
        } else if self.now.is_some() {
            self.seek_to(std::time::Duration::ZERO);
        }
    }

    /// Click-to-seek / ±5s. Sink mode restarts the stream at the offset;
    /// decode-only shifts the fake clock.
    fn seek_to(&mut self, target: std::time::Duration) {
        let Some(t) = self.now.as_ref().map(|t| t.duration_ms) else {
            return;
        };
        let total = std::time::Duration::from_millis(t);
        let target = target.min(total);
        if self.player.decode_only {
            let now = std::time::Instant::now();
            let end = std::time::Duration::from_millis(t.min(8_000));
            self.fake_play = Some((now.checked_sub(target).unwrap_or(now), end));
            if self.fake_pause_at.is_some() {
                self.fake_pause_at = Some(now);
            }
            self.play_offset = target;
        } else if self.now.is_some() {
            self.play_now_at(
                self.now.clone().expect("checked above"),
                target,
                false, // don't push the restarted track onto history
            );
        }
    }

    fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.player.volume(self.volume);
    }

    /// Pause without toggling (sleep timer).
    fn pause_now(&mut self) {
        if self.player.decode_only {
            if self.fake_play.is_some() && self.fake_pause_at.is_none() {
                self.fake_pause_at = Some(std::time::Instant::now());
            }
        } else {
            self.player.sink().map(|s| if !s.is_paused() { s.pause(); });
        }
    }

    fn save_ui_state(&self) {
        save_state(&UiState {
                volume: Some(self.volume),
                last_view: Some(
                    match self.view {
                        View::Search => "search",
                        View::Albums => "albums",
                        View::Library => "library",
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
            },
        );
    }

    // ---- keyboard ----

    fn handle_keys(&mut self, ctx: &egui::Context) -> bool {
        let mut focus_search = false;
        let typing = ctx.wants_keyboard_input();
        ctx.input(|i| {
            // Esc works even while a text field has focus
            if i.key_pressed(egui::Key::Escape) {
                if self.cheatsheet {
                    self.cheatsheet = false;
                } else if self.album_modal.is_some() {
                    self.close_album_modal();
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
                    self.switch_view(View::Albums);
                }
                if i.key_pressed(egui::Key::Num3) {
                    self.switch_view(View::Library);
                }
                if i.key_pressed(egui::Key::Q) {
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
                    self.results = Some(*r);
                    self.searching = false;
                }
                Msg::Library(songs, next) => {
                    self.library.get_or_insert_with(Vec::new).extend(songs);
                    self.library_next = next;
                    self.library_loading = false;
                }
                Msg::Source(g, src) => {
                    if g == self.generation {
                        self.player.enqueue(*src);
                        self.preparing = false;
                        if self.resume_paused {
                            self.player.sink().map(|s| s.pause());
                            self.resume_paused = false;
                        }
                        // start the decode-only fake clock (keep a seek target);
                        // tuple = (clock start, elapsed-at-end)
                        if self.player.decode_only && self.fake_play.is_none() {
                            if let Some(t) = &self.now {
                                let end =
                                    std::time::Duration::from_millis(t.duration_ms.min(8_000));
                                let start = std::time::Instant::now()
                                    .checked_sub(self.play_offset)
                                    .unwrap_or_else(std::time::Instant::now);
                                self.fake_play = Some((start, end));
                            }
                        }
                    } // stale: dropping src also cancels its download
                }
                Msg::AlbumTracks { id, tracks, .. } => {
                    let modal_want =
                        matches!(&self.album_job, Some(AlbumJob::Modal { id: w }) if w.as_str() == id.as_str());
                    let queue_want =
                        matches!(&self.album_job, Some(AlbumJob::Queue { id: w }) if w.as_str() == id.as_str());
                    if modal_want {
                        self.album_job = None;
                        if let Some(m) = &mut self.album_modal {
                            if m.album.id == id {
                                m.tracks = Some(tracks);
                            }
                        }
                    } else if queue_want {
                        self.album_job = None;
                        self.album_busy = None;
                        self.queue.extend(tracks);
                    }
                }
                Msg::Error(e) => {
                    self.fail(e);
                    self.searching = false;
                    self.preparing = false;
                    self.album_job = None;
                    self.album_busy = None;
                    self.library_loading = false;
                }
            }
        }
    }

    // ---- deterministic selftest (AMC_SELFTEST=1) ----

    fn selftest_step(&mut self) {
        if self
            .selftest
            .as_ref()
            .is_some_and(|st| st.stage == 0 && self.results.as_ref().is_some_and(|r| r.tracks.len() >= 3))
        {
            let (n0, _n1, n2) = {
                let tracks = &self.results.as_ref().expect("checked").tracks;
                (
                    tracks[0].name.clone(),
                    tracks[1].name.clone(),
                    tracks[2].name.clone(),
                )
            };
            let (_id0, id1) = {
                let tracks = &self.results.as_ref().expect("checked").tracks;
                (tracks[0].id.clone(), tracks[1].id.clone())
            };
            self.queue = self
                .results
                .as_ref()
                .expect("checked")
                .tracks
                .iter()
                .take(2)
                .cloned()
                .collect();
            // reorder assertion: same method the ↑↓ buttons call
            self.queue_move(1, -1);
            let ok_up = self.queue.front().map(|t| t.id.as_str()) == Some(id1.as_str());
            self.queue_move(0, 1);
            let ok_down = self.queue.get(1).map(|t| t.id.as_str()) == Some(id1.as_str());
            // shuffle toggle must restore the original order deterministically
            let before: Vec<String> = self.queue.iter().map(|t| t.id.clone()).collect();
            self.toggle_shuffle();
            self.toggle_shuffle();
            let after: Vec<String> = self.queue.iter().map(|t| t.id.clone()).collect();
            let ok_shuffle = after == before;
            if !ok_up || !ok_down || !ok_shuffle {
                eprintln!(
                    "[selftest] stage0 FAIL (reorder up={ok_up} down={ok_down} shuffle_restore={ok_shuffle})"
                );
                eprintln!("[selftest] RESULT: FAIL");
                std::process::exit(1);
            }
            eprintln!("[selftest] stage0 reorder+shuffle PASS");
            self.play_now(
                self.results
                    .as_ref()
                    .expect("checked")
                    .tracks
                    .get(2)
                    .expect("len>=3")
                    .clone(),
            );
            if let Some(st) = &mut self.selftest {
                st.expect = [n2, n0];
                st.stage = 1;
                st.started = std::time::Instant::now();
            }
        }
        if let Some(st) = &mut self.selftest {
            let elapsed = st.started.elapsed();
            let now_name = self.now.as_ref().map(|t| t.name.clone()).unwrap_or_default();
            let matches = |want: &str| now_name == *want;
            // stage 1: the play_now target must eventually play (any time ≤ 15 s)
            if st.stage == 1 {
                if matches(&st.expect[0]) {
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
            // stage 2: auto-advance must move to the first queued track
            if st.stage == 2 {
                if matches(&st.expect[1]) && self.queue.len() == 1 {
                    eprintln!("[selftest] stage2 PASS (now={now_name:?}, queue={})", self.queue.len());
                    eprintln!("[selftest] RESULT: PASS");
                    std::process::exit(0);
                } else if elapsed > std::time::Duration::from_secs(15) {
                    eprintln!(
                        "[selftest] stage2 FAIL (now={now_name:?}, want {:?}, queue={})",
                        st.expect[1],
                        self.queue.len()
                    );
                    eprintln!("[selftest] RESULT: FAIL");
                    std::process::exit(1);
                }
            }
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
        if let Some(t) = self.theme_poll {
            if t.elapsed() < std::time::Duration::from_millis(1000) {
                return;
            }
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
            self.error = Some((w.clone(), std::time::Instant::now()));
        }
        self.theme = theme;
        self.theme.apply(ctx);
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_theme(ctx);
        let mut focus_search = self.handle_keys(ctx);
        self.poll_messages();
        self.selftest_step();

        // ---- sleep timer ----
        if let Some((t0, d)) = self.sleep_at {
            if t0.elapsed() >= d {
                self.sleep_at = None;
                self.pause_now();
                self.fail("sleep timer expired — playback paused".into());
            }
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
            self.advance(false);
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
                    .selectable_label(self.view == View::Albums, "Albums")
                    .clicked()
                {
                    self.switch_view(View::Albums);
                }
                if ui
                    .selectable_label(self.view == View::Library, "Library")
                    .clicked()
                {
                    self.switch_view(View::Library);
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
                View::Albums => self.albums_central(ui, &mut act),
                View::Library => self.library_central(ui, &mut act),
            }
            match act {
                Some(CentralAction::Play(t)) => self.play_now(t),
                Some(CentralAction::Enqueue(t)) => self.enqueue(t),
                Some(CentralAction::OpenAlbum(a)) => self.open_album(a),
                Some(CentralAction::QueueAlbum(a)) => self.queue_album(a),
                None => {}
            }
        });

        // ---- overlays ----
        if self.album_modal.is_some() {
            self.album_modal_ui(ctx);
        }
        if self.cheatsheet {
            self.cheatsheet_ui(ctx);
        }
        self.toast_ui(ctx);

        let repaint = if self.now.is_some() { 100 } else { 250 };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint));
    }

    /// Transparent-viewport clear color: the theme background at the theme's
    /// opacity. See `theme.rs` for the compositor caveat.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.clear_color()
    }
}

// ---- central views (all gather clicks into `act`; applied by caller) ----

impl App {
    fn search_central(&self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        match self.results.as_ref() {
            None => self.empty_state(
                ui,
                if self.searching {
                    "searching…".to_string()
                } else {
                    "type to search · enter plays · + queues · Q queue · ? shortcuts".to_string()
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
        }
    }

    fn library_central(&mut self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
        let loading = self.library_loading;
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
                self.track_grid(ui, v, act);
                if self.library_next.is_some() && !loading {
                    ui.add_space(4.0);
                    if ui.small_button("load more").clicked() {
                        if let Some(next) = self.library_next {
                            self.spawn_library(next);
                        }
                    }
                }
            }
        }
    }

    fn albums_central(&self, ui: &mut egui::Ui, act: &mut Option<CentralAction>) {
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
                "albums from your last search appear here — run a search first".to_string()
            };
            self.empty_state(ui, msg, self.searching);
            return;
        }
        ui.add_space(2.0);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("albums")
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
                        ui.weak(&a.release_date);
                        if self.album_busy.as_deref() == Some(a.id.as_str()) {
                            ui.weak("adding…");
                        } else if hand(ui.small_button("+")).clicked() {
                            *act = Some(CentralAction::QueueAlbum(a.clone()));
                        }
                        ui.end_row();
                    }
                });
        });
    }

    fn track_grid(&self, ui: &mut egui::Ui, rows: &[Track], act: &mut Option<CentralAction>) {
        ui.add_space(2.0);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
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
        });
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
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for (i, t) in self.queue.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.monospace(format!("{:>2}", i + 1));
                            ui.add_sized(
                                [130.0, 16.0],
                                egui::Label::new(egui::RichText::new(&t.name).size(12.0))
                                    .halign(egui::Align::LEFT)
                                    .truncate(),
                            );
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
                    }
                });
                if let Some((i, d)) = mv {
                    self.queue_move(i, d);
                }
                if let Some(i) = remove {
                    self.queue.remove(i);
                }
                if clear {
                    self.queue.clear();
                }
                if shuf {
                    self.toggle_shuffle();
                }
            });
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
            self.queue.extend(tracks);
        }
        match act {
            Some(CentralAction::Play(t)) => self.play_now(t),
            Some(CentralAction::Enqueue(t)) => self.enqueue(t),
            _ => {}
        }
        if close {
            self.close_album_modal();
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
                            ("ctrl+1 / 2 / 3", "search / albums / library"),
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
        let expired = matches!(&self.error, Some((_, at)) if at.elapsed() > std::time::Duration::from_secs(8));
        if expired {
            self.error = None;
        }
        if self.error.is_none() {
            return;
        }
        let mut dismiss = false;
        egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, [-14.0, -60.0])
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(0x3a, 0x1a, 0x1e))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xa0, 0x40, 0x50)))
                    .inner_margin(8.0)
                    .corner_radius(6.0_f32)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some((msg, _)) = &self.error {
                                ui.add_sized(
                                    [420.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new(msg)
                                            .size(12.0)
                                            .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
                                    )
                                    .truncate(),
                                );
                            }
                            if hand(ui.small_button("×")).clicked() {
                                dismiss = true;
                            }
                        });
                    });
            });
        if dismiss {
            self.error = None;
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
    if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let f = ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0) as f64;
            *act = Some(BarIntent::Seek(std::time::Duration::from_secs_f64(
                f * total.as_secs_f64(),
            )));
        }
    }
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
        Box::new(move |cc| {
            // egui's compiled-in fonts lack arrows (↑ ↓); add a system
            // fallback once at startup when available.
            let mut fonts = egui::FontDefinitions::default();
            if let Ok(data) =
                std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")
            {
                fonts.font_data.insert(
                    "dejavu".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(data)),
                );
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push("dejavu".into());
                }
                cc.egui_ctx.set_fonts(fonts);
            }
            Ok(Box::new(App::new(cfg)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
