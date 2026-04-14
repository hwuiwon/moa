//! Center-panel viewer that renders a selected memory page as Markdown.

use gpui::{
    Context, IntoElement, Render, SharedString, Styled, Window, div, prelude::*, px, rems,
};
use gpui_component::{ActiveTheme, text::TextView};
use moa_core::{MemoryPath, WikiPage};

use crate::components::skeletons;
use crate::services::{ServiceBridgeHandle, bridge::spawn_into};

/// Center panel for viewing a memory page in full width.
pub struct MemoryViewer {
    bridge: ServiceBridgeHandle,
    path: Option<MemoryPath>,
    page: Option<WikiPage>,
    loading: bool,
    last_error: Option<String>,
}

impl MemoryViewer {
    pub fn new(bridge: ServiceBridgeHandle, _cx: &mut Context<Self>) -> Self {
        Self {
            bridge,
            path: None,
            page: None,
            loading: false,
            last_error: None,
        }
    }

    /// Returns whether a page is currently being viewed.
    pub fn has_page(&self) -> bool {
        self.path.is_some()
    }

    /// Clears the currently viewed page (returns to chat mode).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.path = None;
        self.page = None;
        self.last_error = None;
        cx.notify();
    }

    /// Loads and displays the page at the given path.
    pub fn open(&mut self, path: MemoryPath, cx: &mut Context<Self>) {
        self.path = Some(path.clone());
        self.page = None;
        self.loading = true;
        self.last_error = None;
        cx.notify();

        let bridge = self.bridge.entity().read(cx);
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.read_memory_page(&path).await },
            |this, result, _cx| {
                this.loading = false;
                match result {
                    Ok(page) => this.page = Some(page),
                    Err(err) => this.last_error = Some(format!("{err:#}")),
                }
            },
        );
    }
}

impl Render for MemoryViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let body: gpui::AnyElement = if let Some(err) = self.last_error.clone() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_1()
                .min_h_0()
                .p_4()
                .text_sm()
                .text_color(theme.danger)
                .child(SharedString::from(err))
                .into_any_element()
        } else if let Some(page) = self.page.as_ref() {
            let md_id = ("memory-viewer-md", hash_key(&page.title));
            let md =
                TextView::markdown(md_id, SharedString::from(page.content.clone()), window, cx)
                    .style(crate::components::markdown::markdown_style(cx))
                    .selectable(true);
            div()
                .id("memory-viewer-scroll")
                .flex()
                .flex_col()
                .gap_3()
                .p_6()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .pb_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_size(rems(1.25))
                                .text_color(theme.foreground)
                                .child(SharedString::from(page.title.clone())),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .px_1p5()
                                        .rounded_sm()
                                        .bg(theme.muted)
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{:?}", page.page_type).to_lowercase()),
                                )
                                .child(crate::components::badges::confidence_badge(
                                    cx,
                                    &page.confidence,
                                ))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!(
                                            "updated {}",
                                            page.updated.format("%Y-%m-%d %H:%M")
                                        )),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .line_height(crate::density::current(cx).spacing().markdown_line_height)
                        .max_w(px(720.0))
                        .child(md),
                )
                .into_any_element()
        } else {
            skeletons::memory_page().into_any_element()
        };

        let header: Option<gpui::AnyElement> = self.path.as_ref().map(|_| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Memory"),
                )
                .child(
                    div()
                        .id("memory-viewer-close")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .hover(|s| s.text_color(theme.foreground))
                        .child("Close")
                        .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                )
                .into_any_element()
        });

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(theme.background);
        if let Some(h) = header {
            root = root.child(h);
        }
        root.child(body)
    }
}

fn hash_key(title: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut hasher);
    hasher.finish()
}
