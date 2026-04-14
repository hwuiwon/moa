//! Top-level chat panel. Streams events for the active session in real time
//! and exposes a composer for sending new prompts.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Context, Entity, IntoElement, MouseButton, Render, ScrollHandle, SharedString, Styled, Task,
    Window, div, prelude::*,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputEvent, InputState},
};
use moa_core::{ApprovalDecision, RuntimeEvent, SessionId};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::components::{
    empty_state::empty_state,
    error_banner::{error_banner, with_retry},
    skeletons,
};
use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};
use crate::streaming::StreamBatcher;

use super::{message_bubble::render_message, messages::ChatMessage};

const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Renders the message stream for the selected session and the message composer.
pub struct ChatPanel {
    bridge: ServiceBridgeHandle,
    session_id: Option<SessionId>,
    messages: Vec<ChatMessage>,
    loading: bool,
    error: Option<String>,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    streaming_text: String,
    streaming_active: bool,
    running: bool,
    clear_input_pending: bool,
    expanded_tools: HashSet<Uuid>,
    // Toasts must be pushed from render where `Window` is available, so
    // async runtime-event handlers enqueue them here and the next render
    // drains the queue.
    pending_toasts: Vec<PendingToast>,
    // `attachments` collects file paths dragged onto the panel; they
    // get prepended to the next prompt submission.
    attachments: Vec<PathBuf>,
    _stream_task: Option<Task<()>>,
}

#[derive(Clone)]
enum PendingToast {
    TurnCompleted,
    ApprovalNeeded,
    Error(String),
}

impl ChatPanel {
    /// Creates an empty chat panel (no session selected yet).
    pub fn new(bridge: ServiceBridgeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Send a message… (⌘L to focus, ⏎ to submit)")
        });
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_prompt(cx);
            }
        })
        .detach();
        Self {
            bridge,
            session_id: None,
            messages: Vec::new(),
            loading: false,
            error: None,
            input,
            scroll: ScrollHandle::default(),
            streaming_text: String::new(),
            streaming_active: false,
            running: false,
            clear_input_pending: false,
            expanded_tools: HashSet::new(),
            pending_toasts: Vec::new(),
            attachments: Vec::new(),
            _stream_task: None,
        }
    }

    /// Loads and renders events for the given session, clearing any prior state.
    pub fn set_session(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.session_id.as_ref() == Some(&session_id) {
            return;
        }
        self.session_id = Some(session_id.clone());
        self.messages.clear();
        self.error = None;
        self.loading = true;
        self.streaming_text.clear();
        self.streaming_active = false;
        self.running = false;
        self._stream_task = None;
        cx.notify();
        self.reload(cx);
        self.start_stream(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            self.loading = false;
            self.error = Some("services not ready".to_string());
            cx.notify();
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
            async move { chat.session_events(session_id).await },
            move |this, result, _cx| {
                this.loading = false;
                match result {
                    Ok(events) => {
                        this.messages = super::messages::events_to_messages(&events);
                        this.error = None;
                        this.scroll.scroll_to_bottom();
                    }
                    Err(err) => {
                        this.error = Some(format!("{err:#}"));
                    }
                }
            },
        );
    }

    fn start_stream(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();

        let task = cx.spawn(async move |weak, cx| {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let session_for_task = session_id.clone();
            let _observer = handle.spawn(async move {
                let _ = chat.observe_session(session_for_task, tx).await;
            });

            let mut batcher = StreamBatcher::new(BATCH_INTERVAL);
            while let Some(session_event) = rx.recv().await {
                if let Some(batch) = batcher.push(session_event.event)
                    && weak
                        .update(cx, |this, cx| {
                            for evt in batch {
                                this.apply_runtime_event(evt, cx);
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
            let remaining = batcher.flush();
            if !remaining.is_empty() {
                let _ = weak.update(cx, |this, cx| {
                    for evt in remaining {
                        this.apply_runtime_event(evt, cx);
                    }
                    cx.notify();
                });
            }
        });
        self._stream_task = Some(task);
    }

    fn apply_runtime_event(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
        match event {
            RuntimeEvent::AssistantStarted => {
                self.streaming_text.clear();
                self.streaming_active = true;
                self.running = true;
                self.scroll.scroll_to_bottom();
            }
            RuntimeEvent::AssistantDelta(c) => {
                self.streaming_text.push(c);
                self.streaming_active = true;
                self.running = true;
                self.scroll.scroll_to_bottom();
            }
            RuntimeEvent::AssistantFinished { text: _ } => {
                self.streaming_text.clear();
                self.streaming_active = false;
                // Reload from persisted events so we get the proper ChatMessage bubble
                self.reload(cx);
            }
            RuntimeEvent::ToolUpdate(_) => {
                // Detailed tool rendering arrives in G07; just mark running and refresh.
                self.running = true;
                self.reload(cx);
            }
            RuntimeEvent::ApprovalRequested(_) => {
                self.running = true;
                self.pending_toasts.push(PendingToast::ApprovalNeeded);
                self.reload(cx);
            }
            RuntimeEvent::UsageUpdated { .. } | RuntimeEvent::Notice(_) => {}
            RuntimeEvent::TurnCompleted => {
                self.running = false;
                self.streaming_active = false;
                self.streaming_text.clear();
                self.pending_toasts.push(PendingToast::TurnCompleted);
                self.reload(cx);
            }
            RuntimeEvent::Error(msg) => {
                self.running = false;
                self.streaming_active = false;
                self.streaming_text.clear();
                self.pending_toasts.push(PendingToast::Error(msg.clone()));
                self.error = Some(msg);
            }
        }
    }

    fn submit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let user_text = self.input.read(cx).text().to_string();
        if user_text.trim().is_empty() && self.attachments.is_empty() {
            return;
        }
        // Prepend attachment paths as markdown-style references. The
        // backend doesn't yet parse these specially — the paths just show
        // up in the prompt so the model sees them.
        let prompt = if self.attachments.is_empty() {
            user_text
        } else {
            let refs: Vec<String> = self
                .attachments
                .iter()
                .map(|p| format!("[attachment: {}]", p.display()))
                .collect();
            self.attachments.clear();
            format!("{}\n\n{}", refs.join("\n"), user_text)
        };

        let bridge = self.bridge.entity().read(cx);
        if !matches!(bridge.status(), ServiceStatus::Ready) {
            return;
        }
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();

        // Optimistically render the user's bubble so it appears before
        // the actor persists `Event::UserMessage`. Subsequent reloads
        // (driven by stream events) replace this with the persisted
        // record from the store.
        self.messages.push(ChatMessage::User {
            text: prompt.clone(),
            timestamp: chrono::Utc::now(),
        });
        self.running = true;
        self.clear_input_pending = true;
        self.scroll.scroll_to_bottom();
        cx.notify();

        spawn_into(
            cx,
            handle,
            entity,
            async move { chat.queue_message(session_id, prompt).await },
            move |this, result, cx| match result {
                Ok(()) => {
                    // queue_message (re)spawns the actor; re-subscribe.
                    // Skip immediate reload — it would race the actor's
                    // persistence and momentarily wipe the optimistic
                    // user bubble. Stream events drive the next reload.
                    this.start_stream(cx);
                }
                Err(err) => {
                    this.running = false;
                    this.error = Some(format!("send failed: {err:#}"));
                    cx.notify();
                }
            },
        );
    }

    /// Toggles the expand/collapse state of a single tool card.
    pub(super) fn toggle_tool(&mut self, tool_id: Uuid, cx: &mut Context<Self>) {
        tracing::info!(%tool_id, "tool card toggle clicked");
        if self.expanded_tools.contains(&tool_id) {
            self.expanded_tools.remove(&tool_id);
        } else {
            self.expanded_tools.insert(tool_id);
        }
        cx.notify();
    }

    /// Forwards an approval decision to the orchestrator.
    pub(super) fn decide_approval(
        &mut self,
        request_id: Uuid,
        decision: ApprovalDecision,
        _pattern: &str,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(%request_id, ?decision, "approval button clicked");

        // Optimistic UI: flip the matching approval card to "decided" right away
        // so the buttons disappear and the user sees immediate feedback. The
        // subsequent reload will confirm with the persisted ApprovalDecided event.
        let mut matched = false;
        let total_approvals = self
            .messages
            .iter()
            .filter(|m| matches!(m, ChatMessage::Approval { .. }))
            .count();
        for msg in self.messages.iter_mut().rev() {
            if let ChatMessage::Approval {
                prompt,
                decision: slot_decision,
                decided_by: slot_by,
                decided_at: slot_at,
                ..
            } = msg
                && prompt.request.request_id == request_id
            {
                *slot_decision = Some(decision.clone());
                *slot_by = Some("you".to_string());
                *slot_at = Some(chrono::Utc::now());
                matched = true;
                break;
            }
        }
        tracing::info!(
            matched,
            total_approvals,
            total_messages = self.messages.len(),
            "optimistic approval update"
        );
        cx.notify();

        let Some(session_id) = self.session_id.clone() else {
            tracing::error!("approval click without an active session");
            return;
        };
        let bridge = self.bridge.entity().read(cx);
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        let decision_for_task = decision.clone();
        spawn_into(
            cx,
            handle,
            entity,
            async move {
                chat.respond_to_session_approval(session_id, request_id, decision_for_task)
                    .await
            },
            move |this, result, cx| match result {
                Ok(()) => {
                    tracing::info!("approval ack received");
                    cx.notify();
                }
                Err(err) => {
                    tracing::error!(%err, "approval failed");
                    this.error = Some(format!("approval failed: {err:#}"));
                    cx.notify();
                }
            },
        );
    }

    fn stop_session(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
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
            async move { chat.soft_cancel_session(session_id).await },
            |this, result, cx| {
                if let Err(err) = result {
                    this.error = Some(format!("cancel failed: {err:#}"));
                }
                this.running = false;
                cx.notify();
            },
        );
    }
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // Deferred input clearing: set_value needs Window, which is only available
        // during render. Submit sets `clear_input_pending`; we consume it here.
        if self.clear_input_pending {
            self.clear_input_pending = false;
            self.input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }

        // Drain pending toasts (enqueued from async runtime-event handlers
        // that don't have `Window` access). Pushing a `Notification` here
        // routes through `Root`'s NotificationList, which handles stacking
        // and auto-dismiss.
        for toast in std::mem::take(&mut self.pending_toasts) {
            match toast {
                PendingToast::TurnCompleted => {
                    crate::notifications::success(window, cx, "Session completed");
                }
                PendingToast::ApprovalNeeded => {
                    crate::notifications::warning(window, cx, "Approval needed");
                }
                PendingToast::Error(msg) => {
                    crate::notifications::error(window, cx, msg);
                }
            }
        }

        let middle: gpui::AnyElement = if let Some(err) = &self.error {
            let detail = err.clone();
            with_retry(
                error_banner(cx, "Failed to load messages", &detail),
                cx,
                cx.listener(|this, _, _, cx| {
                    this.error = None;
                    this.reload(cx);
                }),
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else if self.session_id.is_none() {
            empty_state(
                cx,
                "Select a session",
                "Pick one from the sidebar or create a new one to get started.",
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else if self.loading && self.messages.is_empty() {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(skeletons::chat_messages(3))
                .into_any_element()
        } else if self.messages.is_empty() && !self.streaming_active {
            empty_state(
                cx,
                "No messages yet",
                "Send a prompt below to start the conversation.",
            )
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else {
            let messages = self.messages.clone();
            let expanded_tools = self.expanded_tools.clone();
            let mut list = div()
                .id("chat-message-list")
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .w_full()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll();
            for (idx, message) in messages.iter().enumerate() {
                list = list.child(render_message(idx, message, &expanded_tools, window, cx));
            }
            if self.streaming_active {
                // Heal the streaming text first — close any unterminated
                // **/`/_/* markers before handing it to the markdown
                // renderer. Without this, a half-typed `**bold` would
                // consume the rest of the bubble until the model emits
                // the closer.
                let healed = crate::streaming::heal(&self.streaming_text);
                let md_id = (
                    "streaming-md",
                    self.session_id
                        .as_ref()
                        .map(|s| s.0.as_u128() as u64)
                        .unwrap_or(0),
                );
                let md = gpui_component::text::TextView::markdown(
                    md_id,
                    SharedString::from(healed),
                    window,
                    cx,
                )
                .style(crate::components::markdown::markdown_style(cx))
                .selectable(true);
                list = list.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded_md()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.primary)
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Assistant · streaming"),
                        )
                        .child(div().text_sm().text_color(theme.foreground).child(md)),
                );
            }
            list.into_any_element()
        };

        let action_button = if self.running {
            div()
                .id("chat-stop")
                .px_3()
                .py_1()
                .rounded_md()
                .bg(theme.danger)
                .text_color(theme.danger_foreground)
                .text_xs()
                .hover(|s| s.bg(theme.danger_hover))
                .child("Stop")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.stop_session(cx)),
                )
                .into_any_element()
        } else {
            div()
                .id("chat-send")
                .px_3()
                .py_1()
                .rounded_md()
                .bg(theme.primary)
                .text_color(theme.primary_foreground)
                .text_xs()
                .hover(|s| s.bg(theme.primary_hover))
                .child("Send")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.submit_prompt(cx)),
                )
                .into_any_element()
        };

        // Attachment chips, if any — rendered just above the composer so
        // users see what will be sent alongside the prompt.
        let attachments: Option<gpui::AnyElement> = if self.attachments.is_empty() {
            None
        } else {
            let mut row = div()
                .flex()
                .flex_wrap()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.background);
            for (idx, path) in self.attachments.clone().into_iter().enumerate() {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                let chip_id = format!("attach-chip-{idx}");
                let path_for_remove = path.clone();
                row = row.child(
                    div()
                        .id(gpui::ElementId::Name(chip_id.into()))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(SharedString::from(name))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .hover(|s| s.text_color(theme.danger))
                                .child("×")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    // Remove by path equality, not by
                                    // captured index — rapid clicks
                                    // could otherwise misfire after the
                                    // first removal shifts subsequent
                                    // indices.
                                    cx.listener(move |this, _, _, cx| {
                                        this.attachments.retain(|p| p != &path_for_remove);
                                        cx.notify();
                                    }),
                                ),
                        ),
                );
            }
            Some(row.into_any_element())
        };

        let composer = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.input).cleanable(false)),
            )
            .child(action_button);

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(theme.background)
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |this, paths: &gpui::ExternalPaths, _window, cx| {
                    for path in paths.paths() {
                        if !this.attachments.contains(path) {
                            this.attachments.push(path.clone());
                        }
                    }
                    cx.notify();
                },
            ))
            .child(middle);
        if let Some(chips) = attachments {
            root = root.child(chips);
        }
        root.child(composer)
    }
}
