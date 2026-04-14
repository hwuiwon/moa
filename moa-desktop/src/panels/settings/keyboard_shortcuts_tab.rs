//! Keyboard-shortcuts tab — read-only reference table generated from
//! `keybindings::DISPLAY_BINDINGS`, so adding a binding there
//! automatically updates this page.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*,
};
use gpui_component::ActiveTheme;

use crate::components::section::section_card;
use crate::keybindings::DISPLAY_BINDINGS;

use super::settings_panel::SettingsPage;

pub fn render_keyboard_shortcuts_tab(cx: &mut Context<SettingsPage>) -> AnyElement {
    let theme = cx.theme().clone();
    let mut card = section_card(cx, None::<&str>, None::<&str>);
    let total = DISPLAY_BINDINGS.len();
    for (idx, (label, shortcut)) in DISPLAY_BINDINGS.iter().enumerate() {
        let is_first = idx == 0;
        let _is_last = idx + 1 == total;
        let row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_2p5()
            .when(!is_first, |d| d.border_t_1().border_color(theme.border))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(SharedString::from(label.to_string())),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(shortcut.to_string())),
            );
        card = card.child(row);
    }
    card.into_any_element()
}
