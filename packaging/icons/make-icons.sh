#!/usr/bin/env bash
# Regenerate assets/icon/ from the brand master (assets/brand/mordent-mark-on-dark.svg).
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
svg=$(cd "$here/../../assets/brand" && pwd)/mordent-mark-on-dark.svg
[[ -f "$svg" ]] || { echo "error: missing brand master: $svg" >&2; exit 1; }

python3 - "$svg" "$dir" <<'PY'
import sys, os, cairosvg
svg, out = sys.argv[1], sys.argv[2]

# Flat PNG set. `scripts/install.sh` maps every mordent-<N>.png to
# <N>x<N> in the XDG icon theme; packaging/appimage uses mordent-256.png.
for size in (16, 32, 48, 64, 128, 256, 512):
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
convert "$dir/mordent-16.png" "$dir/mordent-32.png" "$dir/mordent-48.png" \
        "$dir/mordent-64.png" "$dir/mordent-128.png" "$dir/mordent-256.png" \
        "$dir/mordent.ico"

ls -l "$dir"
