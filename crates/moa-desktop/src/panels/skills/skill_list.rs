//! Sidebar skill list placeholder view.

use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Window, div, prelude::*};
use gpui_component::ActiveTheme;

use crate::components::empty_state::empty_state;
use crate::panels::memory::MemoryPageSelected;
use crate::services::ServiceBridgeHandle;

/// Sidebar list of graph-backed skills.
pub struct SkillList {
    bridge: ServiceBridgeHandle,
}

impl EventEmitter<MemoryPageSelected> for SkillList {}

impl SkillList {
    /// Creates the skill list panel.
    pub fn new(bridge: ServiceBridgeHandle, _cx: &mut Context<Self>) -> Self {
        Self { bridge }
    }

    /// Clears the selection highlight.
    #[allow(dead_code)]
    pub fn clear_selection(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for SkillList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let _ = &self.bridge;

        div().size_full().bg(theme.sidebar).child(empty_state(
            cx,
            "Skills",
            "Graph-backed skills load during context compilation.",
        ))
    }
}
