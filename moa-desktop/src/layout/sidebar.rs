//! Left-hand sidebar placeholder panel.

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px, rems};
use gpui_component::ActiveTheme;

/// Sidebar panel view displaying placeholder content.
pub struct SidebarPanel;

impl SidebarPanel {
    /// Creates an empty sidebar view.
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_size(rems(0.85))
                    .text_color(theme.sidebar_foreground)
                    .child("Sessions"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No sessions yet"),
            )
            .child(
                div()
                    .mt(px(12.))
                    .text_size(rems(0.85))
                    .text_color(theme.sidebar_foreground)
                    .child("Memory"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Empty"),
            )
    }
}
