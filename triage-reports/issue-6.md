# Triage: Lyrics pane stays blank until window reopened

- **Issue:** web submission
- **Class:** bug
- **Confidence:** low
- **Suggested labels:** bug, lyrics, needs-repro
- **Suggested priority:** P3 (with one-line rationale: single unverified report, no crash/data loss, workaround exists)

## Summary

Reporter claims that on v2026.10.10 / Ubuntu 24.04, opening any track and switching to the lyrics pane shows a completely blank pane, and lyrics only appear after closing and reopening the window. No injection content found in the submission — it is a plain one-paragraph bug report.

## Verdict

- Unverified. Could not reproduce: needs a running wrapper-lite with an Apple Music subscription, a GUI session, and audio/catalog access, none of which this sandbox has; skipped `cargo build`/`cargo test` per time budget.
- Code inspection does not reveal an obvious fetch-or-render break on the reported path: `update` calls `ensure_lyrics` + `lyrics_panel` every frame while the panel is open (src/ui.rs:2363-2366), `ensure_lyrics` spawns the background fetch on track change (src/ui.rs:1074-1115), the result returns via `Msg::Lyrics` and is applied in `poll_messages` on the next frame (src/ui.rs:1640-1648), and every `LyricsState` arm renders something (spinner/text/lines, src/ui.rs:2998-3069) — there is no state that renders a literally empty panel, so "completely blank" does not match any inspected state and may be loose wording for the loading/empty state. The periodic repaint driver (100 ms while playing, 250 ms while a track is loaded, src/ui.rs:304-321,2409-2418) should also pick up the fetch result without any reopen, so the reported "reopen fixes it" behavior has no matching mechanism in current `main` — speculation, not confirmed.

## Evidence

- `src/ui.rs:77-87` — `LyricsState` (Idle/Loading/Ready/NoLyrics/Unavailable); all arms render content in `lyrics_panel` (`src/ui.rs:2998-3069`).
- `src/ui.rs:1059-1068` — `toggle_lyrics`: reopen only refetches after `Unavailable`; a `Loading` fetch is not restarted by reopening, which cuts against the "reopen fixes it" story being a refetch.
- `src/ui.rs:1074-1115` — `ensure_lyrics` spawns `lyrics::fetch(cfg, song, true)` with word-timing-first/line fallback (`src/lyrics.rs:83-103`).
- `src/ui.rs:1640-1648` — `Msg::Lyrics` applied only if the arrival matches the current track (stale arrivals dropped by design).
- `src/ui.rs:304-321,2409-2418` — repaint driver keeps frames flowing while a track is loaded, so no explicit repaint-on-arrival exists (a possible robustness gap if frames ever park mid-fetch, but no parked-with-track-loaded path was found — unverified speculation).
- Commands run: `git log --oneline -15` (HEAD `fd2767a`), plus file reads above. No build/tests run (skipped per budget).

## Suggested next step

- Ask the reporter whether the pane shows the "loading lyrics…" spinner, an empty panel with header only, or an error text; and check stderr for the `[lyrics] <songId>: …` line (`src/ui.rs:1100-1111`) to separate fetch failure from render failure. Optionally reproduce headless with `MORDENT_DEMO_TERM=<q> MORDENT_DEMO_LYRICS=1` and consider adding `ctx.request_repaint()` on `Msg::Lyrics` arrival as cheap insurance.
