# amc — Apple Music, native and fast

A lightweight native Apple Music client for Linux (Rust + egui). Goals:
instant startup, minimal resource use, and the shortest possible path from
"I want to hear music" to hearing it — mirroring [spotifast]'s philosophy.

Full-quality playback (ALAC up to 24-bit / hi-res, word-synced lyrics) is
powered by [wrapper-lite], the FairPlay SAMPLE-AES decryption backend.
Catalog search and 90-second previews work anonymously; full-track playback
requires an active Apple Music subscription logged into the wrapper.

[spotifast]: https://github.com/crmne/spotifast
[wrapper-lite]: https://github.com/WorldObservationLog/wrapper/tree/lite

## Architecture

```
┌─────────────────────────────── amc (single Rust binary) ──────────────────────┐
│  egui UI ── queue / player bar / search                                      │
│     │                                                                        │
│  amp-api client          wrapper-lite client        streaming pipeline        │
│  · web bearer scrape  ── · /m3u8 manifest URLs    · HLS master→variant pick  │
│  · search / catalog      · /key FairPlay templates  · single-file fetch ahead  │
│  · previews              · /lyrics (word-synced)    · cbcs decrypt in-flight  │
│  · media-user-token*     · /status /license         · ALAC packets → symphonia│
└──────────────────────────────────────────────────────────────────────────────┘
                                        │ HTTP :12340
                        wrapper-lite (chroot'd Android-layer `lite` binary)
```

*optional — personalized library/playlists features.

## Building

```sh
cargo build --release
```

## Setup

1. **wrapper-lite** (full playback):
   ```sh
   git clone -b lite https://github.com/WorldObservationLog/wrapper wrapper
   cd wrapper && mkdir build && cd build && cmake .. -DCMAKE_BUILD_TYPE=Release && make -j$(nproc)
   cd ..
   sudo ./wrapper-lite --login 'EMAIL:PASSWORD' --host 127.0.0.1 --port 12340
   # 2FA: echo CODE | sudo tee rootfs/data/2fa.txt  (while login waits)
   sudo ./wrapper-lite --host 127.0.0.1 --port 12340   # service mode
   ```
   Requires the Android NDK r23b at `wrapper/android-ndk-r23b/`
   (Google's CDN is blocked on some networks — the gvt1 mirror works:
   `https://redirector.gvt1.com/edgedl/android/repository/android-ndk-r23b-linux.zip`).

2. **config** `~/.config/amc/config.toml` (created on first run):
   ```toml
   storefront = "us"                      # your country code
   wrapper_url = "http://127.0.0.1:12340"
   media_user_token = ""                  # optional: library features
   ```

## Usage

```sh
amc ui                    # the app (tabs, queue, seek — see UI section below)
amc search "daft punk"    # catalog search
amc play "get lucky"      # CLI playback with time-to-music stats
amc lyrics <songId> [--syllable]   # line- or word-timed lyrics
amc decrypt <songId> [out]  # stream + decrypt to file
amc decode <file>         # verify a decrypted file decodes
```

## UI (`amc ui`)

Three tabs — **Search**, **Albums**, **Library**. Clicking a track plays it
immediately (replacing the current track and cancelling its download); `+`
appends to the queue. The Albums tab lists the albums of the current/last
search; clicking one opens an album detail page (per-track play/enqueue,
**Play** = play from the top and queue the rest, **Queue All**, esc/outside
click closes).

The queue panel (right side) has per-row ▲▼ reordering, ✕ remove, clear, and
a shuffle toggle that restores the original order when switched off. The
player bar shows elapsed/remaining time, a click-to-seek progress bar, ⏮
(restarts after 3 s, else previous track), shuffle, repeat off/all/one, and a
compact volume slider. The window title mirrors the playing track. Errors
surface as a dismissible toast, empty states are explicit, and the pointer
turns into a hand over clickable rows.

### Keyboard shortcuts

| Key | Action |
|---|---|
| `space` | play / pause |
| `→` / `←` | seek ±5 s |
| `↑` / `↓` | volume up / down |
| `N` / `P` | next / previous track |
| `Q` | toggle queue panel |
| `/` or `ctrl+F` | focus search |
| `ctrl+1` / `2` / `3` | switch to Search / Albums / Library |
| `esc` | close modal / panel |
| `?` | shortcut cheatsheet overlay |
| click progress bar | seek |

`AMC_SELFTEST=1 amc ui` runs a deterministic headless state-machine test
(play-now replaces current, queue ordering, ▲▼ reorder, shuffle
restore, auto-advance) and prints `RESULT: PASS/FAIL`.
`AMC_DEMO_TERM=<query>` / `AMC_DEMO_VIEW=library|albums` pre-seed the UI for
headless screenshots.

## Theming

Optional `~/.config/amc/theme.toml`; a missing file (or missing keys) falls
back to the built-in dark defaults. It is read at startup and hot-reloaded
(polled about once a second), so editing it recolors the running window — no
restart. Bad values are ignored with a one-off warning (toast + stderr) and the
default for that key is kept.

| Key | Default | Meaning |
|---|---|---|
| `accent` | `#fa4d6a` | selection, hyperlinks, progress bar, active toggles |
| `lossless` | `#6ad1a8` | `lossless` badge |
| `bg` | `#1b1b1b` | window background / clear color |
| `panel` | `#1b1b1b` | panel background |
| `text` | `#969696` | normal text |
| `weak` | `#5a5a5a` | secondary text |
| `opacity` | `1.0` | background alpha, `0.0`–`1.0` |

Colors accept `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.

```toml
accent = "#4dd0fa"
opacity = 0.9
```

Transparency caveat: the window is created transparent, but see-through only
works with a running compositor (X11 compositing manager / Wayland). On a bare
X server it simply renders solid — neither case errors.

## Measured (VPS, download-throttled Apple CDN)

| Metric | Value |
|---|---|
| anonymous search incl. token scrape | 214 ms |
| catalog search (warm) | ~50 ms |
| full ALAC stream: first second of audio | 6.9 s |
| decode throughput (debug build) | 12.4× realtime |
| full track streamed + decrypted + decoded | 369 s of ALAC, 0 errors |

## Status

Working: search, your library (songs/albums/search), ALAC/lossless streaming playback, previews fallback,
lyrics endpoint, decrypt-to-file, dense search-first UI with albums tab +
album detail modal, queue with reorder/shuffle, seekable player bar with
shuffle/repeat, keyboard shortcuts.
Next: animated word-synced lyrics
view, media keys, wrapper auto-spawn from the app.
