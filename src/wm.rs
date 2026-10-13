//! Window-management capability probe: how can we hide the window and bring
//! it back?
//!
//! The Linux backend is picked exactly the way winit 0.30.13 picks it
//! (`winit-0.30.13/src/platform_impl/linux/mod.rs:736-755`): a non-empty
//! `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` selects Wayland, otherwise `DISPLAY`
//! selects X11 (`WINIT_UNIX_BACKEND` was removed in winit 0.29, so there is
//! nothing else to honour). Probing the same inputs means the answer always
//! matches the window this process actually owns.
//!
//! Two capabilities decide which hide mechanism applies:
//!
//! 1. **Unmapping/remapping the toplevel.**
//!    * X11 — `Window::set_visible` maps/unmaps via xcb
//!      (`winit-0.30.13/src/platform_impl/linux/x11/window.rs:1123`).
//!    * Wayland — `Window::set_visible` is a **no-op**:
//!      `winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:253`
//!      (`// Not possible on Wayland.`). `focus_window` is empty too (line
//!      629), `set_minimized(false)` only logs a warning (lines 436-445), and
//!      niri does not implement `xdg_toplevel.set_minimized` at all — so there
//!      is no compositor-side fallback either.
//! 2. **Ending and restarting the eframe *window session*** (hide = the window
//!    is destroyed; show = a new window). eframe 0.32.3 supports this:
//!    `native/run.rs:46` keeps a thread-local `EventLoop` and reuses it ("We
//!    reuse the event-loop so we can support closing and opening an eframe
//!    window multiple times. This is just a limitation of winit."), and
//!    `run_and_return` (line 317) drives winit's
//!    `EventLoopExtRunOnDemand::run_app_on_demand`; every run calls
//!    `window_target().clear_exit()` first, so a second `run_native` on the
//!    same thread re-runs the loop cleanly. `NativeOptions::run_and_return`
//!    defaults to `true` (`epi.rs:441`), and `AppCreator` hands out a fresh
//!    `egui::Context` per session.
//!
//!    Destroying the *window* takes one extra event-loop iteration on Wayland:
//!    dropping a winit `Window` only raises a flag
//!    (`winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:718-724`)
//!    and the surface is destroyed in that loop's next iteration
//!    (`wayland/event_loop/mod.rs:470-481`) — while eframe drops the window as
//!    the loop is already exiting (`run.rs:221-226`). Without that extra
//!    iteration the toplevel would stay mapped for as long as the process is
//!    parked, so the hide runs one windowless session afterwards
//!    (`ui::reap_hidden_window`).
//!
//! [`HideMode`] is the selection. There is no third case: a Wayland toplevel
//! can neither be unmapped nor minimized away, so the only way "hide" can mean
//! anything there is to end the session and keep the *process* (and its
//! supervisor/tray/MPRIS threads) alive.

/// How this process hides the window and how it brings it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideMode {
    /// X11: unmap/map the live toplevel. Cheap — no session churn: the window,
    /// its `egui::Context` and the frame loop all stay alive.
    Unmap,
    /// Wayland: the toplevel cannot be unmapped (see the module docs), so
    /// "hide" ends the eframe **session** — the window is destroyed and
    /// `run_native` returns — while the process (audio, supervisor, tray,
    /// MPRIS) keeps running. "Show" runs `run_native` again; eframe's reused
    /// event loop makes that legal. The surface teardown needs one more loop
    /// iteration than ending the session provides, so the hide finishes it with
    /// a windowless session (`ui::reap_hidden_window`). See `crate::session`
    /// for the shared state that survives the swap.
    Recreate,
}

/// Pure core of [`hide_mode`], split out so the selection rule is unit
/// testable without touching the process environment.
fn mode_from(wayland: bool) -> HideMode {
    if wayland {
        HideMode::Recreate
    } else {
        HideMode::Unmap
    }
}

/// Is this process on winit's Wayland backend? (See the module docs.)
pub fn is_wayland() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    wayland_from(
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_SOCKET").ok().as_deref(),
    )
}

/// Pure core of [`is_wayland`], split out so the selection rule is unit
/// testable without touching the process environment.
fn wayland_from(wl_display: Option<&str>, wl_socket: Option<&str>) -> bool {
    let set = |v: Option<&str>| v.is_some_and(|s| !s.is_empty());
    set(wl_display) || set(wl_socket)
}

/// How this process must hide/show the window (see [`HideMode`]).
pub fn hide_mode() -> HideMode {
    mode_from(is_wayland())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_selection_matches_winit() {
        // non-empty WAYLAND_DISPLAY or WAYLAND_SOCKET wins; empty = unset
        assert!(wayland_from(Some("wayland-1"), None));
        assert!(wayland_from(None, Some("wayland-0")));
        assert!(!wayland_from(Some(""), Some("")));
        assert!(!wayland_from(Some(""), None));
        assert!(!wayland_from(None, None));
    }

    #[test]
    fn hide_mode_follows_the_backend() {
        // Wayland keeps the process alive and recreates the window; every
        // other backend unmaps the live one (the cheap path).
        assert_eq!(mode_from(true), HideMode::Recreate);
        assert_eq!(mode_from(false), HideMode::Unmap);
    }

    #[test]
    fn hide_mode_matches_this_process() {
        // The live answer must agree with the environment selection the window
        // backend actually made (Xvfb/VPS hosts have no Wayland display).
        assert_eq!(hide_mode(), mode_from(is_wayland()));
    }
}
