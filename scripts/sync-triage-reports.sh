#!/usr/bin/env bash
#
# sync-triage-reports.sh — mirror the `triage-reports` branch to this machine.
#
# The triage pipeline publishes one Markdown report per issue to the
# `triage-reports` branch. This script exports that branch (via `git archive`,
# so no worktree or branch checkout is needed) into a local directory, which
# makes the writeups readable without touching the working copy of the repo.
#
# Designed for cron: silent on success, non-zero on failure. Set
# MORDENT_SYNC_VERBOSE=1 (or pass -v) to always print a summary.
#
# Usage:
#   scripts/sync-triage-reports.sh [-v]
#
# Environment overrides:
#   MORDENT_REPO        owner/name            (default mordentplayer/mordent)
#   MORDENT_REMOTE      git remote URL        (default https://github.com/<repo>.git)
#   MORDENT_TRIAGE_DIR  destination           (default ~/mordent-triage-reports)
#   TRIAGE_REPORT_BRANCH branch name          (default triage-reports)
#
set -euo pipefail

verbose=0
if [ "${1:-}" = "-v" ] || [ "${1:-}" = "--verbose" ]; then
  verbose=1
fi
if [ "${MORDENT_SYNC_VERBOSE:-0}" = "1" ]; then
  verbose=1
fi

repo="${MORDENT_REPO:-mordentplayer/mordent}"
branch="${TRIAGE_REPORT_BRANCH:-triage-reports}"
dest="${MORDENT_TRIAGE_DIR:-$HOME/mordent-triage-reports}"
remote="${MORDENT_REMOTE:-https://github.com/${repo}.git}"

log() { [ "$verbose" = "1" ] && echo "sync-triage-reports: $*" || true; }

mkdir -p "$dest"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

git -C "$tmp" init -q
git -C "$tmp" remote add origin "$remote"

if ! git -C "$tmp" fetch -q --depth=1 origin "$branch"; then
  echo "sync-triage-reports: branch '$branch' not found on $repo" >&2
  exit 0
fi

# Replace the mirror contents (never delete $dest itself).
if [ -d "$dest" ]; then
  find "$dest" -mindepth 1 -delete
fi

# The branch layout is always `triage-reports/<file>.md` (see bin/publish.sh),
# so strip that one leading component: the mirror then reads as a flat
# directory of reports rather than ~/mordent-triage-reports/triage-reports/.
git -C "$tmp" archive FETCH_HEAD | tar -x -C "$dest" --strip-components=1

count="$(find "$dest" -type f | wc -l | tr -d ' ')"
log "mirrored ${count} file(s) from ${repo}#${branch} into ${dest}"

if [ "$verbose" = "1" ]; then
  find "$dest" -type f -printf '  %P\n' | sort
fi
