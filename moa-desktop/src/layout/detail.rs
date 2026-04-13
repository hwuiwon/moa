//! Right-hand detail panel placeholder.

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, rems};
use gpui_component::ActiveTheme;

/// Detail panel view displaying placeholder content.
pub struct DetailPanel;

impl DetailPanel {
    /// Creates an empty detail view.
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for DetailPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_size(rems(0.85))
                    .text_color(theme.sidebar_foreground)
                    .child("Details"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Select a tool call to inspect"),
            )
    }
}
