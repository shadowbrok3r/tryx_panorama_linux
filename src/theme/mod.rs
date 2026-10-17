// ============================================================================
// Mastertech theming for the GUI, ported from mtech-ui (`tokens`, `metrics`,
// `fonts`, `icons`, `theme` → `colors`) plus the Mastertech displays app's
// saved-theme loader.
//
// A theme is a Mastertech theme export: an egui `Style` plus the parts egui has
// no slot for (density, text scale, glass). The bundled default is
// `assets/themes/mastertech.json`; a file at `user_theme_path()` replaces it
// (the GUI's Appearance controls write one).
// ============================================================================

pub mod colors;
pub mod fonts;
pub mod icons;
pub mod metrics;
pub mod tokens;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use eframe::egui::{Context, FontDefinitions, FontFamily, Style, Theme};

use fonts::TextScale;
use metrics::Density;

const BUNDLED: &str = include_str!("../../assets/themes/mastertech.json");

const STYLE_KEY: &str = "style";
const GLASS_KEY: &str = "glass";
const DENSITY_KEY: &str = "density";
const TEXT_SCALE_KEY: &str = "text_scale";

/// A saved theme: the egui `Style` plus the parts of a theme that have no `Style` slot.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedTheme {
    pub style: Style,
    /// Mastertech's backdrop-blur material, carried so a theme round-trips. Not
    /// rendered here: the blur pass lives in Mastertech's backdrop-blur crates,
    /// and the bundled theme's opaque `window_fill` would hide it anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glass: Option<serde_json::Value>,
    #[serde(default)]
    pub density: Density,
    #[serde(default)]
    pub text_scale: TextScale,
}

/// The Mastertech theme this app ships with.
pub fn bundled() -> SavedTheme {
    theme_from_json(BUNDLED.as_bytes()).expect("bundled theme is valid JSON")
}

/// Where the GUI keeps a user-chosen theme: next to the gallery file.
pub fn user_theme_path() -> PathBuf {
    crate::gallery::Gallery::resolve_path(None).with_file_name("theme.json")
}

/// The user's theme if one is saved and readable, else the bundled one.
pub fn load() -> SavedTheme {
    let path = user_theme_path();
    match std::fs::read(&path) {
        Ok(bytes) => match theme_from_json(&bytes) {
            Ok(theme) => return theme,
            Err(e) => log::warn!("Ignoring theme {}: {e:#}", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("Ignoring theme {}: {e}", path.display()),
    }
    bundled()
}

/// Write `theme` to the user theme file.
pub fn save(theme: &SavedTheme, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, theme_to_json(theme)?).with_context(|| format!("writing {}", path.display()))
}

/// First install at startup: the app's font stack, then `theme`.
pub fn install(ctx: &Context, theme: &SavedTheme) {
    ctx.set_fonts(font_definitions(fonts::text_scale(ctx)));
    apply(ctx, theme);
}

/// Install `theme` on `ctx`: the density overlay on its style, its tokens, and
/// its text scale (rebuilding the font atlas when that changes).
pub fn apply(ctx: &Context, theme: &SavedTheme) {
    // egui keeps a dark and a light style and picks one by the OS theme; pin
    // the choice to this theme's own mode so the OS can't swap in egui's stock
    // style for the other one.
    let mode = if theme.style.visuals.dark_mode { Theme::Dark } else { Theme::Light };
    ctx.set_theme(mode);
    metrics::set_density(ctx, theme.density);
    metrics::install_overlaid_style(ctx, theme.style.clone());
    fonts::set_text_scale(ctx, theme.text_scale, font_definitions);
}

/// egui's default faces plus Phosphor icons and Inter, scaled to `scale`.
pub fn font_definitions(scale: TextScale) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    icons::install_fonts(&mut fonts);
    fonts::install(&mut fonts);
    fonts::apply_text_scale(&mut fonts, scale);
    fonts
}

/// Serialize a theme to the Mastertech export shape.
pub fn theme_to_json(theme: &SavedTheme) -> Result<Vec<u8>> {
    let mut theme = theme.clone();
    strip_weight_families(&mut theme.style);
    serde_json::to_vec_pretty(&theme).context("serialize theme to JSON")
}

/// Resets text styles in the overlay's weight families to `Proportional`, so a
/// saved theme doesn't depend on faces only this app's font stack defines.
fn strip_weight_families(style: &mut Style) {
    for font in style.text_styles.values_mut() {
        if let FontFamily::Name(name) = &font.family
            && (&**name == fonts::MEDIUM || &**name == fonts::SEMIBOLD)
        {
            font.family = FontFamily::Proportional;
        }
    }
}

/// Read a theme from JSON in either shape: the Mastertech wrapper, or a bare
/// `egui::Style` (which has no `style` field — that key tells them apart).
pub fn theme_from_json(json: &[u8]) -> Result<SavedTheme> {
    let value: serde_json::Value =
        serde_json::from_slice(json).context("theme is not valid JSON")?;
    match value {
        serde_json::Value::Object(mut map) if map.contains_key(STYLE_KEY) => {
            let glass = map.remove(GLASS_KEY).filter(|g| !g.is_null());
            let density = take_field(&mut map, DENSITY_KEY).unwrap_or_default();
            let text_scale = take_field(&mut map, TEXT_SCALE_KEY).unwrap_or_default();
            let style = map.remove(STYLE_KEY).unwrap_or(serde_json::Value::Null);
            Ok(SavedTheme { style: style_from_value(style), glass, density, text_scale })
        }
        bare => Ok(SavedTheme {
            style: style_from_value(bare),
            glass: None,
            density: Density::default(),
            text_scale: TextScale::default(),
        }),
    }
}

/// Removes `key` from `map` and decodes it, or `None` when absent or malformed.
fn take_field<T: serde::de::DeserializeOwned>(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<T> {
    map.remove(key).and_then(|v| serde_json::from_value(v).ok())
}

/// Deserialize an egui `Style`, migrating across egui versions: on a strict
/// decode failure (egui added or renamed a field), overlay the stored values on
/// `Style::default()` so new fields take their default, removed ones drop, and
/// matching values (colors, spacing, text styles…) survive.
fn style_from_value(value: serde_json::Value) -> Style {
    match serde_json::from_value::<Style>(value.clone()) {
        Ok(style) => style,
        Err(e) => {
            log::debug!("theme predates the current egui Style schema ({e}); migrating");
            let default = serde_json::to_value(Style::default()).unwrap_or_default();
            serde_json::from_value::<Style>(merge_over_default(default, value)).unwrap_or_else(|e2| {
                log::error!("theme migration failed ({e2}); using the default Style");
                Style::default()
            })
        }
    }
}

/// Recursively overlay `stored` onto `default`, keyed by the default's
/// structure: objects recurse over the default's keys, scalars and arrays take
/// the stored value, and a shape mismatch keeps the default.
fn merge_over_default(default: serde_json::Value, stored: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (default, stored) {
        (Value::Object(mut d), Value::Object(s)) => {
            for (k, dv) in d.iter_mut() {
                if let Some(sv) = s.get(k) {
                    *dv = merge_over_default(dv.take(), sv.clone());
                }
            }
            Value::Object(d)
        }
        (Value::Object(d), _) => Value::Object(d),
        (_, stored) => stored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Color32, TextStyle};

    #[test]
    fn the_bundled_mastertech_theme_loads_whole() {
        let theme = bundled();
        assert_eq!(theme.density, Density::Compact);
        assert_eq!(theme.text_scale, TextScale::Small);
        assert!(theme.glass.is_some());
        let v = &theme.style.visuals;
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, Color32::from_rgba_premultiplied(8, 8, 10, 255));
        assert_eq!(v.error_fg_color, Color32::from_rgba_premultiplied(236, 98, 132, 255));
        assert_eq!(theme.style.text_styles[&TextStyle::Body].size, 12.5);
    }

    #[test]
    fn a_bare_egui_style_is_a_theme_with_default_extras() {
        let mut style = Style::default();
        style.visuals.panel_fill = Color32::from_rgb(1, 2, 3);
        let json = serde_json::to_vec(&style).unwrap();
        let theme = theme_from_json(&json).unwrap();
        assert_eq!(theme.style.visuals.panel_fill, Color32::from_rgb(1, 2, 3));
        assert_eq!(theme.density, Density::Regular);
        assert!(theme.glass.is_none());
    }

    #[test]
    fn a_theme_from_another_egui_version_keeps_its_colors() {
        let mut value = serde_json::to_value(bundled().style).unwrap();
        let visuals = value["visuals"].as_object_mut().unwrap();
        visuals.remove("text_options"); // a field this egui needs but an older one lacked
        visuals.insert("retired_field".into(), serde_json::json!(1.0));
        let style = style_from_value(value);
        assert_eq!(style.visuals.panel_fill, bundled().style.visuals.panel_fill);
    }

    #[test]
    fn an_installed_theme_saves_and_reloads_without_weight_families() {
        let ctx = Context::default();
        let theme = bundled();
        install(&ctx, &theme);
        assert_eq!(metrics::density(&ctx), Density::Compact);
        assert_eq!(fonts::text_scale(&ctx), TextScale::Small);
        let heading = &ctx.global_style().text_styles[&TextStyle::Heading];
        assert_eq!(heading.family, FontFamily::Name(fonts::SEMIBOLD.into()));

        let live = SavedTheme { style: (*ctx.global_style()).clone(), ..theme };
        let back = theme_from_json(&theme_to_json(&live).unwrap()).unwrap();
        assert_eq!(back.density, Density::Compact);
        assert_eq!(back.text_scale, TextScale::Small);
        assert_eq!(back.style.text_styles[&TextStyle::Heading].family, FontFamily::Proportional);
        assert_eq!(back.style.visuals, ctx.global_style().visuals);
    }

    #[test]
    fn the_theme_pins_egui_to_its_own_dark_or_light_mode() {
        let ctx = Context::default();
        ctx.set_theme(Theme::Light);
        apply(&ctx, &bundled());
        assert!(ctx.global_style().visuals.dark_mode);
        assert_eq!(ctx.global_style().visuals.panel_fill, bundled().style.visuals.panel_fill);
    }
}
