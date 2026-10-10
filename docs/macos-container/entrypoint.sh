#!/bin/sh
# Container entrypoint for wrapper-lite (rootless launcher inside the container).
#
# The launcher (wrapper-lite-rootless) does all the privileged-looking work
# itself from inside its own user namespace: bind-mount /dev/urandom, mount
# proc, chroot into /app/rootfs, exec /system/bin/lite. The container needs the
# capability/security options documented in compose.yaml — NOT --privileged.
set -e

DATA_DIR="/app/rootfs/data"
TOKEN_DB="${DATA_DIR}/mpl_db/kvs.sqlitedb"

mkdir -p "${DATA_DIR}" 2>/dev/null || true

# The launcher chroot()s and then serves as uid 0 in its namespace and expects
# the data dir to be root-owned (the live deployment does the same). Best-effort
# only: on macOS bind mounts (virtiofs/gRPC-FUSE) chown may be a no-op/EPERM and
# must not abort the container.
if [ "$(stat -c %U "${DATA_DIR}" 2>/dev/null)" != "root" ]; then
  chown -R root:root "${DATA_DIR}" 2>/dev/null || true
fi

if [ ! -f "${TOKEN_DB}" ]; then
  echo "Login required: no cached token database at ${TOKEN_DB}."
  if [ -z "${USERNAME}" ] || [ -z "${PASSWORD}" ]; then
    echo "Set USERNAME and PASSWORD (docker compose / -e) for a one-time" >&2
    echo "login. If Apple asks for 2FA, write the code into the mounted" >&2
    echo "data dir as 2fa.txt and restart the container." >&2
    exit 1
  fi
  echo "Running one-time login (2FA code goes into data/2fa.txt if asked)..."
  exec ./wrapper-lite-rootless \
    --login "${USERNAME}:${PASSWORD}" \
    --code-from-file \
    --base-dir /data \
    --host 0.0.0.0 \
    --port 12340 \
    "$@"
fi

exec ./wrapper-lite-rootless \
  --base-dir /data \
  --host 0.0.0.0 \
  --port 12340 \
  "$@"
