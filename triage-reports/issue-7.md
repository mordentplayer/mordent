# Triage: ~1.5s silence between gapless album tracks

- **Issue:** web submission
- **Class:** bug
- **Confidence:** high
- **Suggested labels:** bug, playback
- **Suggested priority:** P2 (audible regression on gapless albums, no workaround, no data loss/crash)

## Summary
Reporter plays an album with gapless tracks and hears ~1.5s of silence at every track boundary instead of a seamless transition (v2026.10.11, Ubuntu 24.04). No injection content in submission.

## Verdict
Plausible real bug in current `main`, **unverified** by reproduction: no `cargo build`/`cargo test` run (budget; needs Apple/wrapper credentials and audio hardware anyway). Code inspection shows a stop-and-wait advance with no gapless pre-buffering or overlap, so a per-track gap is expected; 1.5s is consistent with worker fetch + init + tick granularity, especially on a cache miss. No `gapless`/`crossfade` handling exists anywhere in the checkout.
- `src/supervisor.rs:920-935`: `tick()` only calls `advance()` once the current track is over (`sink.empty()` in sink mode).
- `src/supervisor.rs:576-602`: `advance()` moves the pointer and calls `play_now_at()` *after* the previous track drained.
- `src/supervisor.rs:521-567`: `play_now_at()` sets `preparing = true`, clears the sink, bumps generation, and dispatches async work — nothing audible until the worker returns.
- `src/supervisor.rs:1008-1033`: `RealFx::spawn_worker` blocks on `stream::open_track()` then `PacketSource::new()` before `Command::Source` is appended to the sink.
- `src/stream.rs:781-820`: `open_track_inner()` does cache lookup, else wrapper plan + CDN fetch (`start_miss`) — the miss path easily accounts for ~1s+.
- `src/player.rs:69-104`: `PacketSource::new()` synchronously reads ftyp+moov and builds the ALAC decoder before any sample is available.
- Audible tick is 100ms (`src/supervisor.rs:64`), so up to ~100ms of the gap is polling granularity alone.

## Evidence
- `grep -r "gapless|crossfade|seamless" src docs README.md` → no playback implementation hits (only prefetch/cache prose).
- `git log --oneline -5` → `b873db7` HEAD; `git status --short` → only untracked `spam-verdict.txt`, tree otherwise clean.
- No reproduction attempted: requires authenticated Apple/wrapper + CDN access and listening hardware; skipped build/tests per budget.

## Suggested next step
Confirm with a cached-album timing log (timestamp `advance()` vs `Command::Source` arrival) to split worker latency from sink-drain detection, then implement ahead-of-end pre-buffering: start the next track's worker N seconds before end and `sink.append()` while current still plays (rodio queues seamlessly).
