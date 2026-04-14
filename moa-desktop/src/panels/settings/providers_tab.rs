//! Providers tab: each provider gets a row inside a single card. The row
//! shows the env-var the app reads for the key, whether the variable is
//! currently set, and a disabled Test button (no backend ping yet).
//!
//! Secrets are never stored in config — only the env-var *name* is.

use gpui::{
    AnyElement, Context, IntoElement, MouseButton, ParentElement, SharedString, Styled, div,
    prelude::*,
};
use gpui_component::ActiveTheme;
use moa_core::ProviderCredentialConfig;

use crate::components::{row::settings_row, section::section_card};

use super::settings_panel::SettingsPage;

struct ProviderInfo {
    label: &'static str,
    env_var: String,
    key_present: bool,
}

fn collect_providers(panel: &SettingsPage) -> Vec<ProviderInfo> {
    let providers = &panel.config().providers;
    vec![
        describe_provider("Anthropic", &providers.anthropic),
        describe_provider("OpenAI", &providers.openai),
        describe_provider("Google", &providers.google),
    ]
}

fn describe_provider(label: &'static str, cred: &ProviderCredentialConfig) -> ProviderInfo {
    let env_var = cred.api_key_env.clone();
    let key_present = !env_var.is_empty()
        && std::env::var(&env_var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    ProviderInfo {
        label,
        env_var,
        key_present,
    }
}

pub fn render_providers_tab(
    panel: &SettingsPage,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let providers = collect_providers(panel);

    let mut card = section_card(
        cx,
        Some("Credentials"),
        Some(
            "API keys are sourced from environment variables. Set the \
             variable below and restart MOA.",
        ),
    );

    for (idx, info) in providers.into_iter().enumerate() {
        let description: SharedString = if info.env_var.is_empty() {
            "no env var configured".into()
        } else {
            format!("env: {}", info.env_var).into()
        };
        let control = provider_control(info.env_var.clone(), info.key_present, &theme);
        card = card.child(settings_row(
            cx,
            info.label,
            Some(description),
            control,
            idx == 0,
        ));
    }

    card.into_any_element()
}

fn provider_control(
    env_var: String,
    key_present: bool,
    theme: &gpui_component::Theme,
) -> AnyElement {
    let (badge_text, badge_bg, badge_fg) = if key_present {
        ("set", theme.success, theme.success_foreground)
    } else if env_var.is_empty() {
        ("unset", theme.muted, theme.muted_foreground)
    } else {
        ("missing", theme.danger, theme.danger_foreground)
    };

    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .px_2()
                .py_0p5()
                .rounded_sm()
                .bg(badge_bg)
                .text_color(badge_fg)
                .text_xs()
                .child(badge_text),
        )
        .child(
            div()
                .id("test-provider")
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.muted)
                .text_color(theme.muted_foreground)
                .text_xs()
                .when(key_present, |d| {
                    d.hover(|s| s.bg(theme.accent).text_color(theme.accent_foreground))
                })
                .child("Test")
                .on_mouse_down(MouseButton::Left, |_, _, _| {
                    tracing::info!("provider test requested (stub — no backend yet)");
                }),
        )
        .into_any_element()
}
