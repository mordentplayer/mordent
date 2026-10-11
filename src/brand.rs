//! The Mordent mark, rasterised in-binary.
//!
//! Two consumers:
//!
//! * [`tray_argb`] — the tray icon (`src/tray.rs`): a transparent accent mark
//!   sized for 16–24 px panels, in the ARGB byte order ksni/StatusNotifierItem
//!   wants.
//! * [`app_icon_rgba`] — the window icon (`src/ui.rs`): the rounded-square app
//!   mark, in the RGBA byte order egui's `IconData` wants.
//!
//! The artwork matches `assets/brand/mordent-mark*.svg`: a mordent ornament
//! abstracted to a single stroke — a 60° zigzag with a 75° descending tail,
//! ribbon 0.56 of a zigzag run, ends cut horizontally on a common baseline,
//! mitred joins.  It is rasterised here instead of being loaded from a PNG
//! because nothing in the dependency tree decodes images, and a logo is not
//! worth a decoder: the geometry below is ten points per variant.
//!
//! The outline constants are the *same* ten vertices as the SVG path, mapped
//! so the ink box is `x 0..1`, `y 0..aspect` (y grows downwards).  If the
//! artwork is ever redrawn, redraw `assets/brand/**` first and copy the new
//! path here; the tray variant differs only in its ribbon width.

/// Accent of the default theme (`src/theme.rs`) and of the brand palette.
const ACCENT: [u8; 3] = [0xfa, 0x4d, 0x6a];
/// Panel of the app mark: #26262a → #161619, lifted towards the top-left,
/// exactly as `assets/brand/mordent-mark-on-dark.svg` paints it.
const PANEL_HI: [u8; 3] = [0x26, 0x26, 0x2a];
const PANEL_LO: [u8; 3] = [0x16, 0x16, 0x19];

/// Tray pixmap edge, in pixels: the wide end of the range hosts use for panel
/// icons, and the size where the mark's five vertices are tightest.
pub const TRAY_SIZE: u32 = 24;

/// Window-icon edge, in pixels (egui wants a multiple of 4).  Taskbars and
/// docks scale this down; 128 covers a 2× 64 px slot without interpolation.
pub const WINDOW_ICON_SIZE: u32 = 128;

/// Brand mark outline (see `assets/brand/mordent-mark-mono.svg`).
const MARK: [[f32; 2]; 10] = [
    [0.156809, 0.595937],
    [0.320905, 0.311714],
    [0.563405, 0.731737],
    [0.782746, 0.351827],
    [0.859409, 0.637939],
    [1.000000, 0.637939],
    [0.829065, 0.000000],
    [0.563405, 0.460136],
    [0.320905, 0.040113],
    [0.000000, 0.595937],
];

/// Tray variant: the same figure with the ribbon thickened 0.56 → 0.70, so the
/// four strokes stay separate once the ink is ~20 px wide.
const MARK_TRAY: [[f32; 2]; 10] = [
    [0.188576, 0.615643],
    [0.327590, 0.374863],
    [0.560892, 0.778954],
    [0.766343, 0.423103],
    [0.830927, 0.664134],
    [1.000000, 0.664134],
    [0.822046, 0.000000],
    [0.560892, 0.452331],
    [0.327590, 0.048240],
    [0.000000, 0.615643],
];

/// Supersampling per axis used for the mark's edges.
const SS: u32 = 4;

/// Fraction of the tray box the mark's ink fills.
const TRAY_INK: f32 = 0.92;

/// App icon: panel inset, corner radius and mark width, as fractions of the
/// square — the numbers `assets/brand/mordent-mark-on-dark.svg` uses
/// (48/1024, 212/1024 and 0.72 × 928/1024).
const PANEL_INSET: f32 = 0.046875;
const PANEL_RADIUS: f32 = 0.20703125;
const PANEL_MARK_W: f32 = 0.6525;

/// The tray icon: [`TRAY_SIZE`]², transparent, ARGB network byte order.
pub fn tray_argb() -> Vec<u8> {
    let n = TRAY_SIZE as usize;
    let nf = TRAY_SIZE as f32;
    let s = TRAY_INK * nf;
    let (ox, oy) = ((nf - s) / 2.0, (nf - s * aspect(&MARK_TRAY)) / 2.0);
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            let a = coverage(&MARK_TRAY, px, py, s, ox, oy);
            if a == 0 {
                continue;
            }
            // ARGB, straight (not premultiplied) alpha: ksni's own example
            // rotates a decoder's RGBA bytes into this order.
            let i = (py * n + px) * 4;
            out[i] = a;
            out[i + 1] = ACCENT[0];
            out[i + 2] = ACCENT[1];
            out[i + 3] = ACCENT[2];
        }
    }
    out
}

/// The window icon: the app mark — dark rounded panel, accent figure — as
/// `size`² RGBA, unmultiplied like `egui::IconData`.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    let n = size as usize;
    let nf = size as f32;
    let half = 0.5 - PANEL_INSET;
    // Mark ink, mapped into the square the same way the SVG maps it.
    let s = PANEL_MARK_W * nf;
    let (ox, oy) = ((nf - s) / 2.0, (nf - s * aspect(&MARK)) / 2.0);
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            // Panel edge: analytic coverage from the rounded box's signed
            // distance, which is smoother than supersampling it.
            let (cu, cv) = ((px as f32 + 0.5) / nf, (py as f32 + 0.5) / nf);
            let cov = (0.5 - rounded_box(cu, cv, half, PANEL_RADIUS) * nf).clamp(0.0, 1.0);
            if cov == 0.0 {
                continue;
            }
            // Panel colour: the SVG's radial gradient, centred at 32 %/24 %
            // of the panel box with radius 0.95.
            let gu = (cu - PANEL_INSET) / (1.0 - 2.0 * PANEL_INSET);
            let gv = (cv - PANEL_INSET) / (1.0 - 2.0 * PANEL_INSET);
            let t = (((gu - 0.32).powi(2) + (gv - 0.24).powi(2)).sqrt() / 0.95).clamp(0.0, 1.0);
            let mut rgb = [
                mix(PANEL_HI[0], PANEL_LO[0], t),
                mix(PANEL_HI[1], PANEL_LO[1], t),
                mix(PANEL_HI[2], PANEL_LO[2], t),
            ];
            // Figure on top: it sits well inside the panel, so its own
            // coverage is all the compositing it needs.
            let m = coverage(&MARK, px, py, s, ox, oy) as f32 / 255.0;
            if m > 0.0 {
                for (c, ch) in rgb.iter_mut().enumerate() {
                    *ch = mix_f(*ch, ACCENT[c] as f32 / 255.0, m);
                }
            }
            let i = (py * n + px) * 4;
            out[i] = (rgb[0] * 255.0 + 0.5) as u8;
            out[i + 1] = (rgb[1] * 255.0 + 0.5) as u8;
            out[i + 2] = (rgb[2] * 255.0 + 0.5) as u8;
            out[i + 3] = (cov * 255.0 + 0.5) as u8;
        }
    }
    out
}

/// One channel of a `u8` colour, blended `t` of the way to `b`.
fn mix(a: u8, b: u8, t: f32) -> f32 {
    mix_f(a as f32 / 255.0, b as f32 / 255.0, t)
}

fn mix_f(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Height of an outline relative to its unit width.
fn aspect(poly: &[[f32; 2]]) -> f32 {
    poly.iter().map(|p| p[1]).fold(0.0, f32::max)
}

/// Signed distance from `(x, y)` to the rounded box of half-extent
/// `0.5 - inset` centred in the unit square, radius `r`, in canvas units.
fn rounded_box(x: f32, y: f32, half: f32, r: f32) -> f32 {
    let qx = (x - 0.5).abs() - (half - r);
    let qy = (y - 0.5).abs() - (half - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// Coverage (0 = empty, 255 = solid) of `poly` for output pixel `(px, py)`,
/// sampled `SS × SS` times.  `s` is the pixel width of the outline's unit and
/// `(ox, oy)` the pixel offset of its origin.
fn coverage(poly: &[[f32; 2]], px: usize, py: usize, s: f32, ox: f32, oy: f32) -> u8 {
    let mut hits = 0u32;
    for sy in 0..SS {
        for sx in 0..SS {
            let u = (px as f32 + (sx as f32 + 0.5) / SS as f32 - ox) / s;
            let v = (py as f32 + (sy as f32 + 0.5) / SS as f32 - oy) / s;
            if inside(poly, u, v) {
                hits += 1;
            }
        }
    }
    ((hits * 255 + SS * SS / 2) / (SS * SS)) as u8
}

/// Crossing-number point-in-polygon test.
fn inside(poly: &[[f32; 2]], x: f32, y: f32) -> bool {
    let mut hit = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = (poly[i][0], poly[i][1]);
        let (xj, yj) = (poly[j][0], poly[j][1]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            hit = !hit;
        }
        j = i;
    }
    hit
}
