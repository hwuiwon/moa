//! Custom title bar showing brand, workspace, model, and cost.

use gpui::{App, Context, Entity, MouseButton, Window, div, prelude::*, px, rems};
use gpui_component::{ActiveTheme, TitleBar};

use crate::layout::Workspace;

/// Title-bar view displaying brand identity and session metadata placeholders.
pub struct MoaTitleBar {
    workspace: Entity<Workspace>,
    workspace_name: String,
    model: String,
    cost_usd: f64,
}

impl MoaTitleBar {
    /// Creates a title bar bound to the workspace entity it toggles panels on.
    pub fn new(workspace: Entity<Workspace>, _cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            workspace_name: "No workspace".to_string(),
            model: "claude-sonnet".to_string(),
            cost_usd: 0.0,
        }
    }

    fn render_chip(&self, label: &str, value: String, cx: &App) -> impl IntoElement + use<> {
        let theme = cx.theme();
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_0p5()
            .rounded_md()
            .bg(theme.muted)
            .text_sm()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(label.to_string()),
            )
            .child(div().text_color(theme.foreground).child(value))
    }
}

impl Render for MoaTitleBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let workspace_entity = self.workspace.clone();
        let sidebar_visible = workspace_entity.read(cx).sidebar_visible();
        let detail_visible = workspace_entity.read(cx).detail_visible();

        let sidebar_toggle = {
            let workspace = workspace_entity.clone();
            div()
                .id("toggle-sidebar")
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_sm()
                .text_color(theme.muted_foreground)
                .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
                .child(if sidebar_visible { "◀" } else { "▶" })
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    workspace.update(cx, |ws, cx| ws.toggle_sidebar(cx));
                })
        };

        let detail_toggle = {
            let workspace = workspace_entity.clone();
            div()
                .id("toggle-detail")
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_sm()
                .text_color(theme.muted_foreground)
                .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
                .child(if detail_visible { "▶" } else { "◀" })
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    workspace.update(cx, |ws, cx| ws.toggle_detail(cx));
                })
        };

        TitleBar::new().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .h(px(36.))
                .px_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme.foreground)
                        .child(sidebar_toggle)
                        .child(
                            div()
                                .text_size(rems(0.95))
                                .text_color(theme.primary)
                                .child("MOA"),
                        )
                        .child(self.render_chip("workspace", self.workspace_name.clone(), cx)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.render_chip("model", self.model.clone(), cx))
                        .child(self.render_chip("cost", format!("${:.2}", self.cost_usd), cx))
                        .child(detail_toggle),
                ),
        )
    }
}
