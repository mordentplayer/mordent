//! Bundled UI fonts, compiled into the binary with `include_bytes!`.
//!
//! The app must render identically on a bare system, so no font file is ever
//! read from the filesystem: Inter Regular is the default proportional face and
//! DejaVu Sans is appended to *both* egui families as a symbol fallback
//! (egui's built-ins lack e.g. `⠿` U+283F, the queue drag handle). egui's own
//! fonts stay in the chain, so glyphs only they have (`⏸` U+23F8, `⏭` U+23ED,
//! `⏮` U+23EE live in its `emoji-icon-font`) keep rendering.
//!
//! Assets live in `assets/fonts/` with their licenses (`LICENSE-Inter.txt`,
//! `LICENSE-DejaVu.txt`); the `font` key in `theme.toml` selects the set.

use eframe::egui;
use std::sync::Arc;

/// Inter Regular (SIL Open Font License 1.1) — default proportional UI font.
const INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
/// DejaVu Sans Book (Bitstream Vera + public-domain additions) — symbols.
const DEJAVU_SANS: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

/// Font family name registered for the DejaVu fallback.
const DEJAVU: &str = "dejavu";
/// Font family name registered for Inter.
const INTER: &str = "inter";

/// `font` key in `theme.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontChoice {
    /// Bundled Inter Regular, with DejaVu Sans symbols behind it (default).
    #[default]
    Inter,
    /// Bundled DejaVu Sans as the primary face (and its own symbol fallback).
    DejaVu,
    /// egui's built-in fonts only — nothing bundled is installed.
    Default,
}

impl FontChoice {
    /// Parse a `font` value; `None` when it is not one of the known names.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "inter" => Some(Self::Inter),
            "dejavu" => Some(Self::DejaVu),
            "default" => Some(Self::Default),
            _ => None,
        }
    }

    /// The `font` key's spelling in `theme.toml` (round-trips with [`parse`]).
    ///
    /// [`parse`]: FontChoice::parse
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inter => "inter",
            Self::DejaVu => "dejavu",
            Self::Default => "default",
        }
    }

    /// Human label for the Appearance section's font menu.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inter => "Inter (bundled)",
            Self::DejaVu => "DejaVu Sans (bundled)",
            Self::Default => "egui built-in",
        }
    }

    /// Every choice, in menu order.
    pub const ALL: [FontChoice; 3] = [Self::Inter, Self::DejaVu, Self::Default];
}

/// Font definitions for `choice`: the choice's primary face first, egui's
/// built-ins behind it, DejaVu last as the symbol fallback for the proportional
/// *and* the monospace family. `FontChoice::Default` returns egui's built-in
/// definitions untouched.
///
/// Inter is only prepended to the proportional chain: `Hack` stays the
/// monospace face (`ui.monospace(...)` rows, ids) and simply gains DejaVu as a
/// symbol fallback.
pub fn definitions(choice: FontChoice) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    if choice == FontChoice::Default {
        return fonts;
    }
    let (primary, data) = match choice {
        FontChoice::Inter => (INTER, INTER_REGULAR),
        FontChoice::DejaVu => (DEJAVU, DEJAVU_SANS),
        FontChoice::Default => unreachable!("handled above"),
    };
    fonts
        .font_data
        .insert(primary.into(), Arc::new(egui::FontData::from_static(data)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, primary.into());
    // DejaVu Sans symbol fallback for both families. Registered once; when it
    // is itself the primary face it is already at the front of Proportional.
    fonts.font_data.insert(
        DEJAVU.into(),
        Arc::new(egui::FontData::from_static(DEJAVU_SANS)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let chain = fonts.families.entry(family).or_default();
        if !chain.iter().any(|n| n == DEJAVU) {
            chain.push(DEJAVU.into());
        }
    }
    fonts
}

/// Install the font chain for `choice` on `ctx`. Safe to call at runtime: egui
/// applies new font definitions at the start of the next pass (this is what the
/// `theme.toml` hot-reload uses to switch fonts without a restart).
pub fn apply(ctx: &egui::Context, choice: FontChoice) {
    ctx.set_fonts(definitions(choice));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn families(choice: FontChoice) -> (Vec<String>, Vec<String>) {
        let d = definitions(choice);
        let get = |f: egui::FontFamily| {
            d.families
                .get(&f)
                .map(|v| v.iter().map(|n| n.to_string()).collect())
                .unwrap_or_default()
        };
        (
            get(egui::FontFamily::Proportional),
            get(egui::FontFamily::Monospace),
        )
    }

    #[test]
    fn parse_names() {
        assert_eq!(FontChoice::parse("inter"), Some(FontChoice::Inter));
        assert_eq!(FontChoice::parse(" DejaVu "), Some(FontChoice::DejaVu));
        assert_eq!(FontChoice::parse("DEFAULT"), Some(FontChoice::Default));
        assert_eq!(FontChoice::parse("noto"), None);
        assert_eq!(FontChoice::default(), FontChoice::Inter);
    }

    #[test]
    fn inter_is_primary_with_dejavu_fallback_last() {
        let (prop, mono) = families(FontChoice::Inter);
        assert_eq!(prop.first().map(String::as_str), Some(INTER));
        assert_eq!(prop.last().map(String::as_str), Some(DEJAVU));
        // built-ins stay in the chain
        assert!(prop.iter().any(|n| n == "Ubuntu-Light"));
        // monospace keeps Hack first, gains the DejaVu symbol fallback
        assert_eq!(mono.first().map(String::as_str), Some("Hack"));
        assert_eq!(mono.last().map(String::as_str), Some(DEJAVU));
        assert!(!mono.iter().any(|n| n == INTER));
    }

    #[test]
    fn dejavu_primary_is_not_duplicated() {
        let (prop, mono) = families(FontChoice::DejaVu);
        assert_eq!(prop.first().map(String::as_str), Some(DEJAVU));
        assert_eq!(prop.iter().filter(|n| *n == DEJAVU).count(), 1);
        assert_eq!(mono.last().map(String::as_str), Some(DEJAVU));
    }

    #[test]
    fn default_leaves_builtins_untouched() {
        let (prop, mono) = families(FontChoice::Default);
        let base = egui::FontDefinitions::default();
        assert_eq!(
            prop,
            base.families[&egui::FontFamily::Proportional]
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            mono,
            base.families[&egui::FontFamily::Monospace]
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
        );
        assert!(!prop.iter().any(|n| n == DEJAVU || n == INTER));
    }
}
