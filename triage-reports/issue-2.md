# Triage: Search results disappear while typing (list blanks mid-query)

- **Issue:** #2
- **Class:** bug
- **Confidence:** low
- **Suggested labels:** bug, search, needs-repro
- **Suggested priority:** P3 (cannot confirm on current `main`; describes UX that doesn't match current search flow)

## Summary
Reporter claims typing a query quickly (e.g. "radi" → "radio") blanks the results list to "no results" while the new query is in flight, instead of keeping stale results visible until new ones arrive. No injection content found in the issue body (the "my guess is the search future…" sentence is ordinary diagnostic speculation, not a directive).

## Verdict
- `unverified` — could not reproduce, and the described mechanism does not match current `main`:
  - Search is Enter/"go"-triggered only (`src/ui.rs:2312-2323`); typing alone never starts a query, so per-keystroke in-flight queries as described don't occur.
  - `spawn_search` (`src/ui.rs:811-840`) sets `searching = true` but does **not** clear `self.results`, and `search_central` (`src/ui.rs:2469-2493`) keeps rendering the old track grid (`Some(r) => track_grid`) while `searching` is true; the empty-tracks branch shows "searching…" (with spinner), not "no results", during flight.
  - The stale/out-of-order worry is a latent robustness gap, not an active bug: `Msg::Results` carries no query tag (`src/ui.rs:42,1557-1561`) and applies unconditionally — but the `self.searching` guard (`src/ui.rs:813`) enforces single-flight, so reordering can't currently happen. (Side effect of that guard: pressing Enter mid-flight silently drops the second query.)
  - No build/test run (skipped per time budget; verdict rests on static reading above).

## Evidence
- `src/ui.rs:811-840` — `spawn_search`: no `self.results = None` / clear before or during the worker thread spawn.
- `src/ui.rs:2469-2493` — `search_central` Songs branch: old results stay rendered while searching; empty + searching shows "searching…", "no results for …" only when idle.
- `src/ui.rs:2312-2323` — only Enter (after `lost_focus`) and the "go" button call `spawn_search`; no `changed()`/debounce/per-keystroke trigger (confirmed via grep for `spawn_search|changed\(\)|debounce`).
- `src/ui.rs:1557-1561` vs `1562-1565` — `Msg::Results` has no term/sort tag, unlike `Msg::Library` which discards mismatched-sort pages.
- Commands run: `git log --oneline -8` (HEAD `f779cd9`), `git status --short` (clean), directory listing, targeted `grep`/`read` of `src/ui.rs`. No `cargo build`/`cargo test`.

## Suggested next step
Ask the reporter for a screen recording plus the exact trigger (Enter-per-query? a fork with live-as-you-type search?) and the version/commit; if live search is desired, that's a feature request (debounced per-keystroke search with term-tagged results, keeping old rows until arrival), not this bug as filed.
