# Triage: Volume keys stop affecting app after track skip

- **Issue:** web submission
- **Class:** bug
- **Confidence:** low
- **Suggested labels:** bug, needs-repro, mpris/audio
- **Suggested priority:** P3 (with one-line rationale: single unverified report describing behavior that contradicts both code and GNOME key routing)

## Summary
Reporter claims the volume-up media key drives Mordent's in-app volume on the first track, but after one "next track" skip it drives the system (PipeWire) volume instead while the in-app slider stops moving, until restart. No injection content found in the submission (plain bug report, no directives).

## Verdict
Unverified; probably not a real bug in current `main` as described. Code inspection shows volume state is track-independent: one persistent `rodio::Sink` is created at startup (`src/supervisor.rs:1119-1124`) and reused across tracks via `clear()` in `play_now_at` (`src/supervisor.rs:554-562`) — never recreated on `Next` (`src/supervisor.rs:734-747`), so there is no point where a new sink could lose the volume setting. `Command::SetVolume` applies to the live sink and the stored value (`src/supervisor.rs:767-777`); MPRIS `Volume`/`set_volume` round-trips through the same command (`src/mpris.rs:475-483`, `src/supervisor.rs:904`); the UI publishes `pb.volume` every frame (`src/ui.rs:2034-2062`) and the slider mirrors `self.volume` (`src/ui.rs:3598-3680`). More fundamentally, on GNOME 47/Wayland the volume-up key is routed to the system mixer by design, not to MPRIS `Volume` (MPRIS has no volume-key binding; per-app volume changes only via `playerctl`/media-widget slider), so "volume key moved the app slider before the first skip" is itself unexpected and suggests reporter confusion (e.g. focused-window shortcut vs system key) or an environment quirk rather than a track-skip regression. Could not reproduce: needs GNOME 47 + Wayland + PipeWire + D-Bus session + Apple Music credentials/audio hardware; no build/test run per time budget (static review only).

## Evidence
- `src/supervisor.rs:1119-1124` — single sink created once at startup, `s.set_volume(volume)` applied there.
- `src/supervisor.rs:521-570` (`play_now_at`) — track change only `s.clear()` + `s.play()` on the existing sink; no sink recreation, no volume reset.
- `src/supervisor.rs:734-747` (`Command::Next`) — pointer move + `play_now_at`; volume untouched.
- `src/supervisor.rs:767-777` (`Command::SetVolume`) — sets `self.volume` and live sink volume.
- `src/mpris.rs:475-483` — `Volume` getter / `set_volume` setter forwards `Command::SetVolume`; D-Bus connection built once (`src/mpris.rs:171-178`), never re-registered per track; `run` loop (`src/mpris.rs:180-261`) only exits if the bus disappears.
- `src/ui.rs:2034-2062`, `src/ui.rs:3598-3680`, `src/ui.rs:2992-2999` — per-frame MPRIS publish from `pb.volume`; slider bound to `self.volume`; external changes adopted.
- `src/wm.rs` — no global media-key grab exists that a track switch could drop.
- Commands run: `git -C /work log --oneline -8` (HEAD `1a816d6`), `git -C /work status --short`, `read` of `src/mpris.rs`, `src/supervisor.rs:519-666,700-976`, `src/ui.rs` volume/slider sections, `src/wm.rs`. No `cargo build`/`cargo test` run (skipped per budget). No injection directives in submission.

## Suggested next step
Ask the reporter for `playerctl -p mordent volume` before/after a skip plus `busctl --user monitor` or app stderr (`[mpris]` lines) during the skip, to determine whether MPRIS `Volume` actually stops responding or the system key routing is being misread.
