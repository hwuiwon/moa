//! Three-panel workspace composing sidebar, center, and detail views.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled, Window, div,
    px,
};
use gpui_component::{
    ActiveTheme,
    resizable::{h_resizable, resizable_panel},
};
use moa_core::SessionId;

use crate::{
    panels::sidebar::{SessionSelected, SessionSidebar},
    services::ServiceBridgeHandle,
};

use super::{center::CenterPanel, detail::DetailPanel};

/// Workspace owns the three panels and tracks their visibility.
pub struct Workspace {
    sidebar: Entity<SessionSidebar>,
    center: Entity<CenterPanel>,
    detail: Entity<DetailPanel>,
    sidebar_visible: bool,
    detail_visible: bool,
    selected_session: Option<SessionId>,
}

impl Workspace {
    /// Creates a workspace with all three panels visible.
    pub fn new(bridge: ServiceBridgeHandle, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SessionSidebar::new(bridge, cx));
        let list = sidebar.read(cx).session_list().clone();
        cx.subscribe(&list, |this, _, event: &SessionSelected, cx| {
            this.selected_session = Some(event.0.clone());
            tracing::info!(session_id = %event.0, "session selected");
            cx.notify();
        })
        .detach();

        Self {
            sidebar,
            center: cx.new(CenterPanel::new),
            detail: cx.new(DetailPanel::new),
            sidebar_visible: true,
            detail_visible: true,
            selected_session: None,
        }
    }

    /// Whether the sidebar panel is currently shown.
    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    /// Whether the detail panel is currently shown.
    pub fn detail_visible(&self) -> bool {
        self.detail_visible
    }

    /// Currently selected session, if any.
    #[allow(dead_code)]
    pub fn selected_session(&self) -> Option<&SessionId> {
        self.selected_session.as_ref()
    }

    /// Toggles sidebar visibility.
    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    /// Toggles detail panel visibility.
    pub fn toggle_detail(&mut self, cx: &mut Context<Self>) {
        self.detail_visible = !self.detail_visible;
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut group = h_resizable("moa-workspace");

        if self.sidebar_visible {
            group = group.child(
                resizable_panel()
                    .size(px(250.))
                    .size_range(px(200.)..Pixels::MAX)
                    .child(self.sidebar.clone()),
            );
        }

        group = group.child(resizable_panel().child(self.center.clone()));

        if self.detail_visible {
            group = group.child(
                resizable_panel()
                    .size(px(300.))
                    .size_range(px(250.)..Pixels::MAX)
                    .child(self.detail.clone()),
            );
        }

        div().flex().flex_1().bg(theme.background).child(group)
    }
}
