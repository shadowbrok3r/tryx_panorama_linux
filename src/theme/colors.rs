//! Semantic color accessors backed by the active egui theme.
//!
//! Ported from mtech-ui's `theme.rs` (the chart and series palettes stay
//! behind). Prefer `colors::error(ui)` over a hardcoded red so every surface
//! follows the installed theme. `success` and `accent_secondary` have no
//! `egui::Visuals` slot; a preset may stash them on the context, otherwise
//! these app-wide defaults apply.

use eframe::egui::{Color32, Context, Id, Ui};

const SUCCESS_KEY: &str = "mtech.theme.success_color";
const ACCENT2_KEY: &str = "mtech.theme.accent_secondary";

fn success_id() -> Id {
    Id::unique(SUCCESS_KEY)
}

fn accent2_id() -> Id {
    Id::unique(ACCENT2_KEY)
}

pub fn warn(ui: &Ui) -> Color32 {
    ui.visuals().warn_fg_color
}

pub fn success_ctx(ctx: &Context) -> Color32 {
    ctx.data(|d| d.get_temp::<Color32>(success_id()))
        .unwrap_or(Color32::from_rgb(72, 199, 142))
}

pub fn accent_secondary_ctx(ctx: &Context) -> Color32 {
    ctx.data(|d| d.get_temp::<Color32>(accent2_id()))
        .unwrap_or(Color32::from_rgb(191, 33, 101))
}

pub fn weak_text(ui: &Ui) -> Color32 {
    ui.visuals().weak_text_color()
}
