//! Segmented pill-style selector used by Settings tabs and anywhere a
//! short list of options needs a compact single-row toggle (Dark/Light,
//! low/medium/high, etc.).
//!
//! Generic over the view type `V` so the same helper works for any
//! settings-style view that holds the config. The active option is
//! rendered on the elevated background; the container sits on `muted`.

use gpui::{AnyElement, Context, IntoElement, MouseButton, ParentElement, Styled, div, prelude::*};
use gpui_component::ActiveTheme;

pub fn segmented<V, F>(
    cx: &mut Context<V>,
    group_id: &'static str,
    options: &'static [(&'static str, &'static str)],
    current: &str,
    on_select: F,
) -> AnyElement
where
    V: 'static,
    F: Fn(&mut V, &'static str, &mut Context<V>) + Clone + 'static,
{
    let theme = cx.theme().clone();
    let mut row = div()
        .flex()
        .items_center()
        .gap_0p5()
        .p_0p5()
        .rounded_md()
        .bg(theme.muted);
    for (value, label) in options {
        let active = current == *value;
        let v = *value;
        let cb = on_select.clone();
        let id = format!("segmented-{group_id}-{v}");
        row = row.child(
            div()
                .id(gpui::ElementId::Name(id.into()))
                .px_3()
                .py_1()
                .rounded_md()
                .text_xs()
                .when(active, |d| {
                    d.bg(theme.background).text_color(theme.foreground)
                })
                .when(!active, |d| d.text_color(theme.muted_foreground))
                .child(*label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| cb(this, v, cx)),
                ),
        );
    }
    row.into_any_element()
}
