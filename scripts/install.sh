#!/usr/bin/env bash
#
# install.sh — install Mordent into a USER directory. No sudo, ever.
#
#   curl -fsSL https://github.com/sliced-paraiba/mordent/releases/latest/download/install.sh | bash
#
# Options:
#   --tag <tag>      install a specific release tag (default: the latest release)
#   --prefix <dir>   install prefix (default: $HOME/.local)
#   --uninstall      remove an install (keeps ~/.config/mordent)
#   --yes, -y        non-interactive (implied when stdin is not a TTY)
#   -h, --help       show this help
#
# Environment overrides: MORDENT_TAG, MORDENT_PREFIX.
#
# Layout (XDG-ish, under --prefix):
#   share/mordent/{mordent, wrapper-lite, rootfs/, scripts/, licenses/, README.md}
#   bin/mordent -> ../share/mordent/mordent
#   share/applications/mordent.desktop
#   share/icons/hicolor/<size>/apps/mordent.png
#
set -euo pipefail

REPO="sliced-paraiba/mordent"
TAG="${MORDENT_TAG:-}"
PREFIX="${MORDENT_PREFIX:-${HOME:-}/.local}"
UNINSTALL=0
ASSUME_YES=0

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
warn() { printf 'install.sh: WARNING: %s\n' "$*" >&2; }
say() { printf '%s\n' "$*"; }

usage() {
  sed -n '2,20p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag) TAG="${2:?--tag needs a value}"; shift 2 ;;
    --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --yes | -y) ASSUME_YES=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

# Piped stdin (curl | bash) has no TTY to prompt on — behave as --yes.
[[ -t 0 ]] || ASSUME_YES=1

PREFIX="${PREFIX%/}"
[[ -n "$PREFIX" ]] || die "empty install prefix (set HOME, MORDENT_PREFIX or --prefix)"
[[ "$PREFIX" == /* ]] || die "--prefix must be an absolute path, got '$PREFIX'"
case "$PREFIX" in
  / | /bin | /etc | /usr | /usr/local | /opt | /var)
    die "refusing system prefix '$PREFIX' — Mordent installs into a user directory (default \$HOME/.local)"
    ;;
esac
if [[ "$(id -u)" -eq 0 ]]; then
  die "do not run this as root/sudo — Mordent installs into a user directory"
fi

SHARE="$PREFIX/share/mordent"
BIN_DIR="$PREFIX/bin"
BIN_LINK="$BIN_DIR/mordent"
DESKTOP_DIR="$PREFIX/share/applications"
DESKTOP="$DESKTOP_DIR/mordent.desktop"
ICON_ROOT="$PREFIX/share/icons/hicolor"

# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------
if (( UNINSTALL )); then
  say "Uninstalling Mordent from $PREFIX"
  [[ -d "$SHARE" ]] && { rm -rf "$SHARE"; say "  removed $SHARE"; }
  [[ -e "$BIN_LINK" || -L "$BIN_LINK" ]] && { rm -f "$BIN_LINK"; say "  removed $BIN_LINK"; }
  [[ -e "$DESKTOP" ]] && { rm -f "$DESKTOP"; say "  removed $DESKTOP"; }
  for f in "$ICON_ROOT"/*/apps/mordent.png; do
    [[ -e "$f" ]] || continue
    rm -f "$f"
    say "  removed $f"
  done
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  fi
  say "Kept ~/.config/mordent (your settings and login) — remove it by hand if you want."
  exit 0
fi

# ---------------------------------------------------------------------------
# architecture
# ---------------------------------------------------------------------------
case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64)
    die "aarch64/arm64 builds are not yet available — only x86_64 is published.
       See https://github.com/$REPO/releases"
    ;;
  *) die "unsupported architecture '$(uname -m)' — only x86_64 is published" ;;
esac

# ---------------------------------------------------------------------------
# resolve the release (tag + asset URLs)
# ---------------------------------------------------------------------------
api_latest_tag() {
  local json
  json="$(curl -fsSL --connect-timeout 15 \
    "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null)" || return 1
  printf '%s\n' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}

if [[ -z "$TAG" ]]; then
  TAG="$(api_latest_tag)" ||
    die "could not resolve the latest release (GitHub API unreachable or rate-limited).
       Re-run with an explicit tag, e.g. --tag 2026-10-10.3"
  [[ -n "$TAG" ]] || die "could not parse the latest release tag — re-run with --tag <tag>"
  # Prefer the /latest/download/ asset URL; the tag above only names the asset.
  BASE="https://github.com/$REPO/releases/latest/download"
  FALLBACK_BASE="https://github.com/$REPO/releases/download/$TAG"
else
  BASE="https://github.com/$REPO/releases/download/$TAG"
  FALLBACK_BASE=""
fi
ASSET="mordent-${TAG}-linux-${ARCH}.tar.gz"

if (( ! ASSUME_YES )); then
  printf 'Install Mordent %s into %s? [y/N] ' "$TAG" "$PREFIX" >&2
  read -r answer || answer=""
  case "$answer" in
    [yY] | [yY][eE][sS]) ;;
    *) die "aborted" ;;
  esac
fi

say "Installing Mordent $TAG (linux-$ARCH) into $PREFIX"

# ---------------------------------------------------------------------------
# download + verify
# ---------------------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() { # url dest
  curl -fL --retry 3 --retry-delay 2 --connect-timeout 15 -o "$2" "$1"
}
try_fetch() { curl -fsL --retry 2 --connect-timeout 15 -o "$2" "$1" 2>/dev/null; }

if ! fetch "$BASE/$ASSET" "$TMP/$ASSET"; then
  if [[ -n "$FALLBACK_BASE" ]]; then
    fetch "$FALLBACK_BASE/$ASSET" "$TMP/$ASSET" ||
      die "download failed: $FALLBACK_BASE/$ASSET"
  else
    die "download failed: $BASE/$ASSET (is $TAG a linux-$ARCH release?)"
  fi
fi

computed="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
if try_fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS"; then
  line="$(awk -v a="$ASSET" '$2 == a { print }' "$TMP/SHA256SUMS")"
  if [[ -n "$line" ]]; then
    ( cd "$TMP" && printf '%s\n' "$line" | sha256sum -c - ) ||
      die "SHA256 mismatch for $ASSET — refusing to install"
    say "  checksum verified (SHA256SUMS)"
  else
    warn "$TAG ships SHA256SUMS but has no entry for $ASSET"
    warn "continuing unverified — computed sha256: $computed"
  fi
else
  warn "$TAG ships no SHA256SUMS (legacy release) — continuing unverified"
  warn "computed sha256 for $ASSET: $computed"
fi

# ---------------------------------------------------------------------------
# extract + stage (idempotent upgrade: stage, then swap)
# ---------------------------------------------------------------------------
mkdir -p "$TMP/x"
tar -xzf "$TMP/$ASSET" -C "$TMP/x"
SRC="$(find "$TMP/x" -maxdepth 1 -mindepth 1 -type d | head -n1)"
[[ -n "$SRC" && -f "$SRC/mordent" ]] || die "unexpected archive layout (no mordent binary)"
for need in wrapper-lite rootfs scripts licenses README.md; do
  [[ -e "$SRC/$need" ]] || die "archive is missing '$need'"
done
[[ -d "$SRC/scripts" && -f "$SRC/scripts/install-wrapper.sh" ]] ||
  die "archive is missing scripts/install-wrapper.sh (the setup wizard needs it)"

mkdir -p "$PREFIX/share"
STAGE="$PREFIX/share/.mordent-stage.$$"
OLD="$PREFIX/share/.mordent-old.$$"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -a "$SRC/." "$STAGE/"
if [[ -e "$SHARE" ]]; then mv "$SHARE" "$OLD"; fi
mv "$STAGE" "$SHARE"
rm -rf "$OLD"

mkdir -p "$BIN_DIR"
ln -sfn "../share/mordent/mordent" "$BIN_LINK"

# ---------------------------------------------------------------------------
# desktop entry + icons (never fail the install over an icon)
# ---------------------------------------------------------------------------
mkdir -p "$DESKTOP_DIR"
cat >"$DESKTOP" <<'EOF'
[Desktop Entry]
Type=Application
Name=Mordent
GenericName=Music Player
Comment=Native, fast Apple Music client
Exec=mordent
Icon=mordent
Terminal=false
Categories=AudioVideo;Audio;Player;
Keywords=music;apple;player;
EOF
chmod 0644 "$DESKTOP"

install_icons_from() { # dir -> prints the number of icons installed
  local src="$1" n=0 f base size w
  local set="$src/mordent.iconset"
  if [[ -d "$set" ]]; then
    for f in "$set"/icon_*.png; do
      [[ -e "$f" ]] || continue
      base="$(basename "$f" .png)"
      base="${base#icon_}"
      size="${base%@2x}"
      if [[ "$base" == *@2x ]]; then
        w="${size%x*}"
        size="$((w * 2))x$((w * 2))"
      fi
      install -Dm0644 "$f" "$ICON_ROOT/$size/apps/mordent.png" && n=$((n + 1))
    done
  fi
  for f in "$src"/mordent-*.png; do
    [[ -e "$f" ]] || continue
    base="$(basename "$f" .png)"
    size="${base#mordent-}"
    install -Dm0644 "$f" "$ICON_ROOT/${size}x${size}/apps/mordent.png" && n=$((n + 1))
  done
  printf '%s' "$n"
}

SELF_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  SELF_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd -P)" || SELF_DIR=""
fi

icon_count=0
for candidate in "$SRC/assets/icon" "${SELF_DIR:+$SELF_DIR/../assets/icon}"; do
  [[ -n "$candidate" && -d "$candidate" ]] || continue
  icon_count="$(install_icons_from "$candidate")"
  [[ "$icon_count" -gt 0 ]] && break
done

if [[ "$icon_count" -gt 0 ]]; then
  say "  installed $icon_count icon(s) from assets/icon/"
else
  # Placeholder so the menu entry is not blank. It is a flat accent-coloured
  # 64x64 PNG; flagged here because it is not the real artwork.
  mkdir -p "$ICON_ROOT/64x64/apps"
  printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAYAAACqaXHeAAAAZklEQVR42u3QMREAAAQAQAUV0X+ykMP54Qt8dNZ8FgIECBAgQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQMB9C9WBAoYPJTcWAAAAAElFTkSuQmCC' |
    base64 -d >"$ICON_ROOT/64x64/apps/mordent.png"
  warn "no assets/icon/ in this release — installed a placeholder icon"
fi

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# finish
# ---------------------------------------------------------------------------
if ! out="$("$BIN_LINK" --help 2>&1)"; then
  die "$BIN_LINK --help failed — the install looks broken"
fi
printf '%s\n' "$out" | grep -q 'Mordent' || die "$BIN_LINK --help did not report Mordent"

say ""
say "Mordent $TAG installed to $PREFIX"
say "  binary:   $BIN_LINK"
say "  app menu: Mordent"
say "Open Mordent from your app menu — the wizard will offer the helper install"
say "(graphical password prompt, system service only)."
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "Add it to PATH:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
