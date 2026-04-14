//! Permissions tab: default posture + auto-approve / always-deny tool lists.

use gpui::{
    AnyElement, Context, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window,
    div, prelude::*,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputState},
};

use crate::components::{row::settings_row, section::section_card, segmented::segmented};

use super::settings_panel::SettingsPage;

const POSTURE_OPTIONS: &[(&str, &str)] =
    &[("approve", "Approve"), ("auto", "Auto"), ("full", "Full")];

pub fn render_permissions_tab(
    panel: &SettingsPage,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let perms = panel.config().permissions.clone();

    let posture_control = segmented(
        cx,
        "posture",
        POSTURE_OPTIONS,
        &perms.default_posture,
        |this, value, cx| {
            this.mutate(cx, |cfg| {
                cfg.permissions.default_posture = value.to_string();
            });
        },
    );

    let posture_row = settings_row(
        cx,
        "Default posture",
        Some(
            "approve = prompt for every tool · auto = auto-approve listed \
             tools · full = auto-approve everything safe.",
        ),
        posture_control,
        true,
    );

    let auto_list = tool_list_card(
        "Auto-approve",
        "Tools that never prompt for approval.",
        &perms.auto_approve,
        panel.auto_approve_input.clone(),
        cx,
        true,
    );

    let deny_list = tool_list_card(
        "Always deny",
        "Tools that are rejected without prompting.",
        &perms.always_deny,
        panel.always_deny_input.clone(),
        cx,
        false,
    );

    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(section_card(cx, None::<&str>, None::<&str>).child(posture_row))
        .child(auto_list)
        .child(deny_list)
        .into_any_element()
}

fn tool_list_card(
    title: &'static str,
    description: &'static str,
    items: &[String],
    input: gpui::Entity<InputState>,
    cx: &mut Context<SettingsPage>,
    is_auto_list: bool,
) -> AnyElement {
    let theme = cx.theme().clone();

    let mut chips = div().flex().flex_wrap().gap_2();
    if items.is_empty() {
        chips = chips.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("(empty)"),
        );
    } else {
        for (idx, name) in items.iter().enumerate() {
            let name_display: SharedString = name.clone().into();
            let name_remove = name.clone();
            let chip_id = format!("tool-chip-{}-{}", title.to_lowercase(), idx);
            chips = chips.child(
                div()
                    .id(gpui::ElementId::Name(chip_id.into()))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(div().child(name_display))
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .hover(|s| s.text_color(theme.danger))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    let target = name_remove.clone();
                                    this.mutate(cx, |cfg| {
                                        if is_auto_list {
                                            cfg.permissions.auto_approve.retain(|n| n != &target);
                                        } else {
                                            cfg.permissions.always_deny.retain(|n| n != &target);
                                        }
                                    });
                                }),
                            ),
                    ),
            );
        }
    }

    let body = div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_3()
        .child(chips)
        .child(div().w_full().child(Input::new(&input).cleanable(true)));

    section_card(cx, Some(title), Some(description))
        .child(body)
        .into_any_element()
}
