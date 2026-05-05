//! Session info summary + event timeline / cost tab, updated live from the stream.

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    ClipboardItem, Context, ElementId, IntoElement, MouseButton, Render, ScrollHandle,
    SharedString, Styled, Task, Window, div, prelude::*, px,
};
use gpui_component::ActiveTheme;
use moa_core::{Event, EventRecord, LiveEvent, SessionId, SessionSummary};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::components::skeletons;
use crate::services::{ServiceBridgeHandle, ServiceStatus, bridge::spawn_into};
use crate::streaming::StreamBatcher;

use super::spans::{Span, build_spans};

const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Tabs for the detail panel body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailTab {
    Timeline,
    Cost,
}

impl DetailTab {
    const ALL: [DetailTab; 2] = [DetailTab::Timeline, DetailTab::Cost];
    fn label(self) -> &'static str {
        match self {
            DetailTab::Timeline => "Timeline",
            DetailTab::Cost => "Cost",
        }
    }
}

/// Right-side detail panel for the selected session.
pub struct DetailPanel {
    bridge: ServiceBridgeHandle,
    session_id: Option<SessionId>,
    session_summary: Option<SessionSummary>,
    events: Vec<EventRecord>,
    expanded: HashSet<Uuid>,
    active_tab: DetailTab,
    loading: bool,
    last_error: Option<String>,
    scroll: ScrollHandle,
    cost_scroll: ScrollHandle,
    _stream_task: Option<Task<()>>,
}

impl DetailPanel {
    /// Creates an empty detail panel (no session selected).
    pub fn new(bridge: ServiceBridgeHandle, _cx: &mut Context<Self>) -> Self {
        Self {
            bridge,
            session_id: None,
            session_summary: None,
            events: Vec::new(),
            expanded: HashSet::new(),
            active_tab: DetailTab::Timeline,
            loading: false,
            last_error: None,
            scroll: ScrollHandle::default(),
            cost_scroll: ScrollHandle::default(),
            _stream_task: None,
        }
    }

    fn set_active_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        if self.active_tab != tab {
            self.active_tab = tab;
            cx.notify();
        }
    }

    /// Switches to a new session, reloading events and restarting the stream.
    pub fn set_session(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.session_id.as_ref() == Some(&session_id) {
            return;
        }
        self.session_id = Some(session_id);
        self.events.clear();
        self.session_summary = None;
        self.expanded.clear();
        self.loading = true;
        self.last_error = None;
        self._stream_task = None;
        cx.notify();

        self.reload_events(cx);
        self.refresh_summary(cx);
        self.start_stream(cx);
    }

    fn reload_events(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
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
            async move { chat.session_events(session_id).await },
            move |this, result, _cx| {
                if this.session_id != Some(session_id) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(events) => {
                        this.events = events;
                        this.last_error = None;
                    }
                    Err(err) => this.last_error = Some(format!("{err:#}")),
                }
            },
        );
    }

    fn refresh_summary(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let bridge = self.bridge.entity().read(cx);
        let Some(chat) = bridge.chat_runtime() else {
            return;
        };
        let handle = bridge.tokio_handle();
        let entity = cx.entity().clone();
        let needle = session_id;
        spawn_into(
            cx,
            handle,
            entity,
            async move {
                chat.list_sessions()
                    .await
                    .map(|sessions| sessions.into_iter().find(|s| s.session_id == needle))
            },
            move |this, result, _cx| {
                if this.session_id != Some(session_id) {
                    return;
                }
                if let Ok(Some(summary)) = result {
                    this.session_summary = Some(summary);
                }
            },
        );
    }

    fn start_stream(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
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
            // Observation doesn't spawn the actor — backoff-poll until it exists.
            let mut retry_delay = Duration::from_millis(500);
            let max_delay = Duration::from_secs(3);
            loop {
                let still_current = weak
                    .update(cx, |this, _| this.session_id.as_ref() == Some(&session_id))
                    .unwrap_or(false);
                if !still_current {
                    return;
                }

                let (tx, mut rx) = mpsc::unbounded_channel();
                let observe_session = session_id;
                let chat_clone = chat.clone();
                let observer = handle.spawn(async move {
                    let _ = chat_clone.observe_session(observe_session, tx).await;
                });

                let mut received_any = false;
                let mut batcher = StreamBatcher::new(BATCH_INTERVAL);
                while let Some(session_event) = rx.recv().await {
                    received_any = true;
                    match session_event.event {
                        LiveEvent::Event(event) => {
                            if let Some(_batch) = batcher.push(event)
                                && weak
                                    .update(cx, |this, cx| {
                                        this.reload_events(cx);
                                        this.refresh_summary(cx);
                                    })
                                    .is_err()
                            {
                                observer.abort();
                                return;
                            }
                        }
                        LiveEvent::Gap { .. } => {
                            if weak
                                .update(cx, |this, cx| {
                                    this.reload_events(cx);
                                    this.refresh_summary(cx);
                                })
                                .is_err()
                            {
                                observer.abort();
                                return;
                            }
                        }
                    }
                }
                // Avoid leaking the observer task across reconnect iterations.
                observer.abort();

                if received_any {
                    retry_delay = Duration::from_millis(500);
                } else {
                    retry_delay = (retry_delay * 2).min(max_delay);
                }
                cx.background_executor().timer(retry_delay).await;
            }
        });
        self._stream_task = Some(task);
    }

    fn toggle_expand(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.expanded.contains(&id) {
            self.expanded.remove(&id);
        } else {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    fn render_session_info(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let (status_label, status_color) = match self.session_summary.as_ref().map(|s| &s.status) {
            Some(status) => (
                crate::components::badges::status_label(status),
                crate::components::badges::status_color(cx, status),
            ),
            None => ("—", theme.muted_foreground),
        };

        let turns = count_turns(&self.events);
        let tool_calls = count_event_type(&self.events, |e| matches!(e, Event::ToolCall { .. }));
        let (in_tokens, out_tokens, cost_cents) = aggregate_brain_usage(&self.events);
        let pending_approvals = count_pending_approvals(&self.events);
        let latest_input = latest_input_tokens(&self.events);
        let model = self
            .session_summary
            .as_ref()
            .map(|s| s.model.clone())
            .unwrap_or_else(|| moa_core::ModelId::new("—"));
        let context_window = estimated_context_window(model.as_str());
        let ctx_pct = if context_window > 0 {
            (latest_input as f32 / context_window as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ctx_bar_color = if ctx_pct >= 0.9 {
            theme.danger
        } else if ctx_pct >= 0.7 {
            theme.warning
        } else {
            theme.primary
        };
        let duration = session_duration(&self.events).map(format_duration_short);
        let idle = last_activity(&self.events).map(format_idle_short);

        let status_row = {
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().size(px(8.)).rounded_full().bg(status_color))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(status_label),
                );
            if let Some(session_id) = self.session_id.as_ref() {
                row = row
                    .child(div().flex_1())
                    .child(session_id_chip(session_id, theme.muted_foreground));
            }
            row
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar)
            .child(status_row)
            .child(metric_row("model", model.as_str(), &theme))
            .child(metric_row("turns", &turns.to_string(), &theme))
            .child(metric_row("tools", &tool_calls.to_string(), &theme))
            .when(pending_approvals > 0, |d| {
                d.child(metric_row(
                    "pending approvals",
                    &pending_approvals.to_string(),
                    &theme,
                ))
            })
            .when_some(duration, |d, dur| {
                d.child(metric_row("duration", &dur, &theme))
            })
            .when_some(idle, |d, idle| {
                d.child(metric_row("last activity", &idle, &theme))
            })
            .child(metric_row(
                "tokens",
                &format!("in {in_tokens} · out {out_tokens}"),
                &theme,
            ))
            .child(metric_row(
                "cost",
                &format!("${:.4}", cost_cents as f64 / 10_000.0),
                &theme,
            ))
            // Context-usage meter: most-recent `BrainResponse.input_tokens`
            // divided by the model's estimated context window. Color grades
            // from primary → warning → danger as the percentage climbs.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_xs()
                            .child(div().text_color(theme.muted_foreground).child("context"))
                            .child(div().text_color(theme.foreground).child(format!(
                                "{} / {} ({}%)",
                                fmt_compact(latest_input),
                                fmt_compact(context_window),
                                (ctx_pct * 100.0) as u32
                            ))),
                    )
                    .child(
                        div()
                            .h(px(4.0))
                            .w_full()
                            .rounded_full()
                            .bg(theme.muted)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(ctx_bar_color)
                                    .w(gpui::relative(ctx_pct)),
                            ),
                    ),
            )
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();

        if self.session_id.is_none() {
            return div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Select a session")
                .into_any_element();
        }
        if self.loading && self.events.is_empty() {
            return skeletons::timeline_nodes(6).into_any_element();
        }
        if self.events.is_empty() {
            return div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No events yet")
                .into_any_element();
        }

        let turns = build_spans(&self.events);
        let mut list = div()
            .id("timeline-list")
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .size_full()
            .overflow_y_scroll();

        for turn in turns.iter() {
            list = list.child(self.render_turn(turn, &theme, cx));
        }
        list.into_any_element()
    }

    fn render_turn(
        &self,
        turn: &Span,
        theme: &gpui_component::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded.contains(&turn.id);
        let duration_ms = turn.duration_ms().max(1);
        let turn_id = turn.id;
        let child_count = turn.children.len();
        let total_ms = turn.duration_ms();

        // Header row. Click toggles expansion; same `expanded` map as before.
        let header = div()
            .id(gpui::ElementId::NamedInteger(
                "turn-header".into(),
                turn.id.as_u128() as u64,
            ))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .bg(theme.sidebar)
            .border_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.muted))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_expand(turn_id, cx)))
            .child(
                div()
                    .w(px(10.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if expanded { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.foreground)
                            .child(SharedString::from(turn.detail.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(turn.start.format("%H:%M:%S").to_string())
                            .child(format!("{child_count} steps"))
                            .child(format_duration(total_ms)),
                    ),
            );

        let mut outer = div().flex().flex_col().gap_1().child(header);

        if expanded {
            // Waterfall body.
            let mut body = div().flex().flex_col().gap_0p5().px_2().pt_1().pb_2();
            for child in &turn.children {
                body = body.child(render_span_row(
                    child,
                    turn.start,
                    duration_ms as f32,
                    0,
                    &self.expanded,
                    theme,
                    cx,
                ));
            }
            outer = outer.child(body);
        }

        outer.into_any_element()
    }
}

/// Renders a single span as an OTel-waterfall row.
fn render_span_row(
    span: &Span,
    turn_start: chrono::DateTime<chrono::Utc>,
    turn_duration_ms: f32,
    depth: u8,
    expanded: &HashSet<Uuid>,
    theme: &gpui_component::Theme,
    cx: &mut Context<DetailPanel>,
) -> gpui::AnyElement {
    let color = span.kind.color(cx);
    let duration_ms = span.duration_ms();
    let offset_ms = (span.start - turn_start).num_milliseconds().max(0) as f32;
    let offset_pct = (offset_ms / turn_duration_ms).clamp(0.0, 1.0);
    // Instant spans get a tiny non-zero width so there's something to see.
    let min_pct = if span.kind.is_instant() { 0.01 } else { 0.015 };
    let width_pct = ((duration_ms as f32) / turn_duration_ms)
        .max(min_pct)
        .min(1.0 - offset_pct);

    let has_children = !span.children.is_empty();
    let show_detail = expanded.contains(&span.id);
    let span_id = span.id;

    // Left column: indented name, duration.
    let name_col = div()
        .flex()
        .items_center()
        .gap_1()
        .w(px(200.))
        .pl(px(f32::from(depth) * 12.0))
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .text_color(theme.foreground)
                .overflow_x_hidden()
                .child(SharedString::from(span.name.clone())),
        );

    // Waterfall bar track + colored bar positioned at offset_pct, width_pct.
    let bar_track = div()
        .relative()
        .flex_1()
        .min_w(px(40.))
        .h(px(6.))
        .rounded_full()
        .bg(theme.muted)
        .child(
            div()
                .absolute()
                .top_0()
                .left(gpui::relative(offset_pct))
                .w(gpui::relative(width_pct))
                .h_full()
                .rounded_full()
                .bg(color),
        );

    let duration_col = div()
        .w(px(58.))
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(format_duration(duration_ms));

    let row = div()
        .id(gpui::ElementId::NamedInteger(
            "span-row".into(),
            span.id.as_u128() as u64,
        ))
        .flex()
        .items_center()
        .gap_2()
        .py_0p5()
        .hover(|s| s.bg(theme.muted))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_expand(span_id, cx)))
        .child(name_col)
        .child(bar_track)
        .child(duration_col);

    let mut container = div().flex().flex_col().child(row);

    // Detail panel (raw event payload) when the user clicks the row.
    if show_detail {
        let detail_text = format!("{:#?}", span.source.event);
        let detail_short: String = if !span.detail.is_empty() {
            format!("{}\n\n{}", span.detail, detail_text)
        } else {
            detail_text
        };
        container = container.child(
            div()
                .mt_0p5()
                .ml(px(f32::from(depth) * 12.0 + 14.0))
                .p_2()
                .rounded_sm()
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(SharedString::from(detail_short)),
        );
    }

    // Nested children (tool → sub-tool not produced currently, but kept for
    // future-proofing). When a span has children, render them indented.
    if has_children {
        for child in &span.children {
            container = container.child(render_span_row(
                child,
                turn_start,
                turn_duration_ms,
                depth + 1,
                expanded,
                theme,
                cx,
            ));
        }
    }

    container.into_any_element()
}

fn format_duration(ms: i64) -> String {
    if ms < 1 {
        "·".into()
    } else if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f32 / 1_000.0)
    }
}

impl Render for DetailPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let info = self.render_session_info(cx);
        let tabs = self.render_tabs(cx);
        let body: gpui::AnyElement = match self.active_tab {
            DetailTab::Timeline => self.render_timeline(cx),
            DetailTab::Cost => self.render_cost(cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .child(info)
            .child(tabs)
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
    }
}

impl DetailPanel {
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let mut row = div()
            .flex()
            .items_center()
            .w_full()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar);
        for tab in DetailTab::ALL {
            let active = self.active_tab == tab;
            let id = format!("detail-tab-{}", tab.label().to_lowercase());
            // Flex-centering + stable text color across idle/hover so the
            // label doesn't shift when the pointer moves between tabs —
            // same fix pattern as the sidebar tabs.
            row = row.child(
                div()
                    .id(gpui::ElementId::Name(id.into()))
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_2()
                    .py_1p5()
                    .text_xs()
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .border_b_2()
                    .border_color(if active {
                        theme.primary
                    } else {
                        theme.transparent
                    })
                    .when(!active, |d| d.hover(|s| s.bg(theme.muted)))
                    .child(tab.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_active_tab(tab, cx)),
                    ),
            );
        }
        row
    }

    fn render_cost(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let turns = collect_turn_costs(&self.events);

        if turns.is_empty() {
            return div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(if self.session_id.is_none() {
                    "Select a session"
                } else {
                    "No brain turns yet"
                })
                .into_any_element();
        }

        let total_cents: u32 = turns.iter().map(|t| t.cost_cents).sum();
        let max_cents = turns.iter().map(|t| t.cost_cents).max().unwrap_or(1).max(1);
        let avg_cents = total_cents as f64 / turns.len() as f64;
        let most_expensive = turns
            .iter()
            .max_by_key(|t| t.cost_cents)
            .map(|t| t.cost_cents)
            .unwrap_or(0);

        // `turns` count was here but is already shown in the session-
        // info block above — keep this summary to cost-only metrics.
        let stats = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(metric_row(
                "total",
                &format!("${:.4}", total_cents as f64 / 10_000.0),
                &theme,
            ))
            .child(metric_row(
                "avg/turn",
                &format!("${:.4}", avg_cents / 10_000.0),
                &theme,
            ))
            .child(metric_row(
                "max/turn",
                &format!("${:.4}", most_expensive as f64 / 10_000.0),
                &theme,
            ));

        let mut list = div()
            .id("cost-list")
            .track_scroll(&self.cost_scroll)
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .size_full()
            .overflow_y_scroll();

        let mut running = 0u32;
        let mut prev_model: Option<String> = None;
        for turn in turns {
            running = running.saturating_add(turn.cost_cents);
            let fraction = turn.cost_cents as f32 / max_cents as f32;
            let bar = div()
                .h(px(6.))
                .w_full()
                .rounded_full()
                .bg(theme.muted)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(theme.primary)
                        .w(gpui::relative(fraction.clamp(0.0, 1.0))),
                );

            // Only surface the model badge when it differs from the
            // previous turn — the session-info block above already
            // shows the overall model, so repeating it on every row
            // is just visual noise unless the user actually switched
            // models mid-session.
            let model_changed = prev_model.as_deref() != Some(turn.model.as_str());
            let header_left = {
                let mut row = div().flex().items_center().gap_2().child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("turn {}", turn.turn)),
                );
                if model_changed {
                    row = row.child(
                        div()
                            .text_xs()
                            .px_1p5()
                            .rounded_sm()
                            .bg(theme.muted)
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(turn.model.clone())),
                    );
                }
                row
            };
            prev_model = Some(turn.model.clone());

            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(header_left)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.foreground)
                                    .child(format!("${:.4}", turn.cost_cents as f64 / 10_000.0)),
                            ),
                    )
                    .child(bar)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "in {} · out {}",
                                turn.input_tokens, turn.output_tokens
                            ))
                            .child(format!("running ${:.4}", running as f64 / 10_000.0)),
                    ),
            );
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(stats)
            .child(div().flex().flex_1().min_h_0().child(list))
            .into_any_element()
    }
}

struct TurnCost {
    turn: usize,
    model: String,
    input_tokens: usize,
    output_tokens: usize,
    cost_cents: u32,
}

fn collect_turn_costs(events: &[EventRecord]) -> Vec<TurnCost> {
    let mut out = Vec::new();
    let mut turn = 0usize;
    for rec in events {
        if let Event::BrainResponse {
            model,
            output_tokens,
            cost_cents,
            ..
        } = &rec.event
        {
            turn += 1;
            out.push(TurnCost {
                turn,
                model: model.as_str().to_string(),
                input_tokens: rec.event.input_tokens(),
                output_tokens: *output_tokens,
                cost_cents: *cost_cents,
            });
        }
    }
    out
}

/// Short (8-char) session-id chip — click copies the full UUID to the
/// clipboard. Shown at the top of the detail panel so a user can
/// reference the active session in logs, scripts, or bug reports
/// without re-surfacing the id on every sidebar row.
fn session_id_chip(id: &SessionId, text_color: gpui::Hsla) -> impl IntoElement + use<> {
    let short: SharedString = format!("{:.8}", id.0.simple()).into();
    let full = id.0.to_string();
    let chip_id = ElementId::NamedInteger("detail-session-id-chip".into(), id.0.as_u128() as u64);
    div()
        .id(chip_id)
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .text_xs()
        .text_color(text_color)
        .child(short)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
            crate::notifications::success(window, cx, "Session id copied");
        })
}

fn metric_row(label: &str, value: &str, theme: &gpui_component::Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .text_xs()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(div().text_color(theme.foreground).child(value.to_string()))
}

fn count_turns(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|rec| {
            matches!(
                &rec.event,
                Event::UserMessage { .. } | Event::QueuedMessage { .. }
            )
        })
        .count()
}

fn count_event_type<F>(events: &[EventRecord], pred: F) -> usize
where
    F: Fn(&Event) -> bool,
{
    events.iter().filter(|rec| pred(&rec.event)).count()
}

fn aggregate_brain_usage(events: &[EventRecord]) -> (usize, usize, u32) {
    let mut in_tokens = 0;
    let mut out_tokens = 0;
    let mut cost_cents = 0u32;
    for rec in events {
        if let Event::BrainResponse {
            output_tokens,
            cost_cents: c,
            ..
        } = &rec.event
        {
            in_tokens += rec.event.input_tokens();
            out_tokens += output_tokens;
            cost_cents = cost_cents.saturating_add(*c);
        }
    }
    (in_tokens, out_tokens, cost_cents)
}

/// Input-token count of the most recent `BrainResponse` in the stream.
/// Used to show "how much context did the last turn consume" in the
/// detail panel's context-usage meter.
fn latest_input_tokens(events: &[EventRecord]) -> usize {
    for rec in events.iter().rev() {
        if let Event::BrainResponse { .. } = &rec.event {
            return rec.event.input_tokens();
        }
    }
    0
}

/// Context-window size for a model id, sourced from the provider
/// catalog (`moa_providers::context_window`). Unknown models fall back
/// to 200 000 so the progress bar still renders rather than blanking.
fn estimated_context_window(model: &str) -> usize {
    moa_providers::context_window(model).unwrap_or(200_000)
}

/// Count of `ApprovalRequested` events that don't yet have a matching
/// `ApprovalDecided` in the same stream. Surfaced prominently in the
/// session-info block so a blocked turn is visible at a glance.
fn count_pending_approvals(events: &[EventRecord]) -> usize {
    let mut pending = std::collections::HashSet::new();
    for rec in events {
        match &rec.event {
            Event::ApprovalRequested { request_id, .. } => {
                pending.insert(*request_id);
            }
            Event::ApprovalDecided { request_id, .. } => {
                pending.remove(request_id);
            }
            _ => {}
        }
    }
    pending.len()
}

/// Wall-clock span from the first event to the last.
fn session_duration(events: &[EventRecord]) -> Option<chrono::Duration> {
    let first = events.first()?;
    let last = events.last()?;
    Some(last.timestamp - first.timestamp)
}

/// Time from the most recent event to now. Used as "last activity".
fn last_activity(events: &[EventRecord]) -> Option<chrono::Duration> {
    let last = events.last()?;
    Some(chrono::Utc::now() - last.timestamp)
}

/// "3h 42m" / "15m" / "42s" formatter — keeps width stable so rows
/// don't wobble as the duration ticks up.
fn format_duration_short(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Same formatter for "idle" — "just now" when the last event was
/// under 5 seconds ago to reassure the user that streaming is active.
fn format_idle_short(d: chrono::Duration) -> String {
    if d.num_seconds() < 5 {
        "just now".into()
    } else {
        format!("{} ago", format_duration_short(d))
    }
}

/// Compact integer formatter: 1234 → "1.2K", 1_200_000 → "1.2M".
fn fmt_compact(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
