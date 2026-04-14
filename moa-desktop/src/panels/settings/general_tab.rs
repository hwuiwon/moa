//! General settings: default provider, default model, reasoning effort,
//! web-search toggle.
//!
//! Theme has moved to the Appearance tab. The Model card shows every
//! model served by the currently-selected provider (sourced from
//! `moa_providers::CATALOG`), with context-window info next to each
//! row — so users can see at a glance how much context each option
//! gives them.

use gpui::{
    AnyElement, Context, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window,
    div, prelude::*, px,
};
use gpui_component::{ActiveTheme, switch::Switch};

use crate::components::{row::settings_row, section::section_card, segmented::segmented};

use super::settings_panel::SettingsPage;

const REASONING_OPTIONS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "X-High"),
];
const PROVIDER_OPTIONS: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("openai", "OpenAI"),
    ("google", "Google"),
];

pub fn render_general_tab(
    panel: &SettingsPage,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let general = panel.config().general.clone();

    let provider_control = segmented(
        cx,
        "provider",
        PROVIDER_OPTIONS,
        &general.default_provider,
        |this, value, cx| {
            // Reset the default model alongside the provider — the old
            // model id is unlikely to belong to the new provider's
            // catalog, so leaving it would persist an invalid pair.
            // Pick the first catalogued model for the new provider.
            let new_provider = value.to_string();
            let next_model = moa_providers::by_provider(&new_provider)
                .next()
                .map(|m| m.id.to_string());
            this.mutate(cx, |cfg| {
                cfg.general.default_provider = new_provider;
                if let Some(model) = next_model {
                    cfg.general.default_model = model;
                }
            });
        },
    );

    let reasoning_control = segmented(
        cx,
        "reasoning",
        REASONING_OPTIONS,
        &general.reasoning_effort,
        |this, value, cx| {
            this.mutate(cx, |cfg| cfg.general.reasoning_effort = value.to_string());
        },
    );

    let web_search_checked = general.web_search_enabled;
    let web_search_control = Switch::new("web-search-toggle")
        .checked(web_search_checked)
        .on_click(
            cx.listener(move |this: &mut SettingsPage, checked: &bool, _, cx| {
                let new_value = *checked;
                this.mutate(cx, |cfg| cfg.general.web_search_enabled = new_value);
            }),
        )
        .into_any_element();

    // First card: provider + reasoning (+ web search).
    let provider_row = settings_row(
        cx,
        "Default provider",
        Some("Used for new sessions when no override is specified."),
        provider_control,
        true,
    );
    let reasoning_row = settings_row(
        cx,
        "Reasoning effort",
        Some("Higher effort = longer thinking traces where supported."),
        reasoning_control,
        false,
    );
    let web_search_row = settings_row(
        cx,
        "Web search",
        Some("Expose provider-native web search to supported models."),
        web_search_control,
        false,
    );

    // Second card: model list for the active provider.
    let model_card = render_model_card(&general.default_provider, &general.default_model, cx);

    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(
            section_card(
                cx,
                Some("Defaults"),
                Some("Provider, reasoning, and feature flags applied to new sessions."),
            )
            .child(provider_row)
            .child(reasoning_row)
            .child(web_search_row),
        )
        .child(model_card)
        .into_any_element()
}

/// Renders the Model section — a row per model served by the selected
/// provider, each clickable to promote it to the default. The active
/// model gets a filled indicator + the primary-colored name; others
/// show hollow indicators and muted names.
fn render_model_card(
    active_provider: &str,
    active_model: &str,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let models: Vec<&moa_providers::ProviderModel> =
        moa_providers::by_provider(active_provider).collect();

    let mut card = section_card(
        cx,
        Some("Model"),
        Some(
            "Models served by the selected provider, sorted by capability.\
             Click a row to promote it to the default.",
        ),
    );

    if models.is_empty() {
        card = card.child(
            div()
                .py_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No models catalogued for this provider yet."),
        );
        return card.into_any_element();
    }

    for (idx, model) in models.iter().enumerate() {
        let is_active = model.id == active_model;
        let id_for_click: &'static str = model.id;
        let row_id = format!("model-row-{}", model.id);

        // Leading indicator mirrors the tool-card pattern: hollow ring
        // for inactive models, filled dot for the active one.
        let indicator = {
            let mut d = div()
                .size(px(12.0))
                .rounded_full()
                .border_1()
                .flex()
                .items_center()
                .justify_center();
            if is_active {
                d = d
                    .border_color(theme.primary)
                    .child(div().size(px(4.0)).rounded_full().bg(theme.primary));
            } else {
                d = d.border_color(theme.muted_foreground);
            }
            d
        };

        let name_label = div()
            .text_sm()
            .text_color(if is_active {
                theme.foreground
            } else {
                theme.muted_foreground
            })
            .child(SharedString::from(model.display_name.to_string()));

        let context_label = div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(format!("{} ctx", fmt_tokens(model.context_window)));

        let row = div()
            .id(gpui::ElementId::Name(row_id.into()))
            .flex()
            .items_center()
            .gap_3()
            .py_2p5()
            .when(idx > 0, |d| d.border_t_1().border_color(theme.border))
            .hover(|s| s.bg(theme.muted))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.mutate(cx, |cfg| {
                        cfg.general.default_model = id_for_click.to_string()
                    });
                }),
            )
            .child(indicator)
            .child(div().flex().flex_col().flex_1().gap_0p5().child(name_label))
            .child(context_label);
        card = card.child(row);
    }
    card.into_any_element()
}

/// Compact "1.0M" / "200K" formatter for catalog context windows.
fn fmt_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}
