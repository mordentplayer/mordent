#!/usr/bin/env bash
#
# install-wrapper.sh — install the bundled wrapper-lite into PREFIX and
# register it as the `amc-wrapper` systemd service.
#
# Driven by the amc setup wizard, which runs this through pkexec (i.e. as root).
# Safe to run repeatedly: re-running stops the service, replaces the launcher
# and the rootfs skeleton, and starts it again. Linux login state under
# PREFIX/rootfs/data/ is preserved (the bundle ships no token files).
#
# Usage:
#   sudo scripts/install-wrapper.sh [--src DIR] [--port 12340]
#                                   [--prefix DIR] [--no-start]
#
# --src defaults to the directory this script lives in, i.e. the extracted
# bundle root (…/amc-<tag>-linux-x86_64). On success the service is up,
# /status has answered, and the base URL is printed on stdout; all
# human-readable logging goes to stderr.
#
set -euo pipefail

SERVICE_NAME="amc-wrapper"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
PORT=12340
PREFIX="/opt/amc-wrapper"
SRC=""
START=1

log() { printf '[amc-install] %s\n' "$*" >&2; }
fail() {
  printf '[amc-install] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install-wrapper.sh [options]

Installs the bundled wrapper-lite into PREFIX and manages the amc-wrapper
systemd service. Must run as root.

Options:
  --src DIR     Extracted bundle directory (default: the parent of this
                script's directory, i.e. <bundle>/scripts/..)
  --port PORT   Port wrapper-lite listens on (default: 12340)
  --prefix DIR  Install prefix (default: /opt/amc-wrapper)
  --no-start    Install and enable the service, but do not start it
  -h, --help    Show this help
EOF
}

# True once wrapper-lite answers on /status. Any HTTP response counts as "up".
probe() {
  local url="http://127.0.0.1:${PORT}/status"
  if command -v curl >/dev/null 2>&1; then
    curl -s -o /dev/null --max-time 2 "$url" 2>/dev/null
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O /dev/null --timeout=2 "$url" 2>/dev/null
  else
    (exec 3<>"/dev/tcp/127.0.0.1/${PORT}") 2>/dev/null
  fi
}

# Poll /status for up to 30s.
wait_for_status() {
  local i
  for ((i = 0; i < 30; i++)); do
    if probe; then return 0; fi
    sleep 1
  done
  return 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --src) SRC="${2:?--src needs a value}"; shift 2 ;;
    --port) PORT="${2:?--port needs a value}"; shift 2 ;;
    --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --no-start) START=0; shift ;;
    -h | --help) usage; exit 0 ;;
    *) fail "unknown argument: $1 (try --help)" ;;
  esac
done

[[ $EUID -eq 0 ]] || fail "must run as root (the wizard invokes this via pkexec)"
[[ "$PORT" =~ ^[0-9]+$ ]] || fail "--port must be a number, got '$PORT'"

if [[ -z $SRC ]]; then
  SRC=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
else
  SRC=$(cd -- "$SRC" 2>/dev/null && pwd) || fail "cannot enter --src directory '$SRC'"
fi

[[ -x "$SRC/wrapper-lite" ]] || fail "wrapper-lite not found in '$SRC'"
[[ -d "$SRC/rootfs" ]] || fail "rootfs/ not found in '$SRC'"
UNIT_SRC="$SRC/scripts/${SERVICE_NAME}.service"
[[ -f "$UNIT_SRC" ]] || fail "systemd unit not found at '$UNIT_SRC'"

log "installing wrapper-lite from '$SRC' to '$PREFIX' (port $PORT)"
install -d -m 0755 "$PREFIX"

# Stop an existing install first: the launcher replaces its own binary.
if systemctl list-unit-files --type=service --no-legend 2>/dev/null |
  grep -q "^${SERVICE_NAME}\.service"; then
  if systemctl is-active --quiet "$SERVICE_NAME"; then
    log "stopping existing $SERVICE_NAME"
    systemctl stop "$SERVICE_NAME" || true
  fi
fi

# wrapper-lite bind-mounts /dev/urandom into rootfs/dev and mounts proc inside
# the chroot; those mounts outlive the process. Clear them so the copy below
# never writes through a stale mount.
for mp in dev/urandom proc; do
  if mountpoint -q "$PREFIX/rootfs/$mp" 2>/dev/null; then
    log "unmounting stale $PREFIX/rootfs/$mp"
    umount -l "$PREFIX/rootfs/$mp" 2>/dev/null || true
  fi
done

install -m 0755 "$SRC/wrapper-lite" "$PREFIX/wrapper-lite"

# Merge rather than replace: refreshes binaries/skeleton, keeps login state.
# The bundle contains no rootfs/data token files, so nothing to clobber.
install -d -m 0755 "$PREFIX/rootfs"
cp -a "$SRC/rootfs/." "$PREFIX/rootfs/"

# Render the unit with the requested port.
sed "s|--port 12340|--port ${PORT}|" "$UNIT_SRC" >"$SERVICE_FILE"
chmod 0644 "$SERVICE_FILE"
log "installed $SERVICE_FILE"

systemctl daemon-reload
systemctl enable "$SERVICE_NAME" >/dev/null

if [[ $START -eq 0 ]]; then
  log "installed and enabled; not starting (--no-start)"
  printf 'http://127.0.0.1:%s\n' "$PORT"
  exit 0
fi

log "starting $SERVICE_NAME"
systemctl restart "$SERVICE_NAME"

if ! wait_for_status; then
  log "wrapper-lite did not answer /status within 30s; last log lines:"
  journalctl -u "$SERVICE_NAME" -n 20 --no-pager >&2 || true
  fail "amc-wrapper failed to start"
fi

log "service is up: $(curl -s --max-time 2 "http://127.0.0.1:${PORT}/status" 2>/dev/null | head -c 200 || true)"
printf 'http://127.0.0.1:%s\n' "$PORT"
