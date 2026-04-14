//! Composed sidebar: tab switcher + session list + Memory/Skills placeholders.

use gpui::{
    App, AppContext, Context, Entity, IntoElement, MouseButton, Render, Window, div, prelude::*,
    rems,
};
use gpui_component::ActiveTheme;

use crate::panels::memory::MemoryList;
use crate::panels::skills::SkillList;
use crate::services::ServiceBridgeHandle;

use super::{session_list::SessionList, sidebar_tabs::SidebarTab};

/// Top-level sidebar view that switches between Sessions/Memory/Skills tabs.
pub struct SessionSidebar {
    active: SidebarTab,
    session_list: Entity<SessionList>,
    memory_list: Entity<MemoryList>,
    skill_list: Entity<SkillList>,
}

impl SessionSidebar {
    /// Creates the sidebar with the given service bridge.
    pub fn new(bridge: ServiceBridgeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session_list = cx.new(|cx| SessionList::new(bridge.clone(), cx));
        let memory_list = cx.new(|cx| MemoryList::new(bridge.clone(), window, cx));
        let skill_list = cx.new(|cx| SkillList::new(bridge, cx));
        Self {
            active: SidebarTab::Sessions,
            session_list,
            memory_list,
            skill_list,
        }
    }

    /// Exposes the inner [`SessionList`] so parents can subscribe to its events.
    pub fn session_list(&self) -> &Entity<SessionList> {
        &self.session_list
    }

    /// Exposes the inner [`MemoryList`] so the workspace can subscribe to page selections.
    pub fn memory_list(&self) -> &Entity<MemoryList> {
        &self.memory_list
    }

    /// Exposes the inner [`SkillList`] so the workspace can subscribe to page selections.
    pub fn skill_list(&self) -> &Entity<SkillList> {
        &self.skill_list
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
            .gap_1()
            .w_full()
            .px_2()
            .py_1p5()
            .bg(theme.sidebar);
        for tab in SidebarTab::ALL {
            let active = self.active == tab;
            let tab_id = format!("sidebar-tab-{}", tab.label().to_lowercase());
            // Linear-style rounded pill: active row fills with `muted`
            // (subtle elevation) rather than a hard colored underline.
            //
            // Layout note: we use `flex_1 + flex + justify_center +
            // items_center` rather than `text_center` so the text stays
            // centered regardless of which sibling is active. Hover only
            // changes `bg` — keeping the text color constant across
            // idle/hover prevents any subpixel/kerning shift that would
            // make the label look like it moves.
            row = row.child(
                div()
                    .id(gpui::ElementId::Name(tab_id.into()))
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .when(active, |d| d.bg(theme.muted))
                    .when(!active, |d| d.hover(|s| s.bg(theme.muted)))
                    .child(tab.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_active(tab, cx)),
                    ),
            );
        }
        row
    }

    #[allow(dead_code)]
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
        let _ = cx;
        let body: gpui::AnyElement = match self.active {
            SidebarTab::Sessions => self.session_list.clone().into_any_element(),
            SidebarTab::Memory => self.memory_list.clone().into_any_element(),
            SidebarTab::Skills => self.skill_list.clone().into_any_element(),
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
