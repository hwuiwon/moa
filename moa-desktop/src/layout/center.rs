//! Center panel placeholder (chat / primary workspace view).

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, rems};
use gpui_component::ActiveTheme;

/// Primary workspace area placeholder.
pub struct CenterPanel;

impl CenterPanel {
    /// Creates an empty center panel view.
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for CenterPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_size(rems(1.1))
                    .text_color(theme.foreground)
                    .child("Start a conversation"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Chat, tools, and approvals will appear here"),
            )
    }
}
