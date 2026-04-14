//! Three-panel workspace composing sidebar, center, and detail views.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme,
    resizable::{h_resizable, resizable_panel},
};
use moa_core::SessionId;

use crate::{
    actions::{NewSession, ToggleDetailPanel, ToggleSidebar},
    panels::{
        chat::ChatPanel,
        detail::DetailPanel,
        memory::{MemoryPageSelected, MemoryViewer},
        sidebar::{SessionSelected, SessionSidebar},
    },
    services::ServiceBridgeHandle,
    window_state::WindowState,
};

/// Workspace owns the three panels and tracks their visibility.
pub struct Workspace {
    sidebar: Entity<SessionSidebar>,
    chat: Entity<ChatPanel>,
    memory_viewer: Entity<MemoryViewer>,
    detail: Entity<DetailPanel>,
    sidebar_visible: bool,
    detail_visible: bool,
    selected_session: Option<SessionId>,
}

impl Workspace {
    /// Creates a workspace with all three panels visible.
    pub fn new(bridge: ServiceBridgeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SessionSidebar::new(bridge.clone(), window, cx));
        let chat = cx.new(|cx| ChatPanel::new(bridge.clone(), window, cx));
        let memory_viewer = cx.new(|cx| MemoryViewer::new(bridge.clone(), cx));
        let detail = cx.new(|cx| DetailPanel::new(bridge, cx));

        let session_list = sidebar.read(cx).session_list().clone();
        let memory_list = sidebar.read(cx).memory_list().clone();
        let skill_list = sidebar.read(cx).skill_list().clone();

        let chat_for_select = chat.clone();
        let viewer_for_session = memory_viewer.clone();
        let detail_for_session = detail.clone();
        cx.subscribe(
            &session_list,
            move |this, _, event: &SessionSelected, cx| {
                let id = event.0.clone();
                this.selected_session = Some(id.clone());
                tracing::info!(session_id = %id, "session selected");
                chat_for_select.update(cx, |panel, cx| panel.set_session(id.clone(), cx));
                detail_for_session.update(cx, |panel, cx| panel.set_session(id, cx));
                viewer_for_session.update(cx, |viewer, cx| viewer.clear(cx));
                cx.notify();
            },
        )
        .detach();

        let viewer_for_memory = memory_viewer.clone();
        cx.subscribe(&memory_list, move |_, _, event: &MemoryPageSelected, cx| {
            let path = event.0.clone();
            tracing::info!(path = ?path, "memory page selected");
            viewer_for_memory.update(cx, |viewer, cx| viewer.open(path, cx));
            cx.notify();
        })
        .detach();

        let viewer_for_skill = memory_viewer.clone();
        cx.subscribe(&skill_list, move |_, _, event: &MemoryPageSelected, cx| {
            let path = event.0.clone();
            tracing::info!(path = ?path, "skill page selected");
            viewer_for_skill.update(cx, |viewer, cx| viewer.open(path, cx));
            cx.notify();
        })
        .detach();

        Self {
            sidebar,
            chat,
            memory_viewer,
            detail,
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

    /// Applies the relevant fields of a restored [`WindowState`] (panel
    /// visibility). Called once during construction — widths are applied by
    /// the resizable group itself using the same state.
    pub fn apply_state(&mut self, state: &WindowState) {
        self.sidebar_visible = state.sidebar_visible;
        self.detail_visible = state.detail_visible;
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

    fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_sidebar(cx);
    }

    fn on_toggle_detail(
        &mut self,
        _: &ToggleDetailPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_detail(cx);
    }

    fn on_new_session(&mut self, _: &NewSession, _window: &mut Window, cx: &mut Context<Self>) {
        self.create_session(cx);
    }

    /// Creates a new session via the embedded sidebar's session list.
    pub fn create_session(&mut self, cx: &mut Context<Self>) {
        let session_list = self.sidebar.read(cx).session_list().clone();
        session_list.update(cx, |list, cx| list.create_session(cx));
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

        // Center panel: memory viewer takes over when a page is active; otherwise show chat.
        let showing_memory = self.memory_viewer.read(cx).has_page();
        let center: gpui::AnyElement = if showing_memory {
            self.memory_viewer.clone().into_any_element()
        } else {
            self.chat.clone().into_any_element()
        };
        group = group.child(resizable_panel().child(center));

        if self.detail_visible {
            group = group.child(
                resizable_panel()
                    .size(px(320.))
                    .size_range(px(260.)..Pixels::MAX)
                    .child(self.detail.clone()),
            );
        }

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .bg(theme.background)
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_detail))
            .on_action(cx.listener(Self::on_new_session))
            .child(group)
    }
}
