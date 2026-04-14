//! Sidebar memory list: search + grouped page list. Emits selections.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, SharedString, Styled, Window, div,
    prelude::*,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputEvent, InputState},
};
use moa_core::{MemoryPath, PageSummary, PageType};

use crate::components::{
    empty_state::empty_state,
    error_banner::{error_banner, with_retry},
    skeletons,
};
use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};

/// Event emitted when the user clicks a page in the memory list.
#[derive(Clone, Debug)]
pub struct MemoryPageSelected(pub MemoryPath);

/// Sidebar memory list view. Keeps selection state locally for highlighting.
pub struct MemoryList {
    bridge: ServiceBridgeHandle,
    pages: Vec<PageSummary>,
    selected_path: Option<MemoryPath>,
    search_query: String,
    search_results: Vec<moa_core::MemorySearchResult>,
    loading: bool,
    last_error: Option<String>,
    search_input: Entity<InputState>,
    /// True from `run_search` start until the async search completes.
    /// Without this, the render path would briefly show "No matches"
    /// for any non-instant query.
    search_pending: bool,
}

impl EventEmitter<MemoryPageSelected> for MemoryList {}

impl MemoryList {
    pub fn new(bridge: ServiceBridgeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search memory… (Enter)"));
        cx.subscribe(&search_input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.run_search(cx);
            }
        })
        .detach();
        cx.observe(bridge.entity(), |this, _, cx| {
            this.refresh(cx);
        })
        .detach();

        let mut this = Self {
            bridge,
            pages: Vec::new(),
            selected_path: None,
            search_query: String::new(),
            search_results: Vec::new(),
            loading: false,
            last_error: None,
            search_input,
            search_pending: false,
        };
        this.refresh(cx);
        this
    }

    /// Clears the selected-row highlight (called when the viewer is closed).
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
            async move { chat.list_memory_pages(None).await },
            |this, result, _cx| {
                this.loading = false;
                match result {
                    Ok(pages) => {
                        this.pages = pages;
                        this.last_error = None;
                    }
                    Err(err) => this.last_error = Some(format!("{err:#}")),
                }
            },
        );
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).text().to_string();
        self.search_query = query.clone();
        if query.trim().is_empty() {
            self.search_results.clear();
            self.search_pending = false;
            cx.notify();
            return;
        }
        let bridge = self.bridge.entity().read(cx);
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        self.search_pending = true;
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.search_memory(&query, 30).await },
            |this, result, _cx| {
                this.search_pending = false;
                match result {
                    Ok(results) => {
                        this.search_results = results;
                        this.last_error = None;
                    }
                    Err(err) => this.last_error = Some(format!("{err:#}")),
                }
            },
        );
    }

    fn select_path(&mut self, path: MemoryPath, cx: &mut Context<Self>) {
        self.selected_path = Some(path.clone());
        cx.emit(MemoryPageSelected(path));
        cx.notify();
    }
}

impl Render for MemoryList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let showing_search = !self.search_query.is_empty();

        let body: gpui::AnyElement = if let Some(err) = self.last_error.clone() {
            with_retry(
                error_banner(cx, "Couldn't load memory", &err),
                cx,
                cx.listener(|this, _, _, cx| {
                    this.last_error = None;
                    this.refresh(cx);
                }),
            )
            .into_any_element()
        } else if showing_search && self.search_results.is_empty() {
            if self.search_pending {
                skeletons::memory_rows(4).into_any_element()
            } else {
                let q = self.search_query.clone();
                empty_state(
                    cx,
                    "No matches",
                    SharedString::from(format!("Nothing matches “{q}”.")),
                )
                .into_any_element()
            }
        } else if !showing_search && self.pages.is_empty() {
            if self.loading {
                skeletons::memory_rows(8).into_any_element()
            } else {
                empty_state(
                    cx,
                    "No knowledge stored yet",
                    "MOA learns as you use it — memory pages appear here after your first conversations.",
                )
                .into_any_element()
            }
        } else {
            let mut list = div()
                .id("memory-list")
                .flex()
                .flex_col()
                .size_full()
                .overflow_y_scroll();
            if showing_search {
                for (idx, result) in self.search_results.iter().enumerate() {
                    let path = result.path.clone();
                    let selected = self.selected_path.as_ref() == Some(&path);
                    list = list.child(
                        div()
                            .id(gpui::ElementId::NamedInteger(
                                "memory-search-row".into(),
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
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(type_badge(&result.page_type, &theme))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.foreground)
                                            .child(SharedString::from(result.title.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(SharedString::from(result.snippet.clone())),
                            ),
                    );
                }
            } else {
                let groups = group_by_type(&self.pages);
                for (page_type, pages) in groups {
                    list = list.child(
                        div()
                            .px_3()
                            .py_1p5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .bg(theme.sidebar)
                            .child(type_label(&page_type).to_string()),
                    );
                    for summary in pages {
                        let path = summary.path.clone();
                        let selected = self.selected_path.as_ref() == Some(&path);
                        list = list.child(
                            div()
                                .id(gpui::ElementId::NamedInteger(
                                    "memory-row".into(),
                                    hash_path(&summary.path),
                                ))
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .px_3()
                                .py_1p5()
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
                                .child(div().flex().gap_2().child(
                                    crate::components::badges::confidence_badge(
                                        cx,
                                        &summary.confidence,
                                    ),
                                )),
                        );
                    }
                }
            }
            list.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(Input::new(&self.search_input).cleanable(true)),
            )
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
    }
}

fn group_by_type(pages: &[PageSummary]) -> Vec<(PageType, Vec<&PageSummary>)> {
    let order = [
        PageType::Index,
        PageType::Topic,
        PageType::Entity,
        PageType::Decision,
        PageType::Skill,
        PageType::Source,
        PageType::Schema,
        PageType::Log,
    ];
    let mut groups: Vec<(PageType, Vec<&PageSummary>)> = Vec::new();
    for page_type in order {
        let items: Vec<&PageSummary> = pages.iter().filter(|p| p.page_type == page_type).collect();
        if !items.is_empty() {
            groups.push((page_type, items));
        }
    }
    groups
}

fn type_label(page_type: &PageType) -> &'static str {
    match page_type {
        PageType::Index => "Index",
        PageType::Topic => "Topics",
        PageType::Entity => "Entities",
        PageType::Decision => "Decisions",
        PageType::Skill => "Skills",
        PageType::Source => "Sources",
        PageType::Schema => "Schemas",
        PageType::Log => "Logs",
    }
}

fn type_badge(page_type: &PageType, theme: &gpui_component::Theme) -> impl IntoElement {
    div()
        .text_xs()
        .px_1p5()
        .rounded_sm()
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .child(type_label(page_type).to_string())
}

fn hash_path(path: &moa_core::MemoryPath) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}
