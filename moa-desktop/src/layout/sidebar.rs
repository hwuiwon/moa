//! Left-hand sidebar placeholder panel.

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px, rems};
use gpui_component::ActiveTheme;

use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};

/// Sidebar panel displaying workspace counters and placeholder sections.
pub struct SidebarPanel {
    bridge: ServiceBridgeHandle,
    session_count: Option<usize>,
    last_error: Option<String>,
    pending: bool,
}

impl SidebarPanel {
    /// Creates a sidebar that refreshes session data once services are ready.
    pub fn new(bridge: ServiceBridgeHandle, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            bridge: bridge.clone(),
            session_count: None,
            last_error: None,
            pending: false,
        };
        cx.observe(bridge.entity(), |this, _, cx| {
            this.maybe_refresh(cx);
            cx.notify();
        })
        .detach();
        this.maybe_refresh(cx);
        this
    }

    fn maybe_refresh(&mut self, cx: &mut Context<Self>) {
        if self.pending || self.session_count.is_some() {
            return;
        }
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        self.pending = true;
        let entity = cx.entity().clone();
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.list_sessions().await.map(|s| s.len()) },
            |this, result, _cx| {
                this.pending = false;
                match result {
                    Ok(count) => {
                        this.session_count = Some(count);
                        this.last_error = None;
                    }
                    Err(err) => {
                        this.last_error = Some(format!("{err:#}"));
                    }
                }
            },
        );
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let sessions_line = match (&self.last_error, self.session_count, self.pending) {
            (Some(err), _, _) => format!("error: {err}"),
            (None, Some(0), _) => "No sessions yet".to_string(),
            (None, Some(count), _) => format!("{count} sessions"),
            (None, None, true) => "Loading…".to_string(),
            (None, None, false) => "Waiting for services…".to_string(),
        };

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
                    .child(sessions_line),
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
