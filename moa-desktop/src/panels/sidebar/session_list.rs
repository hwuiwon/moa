//! Session list panel with search, new-session, and polling refresh.

use std::time::Duration;

use gpui::{
    Context, ElementId, EventEmitter, IntoElement, MouseButton, Render, ScrollHandle, SharedString,
    Task, Window, div, prelude::*, px, rems,
};
use gpui_component::ActiveTheme;
use moa_core::SessionId;
use moa_runtime::SessionPreview;

use crate::components::{
    empty_state::{empty_state, with_action, with_keyboard_hint},
    error_banner::{error_banner, with_retry},
    skeletons,
};
use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};

use super::session_row::SessionRow;

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Event emitted when the user selects a session in the list.
#[derive(Clone, Debug)]
pub struct SessionSelected(pub SessionId);

/// List view showing session previews for the active workspace.
pub struct SessionList {
    bridge: ServiceBridgeHandle,
    previews: Vec<SessionPreview>,
    selected: Option<SessionId>,
    query: String,
    loading: bool,
    last_error: Option<String>,
    pending: bool,
    scroll: ScrollHandle,
    /// `true` until the first successful refresh fires an auto-select of
    /// the newest session. Prevents later refreshes from stealing focus
    /// back to the top preview if the user has picked something else.
    auto_select_pending: bool,
    _poll_task: Option<Task<()>>,
}

impl EventEmitter<SessionSelected> for SessionList {}

impl SessionList {
    /// Creates a session list bound to the given service bridge.
    pub fn new(bridge: ServiceBridgeHandle, cx: &mut Context<Self>) -> Self {
        cx.observe(bridge.entity(), |this, _, cx| {
            this.refresh(cx);
        })
        .detach();

        let poll_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if weak.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });

        let mut this = Self {
            bridge,
            previews: Vec::new(),
            selected: None,
            query: String::new(),
            loading: false,
            last_error: None,
            pending: false,
            scroll: ScrollHandle::default(),
            auto_select_pending: true,
            _poll_task: Some(poll_task),
        };
        this.refresh(cx);
        this
    }

    /// Whether a given session is the currently-selected one.
    fn is_selected(&self, id: &SessionId) -> bool {
        self.selected.as_ref() == Some(id)
    }

    /// Returns previews matching the current search query.
    fn filtered(&self) -> Vec<&SessionPreview> {
        if self.query.is_empty() {
            return self.previews.iter().collect();
        }
        let needle = self.query.to_lowercase();
        self.previews
            .iter()
            .filter(|p| {
                p.summary
                    .title
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&needle))
                    .unwrap_or(false)
                    || p.summary.session_id.to_string().contains(&needle)
            })
            .collect()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        self.loading = self.previews.is_empty();
        self.pending = true;
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.list_session_previews().await },
            |this, result, cx| {
                this.pending = false;
                this.loading = false;
                match result {
                    Ok(previews) => {
                        this.previews = previews;
                        this.last_error = None;
                        // Drop a stale selection: if the previously
                        // selected session was deleted/pruned, the row
                        // is no longer in the list and the highlight
                        // would point at nothing.
                        if let Some(id) = this.selected.as_ref()
                            && !this.previews.iter().any(|p| &p.summary.session_id == id)
                        {
                            this.selected = None;
                        }
                        // First successful load: auto-select the most
                        // recent session so the chat panel isn't empty.
                        // `list_session_previews` returns newest-first
                        // (`ORDER BY updated_at DESC` in the store).
                        if this.auto_select_pending && this.selected.is_none() {
                            this.auto_select_pending = false;
                            if let Some(first) = this.previews.first() {
                                let id = first.summary.session_id;
                                this.selected = Some(id);
                                cx.emit(SessionSelected(id));
                            }
                        }
                    }
                    Err(err) => {
                        this.last_error = Some(format!("{err:#}"));
                    }
                }
            },
        );
    }

    /// Creates a new session, selects it, and emits `SessionSelected`.
    pub fn create_session(&mut self, cx: &mut Context<Self>) {
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.create_session().await },
            |this, result, cx| match result {
                Ok(session_id) => {
                    this.selected = Some(session_id);
                    cx.emit(SessionSelected(session_id));
                    this.refresh(cx);
                }
                Err(err) => {
                    this.last_error = Some(format!("create failed: {err:#}"));
                }
            },
        );
    }

    fn select_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.emit(SessionSelected(id));
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();

        let new_button = div()
            .id("session-new")
            .flex()
            .items_center()
            .justify_center()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.primary)
            .text_color(theme.primary_foreground)
            .text_xs()
            .hover(|s| s.bg(theme.primary_hover))
            .child("+ New")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.create_session(cx);
                }),
            );

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(rems(0.85))
                            .text_color(theme.sidebar_foreground)
                            .child("Sessions"),
                    )
                    .child(new_button),
            )
    }
}

impl Render for SessionList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let filtered = self.filtered();
        let header = self.render_header(cx);

        let body: gpui::AnyElement = if let Some(err) = &self.last_error {
            let detail = err.clone();
            with_retry(
                error_banner(cx, "Couldn't load sessions", &detail),
                cx,
                cx.listener(|this, _, _, cx| {
                    this.last_error = None;
                    this.refresh(cx);
                }),
            )
            .into_any_element()
        } else if self.loading && filtered.is_empty() {
            skeletons::session_rows(5).into_any_element()
        } else if filtered.is_empty() && self.query.is_empty() {
            with_keyboard_hint(
                with_action(
                    empty_state(
                        cx,
                        "Welcome to MOA",
                        "No sessions yet — start your first one to begin.",
                    ),
                    cx,
                    "+ New Session",
                    "empty-new-session",
                    cx.listener(|this, _, _, cx| this.create_session(cx)),
                ),
                cx,
                if cfg!(target_os = "macos") {
                    "⌘N"
                } else {
                    "Ctrl+N"
                },
            )
            .into_any_element()
        } else if filtered.is_empty() {
            let q = self.query.clone();
            empty_state(
                cx,
                "No matches",
                SharedString::from(format!("Nothing matches “{q}”.")),
            )
            .into_any_element()
        } else {
            let mut list = div()
                .id("session-list")
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .size_full()
                .overflow_y_scroll();
            for preview in filtered {
                let session_id = preview.summary.session_id;
                let title = preview
                    .summary
                    .title
                    .clone()
                    .unwrap_or_else(|| "Untitled".to_string());
                let row = SessionRow {
                    id: session_id,
                    title: SharedString::from(title),
                    status: preview.summary.status.clone(),
                    last_message: preview.last_message.clone().map(SharedString::from),
                    updated: preview.summary.updated_at,
                    selected: self.is_selected(&session_id),
                };
                let id_for_click = session_id;
                list = list.child(
                    div()
                        .id(ElementId::NamedInteger(
                            "session-row-wrapper".into(),
                            session_id.0.as_u128() as u64,
                        ))
                        .child(row)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.select_session(id_for_click, cx);
                            }),
                        ),
                );
            }
            list.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(header)
            .child(
                // Search bar
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        div()
                            .id("session-search")
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(theme.muted)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if self.query.is_empty() {
                                format!("/ to search · {} sessions", self.previews.len())
                            } else {
                                format!("filter: {}", self.query)
                            }),
                    ),
            )
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
            .min_w(px(0.))
    }
}
