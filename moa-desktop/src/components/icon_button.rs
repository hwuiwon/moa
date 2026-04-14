//! Icon-only toolbar button.
//!
//! See `design.md` → "Icon button" for the spec. Used by the titlebar and
//! any future toolbar. Body: a single short glyph (one character or a
//! text arrow like `◀`). 28×28 hit target, 14 px glyph, muted until hover.

use gpui::{
    App, MouseButton, MouseDownEvent, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::ActiveTheme;

/// Builds an icon-only toolbar button.
///
/// Pass `active = true` when the button represents an on/selected state
/// (e.g. sidebar visible) — it renders with the accent background.
pub fn icon_button(
    cx: &App,
    id: impl Into<SharedString>,
    glyph: impl Into<SharedString>,
    active: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let id: SharedString = id.into();
    div()
        .id(gpui::ElementId::Name(id))
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.0))
        .rounded_md()
        .text_color(if active {
            theme.accent_foreground
        } else {
            theme.muted_foreground
        })
        .when(active, |d| d.bg(theme.accent))
        .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
        .child(div().text_size(px(14.0)).child(glyph.into()))
        .on_mouse_down(MouseButton::Left, on_click)
}
