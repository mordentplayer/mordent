# Home-server mode (no container needed)

The wrapper is not a Mac thing at all — it is a small HTTP service that decrypts
FairPlay audio. Any **always-on Linux machine** can host it, and this VPS
already does: the live deployment on `127.0.0.1:12340` is exactly this service
running natively (no Docker involved). A friend does not need a Mac-side
container if they have a home server, a NAS that runs Linux containers, or they
can simply share the owner's VPS. (The host must be **x86-64**: the
FairPlay payload inside is Android x86-64, so an ARM box such as a Raspberry Pi
cannot run it without emulation.) Run it the same way as anywhere else
(`docker compose up -d` from `docs/macos-container/`, or the repo's own native
`./wrapper-lite --host 127.0.0.1 --port 12340`), log in once as in
`README-friend.md`, and let `restart: unless-stopped` (or a systemd unit) keep
it alive. The Apple session refreshes itself every ~30 minutes, so the box can
run unattended for days.

Remote clients then just point their player at the server instead of at
localhost: set `wrapper_url = "http://music-box.local:12340"` in `amc`'s
`~/.config/amc/config.toml` (or the equivalent field in any other client).
Because the wrapper carries no authentication of its own, do **not** expose its
port to the open internet — bind it to the LAN/VPN address, or reach it over
Tailscale/WireGuard or an SSH tunnel (`ssh -L 12340:127.0.0.1:12340 user@box`,
then point the client at `127.0.0.1:12340`). One wrapper on one box serves any
number of clients, which makes this the zero-risk fallback for the friend's
MacBook: if the macOS container ever gives trouble, move the wrapper to the VPS
and change one line in the client config.
