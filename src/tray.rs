//! Collapse-to-tray: a StatusNotifierItem via `ksni` — pure-Rust D-Bus, no
//! GTK mainloop, no extra runtime (ksni 0.3 serves everything from its own
//! thread; zbus 5 is already in the tree via eframe/accesskit).
//!
//! Shape: one thread owns the tray service. The item's properties (title,
//! tooltip, menu, icon) read the SHARED [`PbSnapshot`] at call time, so the
//! menu Play/Pause label is always fresh when a host fetches the layout; a
//! 1 Hz poll refreshes title/tooltip signals while music runs (via
//! `Handle::update`, which diffs and only emits on change).
//!
//! Menu: Show/Hide, Play/Pause (reflects state), Next, Previous, Quit.
//! Activate (left click) = toggle window visibility. Quit sends
//! [`Command::Quit`] — the supervisor flushes the report segment and exits,
//! exactly like Ctrl+Q / MPRIS Quit.
//!
//! FALLBACKS (never panics the app — every failure path degrades silently):
//! * no session bus / no StatusNotifierWatcher / no host → `spawn` returns
//!   `Err`, logged once to stderr, no tray icon; the app behaves as before
//!   (✕ still hides the window — restore via MPRIS Raise, `playerctl`, or
//!   simply `amc ui` again / relaunch).
//! * the watcher going away mid-run is ksni's `watcher_offline` — the
//!   default keeps the service registered so it re-appears when the watcher
//!   returns (e.g. waybar restart).
//!
//! Host support: waybar/Hyprland (niri, umbriel) speak SNI natively; KDE
//! Plasma natively; GNOME needs an AppIndicator extension.

use crate::supervisor::{Command, PbSnapshot};
use eframe::egui;
use ksni::blocking::TrayMethods;
use ksni::menu::MenuItem;
use ksni::{Category, Icon, Status, ToolTip, Tray};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Everything the tray reads; all Send + Sync, none of it blocks audio.
pub struct TrayItem {
    cmd: Sender<Command>,
    snap: Arc<Mutex<PbSnapshot>>,
    /// Mirrored window visibility (✕/tray/MPRIS Raise all keep it in sync).
    hidden: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl TrayItem {
    fn show_toggle(&mut self) {
        let show = self.hidden.load(Ordering::Relaxed);
        // Visible first: Focus alone does not un-hide the window.
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Visible(show));
        if show {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.hidden.store(!show, Ordering::Relaxed);
    }

    fn playing(&self) -> bool {
        let pb = self.snap.lock();
        pb.has_track && !pb.paused
    }

    /// One "now playing" line for the title/tooltip (idle text when stopped).
    fn line_for(&self) -> String {
        let pb = self.snap.lock();
        match &pb.track {
            Some(t) if pb.has_track => {
                let state = if pb.paused { "⏸ " } else { "▶ " };
                format!("{state}{} — {}", t.name, t.artist)
            }
            _ => "amc — nothing playing".to_string(),
        }
    }
}

impl Tray for TrayItem {
    fn id(&self) -> String {
        "amc".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        if self.playing() {
            Status::Active
        } else {
            Status::Passive
        }
    }

    fn title(&self) -> String {
        self.line_for()
    }

    fn icon_name(&self) -> String {
        // pixmap-only icon: hosts differ in theme-icon support, and the
        // in-binary pixmap needs no icon theme installed
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![note_icon()]
    }

    fn tool_tip(&self) -> ToolTip {
        let pb = self.snap.lock();
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "amc".into(),
            description: match &pb.track {
                Some(t) if pb.has_track => {
                    let state = if pb.paused {
                        "paused".to_string()
                    } else {
                        format!("{} / {}", fmt_pos(pb.position), fmt_pos(pb.duration))
                    };
                    format!("{} — {}\n{}", t.artist, t.name, state)
                }
                _ => "nothing playing".into(),
            },
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_toggle();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let hidden = self.hidden.load(Ordering::Relaxed);
        let (play_label, play_icon) = if self.playing() {
            ("Pause", "media-playback-pause")
        } else {
            ("Play", "media-playback-start")
        };
        use ksni::menu::StandardItem as StdItem;
        vec![
            MenuItem::Standard(StdItem {
                label: if hidden { "Show amc" } else { "Hide amc" }.into(),
                icon_name: if hidden {
                    "view-refresh".into()
                } else {
                    "window-close".into()
                },
                activate: Box::new(|t: &mut Self| t.show_toggle()),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StdItem {
                label: play_label.into(),
                icon_name: play_icon.into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd.send(Command::Toggle);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StdItem {
                label: "Next".into(),
                icon_name: "media-skip-forward".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd.send(Command::Next);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StdItem {
                label: "Previous".into(),
                icon_name: "media-skip-backward".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd.send(Command::Prev);
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StdItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    // the supervisor flushes the report segment, then exits
                    let _ = t.cmd.send(Command::Quit);
                }),
                ..Default::default()
            }),
        ]
    }
}

/// Spawn the tray thread. Best-effort by design: ANY failure (no session
/// bus, no SNI watcher, no host) logs once and leaves the app unchanged.
pub fn spawn(
    snap: Arc<Mutex<PbSnapshot>>,
    cmd: Sender<Command>,
    hidden: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let spawned = std::thread::Builder::new()
        .name("amc-tray".into())
        .spawn(move || {
            let snap_poll = snap.clone();
            let item = TrayItem {
                cmd,
                snap,
                hidden,
                ctx,
            };
            let handle = match item.spawn() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "[tray] unavailable ({e}) — running without a tray icon; \
                         ✕ still hides the window, MPRIS Raise (or a relaunch) restores it"
                    );
                    return;
                }
            };
            eprintln!("[tray] StatusNotifierItem registered (menu: show/hide, play/pause, next, prev, quit)");
            // 1 Hz refresh: title/tooltip follow the shared snapshot; update()
            // diffs server-side and only signals real changes, so an idle
            // session costs one cheap D-Bus round-trip per second.
            let mut last = String::new();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if handle.is_closed() {
                    return;
                }
                let pb = snap_poll.lock().clone();
                let line = match &pb.track {
                    Some(t) if pb.has_track => format!("{} — {}", t.name, t.artist),
                    _ => String::new(),
                };
                if line != last {
                    last = line;
                    // re-read all properties + emit changed signals
                    let _ = handle.update(|_| {});
                }
            }
        });
    if let Err(e) = spawned {
        // tray is an amenity — never panic the app over a missing thread
        eprintln!("[tray] could not spawn tray thread: {e}");
    }
}

/// A 32×32 ARGB32 (network byte order: a, r, g, b per pixel) eighth-note on
/// a transparent tile, painted at startup — no asset, no PNG decoder.
fn note_icon() -> Icon {
    const S: i32 = 32;
    // accent + a darker shade for the head/stem split
    const ACCENT: (u8, u8, u8) = (0x7a, 0xa6, 0xff);
    let mut data = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            // note head: filled circle at (12, 25), r 5.5
            let dx = x - 12;
            let dy = y - 25;
            let head = dx * dx + dy * dy <= 30;
            // stem: 3 px wide, from the head's right side up to the flag
            let stem = (19..=21).contains(&x) && (8..=25).contains(&y);
            // flag: wedge slanting down-right from the stem top
            let flag = x > 21 && x < 29 && y >= 6 && y <= 7 + (x - 21);
            if head || stem || flag {
                data.extend_from_slice(&[255, ACCENT.0, ACCENT.1, ACCENT.2]);
            } else {
                data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon {
        width: S,
        height: S,
        data,
    }
}

fn fmt_pos(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}
