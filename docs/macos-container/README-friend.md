# Running the Apple Music (FairPlay) helper on your Mac

This box runs a small "decryption helper" that lets a music player on your Mac
play **lossless** Apple Music tracks. You only have to set it up once.

> You need an **Apple Music subscription** (your own Apple ID) and a **player
> that knows how to talk to this helper** — see the last section.

---

## Step 0 — what you'll need

* A Mac (Apple Silicon M-series is fine — see the note in Step 1).
* Your Apple ID + password (only during the first-time login).
* About 5 minutes.

Everything below is typed into the **Terminal** app (press ⌘-Space, type
`Terminal`, hit Enter). Copy each grey line, paste it, press Enter.

---

## Step 1 — install a container runtime (once)

Any one of these works. **OrbStack is the easiest.**

### OrbStack (recommended)
```sh
brew install --cask orbstack
```
Then open OrbStack from Applications. (No Homebrew? Install it first from
<https://brew.sh> — it's one line, shown on that page.)

### Docker Desktop (alternative)
Download from <https://www.docker.com/products/docker-desktop/> and install.
**On an Apple Silicon Mac**, open Docker Desktop once, go to
*Settings → General* and tick
**"Use Rosetta for x86/amd64 emulation"**, then click *Apply & Restart*.
This helper is an x86-64 program, so this makes it fast.

### colima (free / open-source, advanced)
```sh
brew install colima docker docker-compose
colima start --arch x86_64
```
(If you use colima, replace `docker compose` below with `docker compose` as
installed by the `docker-compose` formula.)

> **Apple Silicon note:** the helper is x86-64 inside. OrbStack handles that
> automatically; Docker Desktop needs the Rosetta tick above. Without Rosetta
> it still works, just slower. Intel Macs need nothing.

---

## Step 2 — load the helper

You should have received a file called **`wrapper-lite-amd64.tar.gz`** — it is
attached to the project's GitHub **Releases** page (the file name must match
exactly). In Terminal, `cd` to wherever you saved it (e.g. `cd ~/Downloads`)
and run:

```sh
gunzip -c wrapper-lite-amd64.tar.gz | docker load
```

You should see a line ending with `Loaded image: wrapper-lite:amd64`.

---

## Step 3 — start it

Make a folder for it and start the helper:

```sh
mkdir -p ~/apple-music-helper/data && cd ~/apple-music-helper
```

Copy the `compose.yaml` file that came with the release (it also lives at
`docs/macos-container/compose.yaml` in the repository) into that folder, then:

```sh
docker compose up -d
```

Check that it's alive:

```sh
curl 127.0.0.1:12340/status
```

You should see something like
`{"code":0,"msg":"SUCCESS","data":{"regions":["us"]}}`.
If you see that, jump to Step 5.

*(No `docker compose`? Use this one-liner instead:)*
```sh
docker run -d --name wrapper-lite --restart unless-stopped \
  --cap-add SYS_ADMIN \
  --security-opt seccomp:unconfined \
  --security-opt apparmor:unconfined \
  --security-opt systempaths=unconfined \
  -p 127.0.0.1:12340:12340 \
  -v "$PWD/data:/app/rootfs/data" \
  wrapper-lite:amd64
```

---

## Step 4 — log in to Apple Music (once)

The helper needs your Apple Music session. It stores it in
`~/apple-music-helper/data` — **keep that folder private and don't delete it**.

1. In `~/apple-music-helper`, create a file named **`.env`** containing:
   ```
   USERNAME=your.apple.id@example.com
   PASSWORD=your-apple-id-password
   ```
   (Careful: the password may contain characters `$`, `` ` ``… that `.env`
   mangles. If the login fails, put quotes around it: `PASSWORD="..."`.)

2. Restart it, and watch the log:
   ```sh
   docker compose up -d --force-recreate
   docker compose logs -f wrapper
   ```
3. If Apple asks for a 2FA code you'll see a line like
   `Enter your 2FA code into /data/2fa.txt`. When that happens:
   * approve the prompt on your iPhone/Mac, or read the 6-digit code,
   * in another Terminal window run
     ```sh
     echo 123456 > ~/apple-music-helper/data/2fa.txt
     ```
     (using **your** code),
   * restart once more: `docker compose restart`.
4. You'll know it worked when the log ends with
   `wrapper-lite listening on 0.0.0.0:12340` and the login didn't error.

5. Now remove the password from `.env` (the session is cached in `data/`; the
   password is no longer needed):
   ```sh
   rm ~/apple-music-helper/.env && docker compose up -d
   ```

---

## Step 5 — point your player at it

The helper answers on **`http://127.0.0.1:12340`**. Set that as the
"wrapper URL" in your player's settings (for `Mordent`, that's the
`wrapper_url = "http://127.0.0.1:12340"` line in
`~/.config/mordent/config.toml`, and set `storefront` to your country code —
the `regions` value from Step 3 tells you which).

The helper starts automatically with your Mac (the `restart: unless-stopped`
setting) and keeps your Apple session refreshed.

---

## Troubleshooting

| symptom | fix |
|---|---|
| `Cannot connect to the Docker daemon` | OrbStack/Docker Desktop isn't running — open the app. |
| `pull access denied` on `docker compose up` | the image isn't loaded — redo Step 2. |
| container exits right away, log says `Login required` | do Step 4 (login) — there's no session yet. |
| login fails with `Check the account information you entered` | wrong Apple ID/password, or `.env` mangled the password — quote it. |
| `docker compose` command not found | use the `docker run` one-liner in Step 3, or install the compose plugin. |
| `/status` works but playback fails | that's the player, not the helper — check the player's wrapper URL. |

Every restart is automatic; the only manual thing ever needed again is another
login **if Apple invalidates the session** (e.g. after changing your Apple ID
password). If that happens, repeat Step 4.
