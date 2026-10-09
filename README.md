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
   cache_size_gb = 10                     # audio-cache budget; 0 disables
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

Four tabs — **Search**, **Library**, **Favorites**, **Playlists**.
Clicking a track plays it immediately (replacing the current track and
cancelling its download); `+` appends to the queue; the `♡`/`♥` button toggles
a favorite (instant, persisted). The Search tab has a segmented sub-bar —
**Songs** (the track results grid) | **Albums** (the album results of the
current/last search); switching sub-views does not re-query, and clicking an
album row opens an album detail page (per-track play/enqueue, **Play** = play
from the top and queue the rest, **Queue All**, esc/outside click closes;
`+` on an album row queues the whole album). The Library tab lists your
library **albums** (name, artist, track count, release date) with a sort
toggle in the header — **add date** (newest first) or **name** — plus a
**load more** button when the list paginates; clicks and `+` behave like
album rows in search. The Playlists tab lists your library playlists
(name, track count, description; click one for its tracks with per-track
play/enqueue, **Play**/**Queue All** and a **load more** button when the list
paginates). The Favorites tab lists favorited tracks — click to play, `+` to
queue; a favorite whose metadata has not been seen in the session (only its id
is persisted) shows as a bare id until the track is played or searched.

The queue is a single non-consuming list with a pointer to the current entry:
played tracks stay in place (dimmed) as history, the current row is
highlighted with a ▶ marker, and advancing only moves the pointer. "Play now"
(search/album rows, album **Play**) inserts right after the current entry and
jumps to it; `+`, album `+` and **Queue All** always append at the end — and
start playing immediately when nothing has been started or the queue has run
out (otherwise playback is untouched). The queue panel has per-row favorites,
▲▼ reordering, ✕ remove (removing the current track keeps playing its
successor), click-a-row to jump there, drag-and-drop reordering by the ⠿
handle (the ↑↓ buttons stay as the keyboard path), clear, and a shuffle toggle
that keeps the played region + current intact, shuffles only the up-next
tracks, and restores the original order when switched off. ⏭/⏮ move the
pointer (⏮ restarts after 3 s, else steps back); at the end of the list ⏭
does nothing and a finished queue simply stops, with the session still listed.
The player bar shows elapsed/remaining time, a click-to-seek progress bar,
shuffle, repeat off/all/one (repeat-one replays the current entry, repeat-all
wraps the pointer to the top), and a compact volume slider. The window title
mirrors the playing track. Errors surface as a dismissible toast, empty states
are explicit, and the pointer turns into a hand over clickable rows.

### Keyboard shortcuts

| Key | Action |
|---|---|
| `space` | play / pause |
| `→` / `←` | seek ±5 s |
| `↑` / `↓` | volume up / down |
| `N` / `P` | next / previous track |
| `Q` | toggle queue panel |
| `/` or `ctrl+F` | focus search |
| `ctrl+1` / `2` / `3` | switch to Search / Library / Favorites |
| `ctrl+4` | switch to Playlists |
| `esc` | close modal / panel |
| `?` | shortcut cheatsheet overlay |
| click progress bar | seek |

`AMC_SELFTEST=1 amc ui` runs a deterministic headless state-machine test
(queue pointer model: non-consumption, `+` auto-start rules, ▲▼ + drag
reorder and shuffle bookkeeping, prev/next, ✕ of the current row,
auto-advance, stop-at-end and resume-on-append) and prints
`RESULT: PASS/FAIL`.
`AMC_DEMO_TERM=<query>` / `AMC_DEMO_VIEW=library|albums|favorites|playlists`
pre-seed the UI for headless screenshots (`albums` opens the Search tab's
Albums sub-view); `AMC_DEMO_PLAYLIST=<n>` additionally
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
| `font` | `inter` | bundled font set: `inter`, `dejavu` or `default` (egui's built-ins) |

Colors accept `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`; `font` hot-reloads
with the colors, so the typeface switches without a restart.

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
client, so no private-session handling exists. A seek while playing reports
END 12 (old position) + END 13 (new position) with no new START — the
segment continues; a seek while paused reports nothing, and the resume START
carries the seeked position.

## Caching

`amc` keeps a local disk cache under `cache_dir` (default `~/.cache/amc`):

- `meta/` — amp-api responses (library UI, search, albums, artists, playlists)
  keyed by URL+storefront. Catalog entries stay fresh for 7 days, library
  entries for 1 hour; stale entries are re-fetched, and any cached copy is
  served when the network is unreachable (offline browsing) — with a
  visible "showing cached … — offline?" hint, though never over an auth
  error (401/403). Cleaned on startup; exempt from the size budget.
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

Whenever the queue or the playing track changes, amc also prefetches the
up-next queue sequentially in the background (one worker, nearest first,
skipping already-cached tracks), so when a song ends the next one starts
from the cache instead of waiting on a download. Queue edits re-target the
walk, and eviction protects the current track plus everything up-next, so a
long queued album never gets its near-term entries pruned. `cache_size_gb = 0`
disables prefetch along with the cache.

## Measured (VPS, download-throttled Apple CDN)

| Metric | Value |
|---|---|
| anonymous search incl. token scrape | 214 ms |
| catalog search (warm) | ~50 ms |
| full ALAC stream: first second of audio | 6.9 s |
| decode throughput (debug build) | 12.4× realtime |
| full track streamed + decrypted + decoded | 369 s of ALAC, 0 errors |

## Status

Working: search, your library (albums/songs/search), ALAC/lossless streaming playback, previews fallback,
lyrics endpoint, decrypt-to-file, dense search-first UI with a Songs/Albums
search sub-bar + album detail modal, library albums view with add-date/name
sort, library playlists tab with playlist detail modal, persisted
track favorites with a favorites tab, queue with reorder/shuffle, seekable
player bar with shuffle/repeat, keyboard shortcuts.
Next: animated word-synced lyrics
view, wrapper auto-spawn from the app.
