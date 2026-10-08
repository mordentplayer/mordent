//! Dense egui UI: search-first, instant results, queue, player bar.
//! Keyboard: `/` or Ctrl+F focus search · Enter search · Space pause ·
//! N or ⏭ skip · Ctrl+Q quit.

use crate::api::{Api, SearchResults, Track};
use crate::config::Config;
use crate::player::Player;
use crate::{stream, wrapper};
use eframe::egui;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};

enum Msg {
    Results(Box<SearchResults>),
    Library(Vec<Track>, Option<u32>),
    Source(Box<crate::player::PacketSource>, Track),
    Error(String),
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Search,
    Library,
}

pub struct App {
    cfg: Config,
    player: Player,
    search: String,
    results: Option<SearchResults>,
    searching: bool,
    queue: VecDeque<Track>,
    now: Option<Track>,
    view: View,
    library: Option<Vec<Track>>,
    library_loading: bool,
    library_next: Option<u32>,
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
            queue: VecDeque::new(),
            now: None,
            view: View::Search,
            library: None,
            library_loading: false,
            library_next: None,
            volume: 0.8,
            error: None,
            rx,
            tx,
        };
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

    fn play_track(&mut self, track: Track) {
        self.now = Some(track.clone());
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let t = track.clone();
        std::thread::spawn(move || {
            let send = |src: crate::player::PacketSource| tx.send(Msg::Source(Box::new(src), t.clone())).map_err(|_| anyhow::anyhow!("channel closed"));
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
                send(crate::player::PacketSource::new(Box::new(prog))?)
            })();
            if let Err(e) = res {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        });
    }

    fn play_next(&mut self) {
        if let Some(t) = self.queue.pop_front() {
            self.play_track(t);
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
            self.play_next();
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
                Msg::Source(src, _t) => {
                    self.player.enqueue(*src);
                }
                Msg::Error(e) => {
                    self.error = Some(e);
                    self.searching = false;
                }
            }
        }
        // auto-advance queue
        if self.now.is_some() && self.player.is_idle() {
            self.play_next();
        }

        // ---- top: search ----
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
                        .desired_width(420.0)
                        .font(egui::TextStyle::Body),
                );
                if resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.spawn_search();
                    resp.request_focus();
                }
                if ui.button("⌕").clicked() {
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
                if ui.button(if self.player.sink().is_some_and(|s| s.is_paused()) { "▶" } else { "⏸" }).clicked() {
                    self.player.toggle_pause();
                }
                if ui.button("⏭").clicked() {
                    self.play_next();
                }
                match &self.now {
                    Some(t) => {
                        ui.vertical(|ui| {
                            ui.add_sized([240.0, 14.0], egui::Label::new(
                                egui::RichText::new(&t.name).strong().size(13.0)));
                            ui.add_sized([240.0, 12.0], egui::Label::new(
                                egui::RichText::new(&t.artist).weak().size(11.0)));
                        });
                        let pos = self.player.position().unwrap_or_default();
                        let total = std::time::Duration::from_millis(t.duration_ms);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(fmt_pos(total));
                            ui.add(
                                egui::ProgressBar::new(
                                    (pos.as_secs_f32() / total.as_secs_f32().max(0.001)).clamp(0.0, 1.0),
                                )
                                .desired_height(6.0)
                                .desired_width(220.0)
                                .show_percentage(),
                            );
                            ui.label(fmt_pos(pos));
                            ui.separator();
                            ui.add_sized(
                                [90.0, 16.0],
                                egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false),
                            );
                        });
                    }
                    None => {
                        ui.weak("nothing playing");
                    }
                }
                ui.add_space(8.0);
            });
            ui.add_space(6.0);
        });

        // ---- central: results ----
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
                    ui.weak("type to search · enter to play · space pauses");
                });
                return;
            }
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show_rows(ui, 24.0, rows.len(), |ui, range| {
                    for i in range {
                        let t = &rows[i];
                        let playing = self.now.as_ref().is_some_and(|n| n.id == t.id);
                        ui.horizontal(|ui| {
                            let mark = if playing { "▶" } else { &format!("{:>2}", i + 1) };
                            ui.monospace(format!("{mark:>3}"));
                            let name = egui::RichText::new(&t.name).size(13.0).strong();
                            if ui
                                .add_sized(
                                    [300.0, 18.0],
                                    egui::Label::new(name)
                                        .sense(egui::Sense::click())
                                        .halign(egui::Align::LEFT)
                                        .truncate(),
                                )
                                .clicked()
                            {
                                self.queue.push_back(t.clone());
                                if self.player.is_idle() {
                                    self.play_next();
                                }
                            }
                            ui.add_sized(
                                [170.0, 18.0],
                                egui::Label::new(egui::RichText::new(&t.artist).weak().size(12.0))
                                    .halign(egui::Align::LEFT)
                                    .truncate(),
                            );
                            ui.add_sized(
                                [210.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(if t.album.is_empty() { "" } else { &t.album })
                                        .weak()
                                        .size(11.0),
                                )
                                .halign(egui::Align::LEFT)
                                .truncate(),
                            );
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
        });

        // keep focus on search when idle
        if self.results.is_none() && !self.searching {
            ctx.memory_mut(|m| {
                if let Some(_focus) = None::<egui::Id> {}
                let _ = m;
            });
        }
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
