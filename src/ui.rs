//! Dense egui UI: search-first, albums, explicit queue, player bar.
//! Keyboard: `/` or Ctrl+F focus search · Enter search · Space pause ·
//! N or ⏭ skip · Ctrl+Q quit.
//!
//! Playback model: clicking a row plays it NOW (replacing current; the stale
//! async source is dropped, which also cancels its download). `+` appends to
//! the queue. Album rows queue their tracks and start the first one.

use crate::api::{Api, SearchResults, Track};
use crate::config::Config;
use crate::player::{PacketSource, Player};
use crate::{stream, wrapper};
use eframe::egui;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver, Sender};

enum Msg {
    Results(Box<SearchResults>),
    Library(Vec<Track>, Option<u32>),
    /// Fully prepared decoder for the given playback generation.
    Source(u64, Box<PacketSource>),
    /// Tracks of a clicked album, in order.
    AlbumTracks(Vec<Track>),
    Error(String),
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Search,
    Library,
}

struct SelftestState {
    stage: u8,
    expect: [String; 3],
    started: std::time::Instant,
}

pub struct App {
    cfg: Config,
    player: Player,
    search: String,
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
    /// decode-only (no audio device): simulated playback window so the
    /// queue/auto-advance state machine behaves like a real player
    fake_play: Option<(std::time::Instant, std::time::Duration)>,
    /// AMC_SELFTEST=1: drive play_now/queue/auto-advance deterministically
    /// and print SELFTEST PASS/FAIL (headless CI aid).
    selftest: Option<SelftestState>,
    volume: f32,
    error: Option<String>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let player = Player::new();
        player.volume(0.8);
        let (tx, rx) = channel();
        let mut app = Self {
            cfg,
            player,
            search: String::new(),
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
            selftest: std::env::var("AMC_SELFTEST").is_ok().then(|| SelftestState {
                stage: 0,
                expect: Default::default(),
                started: std::time::Instant::now(),
            }),
            volume: 0.8,
            error: None,
            rx,
            tx,
        };
        if app.selftest.is_some() {
            eprintln!("[selftest] armed");
        }
        // headless verification hook: AMC_DEMO_TERM=<query> auto-searches,
        // AMC_DEMO_VIEW=library opens the library view
        if let Ok(term) = std::env::var("AMC_DEMO_TERM") {
            app.search = term;
            app.spawn_search();
        }
        if std::env::var("AMC_DEMO_VIEW").as_deref() == Ok("library") {
            app.switch_view(View::Library);
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

    fn switch_view(&mut self, v: View) {
        self.view = v;
        if v == View::Library && self.library.is_none() && !self.library_loading {
            self.library_loading = true;
            let tx = self.tx.clone();
            let cfg = self.cfg.clone();
            std::thread::spawn(move || {
                match Api::new(cfg).and_then(|api| api.library_songs(100, 0)) {
                    Ok((songs, next)) => {
                        let _ = tx.send(Msg::Library(songs, next));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("{e:#}")));
                    }
                }
            });
        }
    }

    /// Play a track immediately: cancels whatever is current/preparing.
    fn play_now(&mut self, track: Track) {
        eprintln!("[play] {} — {}", track.name, track.artist);
        self.preparing = true;
        self.now = Some(track.clone());
        self.fake_play = None;
        self.player.sink().map(|s| s.clear());
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let generation = self.generation;
        let t = track;
        std::thread::Builder::new().name("play-worker".into()).spawn(move || {
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
                let src = PacketSource::with_cancel(Box::new(prog), cancel)?;
                tx.send(Msg::Source(generation, Box::new(src)))
                    .map_err(|_| anyhow::anyhow!("channel closed"))
            })();
            if let Err(e) = res {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        });
    }

    fn enqueue(&mut self, track: Track) {
        self.queue.push_back(track);
    }

    fn play_album(&mut self, album_id: String, name: String) {
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let res = Api::new(cfg).and_then(|api| api.album_tracks_any(&album_id));
            match res {
                Ok(tracks) if tracks.is_empty() => {
                    let _ = tx.send(Msg::Error(format!("album {name}: no tracks")));
                }
                Ok(tracks) => {
                    let _ = tx.send(Msg::AlbumTracks(tracks));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn play_next(&mut self) {
        if let Some(t) = self.queue.pop_front() {
            self.play_now(t);
        } else {
            self.now = None;
            self.generation += 1; // invalidate any in-flight source
            self.preparing = false;
            self.fake_play = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut focus_search = false;

        // ---- global keyboard ----
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) && !ctx.wants_keyboard_input() {
            self.player.toggle_pause();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::N)) && !ctx.wants_keyboard_input() {
            self.player.skip();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Q)) && !ctx.wants_keyboard_input() {
            self.show_queue = !self.show_queue;
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Q)) {
            std::process::exit(0);
        }

        // ---- poll worker messages ----
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
                    } // stale: dropping src also cancels its download
                }
                Msg::AlbumTracks(tracks) => {
                    let mut it = tracks.into_iter();
                    if let Some(first) = it.next() {
                        self.queue = it.collect();
                        self.play_now(first);
                    }
                }
                Msg::Error(e) => {
                    self.error = Some(e);
                    self.searching = false;
                    self.preparing = false;
                }
            }
        }
        // ---- deterministic selftest (AMC_SELFTEST=1) ----
        if self
            .selftest
            .as_ref()
            .is_some_and(|st| st.stage == 0 && self.results.as_ref().is_some_and(|r| r.tracks.len() >= 3))
        {
            let tracks = &self.results.as_ref().unwrap().tracks;
            let expect = [
                tracks[2].name.clone(),
                tracks[0].name.clone(),
                tracks[1].name.clone(),
            ];
            self.queue = [tracks[0].clone(), tracks[1].clone()].into();
            self.play_now(tracks[2].clone());
            if let Some(st) = &mut self.selftest {
                st.expect = expect;
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

        // ---- auto-advance ----
        let idle = if self.player.decode_only {
            match &self.fake_play {
                Some((t0, d)) => t0.elapsed() >= *d,
                None => true,
            }
        } else {
            self.player.is_idle()
        };
        if self.now.is_some() && !self.preparing && idle {
            self.play_next();
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
                    self.view = View::Search;
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
                        .desired_width(380.0)
                        .font(egui::TextStyle::Body),
                );
                if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.view = View::Search;
                    self.spawn_search();
                    resp.request_focus();
                }
                if ui.button("⌕").clicked() {
                    self.view = View::Search;
                    self.spawn_search();
                }
                if ui.small_button("/").clicked() {
                    focus_search = true;
                }
                if self.searching {
                    ui.spinner();
                }
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::RED, egui::RichText::new(e).small());
                }
            });
            ui.add_space(2.0);
        });

        // ---- bottom: player bar ----
        egui::TopBottomPanel::bottom("player").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                if ui
                    .button(
                        if self.player.sink().is_some_and(|s| s.is_paused()) {
                            "▶"
                        } else {
                            "⏸"
                        },
                    )
                    .clicked()
                {
                    self.player.toggle_pause();
                }
                if ui.button("⏭").clicked() {
                    self.player.skip();
                }
                match &self.now {
                    Some(t) => {
                        ui.vertical(|ui| {
                            ui.add_sized(
                                [240.0, 14.0],
                                egui::Label::new(
                                    egui::RichText::new(&t.name).strong().size(13.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                            ui.add_sized(
                                [240.0, 12.0],
                                egui::Label::new(
                                    egui::RichText::new(&t.artist).weak().size(11.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                        });
                        let pos = self.player.position().unwrap_or_default();
                        let total = std::time::Duration::from_millis(t.duration_ms);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(fmt_pos(total));
                            ui.add(
                                egui::ProgressBar::new(
                                    (pos.as_secs_f32()
                                        / total.as_secs_f32().max(0.001))
                                    .clamp(0.0, 1.0),
                                )
                                .desired_height(6.0)
                                .desired_width(200.0)
                                .show_percentage(),
                            );
                            ui.label(fmt_pos(pos));
                            ui.separator();
                            ui.add_sized(
                                [90.0, 16.0],
                                egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false),
                            );
                            if ui.button("queue").clicked() {
                                self.show_queue = !self.show_queue;
                            }
                        });
                    }
                    None => {
                        ui.weak("nothing playing");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("queue").clicked() {
                                self.show_queue = !self.show_queue;
                            }
                        });
                    }
                }
                ui.add_space(8.0);
            });
            ui.add_space(6.0);
        });

        // ---- right: queue panel ----
        if self.show_queue {
            egui::SidePanel::right("queue")
                .exact_width(250.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Queue");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("clear").clicked() {
                                self.queue.clear();
                            }
                        });
                    });
                    ui.separator();
                    if self.queue.is_empty() {
                        ui.weak("empty — add with +");
                    }
                    let mut remove: Option<usize> = None;
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        for (i, t) in self.queue.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [168.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new(&t.name).size(12.0),
                                    )
                                    .halign(egui::Align::LEFT)
                                    .truncate(),
                                );
                                if ui.small_button("✕").clicked() {
                                    remove = Some(i);
                                }
                            });
                        }
                    });
                    if let Some(i) = remove {
                        self.queue.remove(i);
                    }
                });
        }

        // ---- central: rows ----
        egui::CentralPanel::default().show(ctx, |ui| {
            if focus_search {
                ctx.memory_mut(|m| m.request_focus(egui::Id::new("search")));
            }
            self.player.volume(self.volume);

            let rows: Vec<Track> = match self.view {
                View::Search => self.results.clone().map(|r| r.tracks).unwrap_or_default(),
                View::Library => self.library.clone().unwrap_or_default(),
            };
            if rows.is_empty() && self.view == View::Library && self.library_loading {
                ui.spinner();
                return;
            }
            if rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("type to search · enter to play · + queues · space pauses");
                });
                return;
            }
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show_rows(ui, 24.0, rows.len(), |ui, range| {
                    for i in range {
                        let t = rows[i].clone();
                        let playing = self.now.as_ref().is_some_and(|n| n.id == t.id);
                        ui.horizontal(|ui| {
                            let mark = if playing {
                                "▶".to_string()
                            } else {
                                format!("{:>2}", i + 1)
                            };
                            ui.monospace(format!("{mark:>3}"));
                            let name = egui::RichText::new(&t.name).size(13.0).strong();
                            if ui
                                .add_sized(
                                    [280.0, 18.0],
                                    egui::Label::new(name)
                                        .sense(egui::Sense::click())
                                        .halign(egui::Align::LEFT)
                                        .truncate(),
                                )
                                .clicked()
                            {
                                self.play_now(t.clone());
                            }
                            ui.add_sized(
                                [160.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(&t.artist).weak().size(12.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                            ui.add_sized(
                                [190.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(if t.album.is_empty() {
                                        ""
                                    } else {
                                        &t.album
                                    })
                                    .weak()
                                    .size(11.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                            if ui.small_button("+").clicked() {
                                self.enqueue(t.clone());
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(fmt_ms(t.duration_ms));
                                if t.lossless {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(0x6a, 0xd1, 0xa8),
                                        egui::RichText::new("lossless").size(10.0),
                                    );
                                }
                            });
                        });
                        ui.separator();
                    }
                });

            // ---- albums (search view only) ----
            if self.view == View::Search {
                let albums = self.results.as_ref().map(|r| r.albums.clone()).unwrap_or_default();
                if !albums.is_empty() {
                    ui.add_space(8.0);
                    ui.strong("ALBUMS  (click to play)");
                    ui.separator();
                    for a in albums.iter().take(12) {
                        ui.horizontal(|ui| {
                            ui.monospace("  ♫ ");
                            let label = egui::RichText::new(&a.name).size(13.0);
                            if ui
                                .add_sized(
                                    [300.0, 18.0],
                                    egui::Label::new(label)
                                        .sense(egui::Sense::click())
                                        .halign(egui::Align::LEFT)
                                        .truncate(),
                                )
                                .clicked()
                            {
                                self.play_album(a.id.clone(), a.name.clone());
                            }
                            ui.add_sized(
                                [170.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(&a.artist).weak().size(12.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.weak(format!("{} tracks", a.track_count));
                            });
                        });
                        ui.separator();
                    }
                }
            }
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(200));
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
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "amc",
        native,
        Box::new(move |_cc| Ok(Box::new(App::new(cfg)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
