# FINDINGS — running wrapper-lite in a container (friend's MacBook)

Status: **container prototype VERIFIED end-to-end on x86-64 Linux** (this VPS).
macOS behaviour is **reasoned, not measured** — there is no Mac in this lab.

---

## TL;DR / GO-NO-GO

**GO — ship it, as a beta, for Apple Silicon Macs via Docker Desktop or OrbStack
(amd64 emulation), with the owner's VPS kept as the zero-risk fallback.**

* The FairPlay wrapper runs fully inside a plain (non-`--privileged`) container
  and serves the real API on loopback. Verified: `/status`, `/m3u8`, `/key`
  (FairPlay key exchange) and a **full lossless decrypt of a 5:20 ALAC track**;
  the decrypted file is **byte-identical** to the one produced through the
  native (non-container) wrapper.
* Container overhead is **unmeasurable**: `/m3u8` median 149 ms (container) vs
  146 ms (native); decrypt 1.9 s vs 2.3 s for the same 40.9 MB track. The
  wrapper is network-bound, not CPU-bound.
* Confidence: **high** on the container mechanics (all tested here),
  **medium** on the macOS specifics (see "Risks"), and **medium-low** on the
  end-user experience (2FA login is the rough edge).

Reason it is not an unqualified GO: the friend needs a *player* that speaks the
wrapper's HTTP API. The `Mordent` client is a Rust/egui app that is only built and
tested on Linux (its Cargo.toml gates only the tray icon to Linux; `rodio` and
`eframe` are cross-platform, so a macOS build is *expected* to work but was not
attempted here). This container solves the FairPlay half only.

---

## 1. Architecture discovery: the payload is x86-64 Android, not arm64

The previous (killed) worker's finding is confirmed:

| artefact | `file` result |
|---|---|
| `rootfs/system/bin/lite` | ELF 64-bit LSB pie executable, **x86-64**, interp `/system/bin/linker64` |
| `rootfs/system/bin/linker64` | ELF 64-bit LSB shared object, **x86-64** (bionic) |
| `rootfs/system/lib64/*` (98 libs) | ELF 64-bit LSB shared object, **x86-64** |

`rootfs/CMakeLists.txt` builds with
`x86_64-linux-android22-clang` — the Android-layer `lite` binary is an
**Android x86-64 (bionic)** program. There is **no qemu-user emulation**
anywhere: the wrapper runs natively on an x86-64 kernel. No `qemu*` binary
exists anywhere under `rootfs/`.

What `wrapper/qemu/` actually is: a *separate* upstream launcher
(`wrapper-lite-qemu`) that boots a full x86_64 Linux VM
(`qemu-system-x86_64` + `vmlinuz-lite-qemu` + busybox initramfs) and forwards
its HTTP port to the host — the repo's own "no rooted device" story. On macOS
it uses HVF when both host and guest are the same arch, and falls back to TCG
otherwise. That is **not** what this container uses.

### Per-Mac consequence

| Mac | how the wrapper runs | expected perf |
|---|---|---|
| **Apple Silicon (M1..M4)** | Docker Desktop / OrbStack run the **linux/amd64** image under x86-64 emulation. Docker Desktop: enable *Settings → General → "Use Rosetta for x86/amd64 emulation"* (Rosetta is a near-native x86-64 translator). OrbStack: Rosetta used automatically. | good — workload is network/crypto, light |
| **Intel Mac** | same image, **native** x86-64 container | best |

**Do not ship an arm64 image.** The launcher could be cross-compiled to
aarch64, but the payload it `exec`s is x86-64; an arm64 image would still have
to emulate x86-64 for the actual FairPlay work, while adding a second moving
part. The `linux/arm64` image built by the previous worker was therefore
deleted, along with `wrapper-lite-rootless-arm64`.

---

## 2. Mount/security forensics (the exact working invocation)

The rootless launcher (`wrapper-lite-rootless.c`) does all of this **from inside
the container**, as a normal process — it needs no host devices:

1. `unshare(CLONE_NEWUSER)` + write `/proc/self/uid_map`,
2. `mount("/dev/urandom", "./rootfs/dev/urandom", MS_BIND)`,
3. `mount("proc", "./rootfs/proc", "proc")`,
4. `chroot("./rootfs")`, then `exec /system/bin/lite`.

Docker's defaults block steps 1–3 in three *different* ways. Measured ladder
(Ubuntu 24.04, Docker 29.1.3, image `wrapper-lite:amd64`, port 12341):

| # | security opts / caps | host sysctl | first failure |
|---|---|---|---|
| 1 | `seccomp:unconfined` | 1 | AppArmor denies `mount()` (docker-default profile) |
| 2 | `apparmor:unconfined` | 1 | `uid_map: Operation not permitted` |
| 3 | `apparmor:unconfined` | 0 | `mount proc failed: Operation not permitted` ("too revealing") |
| 4 | `apparmor + seccomp + systempaths` | 0 | **works** |
| 5 | `apparmor + seccomp + systempaths` | 1 | `uid_map: Operation not permitted` |
| 6 | `apparmor + seccomp + systempaths + --cap-add SYS_ADMIN` | **1 (distro default)** | **works** ✅ |
| 7 | `seccomp + systempaths` (no apparmor) | 0 | `mount /dev/urandom failed: Permission denied` |
| 8 | `apparmor + systempaths` (no seccomp) | 0 | `unshare: Operation not permitted` |

Readings:

* **`seccomp:unconfined` is required** — Docker's default seccomp profile denies
  `unshare(CLONE_NEWUSER)` (row 8).
* **`apparmor:unconfined` is required** — `docker-default` denies the bind
  mounts (rows 1, 7).
* **`systempaths=unconfined` is required** — Docker masks many `/proc` paths
  with `/dev/null` bind mounts, and the kernel refuses to mount a fresh procfs
  when those masked paths would stay hidden ("too revealing", row 3).
* **The `uid_map` blocker (rows 2, 5) is Ubuntu 24.04's
  `kernel.apparmor_restrict_unprivileged_userns=1`**: the container lacks
  `CAP_SYS_ADMIN`, so its `unshare()` counts as an *unprivileged* user
  namespace and AppArmor's `unprivileged_userns` profile vetoes the uid map.
  Two fixes, both verified:
  * **(A, recommended — shipping config)** `--cap-add SYS_ADMIN`: makes the
    container's `unshare()` privileged → works with the **distro default
    sysctl**. This is what `compose.yaml` ships.
  * **(B, dev-box hack)** `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`
    + no extra caps. Previously applied on this VPS; **now reverted to the
    distro default (1)** because (A) makes it unnecessary.

Note: `docker inspect --format '{{json .HostConfig.SecurityOpt}}'` does **not**
list `systempaths=unconfined`. That is how the previous worker's "working"
container appeared to run without it; it is a runc knob surfaced outside
`SecurityOpt`. Trust the ladder, not the inspect output.

### Working invocation (verified)

```sh
# 1. build (amd64 only; the repo's own Dockerfile does the same in a full CI build)
docker build -t wrapper-lite:amd64 .

# 2. run (mount a data/ dir that is already logged in — see README-friend.md)
docker run -d --name wrapper-lite --restart unless-stopped \
  --cap-add SYS_ADMIN \
  --security-opt seccomp:unconfined \
  --security-opt apparmor:unconfined \
  --security-opt systempaths=unconfined \
  -p 127.0.0.1:12340:12340 \
  -v "$PWD/data:/app/rootfs/data" \
  wrapper-lite:amd64

curl 127.0.0.1:12340/status
# {"code":0,"msg":"SUCCESS","data":{"regions":["ca"]}}
```

This is **not `--privileged`**: no host devices, no host paths, no extra
namespaces. `SYS_ADMIN` is nonetheless a broad capability; the container serves
only on loopback and runs one binary. The upstream repo's own README uses
`docker run --privileged` and its compose file uses only `seccomp:unconfined` —
the extra opts here are what this Ubuntu 24.04 host actually needed.

---

## 3. Verified results (all on this VPS, x86-64 Linux)

### 3a. Service

```
$ docker compose up -d          # → /status 200 in ~2.8 s (warm token cache)
$ curl 127.0.0.1:12341/status
{"code":0,"msg":"SUCCESS","data":{"regions":["ca"]}}
```

Container logs on start:
```
[INFO ] initializing...
[INFO ] token cache loaded (dev=REDACTED... music=REDACTED...)
[INFO ] wrapper-lite listening on 0.0.0.0:12340
[INFO ] request: GET /status
```
The wrapper refreshes the Apple session every ~30 min ("background token
refresh" → "token cache saved"), which has been running unattended for days.

### 3b. Playback proof *through the container*

`Mordent` with `HOME`-isolated config, `wrapper_url = http://127.0.0.1:12341`,
storefront `ca`, real `media_user_token`:

```
$ mordent decrypt 697195462 /tmp/mordent-proof.m4a     # Daft Punk — One More Time
plan ready in 250.4ms: .../P1479228485_A697195462_audio_en_gr1411_alac_m.mp4 (22 fragments)
first bytes in 551.2ms
  ... 38 MB decrypted (1.9s)
download+decrypt total: 2.0s (39937 KB)
codec: alac, sample_rate: 44100, channels: 2, bit_depth: 16, frame_len: 4096
decoded 14127744 frames / 3450 packets in 1.4s (rate: 221.2x realtime)
```

Wrapper-side log for the same request (proves FairPlay key exchange ran in the
container):
```
[INFO ] request: GET /m3u8?adamId=697195462
[INFO ] request: GET /key?adamId=0&uri=skd%3A%2F%2Fitunes.apple.com%2FP000000000%2Fs1%2Fe1
[INFO ] adamId: 0, uri: skd://itunes.apple.com/P000000000/s1/e1
[INFO ] request: GET /key?adamId=697195462&uri=skd%3A%2F%2Fitunes.apple.com%2Fp1479228485%2Fc6
[INFO ] adamId: 697195462, uri: skd://itunes.apple.com/p1479228485/c6
```
`mordent decode` re-reads the container-produced file: ALAC 44.1 kHz/16-bit/2ch,
3 450 packets, 164x realtime.

`cmp /tmp/mordent-proof.m4a /tmp/mordent-native-out.m4a` → **byte-identical** to the
native-wrapper output.

`mordent play "one more time"` against the container (streaming path, not file):
```
▶ One More Time — Daft Punk [697195462]
[play] source: CDN stream (ready in 476.4ms)
ALSA lib ...: cannot find card '0'          # headless VPS has no sound card
```
i.e. the FairPlay key exchange + HLS manifest resolve in 476 ms through the
container; playback only stops at the host's missing audio device (expected on
a server). On a Mac the audio device exists, so this is the last step.

### 3c. Performance: container vs the native wrapper on this box

Native = the live deployment on 127.0.0.1:12340 (`wrapper-lite`, non-container).
Container = 127.0.0.1:12341. 15 requests each, median of `curl %{time_total}`:

| measurement | native 12340 | container 12341 | delta |
|---|---|---|---|
| `GET /status` (pure local) | 1 ms | 1 ms | none |
| `GET /m3u8?adamId=...` (hits Apple) | **146 ms** | **149 ms** | +2 % |
| decrypt, 40.9 MB ALAC (Daft Punk – One More Time) | 2.3 s | **1.9–2.0 s** | –15 % (noise) |
| first bytes after request | 1.1 s | 0.55 s | noise |
| container cold start → `/status` 200 | n/a (long-running) | **≈3–7 s** (≈3 s compose + token refresh) | — |

Conclusion: the container adds **no measurable latency**; the wrapper's work is
HTTPS + FairPlay crypto, and the only fixed cost is the ~3 s container boot.

### 3d. Image facts

| | |
|---|---|
| disk size | 290 MB (`docker images`) |
| shippable tarball (`docker save \| gzip -1`) | **76 MB** |
| contents | `debian:stable-slim` + `wrapper-lite-rootless` + `rootfs/` (117 MB) |
| login data | **not** in the image (`rm -rf /app/rootfs/data`; verified empty inside) |
| platform | `linux/amd64` only (see §1) |

### 3e. Login flow (entrypoint branches)

Verified with an empty data dir:

* no `data/mpl_db/kvs.sqlitedb` and no `USERNAME`/`PASSWORD` → prints the
  login-required hint and exits 1;
* bogus credentials → the wrapper reaches Apple and gets a real rejection
  (`iTunesStoreErrorCategory`), proving the login path and network egress work;
* 2FA: the launcher reads the code from `<base-dir>/2fa.txt`
  (`lite/auth.cpp:62`), i.e. container `/app/rootfs/data/2fa.txt` =
  host `./data/2fa.txt`.

---

## 4. Risks / caveats

1. **macOS specifics are unverified.** Here the run needed
   `systempaths=unconfined` because Docker masks `/proc` paths, and `SYS_ADMIN`
   because of Ubuntu's AppArmor userns hardening. On Docker Desktop/OrbStack the
   container runtime lives in a LinuxKit VM: `apparmor:unconfined` is likely a
   no-op (no profiles loaded) and the AppArmor userns policy does not exist, so
   `SYS_ADMIN` is likely unnecessary. It is included because it is harmless
   either way. If OrbStack rejects `systempaths=unconfined`, drop that opt there
   (it is only needed where runc masks `/proc`).
2. **Apple Silicon emulation cost is unmeasured.** Rosetta-translated amd64 is
   typically 50-90 % of native for this kind of code; plain qemu (Rosetta off)
   is much slower but the peer's workload is light. Ask the friend for
   `mordent decrypt <id>` timing on the Mac to confirm.
3. **Login UX is the weak point.** A fresh Apple session needs the Apple ID
   password **plus 2FA**, and the container has no TTY: the code must be
   written into `data/2fa.txt` and the container restarted. Apple may also
   trigger device-verification prompts. This is fine for a technical user and
   awkward for a non-technical one — do the first login *with* the friend.
4. **Session lifetime.** The cached session has refreshed unattended for days
   here (~30 min cadence), but any Apple-side invalidation (password change,
   Apple ID security reset, long offline period) forces a full re-login. Keep
   the `data/` volume; never delete it.
5. **Docker Desktop licensing.** Free for personal use and small businesses
   (<250 employees and <$10 M revenue); paid otherwise. **OrbStack** is free for
   personal use (paid commercial). **colima** (`brew install colima docker
   docker-compose`) is fully free/open-source (Lima VM, Rosetta support) if
   licensing matters.
6. **Capability surface.** `--cap-add SYS_ADMIN` + three `unconfined` knobs is
   deliberately *less* than the upstream repo's `--privileged`, but it is still
   more than a stock container. Mitigations: loopback-only port, single-purpose
   image, the friend's own machine, and (on Linux) rootless/user-namespaced
   Docker if desired.
7. **Player gap.** The container provides the FairPlay backend; the friend still
   needs a client that speaks `/m3u8`, `/key`, `/lyrics`. `Mordent` is Linux-first
   (only its tray icon is Linux-gated), so a macOS build is plausible but
   unproven. Until that exists, the practical path for the friend is the **home
   server** (see `README-home-server.md`): one wrapper, any client.
8. **The upstream qemu-VM path (`wrapper-lite-qemu`) is an alternative**, and it
   is the repo's own "recommended" macOS story. On Apple Silicon it runs under
   TCG emulation (x86_64 guest on arm64 host = no HVF), which is *slower* than
   Docker's Rosetta-backed amd64 emulation, and it requires the prebuilt
   macOS QEMU bundle from upstream releases (not present in this checkout). The
   container is the better bet on Apple Silicon; mention qemu only as a
   fallback for Intel Macs.

---

## 5. What could not be done here

* No macOS host: Rosetta/OrbStack behaviour, Docker Desktop VM security policy
  and real decrypt timing on a Mac are reasoned, not tested.
* `docker compose` had to be installed (`docker-compose-v2`) and validated on a
  spare port (12341) because host port 12340 is held by the live native wrapper.
  The `compose.yaml` in this directory is the port-12340 friend-facing file.
* The live native deployment at 127.0.0.1:12340 was never modified — its
  `rootfs/data` was **copied** (never moved, never committed) into `./data` for
  the container test. (The live process was restarted externally during the
  test window: its PID changed from 2895 to 176650, and one early native
  `/m3u8` measurement failed while it was briefly down. All reported native
  numbers were re-measured afterwards.)

## 6. Reproduce

The container's build context lives at `docs/macos-container/` in the `Mordent`
repo. Stage the two built inputs next to the committed `Dockerfile` /
`entrypoint.sh` first (in CI the `build-wrapper` job produces them; locally
take them from a wrapper `lite` checkout — `wrapper-lite-rootless` and the
`rootfs/` skeleton **minus `rootfs/data`**), then:

```sh
cd docs/macos-container
docker build -t wrapper-lite:amd64 .
docker compose up -d                     # stop the native one on 12340 first, or use a port override
curl 127.0.0.1:12340/status
HOME=/tmp/mordent-ctr-home mordent decrypt 697195462 /tmp/out.m4a   # with wrapper_url=127.0.0.1:12340
```

Equivalent one-shot form used by the release workflow:

```sh
docker build -t wrapper-lite:amd64 .
docker save wrapper-lite:amd64 | gzip -1 > wrapper-lite-amd64.tar.gz
```
