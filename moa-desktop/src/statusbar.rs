//! Bottom status bar showing connection, turn, token, and cost counters.

use gpui::{App, Context, IntoElement, ParentElement, Render, Styled, Window, div, px, rgb};
use gpui_component::ActiveTheme;

/// Status-bar view rendered at the bottom of the workspace.
pub struct MoaStatusBar {
    connected: bool,
    turns: u32,
    tokens: u64,
    cumulative_cost_usd: f64,
}

impl MoaStatusBar {
    /// Creates a status bar with placeholder values.
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            connected: true,
            turns: 0,
            tokens: 0,
            cumulative_cost_usd: 0.0,
        }
    }

    fn render_metric(&self, label: &str, value: String, cx: &App) -> impl IntoElement + use<> {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(label.to_string()),
            )
            .child(div().text_color(theme.foreground).child(value))
    }
}

impl Render for MoaStatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dot_color = if self.connected {
            rgb(0x10b981).into()
        } else {
            theme.danger
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(26.))
            .px_3()
            .bg(theme.sidebar)
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(dot_color))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if self.connected {
                            "connected"
                        } else {
                            "offline"
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(self.render_metric("turns", self.turns.to_string(), cx))
                    .child(self.render_metric("tokens", self.tokens.to_string(), cx))
                    .child(self.render_metric(
                        "total",
                        format!("${:.2}", self.cumulative_cost_usd),
                        cx,
                    )),
            )
    }
}
