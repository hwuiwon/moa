//! Center-panel graph-memory placeholder view.

use gpui::{Context, IntoElement, Render, Styled, Window, div, prelude::*};
use gpui_component::ActiveTheme;

use crate::components::empty_state::empty_state;
use crate::services::ServiceBridgeHandle;

/// Center panel for viewing a selected graph-memory node.
pub struct MemoryViewer {
    bridge: ServiceBridgeHandle,
    uid: Option<String>,
}

impl MemoryViewer {
    /// Creates the graph-memory viewer panel.
    pub fn new(bridge: ServiceBridgeHandle, _cx: &mut Context<Self>) -> Self {
        Self { bridge, uid: None }
    }

    /// Returns whether a node is currently being viewed.
    pub fn has_page(&self) -> bool {
        self.uid.is_some()
    }

    /// Clears the current graph-memory selection.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.uid = None;
        cx.notify();
    }

    /// Opens a graph-memory node by uid.
    pub fn open(&mut self, uid: String, cx: &mut Context<Self>) {
        self.uid = Some(uid);
        cx.notify();
    }
}

impl Render for MemoryViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let _ = &self.bridge;
        let title = self
            .uid
            .clone()
            .unwrap_or_else(|| "Graph memory".to_string());

        div().size_full().bg(theme.background).child(empty_state(
            cx,
            title,
            "Select a graph memory node to inspect it.",
        ))
    }
}
