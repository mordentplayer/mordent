#!/usr/bin/env bash
# Regenerate assets/icon/ from the master SVG (assets/icon/mordent-icon.svg).
#
# Only needed when the artwork changes; the generated files are committed so
# neither CI nor a user needs these tools.
#
# Requirements: python3 with `cairosvg` (SVG → PNG), ImageMagick (`convert`, for
# the multi-size .ico). On macOS, `iconutil` compiles the iconset to .icns; on
# Linux `icnsutil`/`png2icns` can do the same, and CI uses `iconutil` directly
# from assets/icon/mordent.iconset (which is why it is checked in).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
dir=$(cd "$here/../../assets/icon" && pwd)
svg="$dir/mordent-icon.svg"

python3 - "$svg" "$dir" <<'PY'
import sys, os, cairosvg
svg, out = sys.argv[1], sys.argv[2]

for size in (256, 512):
    cairosvg.svg2png(url=svg, write_to=f"{out}/mordent-{size}.png",
                     output_width=size, output_height=size)

# iconutil expects exactly these names/sizes in the .iconset directory.
iconset = f"{out}/mordent.iconset"
os.makedirs(iconset, exist_ok=True)
for base in (16, 32, 128, 256, 512):
    for scale in (1, 2):
        size = base * scale
        name = f"icon_{base}x{base}" + ("@2x" if scale == 2 else "") + ".png"
        cairosvg.svg2png(url=svg, write_to=os.path.join(iconset, name),
                         output_width=size, output_height=size)
PY

# multi-size .ico for Windows/desktop integration (16…256)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
python3 - "$svg" "$tmp" <<'PY'
import sys, cairosvg
svg, out = sys.argv[1], sys.argv[2]
for size in (48, 64):
    cairosvg.svg2png(url=svg, write_to=f"{out}/{size}.png",
                     output_width=size, output_height=size)
PY
convert "$dir/mordent.iconset/icon_16x16.png" \
        "$dir/mordent.iconset/icon_32x32.png" \
        "$tmp/48.png" "$tmp/64.png" \
        "$dir/mordent.iconset/icon_128x128.png" \
        "$dir/mordent.iconset/icon_256x256.png" \
        "$dir/mordent.ico"

ls -l "$dir"
