#!/usr/bin/env bash
# Assemble Mordent.app from a release binary and zip it for the release page.
#
# Must run on macOS: `iconutil`, `plutil` and `ditto` are part of the OS / Xcode
# command line tools. The icon comes from the checked-in iconset
# (assets/icon/mordent.iconset), which `iconutil` compiles to Resources/mordent.icns.
#
# Usage: build-app.sh <binary> <version> <work-dir> <out.zip>
#   <binary>    target/<triple>/release/mordent
#   <version>   CalVer release tag (or "test"); goes into CFBundle*VersionString
#   <work-dir>  where Mordent.app is assembled (created if missing)
#   <out.zip>   zip written with `ditto -c -k --keepParent` (contains Mordent.app/)
set -euo pipefail

bin=${1:?usage: build-app.sh <binary> <version> <work-dir> <out.zip>}
version=${2:?missing version}
work=${3:?missing work dir}
out_zip=${4:?missing output zip}

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
iconset="$repo/assets/icon/mordent.iconset"

[ -x "$bin" ] || { echo "error: binary not executable: $bin" >&2; exit 1; }
[ -d "$iconset" ] || { echo "error: missing iconset: $iconset" >&2; exit 1; }

app="$work/Mordent.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

install -m 0755 "$bin" "$app/Contents/MacOS/mordent"
sed "s/@@VERSION@@/${version}/g" "$here/Info.plist.in" >"$app/Contents/Info.plist"
iconutil -c icns -o "$app/Contents/Resources/mordent.icns" "$iconset"

# Catch a malformed bundle here rather than in a user's Finder.
plutil -lint "$app/Contents/Info.plist"
test -f "$app/Contents/Resources/mordent.icns"
test -x "$app/Contents/MacOS/mordent"
test -f "$app/Contents/Info.plist"

mkdir -p "$(dirname "$out_zip")"
rm -f "$out_zip"
ditto -c -k --keepParent "$app" "$out_zip"
test -s "$out_zip"

echo "built: $app"
echo "zip:   $out_zip ($(du -h "$out_zip" | cut -f1))"
