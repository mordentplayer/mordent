#!/usr/bin/env bash
# Repackage a Mordent linux bundle as an AppImage.
#
# The input is the *same directory the linux tarball is made from* (see the
# "Assemble bundle" step in .github/workflows/release.yml): a tree containing
#   mordent  wrapper-lite  rootfs/  scripts/  WRAPPER_COMMIT  licenses/  README.md
# so the AppImage payload is identical to the tarball's by construction.
#
# Layout inside the AppDir (this is what makes the wizard's *install the helper
# locally* path work from the mounted image — src/setup.rs `find_scripts_dir_in`
# probes <exe_dir>/scripts, <exe_dir>/../scripts, ./scripts):
#   usr/bin/mordent        ← the app (exe_dir = <AppDir>/usr/bin)
#   usr/scripts/           ← exe_dir/../scripts  ✔
#   usr/wrapper-lite       ← bundle_dir = scripts' parent = <AppDir>/usr
#   usr/rootfs/
#   usr/licenses/  usr/WRAPPER_COMMIT  usr/share/doc/mordent/README.md
#   mordent.desktop  mordent.png  .DirIcon  AppRun   (at the AppDir root)
#
# Usage: build-appimage.sh <bundle-dir> [output.AppImage]
# Env:   APPIMAGETOOL       path to appimagetool (default: appimagetool on PATH)
#        APPIMAGETOOL_ARGS  extra flags; CI passes --appimage-extract-and-run so
#                           the tool runs without FUSE
set -euo pipefail

bundle=${1:?usage: build-appimage.sh <bundle-dir> [output.AppImage]}
out=${2:-Mordent-x86_64.AppImage}
appimagetool=${APPIMAGETOOL:-appimagetool}
# shellcheck disable=SC2086 # deliberate word-splitting of the flag list
appimagetool_args=${APPIMAGETOOL_ARGS:-}

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
icon="$repo/assets/icon/mordent-256.png"

for f in mordent wrapper-lite rootfs scripts licenses README.md WRAPPER_COMMIT; do
  [ -e "$bundle/$f" ] || { echo "error: bundle is missing $f" >&2; exit 1; }
done
[ -f "$icon" ] || { echo "error: missing icon $icon" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir="$work/Mordent.AppDir"
mkdir -p "$appdir/usr/bin" \
         "$appdir/usr/share/icons/hicolor/256x256/apps" \
         "$appdir/usr/share/applications" \
         "$appdir/usr/share/doc/mordent"

# payload, at the offsets `find_scripts_dir_in` expects
install -m 0755 "$bundle/mordent"      "$appdir/usr/bin/mordent"
install -m 0755 "$bundle/wrapper-lite" "$appdir/usr/wrapper-lite"
cp -a "$bundle/rootfs"                 "$appdir/usr/rootfs"
cp -a "$bundle/scripts"                "$appdir/usr/scripts"
install -m 0644 "$bundle/WRAPPER_COMMIT" "$appdir/usr/WRAPPER_COMMIT"
cp -a "$bundle/licenses"               "$appdir/usr/licenses"
install -m 0644 "$bundle/README.md"    "$appdir/usr/share/doc/mordent/README.md"

# desktop integration + icon
install -m 0755 "$here/AppRun"           "$appdir/AppRun"
install -m 0644 "$here/mordent.desktop"  "$appdir/mordent.desktop"
install -m 0644 "$here/mordent.desktop"  "$appdir/usr/share/applications/mordent.desktop"
install -m 0644 "$icon"                  "$appdir/usr/share/icons/hicolor/256x256/apps/mordent.png"
install -m 0644 "$icon"                  "$appdir/mordent.png"
ln -sf mordent.png                       "$appdir/.DirIcon"

# fail loudly rather than shipping an image the wizard cannot self-install from
test -x "$appdir/usr/bin/mordent"
test -x "$appdir/usr/wrapper-lite"
test -x "$appdir/usr/rootfs/system/bin/lite"
test -f "$appdir/usr/scripts/install-wrapper.sh"

ARCH=x86_64 "$appimagetool" $appimagetool_args --no-appstream "$appdir" "$out"
echo "built: $out"
