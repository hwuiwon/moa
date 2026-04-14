//! Designed empty-state blocks for panels that have no data yet.
//!
//! All panels share the same layout so the app feels consistent: a large
//! icon/emoji, a title, and a muted description. Callers can optionally
//! attach a call-to-action button via `with_action`.

use gpui::{
    AnyElement, App, IntoElement, MouseButton, ParentElement, SharedString, Styled, div,
    prelude::*, rems,
};
use gpui_component::ActiveTheme;

/// Builds the standard empty-state block: a title and a muted description,
/// centered in the panel. No decorative glyphs — shape comes from typography
/// and spacing.
pub fn empty_state(
    cx: &App,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> gpui::Div {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_2()
        .p_6()
        .child(
            div()
                .text_size(rems(1.0))
                .text_color(theme.foreground)
                .child(title.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(description.into()),
        )
}

/// Attaches a primary-action button to an empty state returned by
/// [`empty_state`]. The callback receives the raw click event.
pub fn with_action(
    base: gpui::Div,
    cx: &App,
    label: impl Into<SharedString>,
    id: &'static str,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut App) + 'static,
) -> gpui::Div {
    let theme = cx.theme();
    base.child(
        div()
            .id(id)
            .mt_3()
            .px_4()
            .py_1p5()
            .rounded_md()
            .bg(theme.primary)
            .text_color(theme.primary_foreground)
            .text_sm()
            .hover(|s| s.bg(theme.primary_hover))
            .child(label.into())
            .on_mouse_down(MouseButton::Left, on_click),
    )
}

/// Convenience: wraps [`empty_state`] in an `AnyElement` for call sites that
/// need a type-erased value.
#[allow(dead_code)]
pub fn empty_state_any(
    cx: &App,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
) -> AnyElement {
    empty_state(cx, title, description).into_any_element()
}

/// Appends a secondary (text-only) action next to the primary one.
/// Renders as a ghost button so it doesn't compete visually with
/// [`with_action`]'s filled button.
#[allow(dead_code)]
pub fn with_secondary_action(
    base: gpui::Div,
    cx: &App,
    label: impl Into<SharedString>,
    id: &'static str,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut App) + 'static,
) -> gpui::Div {
    let theme = cx.theme();
    base.child(
        div()
            .id(id)
            .mt_3()
            .px_4()
            .py_1p5()
            .rounded_md()
            .text_color(theme.muted_foreground)
            .text_sm()
            .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
            .child(label.into())
            .on_mouse_down(MouseButton::Left, on_click),
    )
}

/// Appends a keyboard-shortcut hint (non-interactive) to the empty state.
/// Used to teach users that a shortcut exists without requiring them to
/// click anything. Example: `with_keyboard_hint(empty_state, cx, "⌘N")`.
#[allow(dead_code)]
pub fn with_keyboard_hint(
    base: gpui::Div,
    cx: &App,
    shortcut: impl Into<SharedString>,
) -> gpui::Div {
    let theme = cx.theme();
    base.child(
        div()
            .mt_3()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("or press")
            .child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(theme.muted)
                    .text_color(theme.foreground)
                    .child(shortcut.into()),
            ),
    )
}
