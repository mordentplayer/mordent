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
   report_plays = true                    # report plays to Apple (Recently Played)
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

Five tabs — **Search**, **Albums**, **Library**, **Favorites**, **Playlists**.
Clicking a track plays it immediately (replacing the current track and
cancelling its download); `+` appends to the queue; the `♡`/`♥` button toggles
a favorite (instant, persisted). The Albums tab lists the albums of the
current/last search; clicking one opens an album detail page (per-track
play/enqueue, **Play** = play from the top and queue the rest, **Queue All**,
esc/outside click closes). The Playlists tab lists your library playlists
(name, track count, description; click one for its tracks with per-track
play/enqueue, **Play**/**Queue All** and a **load more** button when the list
paginates). The Favorites tab lists favorited tracks — click to play, `+` to
queue; a favorite whose metadata has not been seen in the session (only its id
is persisted) shows as a bare id until the track is played or searched.

The queue panel (right side) has per-row favorites, ▲▼ reordering, ✕ remove,
drag-and-drop reordering by the ⠿ handle (the ↑↓ buttons stay as the keyboard
path), clear, and a shuffle toggle that restores the original order when
switched off.
The player bar shows elapsed/remaining time, a click-to-seek progress bar, ⏮
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
| `ctrl+4` / `5` | switch to Favorites / Playlists |
| `esc` | close modal / panel |
| `?` | shortcut cheatsheet overlay |
| click progress bar | seek |

`AMC_SELFTEST=1 amc ui` runs a deterministic headless state-machine test
(play-now replaces current, queue ordering, ▲▼ + drag reorder, shuffle
restore, auto-advance) and prints `RESULT: PASS/FAIL`.
`AMC_DEMO_TERM=<query>` / `AMC_DEMO_VIEW=library|albums|favorites|playlists`
pre-seed the UI for headless screenshots; `AMC_DEMO_PLAYLIST=<n>` additionally
opens the n-th library playlist (0-based) once the list loads.

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

=======
## Media keys / MPRIS

`amc ui` registers `org.mpris.MediaPlayer2.amc` on the session bus: it shows
up in the GNOME/KDE media bar and lock screen with cover, title/artist and
working controls (play/pause, next/prev, seek, stop, shuffle, repeat,
volume). No session bus → MPRIS silently stays off; the app is unchanged.

```sh
playerctl --player=amc status
playerctl --player=amc metadata
playerctl --player=amc play-pause        # media keys: playerctl -a play-pause
playerctl --player=amc next              # also: previous, stop, position 1:23
playerctl --player=amc volume 0.5
playerctl --player=amc loop Track        # None / Track / Playlist
playerctl --player=amc shuffle On
```

## Play reporting

With `media_user_token` set and `report_plays = true` (the default), `amc ui`
reports playback segments to Apple's Universal Activity Service so plays land
in your Recently Played / play counts — the same JSPLAY protocol the web
player uses (PLAY_START / PLAY_END per pause, resume, seek, skip, natural
end, stop; one best-effort POST per event on a background thread, failures
ignored). Reporting needs a real audio sink: on a headless decode-only host
(VPS/CI) nothing is ever sent. Private sessions are not supported by the
client, so no private-session handling exists.

## Caching

`amc` keeps a local disk cache under `cache_dir` (default `~/.cache/amc`):

- `meta/` — amp-api responses (library UI, search, albums, artists, playlists)
  keyed by URL+storefront. Catalog entries stay fresh for 7 days, library
  entries for 1 hour; stale entries are re-fetched, and any cached copy is
  served when the network is unreachable (offline browsing). Cleaned on
  startup; exempt from the size budget.
- `audio/<adam id>/` — per-track playback material: master/variant manifests,
  FairPlay license responses, and the **encrypted** CDN media bytes. A fully
  cached track starts with zero network access; partially cached tracks just
  re-fetch. On every play the track's `last_played` is refreshed.

`cache_size_gb` (default 10) caps the audio cache: when exceeded, whole track
directories are deleted oldest-last-played first. `cache_size_gb = 0` disables
the audio cache entirely. Cache failures never interrupt playback. Privacy:
only encrypted (FairPlay) audio is ever stored — decryption happens in memory
at play time, so the cached files are inert without your device's license
material. (The transient `tracks/<id>.m4a` decode buffer is separate
machinery, not part of the cache budget.)

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
album detail modal, library playlists tab with playlist detail modal, persisted
track favorites with a favorites tab, queue with reorder/shuffle, seekable
player bar with shuffle/repeat, keyboard shortcuts.
Next: animated word-synced lyrics
view, wrapper auto-spawn from the app.
