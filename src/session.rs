//! The live window-session handle: the one piece of state that outlives a
//! window and must be shared with the threads that talk to it.
//!
//! Why this exists (Wayland, `HideMode::Recreate` — see `crate::wm`): the
//! toplevel cannot be unmapped, so "hide" ends the eframe *session* (the
//! window is destroyed and `run_native` returns) and "show" runs `run_native`
//! again. (The destroy itself needs one extra event-loop iteration, which the
//! hide spends in a windowless session — see `ui::reap_hidden_window`.) Two
//! consequences drive this module's shape:
//!
//! * **`egui::Context` is per session.** eframe's `AppCreator` hands out a
//!   fresh `Context` every run, so anything that cached one — the tray, the
//!   supervisor, the wake thread, the update threads — would hold a dead
//!   handle after a hide→show. A stale `Context::request_repaint()` silently
//!   does nothing, which would freeze the progress bar and make tray/MPRIS
//!   actions look dead. [`Session::ctx`] therefore hands out the CURRENT
//!   context; holders call [`Session::request_repaint`] instead of caching.
//! * **The window may not exist at all.** While hidden the main thread parks
//!   in [`Session::wait_until_shown`] (no event loop, no window, no
//!   `Context`), and the supervisor/tray/MPRIS threads keep playing. "Show"
//!   has to be able to wake that parked thread — that is
//!   [`Session::show`]'s condvar.
//!
//! State is one `Arc`, cloned freely; nothing here blocks the audio thread
//! (a lock+move at most, and only on user-initiated show/hide).

use crate::wm::HideMode;
use eframe::egui;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cloneable handle to the current window session (see the module docs).
#[derive(Clone)]
pub struct Session(Arc<Inner>);

struct Inner {
    mode: HideMode,
    /// The CURRENT session's `egui::Context`. `None` while hidden: no window,
    /// no event loop, so there is nothing to send commands or wakes to.
    ctx: Mutex<Option<egui::Context>>,
    /// The window should be up (visible). The parking state machine reads and
    /// waits on this; `true` also means "reopen the window" for a parked
    /// `HideMode::Recreate` app.
    shown: Mutex<bool>,
    shown_cv: Condvar,
    /// Set when the ✕ (or the tray's Hide) ended a `Recreate` session on
    /// purpose, so the main loop parks instead of treating the returned
    /// `run_native` as a real exit. Sticky until the main loop clears it: a
    /// Show racing the teardown must still reopen the window.
    hide_pending: AtomicBool,
}

impl Session {
    pub fn new(mode: HideMode) -> Self {
        Self(Arc::new(Inner {
            mode,
            ctx: Mutex::new(None),
            shown: Mutex::new(true),
            shown_cv: Condvar::new(),
            hide_pending: AtomicBool::new(false),
        }))
    }

    /// How this process hides the window (see `crate::wm::HideMode`).
    pub fn mode(&self) -> HideMode {
        self.0.mode
    }

    /// Session start: this `Context` is live from now on.
    pub fn set_ctx(&self, ctx: egui::Context) {
        *self.0.ctx.lock() = Some(ctx);
    }

    /// Session end: the window (and with it the `Context`) is gone. Dropping
    /// the handle here is what makes a stray `request_repaint` from a
    /// still-running supervisor thread a cheap no-op instead of a use of a
    /// dead context.
    pub fn clear_ctx(&self) {
        *self.0.ctx.lock() = None;
    }

    /// The CURRENT session's context, if a window is up.
    pub fn ctx(&self) -> Option<egui::Context> {
        self.0.ctx.lock().clone()
    }

    /// Wake the UI if a window exists (no-op while hidden). The single wake
    /// path for every off-frame thread.
    pub fn request_repaint(&self) {
        if let Some(ctx) = self.ctx() {
            ctx.request_repaint();
        }
    }

    /// Send a viewport command to the current window, if any.
    pub fn send(&self, cmd: egui::ViewportCommand) {
        if let Some(ctx) = self.ctx() {
            ctx.send_viewport_cmd(cmd);
        }
    }

    /// Is the window hidden (unmapped on X11, destroyed on Wayland)? Mirrored
    /// from the ✕ handler, the tray and MPRIS Raise — it is what the tray's
    /// Show/Hide label reads.
    pub fn hidden(&self) -> bool {
        !*self.0.shown.lock()
    }

    pub fn set_hidden(&self, hidden: bool) {
        let mut shown = self.0.shown.lock();
        *shown = !hidden;
        drop(shown);
        self.0.shown_cv.notify_all();
    }

    /// `HideMode::Unmap` hide: unmap the live toplevel and stay in the same
    /// session (the cheap path).
    pub fn hide_unmap(&self) {
        self.set_hidden(true);
        self.send(egui::ViewportCommand::Visible(false));
        // eframe applies viewport commands while processing a frame, so a
        // parked loop must be woken for the unmap to actually happen.
        self.request_repaint();
    }

    /// A `HideMode::Recreate` hide is in progress: mark the session as ending
    /// on purpose so the main loop reopens it on Show instead of exiting.
    pub fn begin_recreate_hide(&self) {
        self.set_hidden(true);
        self.0.hide_pending.store(true, Ordering::SeqCst);
    }

    /// Did a `Recreate` session end because of a hide (not a real exit)?
    pub fn hide_pending(&self) -> bool {
        self.0.hide_pending.load(Ordering::SeqCst)
    }

    pub fn clear_hide_pending(&self) {
        self.0.hide_pending.store(false, Ordering::SeqCst);
    }

    /// Bring the window up: unmap-hide → map it here; recreate-hide → wake the
    /// parked main loop, which runs `run_native` again. Also raises an
    /// already-visible window (MPRIS Raise), and is what the tray's
    /// Activate/Show calls.
    pub fn show(&self) {
        self.set_hidden(false);
        // Visible first: Focus alone does not un-hide a window. Both are
        // winit no-ops on Wayland (see `wm`), where the notify above — not
        // these commands — is what actually re-opens the window.
        self.send(egui::ViewportCommand::Visible(true));
        self.send(egui::ViewportCommand::Focus);
        self.request_repaint();
    }

    /// The tray's Show/Hide item and Activate (left click). On X11 this is an
    /// unmap/map toggle; on Wayland "hide" is the ✕ policy exactly (the app's
    /// close handler decides between hiding and quitting — see `ui::update`),
    /// because the toplevel cannot be unmapped.
    pub fn tray_toggle(&self) {
        if self.hidden() {
            self.show();
        } else {
            match self.0.mode {
                HideMode::Unmap => self.hide_unmap(),
                HideMode::Recreate => {
                    self.send(egui::ViewportCommand::Close);
                    // The Close only surfaces as `close_requested()` on the
                    // next frame, so a parked loop must be woken.
                    self.request_repaint();
                }
            }
        }
    }

    /// Park the main thread until the window should be up again. Returns
    /// immediately if a Show already landed (a click racing the teardown).
    pub fn wait_until_shown(&self) {
        let mut shown = self.0.shown.lock();
        while !*shown {
            self.0.shown_cv.wait(&mut shown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_releases_a_parked_thread() {
        let s = Session::new(HideMode::Recreate);
        s.begin_recreate_hide();
        assert!(s.hidden() && s.hide_pending());
        // Show from another thread must wake the waiter. The waiter is this
        // thread, so only the state flip is asserted here; the parking itself
        // is the condvar loop above.
        s.show();
        assert!(!s.hidden());
        assert!(s.hide_pending(), "show must not clear the teardown marker");
        s.clear_hide_pending();
        assert!(!s.hide_pending());
    }

    #[test]
    fn unmap_hide_keeps_the_session() {
        let s = Session::new(HideMode::Unmap);
        s.set_ctx(egui::Context::default());
        s.hide_unmap();
        assert!(s.hidden());
        assert!(!s.hide_pending(), "an unmap hide never ends the session");
        assert!(s.ctx().is_some(), "the context stays live across an unmap");
        s.show();
        assert!(!s.hidden());
    }

    #[test]
    fn context_is_per_session() {
        let s = Session::new(HideMode::Recreate);
        assert!(s.ctx().is_none(), "no window yet: nothing to wake");
        s.set_ctx(egui::Context::default());
        assert!(s.ctx().is_some());
        // Session end drops the handle: a stale wake from another thread is
        // then a no-op rather than a use of the dead context.
        s.clear_ctx();
        assert!(s.ctx().is_none());
        s.request_repaint(); // must not panic
    }
}
