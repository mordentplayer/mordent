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
//! Menu: Show/Hide (only where the platform can unmap the window — see
//! below), Play/Pause (reflects state), Next, Previous, Quit. Activate (left
//! click) toggles window visibility. Quit sends [`Command::Quit`] — the
//! supervisor flushes the report segment and exits, exactly like Ctrl+Q /
//! MPRIS Quit.
//!
//! HIDE/SHOW SUPPORT (`crate::wm`): X11 can unmap/map the window, so Show/Hide
//! works. On Wayland winit 0.30.13's `Window::set_visible` is a no-op, so the
//! item is disabled and labelled "Hide unavailable on Wayland" instead of
//! lying (niri implements no `xdg_toplevel.set_minimized` fallback either).
//!
//! FALLBACKS (never panics the app — every failure path degrades silently):
//! * no session bus / no StatusNotifierWatcher / no host → `spawn` returns
//!   `Err`, logged once to stderr, no tray icon; the window-close policy in
//!   `ui::update` still applies (quit when nothing has played; hide on X11
//!   once a track has — restore via MPRIS Raise, `playerctl`, or a relaunch).
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
    /// Toggle the window. Where the platform cannot unmap the toplevel
    /// (Wayland — see `wm`) this is honest about it instead of pretending.
    fn show_toggle(&mut self) {
        if !crate::wm::hide_supported() {
            // winit no-ops `set_visible` on Wayland and `Focus` is a no-op
            // too, so there is nothing to hide/show; say so, best-effort
            // raise, and leave the window alone.
            eprintln!(
                "[tray] show_toggle: hide/show unavailable on Wayland — the window \
                 cannot be unmapped; leaving it as is"
            );
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            return;
        }
        let show = self.hidden.load(Ordering::Relaxed);
        eprintln!(
            "[tray] show_toggle: {} window (hidden {show} → {})",
            if show { "showing" } else { "hiding" },
            !show
        );
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
        let mut description = match &pb.track {
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
        if !crate::wm::hide_supported() {
            // Don't lie: the toplevel cannot be unmapped on Wayland, so the
            // tray cannot hide/show it (see `wm`).
            description.push_str("\nWayland: window cannot be hidden — close quits");
        }
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
        let hidden = self.hidden.load(Ordering::Relaxed);
        let (play_label, play_icon) = if self.playing() {
            ("Pause", "media-playback-pause")
        } else {
            ("Play", "media-playback-start")
        };
        use ksni::menu::StandardItem as StdItem;
        // Hide/Show only where the platform can actually unmap the window
        // (X11). On Wayland it is impossible (winit 0.30.13 no-ops
        // `set_visible`; niri has no minimize either), so the item is
        // disabled and says so rather than lying.
        let visibility_item = if crate::wm::hide_supported() {
            MenuItem::Standard(StdItem {
                label: if hidden { "Show Mordent" } else { "Hide Mordent" }.into(),
                icon_name: if hidden {
                    "view-refresh".into()
                } else {
                    "window-close".into()
                },
                activate: Box::new(|t: &mut Self| t.show_toggle()),
                ..Default::default()
            })
        } else {
            MenuItem::Standard(StdItem {
                label: "Hide unavailable on Wayland".into(),
                icon_name: "window-close".into(),
                enabled: false,
                ..Default::default()
            })
        };
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
pub fn spawn(
    snap: Arc<Mutex<PbSnapshot>>,
    cmd: Sender<Command>,
    hidden: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let spawned = std::thread::Builder::new()
        .name("mordent-tray".into())
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
