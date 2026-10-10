#!/usr/bin/env bash
#
# login-wrapper.sh — one-time Apple Music login for wrapper-lite.
#
# Driven by the amc setup wizard, which runs this through pkexec (i.e. as root).
# It is a thin, logged wrapper around `wrapper-lite --login`:
#
#   * stops the amc-wrapper service (login rewrites the token files),
#   * runs the file-based 2FA flow: the code is NOT passed on stdin, instead
#     the wizard writes it to PREFIX/rootfs/data/2fa.txt and wrapper-lite picks
#     it up from there (that is what --code-from-file enables),
#   * on success the token files exist under PREFIX/rootfs/data/, the service
#     is started again and the base URL is printed on stdout.
#
# Usage:
#   sudo scripts/login-wrapper.sh --email you@example.com [--password PASS]
#                                 [--port 12340] [--prefix /opt/amc-wrapper]
#                                 [--twofa-dir DIR] [--timeout SECS]
#
# Password resolution order: --password, $AMC_WRAPPER_PASSWORD, --password-file,
# interactive prompt (no echo). Prefer the environment variable when running
# interactively: a --password argument is briefly visible to other local users
# via `ps`. The setup wizard uses --password-file (pkexec strips the env).
#
set -euo pipefail

SERVICE_NAME="amc-wrapper"
PREFIX="${AMC_WRAPPER_PREFIX:-/opt/amc-wrapper}"
PORT="${AMC_WRAPPER_PORT:-12340}"
EMAIL="${AMC_WRAPPER_EMAIL:-}"
PASSWORD="${AMC_WRAPPER_PASSWORD:-}"
PASSWORD_FILE=""
TWOFA_DIR=""
TIMEOUT=300

log() { printf '[amc-login] %s\n' "$*" >&2; }
fail() {
  printf '[amc-login] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: login-wrapper.sh [options]

Performs the first-time Apple Music login for wrapper-lite. Must run as root.

Options:
  --email EMAIL     Apple ID email (default: $AMC_WRAPPER_EMAIL, else prompt)
  --password PASS   Apple ID password (default: $AMC_WRAPPER_PASSWORD, else
                    prompt; the env var is preferred over this argument)
  --password-file F Read the password from FILE (first line). This is how the
                    amc setup wizard passes it: pkexec runs this script with a
                    minimal environment, so $AMC_WRAPPER_PASSWORD never
                    arrives, and a 0600 file the wizard owns does.
  --port PORT       Port to report back (default: 12340)
  --prefix DIR      Install prefix (default: /opt/amc-wrapper)
  --twofa-dir DIR   Where the 2FA code file lives
                    (default: PREFIX/rootfs/data/2fa.txt)
  --timeout SECS    Give up waiting for the 2FA code after SECS (default: 300)
  -h, --help        Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --email) EMAIL="${2:?--email needs a value}"; shift 2 ;;
    --password) PASSWORD="${2:?--password needs a value}"; shift 2 ;;
    --password-file) PASSWORD_FILE="${2:?--password-file needs a value}"; shift 2 ;;
    --port) PORT="${2:?--port needs a value}"; shift 2 ;;
    --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --twofa-dir) TWOFA_DIR="${2:?--twofa-dir needs a value}"; shift 2 ;;
    --timeout) TIMEOUT="${2:?--timeout needs a value}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) fail "unknown argument: $1 (try --help)" ;;
  esac
done

[[ $EUID -eq 0 ]] || fail "must run as root (the wizard invokes this via pkexec)"
[[ "$TIMEOUT" =~ ^[0-9]+$ ]] || fail "--timeout must be a number, got '$TIMEOUT'"
[[ -x "$PREFIX/wrapper-lite" ]] || fail "wrapper-lite not found at '$PREFIX' (run install-wrapper.sh first)"

if [[ -z $TWOFA_DIR ]]; then
  TWOFA_DIR="$PREFIX/rootfs/data"
fi
TWOFA_FILE="$TWOFA_DIR/2fa.txt"

if [[ -z $EMAIL ]]; then
  read -r -p "Apple ID email: " EMAIL || true
fi
[[ -n $EMAIL ]] || fail "no Apple ID email given"

if [[ -z $PASSWORD && -n $PASSWORD_FILE ]]; then
  # The wizard's channel: pkexec gives us a minimal environment, so the file
  # (readable only by its owner and root) is the only thing that arrives.
  [[ -r $PASSWORD_FILE ]] || fail "cannot read --password-file '$PASSWORD_FILE'"
  IFS= read -r PASSWORD <"$PASSWORD_FILE" || true
fi

if [[ -z $PASSWORD ]]; then
  read -r -s -p "Apple ID password: " PASSWORD || true
  printf '\n' >&2
fi
[[ -n $PASSWORD ]] || fail "no Apple ID password given"

install -d -m 0755 "$TWOFA_DIR"
# Never let wrapper-lite consume a stale code from a previous attempt.
rm -f -- "$TWOFA_FILE"

# The launcher resolves ./rootfs relative to its working directory, so run it
# from the install prefix.
cd -- "$PREFIX"

had_unit=0
if systemctl list-unit-files --type=service --no-legend 2>/dev/null |
  grep -q "^${SERVICE_NAME}\.service"; then
  had_unit=1
fi

if [[ $had_unit -eq 1 && $(systemctl is-active "$SERVICE_NAME" || true) == "active" ]]; then
  log "stopping $SERVICE_NAME for login"
  systemctl stop "$SERVICE_NAME" || true
fi

log "logging in as '$EMAIL'; the wizard must write the 2FA code to '$TWOFA_FILE'"
log "waiting up to ${TIMEOUT}s for the code"

set +e
if command -v timeout >/dev/null 2>&1; then
  timeout --foreground "$TIMEOUT" ./wrapper-lite \
    --login "${EMAIL}:${PASSWORD}" --code-from-file \
    --host 127.0.0.1 --port "$PORT"
else
  ./wrapper-lite \
    --login "${EMAIL}:${PASSWORD}" --code-from-file \
    --host 127.0.0.1 --port "$PORT"
fi
rc=$?
set -e
unset PASSWORD

# Leave the service usable regardless of the login outcome.
if [[ $had_unit -eq 1 ]]; then
  log "starting $SERVICE_NAME"
  systemctl start "$SERVICE_NAME" || true
fi

if [[ $rc -eq 124 ]]; then
  fail "timed out after ${TIMEOUT}s waiting for the 2FA code at '$TWOFA_FILE'"
fi
if [[ $rc -ne 0 ]]; then
  fail "Apple login failed (wrapper-lite exited $rc)"
fi

# wrapper-lite writes MUSIC_TOKEN once the login succeeds.
[[ -s "$TWOFA_DIR/MUSIC_TOKEN" ]] ||
  fail "login reported success but '$TWOFA_DIR/MUSIC_TOKEN' is missing or empty"

log "login OK — tokens stored in '$TWOFA_DIR'"
printf 'http://127.0.0.1:%s\n' "$PORT"
