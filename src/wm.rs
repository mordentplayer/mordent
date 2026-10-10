//! Window-management capability probe: can we hide the window and bring it
//! back?
//!
//! The Linux backend is picked exactly the way winit 0.30.13 picks it
//! (`winit-0.30.13/src/platform_impl/linux/mod.rs:736-755`): a non-empty
//! `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` selects Wayland, otherwise `DISPLAY`
//! selects X11 (`WINIT_UNIX_BACKEND` was removed in winit 0.29, so there is
//! nothing else to honour). Probing the same inputs means the answer always
//! matches the window this process actually owns.
//!
//! Capability: unmapping/mapping the toplevel.
//! * X11 — `Window::set_visible` maps/unmaps via xcb
//!   (`winit-0.30.13/src/platform_impl/linux/x11/window.rs:1123`).
//! * Wayland — `Window::set_visible` is a **no-op**:
//!   `winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:253`
//!   (`// Not possible on Wayland.`). `focus_window` is empty too (line 629),
//!   `set_minimized(false)` only logs a warning (lines 436-445), and niri does
//!   not implement `xdg_toplevel.set_minimized` at all — so there is no
//!   compositor-side fallback either.
//!
//! Consequence: Hide/Show is offered only where it works. On Wayland the app
//! must not pretend — see `ui::update`'s close handling and `tray`.

/// Pure core of [`is_wayland`], split out so the selection rule is unit
/// testable without touching the process environment.
fn wayland_from(wl_display: Option<&str>, wl_socket: Option<&str>) -> bool {
    let set = |v: Option<&str>| v.is_some_and(|s| !s.is_empty());
    set(wl_display) || set(wl_socket)
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

/// Can the window be hidden (unmapped) and shown again? True on X11 and on
/// non-Linux builds; false on Wayland (see the module docs for citations).
pub fn hide_supported() -> bool {
    !is_wayland()
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
    fn hide_is_supported_off_wayland() {
        // The VPS/Xvfb test host has no Wayland display, so hiding works.
        assert_eq!(hide_supported(), !is_wayland());
    }
}
