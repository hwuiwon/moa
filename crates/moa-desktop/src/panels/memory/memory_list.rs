//! Sidebar graph-memory placeholder view.

use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Window, div, prelude::*};
use gpui_component::ActiveTheme;

use crate::components::empty_state::empty_state;
use crate::services::ServiceBridgeHandle;

/// Event emitted when a graph memory node is selected.
#[derive(Clone, Debug)]
pub struct MemoryPageSelected(pub String);

/// Sidebar memory list view.
pub struct MemoryList {
    bridge: ServiceBridgeHandle,
}

impl EventEmitter<MemoryPageSelected> for MemoryList {}

impl MemoryList {
    /// Creates the graph-memory list panel.
    pub fn new(bridge: ServiceBridgeHandle, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { bridge }
    }

    /// Clears the selected row highlight.
    #[allow(dead_code)]
    pub fn clear_selection(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for MemoryList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let _ = &self.bridge;

        div().size_full().bg(theme.sidebar).child(empty_state(
            cx,
            "Graph memory",
            "Graph memory nodes are available through `moa memory search` and `moa memory show`.",
        ))
    }
}
