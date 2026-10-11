# Triage: Last.fm scrobbling support (now-playing + halfway scrobble + offline queue)

- **Issue:** #5
- **Class:** feature-request
- **Confidence:** high
- **Suggested labels:** enhancement, integrations
- **Suggested priority:** P3 (with one-line rationale: new optional integration, no breakage or data loss without it)

## Summary
Requester asks for opt-in Last.fm scrobbling: enable in settings with an API key, send "now playing" on track start, submit a scrobble past the halfway point, and queue offline plays for later submission. Explicitly scoped to just scrobbling tracks the player already knows about, not a full Last.fm client. No injection detected in the submission — plain feature request, no directives or exfiltration attempts.

## Verdict
Well-scoped and in line with the project. No existing Last.fm/scrobble code (case-insensitive grep for `scrobbl|last\.?fm|lastfm` returns zero hits). The building blocks already exist:
- Track metadata (`artist`/`name`/`album`/`duration_ms`) in `src/api.rs:14-24` (`struct Track`).
- Canonical track-start/track-end hooks with position tracking in `src/supervisor.rs:439-495` (`report_start`, `report_end_for`/`report_end`) — the existing Apple play-reporting (`src/report.rs`, gated on `Config::report_plays` in `src/config.rs:18,32`) is the pattern to mirror for a Last.fm reporter.
- Per-tick position/duration available in `src/supervisor.rs:372` (`position()`) and `src/supervisor.rs:920` (`tick()`), where the halfway-point scrobble trigger and the now-playing update would live.
- Settings persistence pattern in `src/config.rs:7-36,238-281` plus UI settings comment in `src/ui.rs:248` — where the opt-in toggle + API key/session key fields would go.

Implementation sketch: new `src/scrobble.rs` module (auth session, `track.updateNowPlaying` / `track.scrobble` via existing `ureq` dependency per `Cargo.toml:10`, persistent offline queue under `cache_dir`); wire into supervisor start/end/tick hooks alongside `PlayReporter`; add `lastfm_*` options to `Config` and a settings section in `ui.rs`. Note: request says "API key" but Last.fm scrobbling needs the full auth flow (API key + secret + session key via `auth.getMobileSession` or web auth), so session-key handling, not just an API key field, is required. Halfway-only rule differs slightly from Last.fm's standard rule (half duration or 4 min, whichever first, with 30 s minimum) — maintainer should decide whether to follow the request literally or the Last.fm spec.

## Evidence
- `grep -i "scrobbl|last\.?fm|lastfm" /work` → No matches found (no existing implementation).
- `src/api.rs:14-24` — `Track { id, name, artist, album, duration_ms, ... }` has all scrobble fields.
- `src/supervisor.rs:439-495` — `report_start` / `report_end_for` / `report_end` are the start/end hooks to mirror.
- `src/supervisor.rs:372,920` — `position()` + `tick()` for the halfway trigger.
- `src/config.rs:18,32` + `src/report.rs:10,31` — existing opt-in `report_plays` gating pattern for Apple reporting.
- `Cargo.toml:10` — `ureq` with `json` feature already available for Last.fm API calls.
- Skipped `cargo build`/`cargo test` per budget; verdict rests on static reads + grep, sufficient for a feature request with no behavior to reproduce.

## Suggested next step
Maintainer decides: (1) accept as optional P3 enhancement and confirm spec details (Last.fm auth flow + standard scrobble timing rule vs. literal halfway rule), or (2) decline if external-service integrations are out of scope.
