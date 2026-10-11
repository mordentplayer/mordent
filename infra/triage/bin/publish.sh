#!/usr/bin/env bash
#
# Publish one or more repo-relative files to the triage-reports branch.
#
# Runs on the runner, OUTSIDE the model sandbox, using GITHUB_TOKEN. Only the
# paths passed on the command line are staged, so nothing else the model wrote
# into the checkout can be committed.
#
# Usage: publish.sh triage-reports/issue-12.md [more paths...]
# Requires: GH_TOKEN, GITHUB_REPOSITORY
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
branch="${TRIAGE_REPORT_BRANCH:-triage-reports}"

if [ "$#" -eq 0 ]; then
  echo "publish: nothing to publish"
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$work" init -q
# Canonical GitHub Actions bot identity. The previous value
# (mordent-triage[bot] <triage@users.noreply.github.com>) resolved to an
# unrelated real user account named "triage" on github.com, which misattributed
# the report commits to a human.
git -C "$work" config user.name "github-actions[bot]"
git -C "$work" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
# TRIAGE_PUBLISH_REMOTE exists so the publish mechanics can be dry-run against
# a local bare repo; in CI it is unset and the token URL is used.
remote="${TRIAGE_PUBLISH_REMOTE:-https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git}"
git -C "$work" remote add origin "$remote"

if git -C "$work" fetch --depth=1 origin "$branch" 2>/dev/null; then
  git -C "$work" checkout -q FETCH_HEAD
else
  # First ever report: start the branch from scratch.
  git -C "$work" checkout -q --orphan "$branch"
fi

staged=0
for src in "$@"; do
  if [ ! -f "$src" ]; then
    echo "publish: skipping missing $src" >&2
    continue
  fi
  dest="$work/$src"
  mkdir -p "$(dirname "$dest")"
  cp "$src" "$dest"
  git -C "$work" add -- "$src"
  staged=$((staged + 1))
done

if [ "$staged" -eq 0 ]; then
  echo "publish: nothing to publish"
  exit 0
fi

if git -C "$work" diff --cached --quiet; then
  echo "publish: no changes to publish"
  exit 0
fi

git -C "$work" commit -q -m "triage: ${TRIAGE_COMMIT_MSG:-update reports} [skip ci]"
git -C "$work" push -q origin "HEAD:${branch}"
echo "publish: pushed ${staged} file(s) to ${branch}"
