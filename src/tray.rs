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
//! Status is **always `Active`**: hosts (waybar included) hide `Passive`
//! items, so an idle `Passive` item meant no icon at all until the first
//! track — the reported bug. The app is alive from launch; that is what the
//! icon signals. Play/pause state lives in the tooltip and the menu label.
//!
//! Menu: Show/Hide, Play/Pause (reflects state), Next, Previous, Quit.
//! Activate (left click) toggles window visibility. Quit sends
//! [`Command::Quit`] — the supervisor flushes the report segment and exits,
//! exactly like Ctrl+Q / MPRIS Quit.
//!
//! SHOW/HIDE (`crate::session`, `crate::wm`): one call,
//! [`Session::tray_toggle`], works on every backend. X11 unmaps/maps the live
//! toplevel. Wayland cannot unmap at all (winit 0.30.13's `Window::set_visible`
//! is a no-op), so there Hide means the same thing as ✕ — the app's close
//! policy — and the window is instead destroyed and re-created around the
//! parked main thread, with playback untouched. The item is therefore enabled
//! everywhere.
//!
//! FALLBACKS (never panics the app — every failure path degrades silently):
//! * no session bus / no StatusNotifierWatcher / no host → `spawn` returns
//!   `Err`, logged once to stderr, no tray icon; the window-close policy in
//!   `ui::update` still applies (quit when nothing has played; hide once a
//!   track has — restore via MPRIS Raise, `playerctl`, or a relaunch).
//! * the watcher going away mid-run is ksni's `watcher_offline` — the
//!   default keeps the service registered so it re-appears when the watcher
//!   returns (e.g. waybar restart).
//!
//! Host support: waybar/Hyprland (niri, umbriel) speak SNI natively; KDE
//! Plasma natively; GNOME needs an AppIndicator extension.

use crate::session::Session;
use crate::supervisor::{Command, PbSnapshot};
use ksni::blocking::TrayMethods;
use ksni::menu::MenuItem;
use ksni::{Category, Icon, Status, ToolTip, Tray};
use parking_lot::Mutex;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// Everything the tray reads; all Send + Sync, none of it blocks audio.
pub struct TrayItem {
    cmd: Sender<Command>,
    snap: Arc<Mutex<PbSnapshot>>,
    /// The live window session: mirrored visibility for the Show/Hide label,
    /// and the one place a toggle (or a raise) can reach the CURRENT
    /// `egui::Context` — never a cached one (see `crate::session`).
    session: Session,
}

impl TrayItem {
    /// Toggle the window (Show/Hide menu item and Activate/left click). X11
    /// unmaps/maps the live window; Wayland cannot, so Hide runs the ✕ policy
    /// and Show re-creates the window on the parked main thread — `session`
    /// owns both halves.
    fn show_toggle(&mut self) {
        eprintln!(
            "[tray] show_toggle: {} window (mode {:?})",
            if self.session.hidden() {
                "showing"
            } else {
                "hiding"
            },
            self.session.mode()
        );
        self.session.tray_toggle();
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
            _ => "Mordent — nothing playing".to_string(),
        }
    }
}

impl Tray for TrayItem {
    fn id(&self) -> String {
        "mordent".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        // Always Active. StatusNotifier hosts (waybar included) typically HIDE
        // Passive items, so returning Passive while idle made the icon
        // invisible until the first track (the reported bug). The app is alive
        // from launch — that is the signal this property carries. Play/pause
        // state lives in the tooltip and the menu's Play/Pause label.
        Status::Active
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
        vec![mark_icon()]
    }

    fn tool_tip(&self) -> ToolTip {
        let pb = self.snap.lock();
        let description = match &pb.track {
            Some(t) if pb.has_track => {
                let state = if pb.paused {
                    "paused".to_string()
                } else {
                    format!("{} / {}", fmt_pos(pb.position), fmt_pos(pb.duration))
                };
                format!("{} — {}\n{}", t.artist, t.name, state)
            }
            _ => "nothing playing".into(),
        };
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: "Mordent".into(),
            description,
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_toggle();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let hidden = self.session.hidden();
        let (play_label, play_icon) = if self.playing() {
            ("Pause", "media-playback-pause")
        } else {
            ("Play", "media-playback-start")
        };
        use ksni::menu::StandardItem as StdItem;
        // Enabled everywhere: `Session::tray_toggle` knows how hide/show works
        // on this backend (X11 unmap/map, Wayland session swap) — see
        // `crate::session`.
        let visibility_item = MenuItem::Standard(StdItem {
            label: if hidden { "Show Mordent" } else { "Hide Mordent" }.into(),
            icon_name: if hidden {
                "view-refresh".into()
            } else {
                "window-close".into()
            },
            activate: Box::new(|t: &mut Self| t.show_toggle()),
            ..Default::default()
        });
        vec![
            visibility_item,
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
pub fn spawn(snap: Arc<Mutex<PbSnapshot>>, cmd: Sender<Command>, session: Session) {
    let spawned = std::thread::Builder::new()
        .name("mordent-tray".into())
        .spawn(move || {
            let snap_poll = snap.clone();
            let item = TrayItem {
                cmd,
                snap,
                session,
            };
            let handle = match item.spawn() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!(
                        "[tray] unavailable ({e}) — running without a tray icon; \
                         window close quits when nothing has played, and on X11 \
                         hides the window once a track has played (restore with \
                         MPRIS Raise or a relaunch)"
                    );
                    return;
                }
            };
            eprintln!("[tray] StatusNotifierItem registered (menu: show/hide, play/pause, next, prev, quit)");
            // 1 Hz refresh: title/tooltip follow the shared snapshot; update()
            // diffs server-side and only signals real changes, so an idle
            // session costs one cheap D-Bus round-trip per second.
            //
            // MORDENT_DEMO_TRAY_TOGGLE=<secs>: headless hook — after N seconds,
            // invoke the menu's Show/Hide action through the real
            // `Handle::update` path (pointer-clicking the icon is impossible
            // headlessly), exercising `show_toggle` and its hidden-flag
            // bookkeeping in the same run.
            let mut toggle_at = std::env::var("MORDENT_DEMO_TRAY_TOGGLE")
                .ok()
                .and_then(|s| s.parse::<u64>().ok());
            let started = std::time::Instant::now();
            let mut last = String::new();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if handle.is_closed() {
                    return;
                }
                if let Some(secs) = toggle_at
                    && started.elapsed().as_secs() >= secs
                {
                    toggle_at = None; // once
                    eprintln!("[tray] MORDENT_DEMO_TRAY_TOGGLE: invoking Show/Hide menu action");
                    let _ = handle.update(|item| item.show_toggle());
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

/// The tray pixmap: the Mordent mark (`assets/brand/brand.md`), the same figure
/// as the app icon but without its dark panel, so it sits on whatever
/// background the host draws — the accent reads on both light and dark bars.
/// `brand` draws it at 24 px, where the four strokes need a thicker ribbon;
/// ARGB network byte order, as ksni expects.
fn mark_icon() -> Icon {
    Icon {
        width: crate::brand::TRAY_SIZE as i32,
        height: crate::brand::TRAY_SIZE as i32,
        data: crate::brand::tray_argb(),
    }
}

fn fmt_pos(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}
