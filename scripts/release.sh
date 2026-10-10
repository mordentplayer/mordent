#!/usr/bin/env bash
#
# release.sh — cut and push a CalVer release tag for Mordent.
#
# Tags are `YYYY-MM-DD.N` (N = 1-based counter within the day; see
# .github/workflows/release.yml, which fires on `20[0-9][0-9]-*`). The tag is
# created annotated on the current HEAD and pushed, which triggers the release
# workflow (per-OS bundles + the wrapper/container assets).
#
# The date is taken in UTC (`date -u`), matching the CI runner's clock.
#
# Refuses to run unless the working tree is clean and HEAD is on `main`.
#
# Usage:
#   scripts/release.sh [--dry-run]
#
#   --dry-run   Print what would be tagged and pushed; change nothing.
#
# Environment:
#   RELEASE_REMOTE   Git remote to tag/push (default: origin).
#
set -euo pipefail

DRY_RUN=0
usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "release.sh: unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
done

command -v git >/dev/null 2>&1 || { echo "release.sh: git is required" >&2; exit 1; }
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
  echo "release.sh: not inside a git work tree" >&2
  exit 1
}

# --- guards: on main, clean tree, usable remote -----------------------------
branch="$(git symbolic-ref --quiet --short HEAD || true)"
if [[ "$branch" != "main" ]]; then
  echo "release.sh: refusing to tag: HEAD is on '${branch:-<detached>}', not 'main'" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "release.sh: refusing to tag: working tree is dirty:" >&2
  git status --short >&2
  exit 1
fi

remote="${RELEASE_REMOTE:-origin}"
if ! git remote get-url "$remote" >/dev/null 2>&1; then
  echo "release.sh: remote '$remote' not found (set RELEASE_REMOTE)" >&2
  exit 1
fi

# --- pick the next tag ------------------------------------------------------
day="$(date -u +%Y-%m-%d)"

# Every remote tag for today, one per line (e.g. 2026-10-10.1).
existing="$(git ls-remote --tags --refs "$remote" "refs/tags/${day}.*" 2>/dev/null |
  awk -F/ '{print $NF}')"

count=0
while IFS= read -r t; do
  [[ -z "$t" ]] && continue
  suffix="${t#"${day}".}"
  if [[ "$suffix" =~ ^[0-9]+$ ]]; then
    count=$((count + 1))
  fi
done <<<"$existing"

next=$((count + 1))
tag="${day}.${next}"
# Robust against gaps (a deleted/odd tag): never reuse an existing name.
while printf '%s\n' "$existing" | grep -qx "$tag" ||
  git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; do
  next=$((next + 1))
  tag="${day}.${next}"
done

echo "branch:   $branch"
echo "HEAD:     $(git rev-parse --short HEAD) $(git log -1 --pretty=%s)"
echo "remote:   $remote ($(git remote get-url "$remote"))"
echo "today:    $day (UTC)"
echo "existing: ${existing//$'\n'/ }${existing:+ }"
echo "next tag: $tag"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "[dry-run] would run: git tag -a $tag -m 'Mordent $tag' && git push $remote $tag"
  exit 0
fi

git tag -a "$tag" -m "Mordent ${tag}"
git push "$remote" "$tag"

# --- print the release URL --------------------------------------------------
url="$(git remote get-url "$remote")"
host=""
path="$url"
case "$url" in
  git@*:*) host="${url#git@}"; host="${host%%:*}"; path="${url#*:}" ;;
  ssh://git@*) rest="${url#ssh://git@}"; host="${rest%%/*}"; path="${rest#*/}" ;;
  https://*) host="${url#https://}"; host="${host%%/*}"; path="${url#https://*/}" ;;
  http://*) host="${url#http://}"; host="${host%%/*}"; path="${url#http://*/}" ;;
esac
path="${path%.git}"
if [[ -n "$host" ]]; then
  echo "release: https://${host}/${path}/releases/tag/${tag}"
else
  echo "release: pushed tag ${tag}"
fi
