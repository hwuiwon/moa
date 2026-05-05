//! Left-nav item used in the Settings page and anywhere a vertical label
//! column selects right-side content. See `design.md` → "Left-nav item".

use gpui::{
    App, MouseButton, MouseDownEvent, ParentElement, SharedString, Styled, Window, div, prelude::*,
};
use gpui_component::ActiveTheme;

/// Builds a single left-nav row.
///
/// `active` renders the row with the primary accent background; all other
/// rows get a transparent background with a muted hover state.
pub fn nav_item(
    cx: &App,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let id: SharedString = id.into();
    div()
        .id(gpui::ElementId::Name(id))
        .flex()
        .items_center()
        .w_full()
        .px_3()
        .py_1p5()
        .rounded_md()
        .text_sm()
        .text_color(if active {
            theme.primary_foreground
        } else {
            theme.sidebar_foreground
        })
        .when(active, |d| d.bg(theme.primary))
        .when(!active, |d| d.hover(|s| s.bg(theme.sidebar_accent)))
        .child(label.into())
        .on_mouse_down(MouseButton::Left, on_click)
}
