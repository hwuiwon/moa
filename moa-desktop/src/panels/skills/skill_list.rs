//! Sidebar skill list: shows pages of [`PageType::Skill`] as compact cards.
//!
//! Rich metadata (use count, success rate, auto-generated flag) lives in
//! `SkillMetadata` inside moa-skills, but isn't currently surfaced through
//! [`ChatRuntime`]. This panel renders what's available today: title, tags via
//! confidence badge, and a clickable path that opens the full SKILL.md in the
//! center [`MemoryViewer`].

use gpui::{
    Context, EventEmitter, IntoElement, Render, SharedString, Styled, Window, div, prelude::*,
};
use gpui_component::ActiveTheme;
use moa_core::{MemoryPath, PageSummary, PageType};

use crate::panels::memory::MemoryPageSelected;
use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};

/// Sidebar list of skills. Emits [`MemoryPageSelected`] when a card is clicked
/// so the parent workspace routes it to the memory viewer.
pub struct SkillList {
    bridge: ServiceBridgeHandle,
    pages: Vec<PageSummary>,
    selected_path: Option<MemoryPath>,
    loading: bool,
    last_error: Option<String>,
}

impl EventEmitter<MemoryPageSelected> for SkillList {}

impl SkillList {
    pub fn new(bridge: ServiceBridgeHandle, cx: &mut Context<Self>) -> Self {
        cx.observe(bridge.entity(), |this, _, cx| this.refresh(cx))
            .detach();
        let mut this = Self {
            bridge,
            pages: Vec::new(),
            selected_path: None,
            loading: false,
            last_error: None,
        };
        this.refresh(cx);
        this
    }

    /// Clears the selection highlight (e.g. when the viewer is closed).
    #[allow(dead_code)]
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected_path = None;
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        self.loading = true;
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.list_memory_pages(Some(PageType::Skill)).await },
            |this, result, cx| {
                this.loading = false;
                match result {
                    Ok(pages) => {
                        this.pages = pages;
                        this.last_error = None;
                    }
                    Err(err) => this.last_error = Some(format!("{err:#}")),
                }
                cx.notify();
            },
        );
    }

    fn select_path(&mut self, path: MemoryPath, cx: &mut Context<Self>) {
        self.selected_path = Some(path.clone());
        cx.emit(MemoryPageSelected(path));
        cx.notify();
    }
}

impl Render for SkillList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        if let Some(err) = self.last_error.clone() {
            return div()
                .p_3()
                .text_xs()
                .text_color(theme.danger)
                .child(SharedString::from(err))
                .into_any_element();
        }
        if self.pages.is_empty() {
            let body = if self.loading {
                "Loading skills…"
            } else {
                "No skills yet. Complete tasks and MOA will learn from them."
            };
            return div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .p_4()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(body)
                .into_any_element();
        }

        let mut list = div()
            .id("skill-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll();

        for (idx, summary) in self.pages.iter().enumerate() {
            let path = summary.path.clone();
            let selected = self.selected_path.as_ref() == Some(&path);
            list =
                list.child(
                    div()
                        .id(gpui::ElementId::NamedInteger(
                            "skill-row".into(),
                            idx as u64,
                        ))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.sidebar_border)
                        .bg(if selected {
                            theme.accent
                        } else {
                            theme.sidebar
                        })
                        .hover(|s| s.bg(theme.muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_path(path.clone(), cx);
                        }))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.foreground)
                                .child(SharedString::from(summary.title.clone())),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(crate::components::badges::confidence_badge(
                                    cx,
                                    &summary.confidence,
                                ))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!("updated {}", summary.updated.format("%Y-%m-%d")),
                                )),
                        ),
                );
        }

        list.into_any_element()
    }
}
