//! Runtime theme loaded from `~/.config/amc/theme.toml`, hot-reloadable.
//!
//! The file is optional: when absent, the built-in [`Theme::default`] is used.
//! Every key is optional too and falls back to the built-in default on its
//! own, so a file with only `accent = "#4dd0fa"` is valid. The app polls the
//! file's mtime about once per second while it is open, so editing the file
//! recolors the running UI without a restart.

use crate::fonts::FontChoice;
use eframe::egui;
use std::path::{Path, PathBuf};

/// Raw `theme.toml` shape; all keys optional (`#[serde(default)]`).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ThemeFile {
    accent: Option<String>,
    lossless: Option<String>,
    bg: Option<String>,
    panel: Option<String>,
    text: Option<String>,
    weak: Option<String>,
    opacity: Option<f32>,
    font: Option<String>,
}

/// Active theme. Colors are opaque here; `opacity` is applied when the values
/// are pushed into egui's visuals / the window clear color.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Highlights: selection, hyperlinks, progress bar, active toggles.
    pub accent: egui::Color32,
    /// "lossless" badge.
    pub lossless: egui::Color32,
    /// Window background (egui `window_fill` + clear color).
    pub bg: egui::Color32,
    /// Panel background (`panel_fill`).
    pub panel: egui::Color32,
    /// Normal text.
    pub text: egui::Color32,
    /// Secondary / muted text.
    pub weak: egui::Color32,
    /// Painted background alpha, 0.0-1.0 (needs a compositor to see through).
    pub opacity: f32,
    /// Which bundled font set the UI uses (`font` key).
    pub font: FontChoice,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: egui::Color32::from_rgb(0xfa, 0x4d, 0x6a),
            lossless: egui::Color32::from_rgb(0x6a, 0xd1, 0xa8),
            bg: egui::Color32::from_gray(27),
            panel: egui::Color32::from_gray(27),
            text: egui::Color32::from_gray(150),
            weak: egui::Color32::from_gray(90),
            opacity: 1.0,
            font: FontChoice::Inter,
        }
    }
}

pub fn theme_path() -> PathBuf {
    crate::config::home().join(".config/amc/theme.toml")
}

/// Parse `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` (leading `#` optional) into
/// straight (unmultiplied) RGBA bytes.
pub fn parse_hex(s: &str) -> Result<[u8; 4], String> {
    let h = s.trim();
    let h = h.strip_prefix('#').unwrap_or(h);
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{s:?} is not hexadecimal"));
    }
    let d = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).unwrap() * 17;
    let dd = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap();
    match h.len() {
        3 | 4 => Ok([d(0), d(1), d(2), if h.len() == 4 { d(3) } else { 255 }]),
        6 | 8 => Ok([dd(0), dd(2), dd(4), if h.len() == 8 { dd(6) } else { 255 }]),
        _ => Err(format!("{s:?} must have 3, 4, 6 or 8 hex digits")),
    }
}

/// Linear blend; used to derive widget tints from the accent.
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

impl Theme {
    /// Load `theme.toml` from the default location. The second value is a list
    /// of human-readable warnings (bad hex, bad TOML); a missing file is not a
    /// warning. Any key that fails to parse keeps its built-in default.
    pub fn load() -> (Self, Vec<String>) {
        Self::load_from(&theme_path())
    }

    fn load_from(path: &Path) -> (Self, Vec<String>) {
        let mut theme = Self::default();
        let mut warns = Vec::new();
        // Missing / unreadable file: silently use the built-in defaults.
        let Ok(text) = std::fs::read_to_string(path) else {
            return (theme, warns);
        };
        let file: ThemeFile = match toml::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                let e = e.to_string();
                warns.push(format!("theme.toml: {e}"));
                return (theme, warns);
            }
        };
        for (name, raw, slot) in [
            ("accent", &file.accent, &mut theme.accent),
            ("lossless", &file.lossless, &mut theme.lossless),
            ("bg", &file.bg, &mut theme.bg),
            ("panel", &file.panel, &mut theme.panel),
            ("text", &file.text, &mut theme.text),
            ("weak", &file.weak, &mut theme.weak),
        ] {
            let Some(raw) = raw else { continue };
            match parse_hex(raw) {
                // strip any alpha the user wrote; `opacity` is the single knob
                Ok(c) => *slot = egui::Color32::from_rgb(c[0], c[1], c[2]),
                Err(e) => warns.push(format!("theme.toml: {name}: {e} (using default)")),
            }
        }
        if let Some(o) = file.opacity {
            if o.is_finite() {
                theme.opacity = o.clamp(0.0, 1.0);
            } else {
                warns.push("theme.toml: opacity must be a number in 0.0-1.0".into());
            }
        }
        if let Some(raw) = file.font.as_deref() {
            match FontChoice::parse(raw) {
                Some(f) => theme.font = f,
                // unknown name: keep the built-in default font, warn once
                None => warns.push(format!(
                    "theme.toml: font: {raw:?} is not one of inter|dejavu|default (using inter)"
                )),
            }
        }
        (theme, warns)
    }

    /// (mtime, size) identity of the theme file, used to detect edits cheaply.
    /// `None` when the file is absent. Size is included because some
    /// filesystems only have 1-second mtime granularity.
    pub fn stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
        let md = std::fs::metadata(path).ok()?;
        Some((md.modified().ok()?, md.len()))
    }

    /// Push the theme into egui's global visuals: panel/window fills, text and
    /// weak text, accent-tinted widgets, selection stroke and hyperlinks.
    pub fn apply(&self, ctx: &egui::Context) {
        let mut v = egui::Visuals::dark();

        // Backgrounds honor `opacity`; foregrounds stay opaque.
        v.panel_fill = self.panel.gamma_multiply(self.opacity);
        v.window_fill = self.bg.gamma_multiply(self.opacity);
        v.faint_bg_color = mix(self.panel, self.accent, 0.06);
        v.extreme_bg_color = mix(self.bg, egui::Color32::BLACK, 0.45);
        v.code_bg_color = mix(self.panel, self.accent, 0.14);

        v.weak_text_color = Some(self.weak);
        v.override_text_color = None;
        v.hyperlink_color = self.accent;

        v.selection.bg_fill = self.accent.gamma_multiply(0.55);
        v.selection.stroke = egui::Stroke::new(1.0_f32, self.accent);

        v.widgets.noninteractive.weak_bg_fill = self.panel.gamma_multiply(self.opacity);
        v.widgets.noninteractive.bg_fill = self.panel.gamma_multiply(self.opacity);
        v.widgets.noninteractive.bg_stroke.color = mix(self.panel, self.accent, 0.35);
        v.widgets.noninteractive.fg_stroke.color = self.text;
        v.widgets.inactive.fg_stroke.color = self.text;
        v.widgets.inactive.weak_bg_fill = mix(egui::Color32::from_gray(60), self.accent, 0.12);
        v.widgets.inactive.bg_fill = mix(egui::Color32::from_gray(60), self.accent, 0.12);
        v.widgets.inactive.bg_stroke = egui::Stroke::new(
            1.0_f32,
            mix(egui::Color32::from_gray(60), self.accent, 0.3),
        );
        v.widgets.hovered.fg_stroke.color = mix(self.text, egui::Color32::WHITE, 0.35);
        v.widgets.hovered.weak_bg_fill = mix(egui::Color32::from_gray(70), self.accent, 0.22);
        v.widgets.hovered.bg_fill = mix(egui::Color32::from_gray(70), self.accent, 0.22);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, self.accent);
        v.widgets.active.fg_stroke.color = mix(self.text, egui::Color32::WHITE, 0.6);
        v.widgets.active.weak_bg_fill = mix(egui::Color32::from_gray(55), self.accent, 0.32);
        v.widgets.active.bg_fill = mix(egui::Color32::from_gray(55), self.accent, 0.32);
        v.widgets.active.bg_stroke = egui::Stroke::new(1.5_f32, self.accent);
        v.widgets.open.fg_stroke.color = self.text;
        v.widgets.open.bg_fill = self.panel.gamma_multiply(self.opacity);
        v.widgets.open.weak_bg_fill = mix(self.panel, self.accent, 0.1);
        v.widgets.open.bg_stroke.color = mix(self.panel, self.accent, 0.35);

        // Deterministic theme: the app is always a dark, themed UI, whatever
        // the desktop prefers.
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.set_visuals_of(egui::Theme::Dark, v.clone());
        ctx.set_visuals_of(egui::Theme::Light, v);
    }

    /// Window clear color, so the transparent viewport shows `bg` at `opacity`.
    pub fn clear_color(&self) -> [f32; 4] {
        self.bg.gamma_multiply(self.opacity).to_normalized_gamma_f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms() {
        assert_eq!(parse_hex("#fa4d6a").unwrap(), [0xfa, 0x4d, 0x6a, 0xff]);
        assert_eq!(parse_hex("fa4d6a").unwrap(), [0xfa, 0x4d, 0x6a, 0xff]);
        assert_eq!(parse_hex("#fff").unwrap(), [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(parse_hex("#4dd0fa80").unwrap(), [0x4d, 0xd0, 0xfa, 0x80]);
        assert!(parse_hex("#nope").is_err());
        assert!(parse_hex("#12345").is_err());
    }

    #[test]
    fn missing_file_is_silent_default() {
        let p = std::env::temp_dir().join("amc-theme-does-not-exist.toml");
        let _ = std::fs::remove_file(&p);
        let (t, warns) = Theme::load_from(&p);
        assert!(warns.is_empty());
        assert_eq!(t.accent, Theme::default().accent);
        assert_eq!(t.opacity, 1.0);
        assert_eq!(t.font, FontChoice::Inter);
    }

    /// `font` accepts only the three known names; anything else keeps the
    /// built-in default (inter) and warns once, like a bad colour.
    #[test]
    fn font_key_parses_and_falls_back() {
        let p = std::env::temp_dir().join("amc-theme-font.toml");
        for (text, want, warn) in [
            ("font = \"inter\"\n", FontChoice::Inter, false),
            ("font = \" dejavu \"\n", FontChoice::DejaVu, false),
            ("font = \"default\"\n", FontChoice::Default, false),
            ("font = \"comic sans\"\n", FontChoice::Inter, true),
            ("font = 3\n", FontChoice::Inter, true), // wrong type: whole file fails
        ] {
            std::fs::write(&p, text).unwrap();
            let (t, warns) = Theme::load_from(&p);
            assert_eq!(t.font, want, "for {text:?}");
            assert_eq!(!warns.is_empty(), warn, "for {text:?}: {warns:?}");
            if warn {
                assert!(warns[0].contains("theme.toml"), "for {text:?}");
            }
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn partial_and_invalid_keys() {
        let p = std::env::temp_dir().join("amc-theme-test.toml");
        std::fs::write(
            &p,
            "accent = \"#4dd0fa\"\nopacity = 2.0\nweak = \"zz\"\nbg = \"#4dd0fa80\"\n",
        )
        .unwrap();
        let (t, warns) = Theme::load_from(&p);
        assert_eq!(t.accent, egui::Color32::from_rgb(0x4d, 0xd0, 0xfa));
        assert_eq!(t.bg, egui::Color32::from_rgb(0x4d, 0xd0, 0xfa)); // alpha dropped
        assert_eq!(t.lossless, Theme::default().lossless); // untouched
        assert_eq!(t.opacity, 1.0); // clamped
        assert_eq!(t.weak, Theme::default().weak); // bad hex -> default
        assert_eq!(warns.len(), 1);
        let _ = std::fs::remove_file(&p);
    }
}
