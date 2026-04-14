//! Composed sidebar: tab switcher + session list + Memory/Skills placeholders.

use gpui::{App, Context, Entity, IntoElement, MouseButton, Render, Window, div, prelude::*, rems};
use gpui_component::ActiveTheme;

use crate::services::ServiceBridgeHandle;

use super::{session_list::SessionList, sidebar_tabs::SidebarTab};

/// Top-level sidebar view that switches between Sessions/Memory/Skills tabs.
pub struct SessionSidebar {
    active: SidebarTab,
    session_list: Entity<SessionList>,
}

impl SessionSidebar {
    /// Creates the sidebar with the given service bridge.
    pub fn new(bridge: ServiceBridgeHandle, cx: &mut Context<Self>) -> Self {
        let session_list = cx.new(|cx| SessionList::new(bridge, cx));
        Self {
            active: SidebarTab::Sessions,
            session_list,
        }
    }

    /// Exposes the inner [`SessionList`] so parents can subscribe to its events.
    pub fn session_list(&self) -> &Entity<SessionList> {
        &self.session_list
    }

    fn set_active(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        if self.active != tab {
            self.active = tab;
            cx.notify();
        }
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let mut row = div()
            .flex()
            .items_center()
            .w_full()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar);
        for tab in SidebarTab::ALL {
            let active = self.active == tab;
            let tab_id = format!("sidebar-tab-{}", tab.label().to_lowercase());
            row = row.child(
                div()
                    .id(gpui::ElementId::Name(tab_id.into()))
                    .flex_1()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .text_center()
                    .border_b_2()
                    .border_color(if active {
                        theme.primary
                    } else {
                        theme.transparent
                    })
                    .hover(|s| s.text_color(theme.foreground))
                    .child(tab.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_active(tab, cx)),
                    ),
            );
        }
        row
    }

    fn render_placeholder(&self, title: &str, cx: &App) -> impl IntoElement + use<> {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap_1()
            .child(
                div()
                    .text_size(rems(0.9))
                    .text_color(theme.foreground)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Coming soon"),
            )
    }
}

impl Render for SessionSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body: gpui::AnyElement = match self.active {
            SidebarTab::Sessions => self.session_list.clone().into_any_element(),
            SidebarTab::Memory => self.render_placeholder("Memory", cx).into_any_element(),
            SidebarTab::Skills => self.render_placeholder("Skills", cx).into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(self.render_tabs(cx))
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
    }
}
