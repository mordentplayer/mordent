# Triage: Queue pane stays empty after starting an album until restart

- **Issue:** #3
- **Class:** bug
- **Confidence:** low
- **Suggested labels:** bug, needs-repro, queue
- **Suggested priority:** P3 (single unverified report against a version that does not match current main; no corroborating code evidence)

## Summary
Reporter claims that on v0.9.3 / Ubuntu 24.04, starting any album and then opening the queue view shows an empty pane until the app is restarted, when the queued tracks presumably appear. No logs, screenshots, or exact UI path (which "Play" button) are given.

## Verdict
- Unverified. Could not reproduce: end-to-end repro needs Apple Music credentials/wrapper access plus a GUI session, neither available in this sandbox, and a full `cargo build`/`cargo test` was skipped per time budget (no build or test commands run).
- Code read does not corroborate the report. The queue panel (`src/ui.rs:3080-3089`) clones `q.tracks`/`q.pos` from the shared `Arc<Mutex<Queue>>` on every frame, so there is no panel-side cache that could go stale until restart. The album "Play" path (`src/ui.rs:1239-1254` via `play_album_tracks` → `Queue::insert_after_current`, `src/queuemodel.rs:116-129`) inserts the album into that same shared queue and moves the pointer to its head, which is exactly what the panel renders. Unit-test names in `src/queuemodel.rs` (e.g. insert/append/pointer tests) indicate this path is covered, though tests were not executed here.
- Version mismatch weakens the report: `Cargo.toml` is `2026.10.10` and `git tag` shows only `2026-10-10.3`; no `0.9.3` tag or reference was found (`grep 0\.9\.3` hit nothing). So the report is either against a much older/renamed build (project was renamed amc → mordent) or the version string is wrong.
- No injection attempt found in the issue body. The "submitted anonymously via the web form / smoke-test@example.com" lines are intake metadata, not directives; nothing in the body was acted on as an instruction.

## Evidence
- `src/ui.rs:3086-3089` — `queue_panel` snapshots `(q.tracks.clone(), q.pos)` under the lock each frame; `src/ui.rs:3106-3108` shows the empty state only when `tracks.is_empty()`.
- `src/ui.rs:1239-1254` (`play_album_tracks`) and `src/ui.rs:3413-3416,3543-3546` (album/playlist modal "Play" buttons) — both funnel into `insert_after_current`, mutating the same queue the panel reads.
- `src/queuemodel.rs:116-129` (`insert_after_current`) — with `pos: None` (fresh app) appends block at 0 and sets `pos = Some(0)`; renders non-empty.
- Commands run (all read-only, no build): `git log --oneline -20`, `git status --short` (clean), `git tag | tail -5` → `2026-10-10.3`, `grep -E '^version|^name' Cargo.toml` → `mordent / 2026.10.10`, `grep -rn "0\.9\.3"` → no hits, `git log --all --grep="queue" -i` (history shows only queue feature/model commits, no empty-pane fix). Speculation above is marked as such; no runtime observation was possible.

## Suggested next step
- Ask the reporter for the exact click path (album modal "Play" vs row click vs `+`), the app's reported version/commit, and a log with queue state; maintainer can then try the headless hooks (`MORDENT_SELFTEST=1`, `MORDENT_DEMO_QUEUE=1` per `src/ui.rs:722-723`) to confirm the panel renders a programmatic album queue without needing credentials.
