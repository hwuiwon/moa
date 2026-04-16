//! Groups the flat `EventRecord` stream into OpenTelemetry-style spans so the
//! timeline can render as a waterfall instead of a wall of rows.
//!
//! Conceptual model:
//!   * Every user message opens a **Turn** (root span). Its children are
//!     whatever happens before the next user message.
//!   * Tool calls are matched to their result/error by `tool_id`, so a
//!     Tool span has a real duration.
//!   * Brain responses carry a `duration_ms`, so we back-date `start` to
//!     get an accurate brain-thinking span.
//!   * Approval request/decision pair by `request_id`, producing an
//!     Approval span that spans the wait.
//!   * Other events (memory reads, checkpoints, hand ops, errors) become
//!     instant spans — zero-width duration, rendered as a marker.

use chrono::{DateTime, Duration, Utc};
use gpui::{App, Hsla};
use gpui_component::ActiveTheme;
use moa_core::{Event, EventRecord};
use uuid::Uuid;

/// Categorises spans for colour-coding and iconography.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Turn,
    Brain,
    Tool,
    ToolError,
    Memory,
    Approval,
    Checkpoint,
    Session,
    Hand,
    Notice,
}

impl SpanKind {
    /// Theme-aware accent color, replacing the old hard-coded hex palette.
    /// Maps each kind onto a `cx.theme().*` token so dark/light parity is
    /// automatic.
    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Self::Turn => theme.info,
            Self::Brain => theme.primary,
            Self::Tool => theme.success,
            Self::ToolError => theme.danger,
            Self::Memory => theme.accent_foreground,
            Self::Approval => theme.warning,
            Self::Checkpoint => theme.warning,
            Self::Notice => theme.warning,
            Self::Session => theme.muted_foreground,
            Self::Hand => theme.muted_foreground,
        }
    }

    /// True for spans that have no inherent duration; rendered as a marker.
    pub fn is_instant(self) -> bool {
        matches!(
            self,
            Self::Memory | Self::Checkpoint | Self::Session | Self::Hand | Self::Notice
        )
    }
}

/// A span in the timeline tree.
#[derive(Clone, Debug)]
pub struct Span {
    /// Stable identifier used for expand/collapse tracking. For instant
    /// spans this is the source event id; for Turn / Tool / Approval /
    /// Brain spans it's the id of the span's *opening* event.
    pub id: Uuid,
    pub kind: SpanKind,
    /// Short label, e.g. "You", "claude-sonnet", "tool: read_file".
    pub name: String,
    /// Single-line secondary text shown next to the name.
    pub detail: String,
    pub start: DateTime<Utc>,
    /// For instant spans this equals `start`.
    pub end: DateTime<Utc>,
    pub children: Vec<Span>,
    /// The primary event backing this span, used for the "show raw"
    /// fallback in the expanded view.
    pub source: EventRecord,
}

impl Span {
    /// Effective end of this span and its descendants — needed so Turn
    /// durations expand to cover late-arriving child events.
    pub fn effective_end(&self) -> DateTime<Utc> {
        let mut latest = self.end;
        for child in &self.children {
            let child_end = child.effective_end();
            if child_end > latest {
                latest = child_end;
            }
        }
        latest
    }

    /// Duration in ms, counting the latest descendant.
    pub fn duration_ms(&self) -> i64 {
        (self.effective_end() - self.start)
            .num_milliseconds()
            .max(0)
    }
}

/// Walks a chronologically-ordered event stream and groups events into
/// turn-rooted spans. Events before the first user message are attached
/// to a synthetic "session" root.
pub fn build_spans(events: &[EventRecord]) -> Vec<Span> {
    let mut roots: Vec<Span> = Vec::new();
    let mut current_turn: Option<Span> = None;

    for record in events {
        let ts = record.timestamp;
        match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                // New turn — flush the previous one.
                if let Some(turn) = current_turn.take() {
                    roots.push(turn);
                }
                current_turn = Some(Span {
                    id: record.id,
                    kind: SpanKind::Turn,
                    name: "You".into(),
                    detail: one_line(text, 120),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                });
            }
            Event::BrainResponse {
                model,
                output_tokens,
                duration_ms,
                ..
            } => {
                let start = ts - Duration::milliseconds(*duration_ms as i64);
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Brain,
                        name: format!("brain · {model}"),
                        detail: format!("{output_tokens} output tokens"),
                        start,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::BrainThinking { summary, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Brain,
                        name: "thinking".into(),
                        detail: one_line(summary, 120),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::ToolCall {
                tool_id, tool_name, ..
            } => {
                // Open a new tool span; its end is filled by the matching
                // ToolResult/ToolError. Until then, end == start so the
                // waterfall shows a "pending" marker.
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: *tool_id,
                        kind: SpanKind::Tool,
                        name: format!("tool · {tool_name}"),
                        detail: String::new(),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::ToolResult {
                tool_id,
                success,
                duration_ms,
                ..
            } => {
                if let Some(span) = find_child_mut(&mut current_turn, *tool_id) {
                    span.end = span.start + Duration::milliseconds(*duration_ms as i64);
                    span.detail = if *success { "ok" } else { "failed" }.into();
                    if !*success {
                        span.kind = SpanKind::ToolError;
                    }
                }
            }
            Event::ToolError {
                tool_id,
                tool_name,
                error,
                ..
            } => {
                if let Some(span) = find_child_mut(&mut current_turn, *tool_id) {
                    span.end = ts;
                    span.kind = SpanKind::ToolError;
                    span.detail = one_line(error, 120);
                } else {
                    push_child(
                        &mut current_turn,
                        &mut roots,
                        Span {
                            id: *tool_id,
                            kind: SpanKind::ToolError,
                            name: format!("tool error · {tool_name}"),
                            detail: one_line(error, 120),
                            start: ts,
                            end: ts,
                            children: Vec::new(),
                            source: record.clone(),
                        },
                    );
                }
            }
            Event::ApprovalRequested {
                request_id,
                tool_name,
                ..
            } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: *request_id,
                        kind: SpanKind::Approval,
                        name: format!("approval · {tool_name}"),
                        detail: "awaiting".into(),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::ApprovalDecided {
                request_id,
                decided_by,
                ..
            } => {
                if let Some(span) = find_child_mut(&mut current_turn, *request_id) {
                    span.end = ts;
                    span.detail = format!("by {decided_by}");
                }
            }
            Event::MemoryRead { path, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Memory,
                        name: "memory read".into(),
                        detail: one_line(path, 100),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::MemoryWrite { path, summary, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Memory,
                        name: format!("memory write · {path}"),
                        detail: one_line(summary, 100),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::MemoryIngest { source_name, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Memory,
                        name: "memory ingest".into(),
                        detail: source_name.clone(),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::Checkpoint { summary, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Checkpoint,
                        name: "checkpoint".into(),
                        detail: one_line(summary, 100),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::CacheReport { report } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Notice,
                        name: format!("cache · {}", report.provider),
                        detail: format!(
                            "{} cached / {} input · {:.0}% stable",
                            report.cached_input_tokens,
                            report.input_tokens,
                            report.cache_ratio_estimate * 100.0
                        ),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::Error { message, .. } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Notice,
                        name: "error".into(),
                        detail: one_line(message, 120),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::Warning { message } => {
                push_child(
                    &mut current_turn,
                    &mut roots,
                    Span {
                        id: record.id,
                        kind: SpanKind::Notice,
                        name: "warning".into(),
                        detail: one_line(message, 120),
                        start: ts,
                        end: ts,
                        children: Vec::new(),
                        source: record.clone(),
                    },
                );
            }
            Event::HandProvisioned {
                hand_id, provider, ..
            } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Hand,
                    name: format!("hand · {provider}"),
                    detail: hand_id.clone(),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
            Event::HandDestroyed { hand_id, reason } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Hand,
                    name: format!("hand released · {hand_id}"),
                    detail: reason.clone(),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
            Event::HandError { hand_id, error } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Hand,
                    name: format!("hand error · {hand_id}"),
                    detail: one_line(error, 100),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
            Event::SessionCreated { model, .. } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Session,
                    name: "session created".into(),
                    detail: format!("model {model}"),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
            Event::SessionStatusChanged { from, to } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Session,
                    name: "status".into(),
                    detail: format!("{from:?} → {to:?}"),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
            Event::SessionCompleted {
                summary,
                total_turns,
            } => push_child(
                &mut current_turn,
                &mut roots,
                Span {
                    id: record.id,
                    kind: SpanKind::Session,
                    name: format!("completed · {total_turns} turns"),
                    detail: one_line(summary, 100),
                    start: ts,
                    end: ts,
                    children: Vec::new(),
                    source: record.clone(),
                },
            ),
        }

        // After mutating, bubble the turn's end forward to cover any
        // newly-attached child.
        if let Some(turn) = current_turn.as_mut() {
            let effective = turn.effective_end();
            if effective > turn.end {
                turn.end = effective;
            }
        }
    }

    if let Some(turn) = current_turn.take() {
        roots.push(turn);
    }

    roots
}

fn push_child(current_turn: &mut Option<Span>, roots: &mut Vec<Span>, child: Span) {
    match current_turn {
        Some(turn) => turn.children.push(child),
        None => roots.push(child),
    }
}

fn find_child_mut(current_turn: &mut Option<Span>, id: Uuid) -> Option<&mut Span> {
    current_turn
        .as_mut()?
        .children
        .iter_mut()
        .rev()
        .find(|c| c.id == id)
}

fn one_line(s: &str, limit: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        let short: String = trimmed.chars().take(limit).collect();
        format!("{short}…")
    }
}

// Intentionally no unit tests here: constructing full `EventRecord`s
// touches many unrelated core types (SequenceNum, BrainId, EventType).
// The span grouping is exercised end-to-end via `cargo run`.
