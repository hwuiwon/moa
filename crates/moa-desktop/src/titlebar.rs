//! Minimal header — icon-only toolbar per `design.md`.
//!
//! Layout:
//!   [app-mark] [sidebar toggle]  …  [detail toggle]
//! Center is intentionally empty. The old chips (workspace / model / cost)
//! are gone; model shows on agent bubbles, cost in the status bar.

use gpui::{Context, Entity, Window, div, prelude::*, px};
use gpui_component::{ActiveTheme, TitleBar};

use crate::{components::icon_button::icon_button, layout::Workspace};

/// Thin title-bar view. Holds a handle to `Workspace` so toggle buttons
/// can flip sidebar / detail visibility directly.
pub struct MoaTitleBar {
    workspace: Entity<Workspace>,
}

impl MoaTitleBar {
    /// Creates a title bar bound to the given workspace entity.
    pub fn new(workspace: Entity<Workspace>, _cx: &mut Context<Self>) -> Self {
        Self { workspace }
    }
}

impl Render for MoaTitleBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let workspace = self.workspace.clone();
        let sidebar_visible = workspace.read(cx).sidebar_visible();
        let detail_visible = workspace.read(cx).detail_visible();

        // Small app-mark — a solid blue dot using the primary accent, the
        // same identity we use for the tray glyph.
        let app_mark = div()
            .size(px(10.0))
            .rounded_full()
            .bg(theme.primary)
            .flex_shrink_0();

        let sidebar_button = {
            let workspace = workspace.clone();
            icon_button(
                cx,
                "toggle-sidebar",
                if sidebar_visible { "◀" } else { "▶" },
                sidebar_visible,
                move |_, _, cx| {
                    workspace.update(cx, |ws, cx| ws.toggle_sidebar(cx));
                },
            )
        };

        let detail_button = {
            let workspace = workspace.clone();
            icon_button(
                cx,
                "toggle-detail",
                if detail_visible { "▶" } else { "◀" },
                detail_visible,
                move |_, _, cx| {
                    workspace.update(cx, |ws, cx| ws.toggle_detail(cx));
                },
            )
        };

        TitleBar::new().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .h(px(30.0))
                .px_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(app_mark)
                        .child(sidebar_button),
                )
                .child(div().flex().items_center().gap_2().child(detail_button)),
        )
    }
}
