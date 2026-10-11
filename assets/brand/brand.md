# Mordent brand kit

The mark is a **mordent**: the musical ornament written as a zigzag over a note,
abstracted to a single confident stroke — a 60° zigzag with a 75° descending
tail, its ends cut horizontally on a common baseline (so the figure sits as if
typeset, and reads as an *M* for Mordent). One filled path, ten points, no
gradients on the figure itself.

Everything here is derived from the same geometry. The app does not read these
files at runtime: `src/brand.rs` rasterises the identical outline for the tray
and window icons, so there is no image decoder in the binary.

## Palette

| Token | Value | Use |
| --- | --- | --- |
| accent | `#fa4d6a` | the figure (`src/theme.rs` default accent) |
| panel light | `#26262a` | app-icon panel, radial centre at 32 %/24 % |
| panel dark | `#161619` | app-icon panel, radial edge |
| surface | `#1b1b1b` | dark backgrounds (README banner: `#232327`→`#141416`) |
| surface raised | `#26262a` | cards, header panels |
| ink | `#f5f5f7` | wordmark and monochrome mark on dark |
| muted | `#9a9aa4` | taglines, secondary type |

Contrast: accent on `#1b1b1b` 5.2:1 · ink on `#1b1b1b` 15.8:1 · accent on white
3.3:1 (fine for graphics, not for body type) · ink on white 17.2:1.

## Files

| File | What it is | Where it is used |
| --- | --- | --- |
| `mordent-mark.svg` | primary mark, transparent, accent fill (5 % built-in padding) | README, docs, site, slides — dark *and* light |
| `mordent-mark-mono.svg` | same figure, `fill="currentColor"` | light backgrounds (set `color: #1b1b1b`), print, one-colour merch, favicon/tray source |
| `mordent-mark-on-dark.svg` | rounded-square panel + figure — **app-icon master** | source for `assets/icon/**`; org avatar source |
| `mordent-lockup.svg` | panel mark + "Mordent" (Inter SemiBold outlines), 1439×512 | site header, docs, dark title slides |
| `mordent-lockup-mono.svg` | transparent mark + wordmark, `currentColor` | light surfaces, single-colour lockup |
| `readme-banner.svg` / `.png` | 1280×320 banner (figure + wordmark + tagline) | top of the README |
| `org-avatar-460.png` | 460×460, full-bleed panel, figure at 64 % | GitHub org avatar (survives the circle crop) |
| `assets/icon/**` | generated: `mordent-16/32/48/64/128/256/512.png`, `mordent.ico` (16→256), `mordent.iconset/` (10 files) | AppImage, `Mordent.app`, `scripts/install.sh` (maps each `mordent-<N>.png` to `<N>x<N>` in the icon theme) |

Regenerate the icon set after any artwork change:

```
bash packaging/icons/make-icons.sh          # cairosvg + ImageMagick; icons are committed
```

## Variants and minimum sizes

- **Full colour** — accent figure on the background; use from **16 px** up. At
  16 px it is a pink squiggle on nothing; give it the panel or the mono ink
  treatment to keep the edge sharp.
- **On dark (panel)** — the app icon: panel inset 4.7 %, corner radius 22.8 % of
  the panel, figure 72 % of the panel width. Holds from 16 px (`.iconset`
  floor) to any size.
- **Monochrome** — one path, `currentColor`. Use it on light backgrounds; this
  *is* the contrast variant. A darker keyline around the accent figure was
  tried and muddies the mark below 64 px, so it was dropped.
- **Tray** — 24 px, figure only (transparent), ribbon thickened 0.56 → 0.70 so
  the four strokes stay separate at 16–24 px. Drawn by `src/brand.rs`.

Minimum sizes: mark 16 px (12 px mono, to be checked in place) · tray 16 px ·
app icon 16 px · lockup 240 px wide (below that, use the mark alone) · banner
render at ≥ 640 px wide.

## Clear space

Keep **0.25 × the mark's width** free on all four sides of the ink (the height
ratio makes this ≈ 0.34 × height). The transparent SVGs already carry a 5 %
pad, so the file edge is not the clear-space line — measure from the figure.

The figure must not be redrawn, re-proportioned or rotated. Don't put the mark
inside another shape, don't outline it, don't recolour it outside the palette,
and don't set type inside the mark.

## Type

The wordmark is **Inter SemiBold** (SIL Open Font License), converted to
outlines in the lockups so no font is needed to render them. The repository's
bundled `assets/fonts/Inter-Regular.ttf` is Inter 4.1 Regular — byte-identical
to the release the wordmark was cut from. "Mordent" is set at cap height 0.29 ×
the panel mark's height, tracking 0, aligned on ink (not advance) edges.
