//! Phosphor icon glyphs for egui.
//!
//! Ported from mtech-ui's `icons.rs`.

pub use egui_phosphor::regular as p;

use eframe::egui::{FontDefinitions, FontFamily};

/// Registers the Phosphor face; call before `fonts::install`.
pub fn install_fonts(fonts: &mut FontDefinitions) {
    egui_phosphor::add_to_fonts(fonts, egui_phosphor::Variant::Regular);

    // add_to_fonts only registers phosphor under Proportional; register it under
    // Monospace too, or icons in monospace text (the log panel) render as tofu.
    if let Some(keys) = fonts.families.get_mut(&FontFamily::Monospace)
        && !keys.iter().any(|k| k == "phosphor")
    {
        keys.insert(1.min(keys.len()), "phosphor".into());
    }
}
