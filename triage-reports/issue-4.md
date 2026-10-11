# Triage: Gapless playback broken — ~1.5s silence between continuous album tracks

- **Issue:** web submission
- **Class:** bug
- **Confidence:** medium
- **Suggested labels:** playback, ux
- **Suggested priority:** P2 (audible regression on live albums, but no data loss/crash; rationale: affects continuous-playback listening experience only)

## Summary
Reporter says consecutive tracks from the same (live/continuous) album always have ~1.5s of silence between them, and suspects the next track is only fetched after the current one fully ends rather than pre-buffered. No injection content in the submission (no directives, exfiltration attempts, or spam markers).

## Verdict
Plausible real bug in current `main`, **unverified** (not reproduced). Could not reproduce: needs Apple Music credentials + wrapper-lite + full ALAC tracks + audio output; skipped `cargo build`/`cargo test` per time budget. Code inspection supports the mechanism:

- Natural advance fires only after the sink drains: `supervisor.rs:930` (`s.empty()`) → `tick` → `advance` (`supervisor.rs:920-935,576-603`).
- `advance` → `play_now_at` (`supervisor.rs:595`) only *then* spawns the play-worker (`supervisor.rs:567,1008-1033`), which synchronously runs `stream::open_track` (wrapper manifest fetch + CDN download + FairPlay decrypt, `supervisor.rs:1015-1017`, `stream.rs:768-820`) before `Command::Source` is sent and `s.append(*src)` runs (`supervisor.rs:820`). So the inter-track gap necessarily includes worker fetch/decrypt latency on every natural advance, even in the best case.
- Background `prefetch.rs` (sequential `populate_track` into the audio cache) mitigates this on a cache hit (`SourceMode::Cached`, `stream.rs:796-801`) but does not eliminate it: there is still no pre-staged decoded source appended ahead of the drain — the open/decode/append still happens strictly after `empty()` is observed. A cache miss (prefetch didn't finish, evicted, or failed/cooldown per `prefetch.rs:31-33,237-244`) pays full network cost in the gap, consistent with a ~1.5s figure on PipeWire/ALAC.
- No gapless-specific handling found: no pre-append of the next source, no overlap/crossfade-trim, no silence-strip; `grep` for `gapless|pre-buffer|prebuffer` equivalents returns only the advance/append sites above. rodio is v0.20 (`Cargo.toml:16`), `Sink::append` after `empty()` cannot be seamless by construction.

Exact commands run: `git log --oneline -8`, `grep -n "rodio\|symphonia\|cpal" Cargo.toml`, `grep -rn "append\|empty()" src/supervisor.rs`, plus reads of `src/supervisor.rs:521-603,817-860,920-935,1008-1033`, `src/stream.rs:768-848`, `src/prefetch.rs:1-47`. No build/test run.

## Evidence
- `src/supervisor.rs:930`: `self.sink.as_ref().is_some_and(|s| s.empty())` — end-of-track detected only after drain.
- `src/supervisor.rs:576-596`: `advance()` → `play_now_at(..., PlayKind::AutoAdvance)` — fetch starts after end.
- `src/supervisor.rs:1008-1033` (`RealFx::spawn_worker`): worker does `stream::open_track` → `PacketSource::with_cancel` → `Command::Source`; failure path (`WorkerFailed`, `:851-857`) also resolves via next tick.
- `src/supervisor.rs:820`: `s.append(*src)` — next source queued only on arrival, post-gap.
- `src/stream.rs:768-779`: `[play] source: {mode} (ready in ...)` log exists — maintainer can confirm gap source from user logs (Cached vs Stream + ready time).
- `src/prefetch.rs:1-33,203-244`: prefetch is best-effort background cache fill with 60s failure cooldown — explains intermittent vs consistent gaps.

Speculation (marked): the ~1.5s number itself is unverified; whether reporter's case is cache-hit decode latency vs cache-miss network latency is unknown without their `[play] source:` log line.

## Suggested next step
1. Ask reporter for the `[play] source: Cached|Stream (ready in …)` stderr lines around a track transition plus cache size setting — distinguishes "prefetch not keeping up" from "cache hit still gaps".
2. Maintainer: consider pre-staging the next track's source before the current one ends (e.g. spawn the up-next worker early on cache-hit / when remaining < N sec and `append` before drain so rodio plays it gaplessly), rather than starting the fetch in `advance()`.
