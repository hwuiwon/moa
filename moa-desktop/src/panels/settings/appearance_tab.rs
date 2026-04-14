//! Appearance tab — theme selection + density/font-size stubs.

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme;

use crate::components::{
    row::settings_row, section::section_card, segmented::segmented,
};
use crate::density::Density;
use crate::theme::{apply_theme_name, canonical_theme_key};

use super::settings_panel::SettingsPage;

const THEME_OPTIONS: &[(&str, &str)] = &[("dark", "Dark"), ("light", "Light")];
const DENSITY_OPTIONS: &[(&str, &str)] = &[
    ("comfortable", "Comfortable"),
    ("compact", "Compact"),
];

pub fn render_appearance_tab(
    panel: &SettingsPage,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let current = canonical_theme_key(&panel.config().tui.theme);

    let theme_control = segmented(cx, "theme", THEME_OPTIONS, current, |this, value, cx| {
        let owned = value.to_string();
        this.mutate(cx, |cfg| cfg.tui.theme = owned.clone());
        apply_theme_name(&owned, cx);
    });
    let current_density = Density::from_str(&panel.config().tui.density).as_str();
    let density_control = segmented(
        cx,
        "density",
        DENSITY_OPTIONS,
        current_density,
        |this, value, cx| {
            this.mutate(cx, |cfg| cfg.tui.density = value.to_string());
        },
    );
    let font_control = div()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .bg(theme.muted)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child("14 px")
        .into_any_element();

    let theme_row = settings_row(
        cx,
        "Appearance",
        Some("Pick the color palette."),
        theme_control,
        true,
    );
    let density_row = settings_row(
        cx,
        "Density",
        Some("Compact tightens chat-bubble padding and markdown line-height."),
        density_control,
        false,
    );
    let font_row = settings_row(
        cx,
        "Font size",
        Some("Body text size. Wiring arrives in a future pass."),
        font_control,
        false,
    );

    section_card(cx, Some("Theme"), Some("Applied live across the app."))
        .child(theme_row)
        .child(density_row)
        .child(font_row)
        .into_any_element()
}
