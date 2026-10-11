# Triage: Tray icon does not restore hidden window (KDE Plasma on Wayland)

- **Issue:** #8
- **Class:** bug
- **Confidence:** low
- **Suggested labels:** bug, tray, wayland, needs-repro
- **Suggested priority:** P2 (plausible window-restore failure, but report contradicts current main's Wayland policy; needs version/backend info before work)

## Summary
Reporter claims that with "close to tray" enabled, closing the window hides it to the tray, but left-clicking the tray icon never restores the window; the process stays alive in the tray and only a kill/restart recovers. Environment is Mordent from main on KDE Plasma / Wayland. No injection content found in the submission — plain bug report.

## Verdict
Unverified, and likely stale or mismatched against current `main`:
- Current `main` has **no "close to tray" setting** (verified: `grep` over `src/config.rs`/`src/ui.rs` finds no such option; close policy is automatic). Reporter's step 1 does not match any UI in this checkout.
- Current `main` **does not hide on Wayland at all**: `close_action(ever_played, hide_supported)` in `src/ui.rs:2144-2150` returns `Quit` when `hide_supported()` is false, and `wm::hide_supported()` (`src/wm.rs:44-46`) is false whenever `WAYLAND_DISPLAY`/`WAYLAND_SOCKET` is set. The tray's `show_toggle` (`src/tray.rs:62-86`) and menu (`src/tray.rs:180-198`) are likewise disabled on Wayland with a "Hide unavailable on Wayland" label. So on a pure-Wayland session, close quits the process — there is no hidden-window-plus-live-tray state for left-click to fail to restore, contradicting the report.
- The report could still describe a real bug in one of these cases (all unverified): (a) reporter runs an older build from before commit `9f6b6bf` ("tray/close: real quit-when-idle…", 2026-10-10), which did attempt hide-on-close on Wayland; (b) reporter runs under XWayland/X11 backend on Plasma-Wayland (env-dependent: `wm::is_wayland` keys off `WAYLAND_DISPLAY`/`WAYLAND_SOCKET`, so a forced-X11 launch hides but Plasma may not honor restore via `ViewportCommand::Visible(true)`); (c) KDE Plasma SNI Activate semantics differ (right-click menu vs left-click Activate) so "nothing happens" is a host/menu issue.
- Could not reproduce: sandbox is headless (`WAYLAND_DISPLAY`, `WAYLAND_SOCKET`, `DISPLAY`, `XDG_SESSION_TYPE` all unset) and reproducing needs a Plasma/Wayland session plus Apple Music credentials/playback (`ever_played` must latch). Skipped `cargo build`/`cargo test` per time budget; no code executed.

## Evidence
- `src/wm.rs:14-19,44-46` — `set_visible` no-op on Wayland (citing winit 0.30.13 `wayland/window/mod.rs:253`), `hide_supported() = !is_wayland()`.
- `src/ui.rs:2144-2150` (`close_action`) + `src/ui.rs:2193-2212` (close-requested handler) — Wayland close quits via `exit_flush()`; only X11 takes the `Visible(false)` + `hidden=true` branch.
- `src/tray.rs:62-86` (`show_toggle` early-returns on Wayland, restore path is `Visible(show)` + `Focus` only on X11) and `src/tray.rs:180-198` (menu item disabled on Wayland).
- `README.md:394-426` documents the same policy/platform matrix.
- `git log --oneline -3 -- src/wm.rs src/tray.rs src/ui.rs` → `ae10796`, `9f6b6bf` (the Wayland-quit fix), `f9a39ce`; reporter says "from main" but gives no commit hash.
- Commands run: `ls -R`, `grep close_to_tray|hide_supported|…` over `src/`, `grep tray|setting` over `src/config.rs` (no hits), README read, `env` display-variable check. No build/tests run.

## Suggested next step
Ask reporter for: exact commit/version (`mordent --version` or `git rev-parse --short HEAD`), backend env (`echo $XDG_SESSION_TYPE $WAYLAND_DISPLAY $DISPLAY`), stderr lines matching `[wm]`/`[tray]`, and whether the tray menu's "Show Mordent" (vs left-click Activate) and MPRIS Raise (`playerctl --player=mordent …`) restore the window. If they are on current main + pure Wayland, close should quit (process exits) — confirm whether the process actually stays alive, since that decides between stale-build vs XWayland-restore bugs.
