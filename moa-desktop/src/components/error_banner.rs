//! Shared error-state rendering for panels.
//!
//! Panels that previously rendered raw error strings now use
//! [`error_banner`] so errors look consistent (icon, heading, detail,
//! optional retry action) across the app.

use gpui::{
    AnyElement, App, IntoElement, MouseButton, ParentElement, SharedString, Styled, div,
    prelude::*, rems,
};
use gpui_component::ActiveTheme;

/// Renders a centered error block.
pub fn error_banner(cx: &App, heading: impl Into<SharedString>, detail: &str) -> gpui::Div {
    let theme = cx.theme();
    let detail_short: SharedString = if detail.len() > 400 {
        let mut cut = 400;
        while cut > 0 && !detail.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &detail[..cut]).into()
    } else {
        detail.to_string().into()
    };
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
                .text_color(theme.danger)
                .child(heading.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .max_w(gpui::px(420.0))
                .child(detail_short),
        )
}

/// Adds a "Try again" button that invokes `on_retry`.
pub fn with_retry(
    base: gpui::Div,
    cx: &App,
    on_retry: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut App) + 'static,
) -> gpui::Div {
    let theme = cx.theme();
    base.child(
        div()
            .id("error-retry")
            .mt_3()
            .px_4()
            .py_1p5()
            .rounded_md()
            .bg(theme.muted)
            .text_color(theme.foreground)
            .text_sm()
            .hover(|s| s.bg(theme.accent).text_color(theme.accent_foreground))
            .child("Try again")
            .on_mouse_down(MouseButton::Left, on_retry),
    )
}

/// Convenience that returns an `AnyElement` for callers that want to avoid
/// an intermediate `Div` type.
#[allow(dead_code)]
pub fn error_banner_any(cx: &App, heading: impl Into<SharedString>, detail: &str) -> AnyElement {
    error_banner(cx, heading, detail).into_any_element()
}
