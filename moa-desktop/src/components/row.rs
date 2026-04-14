//! Settings-row primitive: label + muted description on the left, an
//! arbitrary control element on the right. See `design.md` → "Toggle row".
//!
//! The card that owns the rows is responsible for its own padding; this
//! helper draws a 1 px top divider when `first = false` so stacking rows
//! inside a single card produces the reference screenshot's "horizontal
//! rule between rows, no rule above the first row" effect.

use gpui::{AnyElement, App, ParentElement, SharedString, Styled, div, prelude::*};
use gpui_component::ActiveTheme;

/// Builds one row of a settings card.
///
/// `first` should be `true` for the first row in a card (suppresses the
/// top divider) and `false` for every subsequent row.
pub fn settings_row(
    cx: &App,
    label: impl Into<SharedString>,
    description: Option<impl Into<SharedString>>,
    control: AnyElement,
    first: bool,
) -> gpui::Div {
    let theme = cx.theme();
    let mut labels = div()
        .flex()
        .flex_col()
        .gap_0p5()
        .flex_1()
        .min_w(gpui::px(0.0))
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(label.into()),
        );
    if let Some(desc) = description {
        labels = labels.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(desc.into()),
        );
    }

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .py_3()
        .when(!first, |d| d.border_t_1().border_color(theme.border))
        .child(labels)
        .child(control)
}
