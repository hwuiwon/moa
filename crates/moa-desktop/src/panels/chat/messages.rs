//! ChatMessage display model and Event → ChatMessage transformation.

use chrono::{DateTime, Utc};
use moa_core::{ApprovalDecision, ApprovalPrompt, Event, EventRecord, RiskLevel, ToolContent};
use uuid::Uuid;

/// Visual model used by the chat view.
#[allow(dead_code)] // timestamps will be shown in a future polish pass
#[derive(Clone, Debug)]
pub(crate) enum ChatMessage {
    User {
        text: String,
        timestamp: DateTime<Utc>,
    },
    Agent {
        text: String,
        model: String,
        input_tokens: usize,
        output_tokens: usize,
        cost_cents: u32,
        timestamp: DateTime<Utc>,
    },
    Thinking {
        summary: String,
        timestamp: DateTime<Utc>,
    },
    ToolTurn {
        calls: Vec<ToolInvocation>,
        timestamp: DateTime<Utc>,
    },
    Approval {
        prompt: ApprovalPrompt,
        decision: Option<ApprovalDecision>,
        decided_by: Option<String>,
        decided_at: Option<DateTime<Utc>>,
        timestamp: DateTime<Utc>,
    },
    System {
        text: String,
        timestamp: DateTime<Utc>,
    },
    Error {
        text: String,
        recoverable: bool,
        timestamp: DateTime<Utc>,
    },
}

/// Summary of a tool call paired with its result (if present yet).
#[derive(Clone, Debug)]
pub(crate) struct ToolInvocation {
    pub(crate) tool_id: Uuid,
    pub(crate) tool_name: String,
    pub(crate) input_preview: String,
    pub(crate) output_preview: Option<String>,
    pub(crate) success: Option<bool>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) risk_level: Option<RiskLevel>,
}

/// Converts an ordered sequence of event records into display messages.
///
/// Tool calls are folded into grouped [`ChatMessage::ToolTurn`]s; approvals
/// are matched to their decisions by `request_id`.
pub(crate) fn events_to_messages(records: &[EventRecord]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut pending_tools: Vec<ToolInvocation> = Vec::new();
    let mut pending_timestamp: Option<DateTime<Utc>> = None;

    let flush_tools = |out: &mut Vec<ChatMessage>,
                       pending: &mut Vec<ToolInvocation>,
                       ts: &mut Option<DateTime<Utc>>| {
        if !pending.is_empty() {
            let timestamp = ts.take().unwrap_or_else(Utc::now);
            out.push(ChatMessage::ToolTurn {
                calls: std::mem::take(pending),
                timestamp,
            });
        }
        *ts = None;
    };

    for record in records {
        match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                pending_tools.push(ToolInvocation {
                    tool_id: tool_id.0,
                    tool_name: tool_name.clone(),
                    input_preview: preview_json(input),
                    output_preview: None,
                    success: None,
                    duration_ms: None,
                    risk_level: None,
                });
                pending_timestamp.get_or_insert(record.timestamp);
            }
            Event::ToolResult {
                output,
                success,
                duration_ms,
                ..
            } => {
                if let Some(last) = pending_tools.last_mut() {
                    last.output_preview = Some(preview_tool_output(output));
                    last.success = Some(*success);
                    last.duration_ms = Some(*duration_ms);
                } else {
                    pending_tools.push(ToolInvocation {
                        tool_id: Uuid::nil(),
                        tool_name: "(result)".to_string(),
                        input_preview: String::new(),
                        output_preview: Some(preview_tool_output(output)),
                        success: Some(*success),
                        duration_ms: Some(*duration_ms),
                        risk_level: None,
                    });
                    pending_timestamp.get_or_insert(record.timestamp);
                }
            }
            Event::ToolError {
                tool_name, error, ..
            } => {
                if let Some(last) = pending_tools.last_mut() {
                    last.output_preview = Some(error.clone());
                    last.success = Some(false);
                } else {
                    pending_tools.push(ToolInvocation {
                        tool_id: Uuid::nil(),
                        tool_name: tool_name.clone(),
                        input_preview: String::new(),
                        output_preview: Some(error.clone()),
                        success: Some(false),
                        duration_ms: None,
                        risk_level: None,
                    });
                    pending_timestamp.get_or_insert(record.timestamp);
                }
            }
            Event::ApprovalRequested {
                request_id,
                tool_name,
                input_summary,
                risk_level,
                prompt,
                ..
            } => {
                flush_tools(&mut out, &mut pending_tools, &mut pending_timestamp);
                let mut resolved_prompt = prompt.clone();
                resolved_prompt.request.request_id = *request_id;
                resolved_prompt.request.tool_name = tool_name.clone();
                resolved_prompt.request.input_summary = input_summary.clone();
                resolved_prompt.request.risk_level = risk_level.clone();
                out.push(ChatMessage::Approval {
                    prompt: resolved_prompt,
                    decision: None,
                    decided_by: None,
                    decided_at: None,
                    timestamp: record.timestamp,
                });
            }
            Event::ApprovalDecided {
                request_id,
                decision,
                decided_by,
                decided_at,
                ..
            } => {
                // Patch the most recent matching Approval message.
                for msg in out.iter_mut().rev() {
                    if let ChatMessage::Approval {
                        prompt,
                        decision: slot_decision,
                        decided_by: slot_by,
                        decided_at: slot_at,
                        ..
                    } = msg
                        && prompt.request.request_id == *request_id
                    {
                        *slot_decision = Some(decision.clone());
                        *slot_by = Some(decided_by.clone());
                        *slot_at = Some(*decided_at);
                        break;
                    }
                }
            }
            other => {
                flush_tools(&mut out, &mut pending_tools, &mut pending_timestamp);
                if let Some(msg) = render_non_tool(other, record.timestamp) {
                    out.push(msg);
                }
            }
        }
    }
    flush_tools(&mut out, &mut pending_tools, &mut pending_timestamp);
    out
}

/// Returns a transient system row explaining that live runtime updates were dropped.
pub(crate) fn gap_message(count: u64) -> ChatMessage {
    ChatMessage::System {
        text: format!(
            "… {count} events missed (subscriber was behind; see session log for full history) …"
        ),
        timestamp: Utc::now(),
    }
}

fn render_non_tool(event: &Event, timestamp: DateTime<Utc>) -> Option<ChatMessage> {
    match event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
            Some(ChatMessage::User {
                text: text.clone(),
                timestamp,
            })
        }
        Event::BrainResponse {
            text,
            model,
            output_tokens,
            cost_cents,
            ..
        } => Some(ChatMessage::Agent {
            text: text.clone(),
            model: model.as_str().to_string(),
            input_tokens: event.input_tokens(),
            output_tokens: *output_tokens,
            cost_cents: *cost_cents,
            timestamp,
        }),
        Event::BrainThinking { summary, .. } => Some(ChatMessage::Thinking {
            summary: summary.clone(),
            timestamp,
        }),
        Event::SessionCreated { model, .. } => Some(ChatMessage::System {
            text: format!("Session started · {model}"),
            timestamp,
        }),
        Event::SessionStatusChanged { from, to } => Some(ChatMessage::System {
            text: format!("Status: {from:?} → {to:?}"),
            timestamp,
        }),
        Event::SessionCompleted {
            summary,
            total_turns,
        } => Some(ChatMessage::System {
            text: format!("Completed in {total_turns} turns · {summary}"),
            timestamp,
        }),
        Event::Checkpoint { summary, .. } => Some(ChatMessage::System {
            text: format!("Checkpoint · {summary}"),
            timestamp,
        }),
        Event::Warning { message } => Some(ChatMessage::System {
            text: format!("Warning: {message}"),
            timestamp,
        }),
        Event::Error {
            message,
            recoverable,
        } => Some(ChatMessage::Error {
            text: message.clone(),
            recoverable: *recoverable,
            timestamp,
        }),
        _ => None,
    }
}

fn preview_json(value: &serde_json::Value) -> String {
    let compact = serde_json::to_string(value).unwrap_or_default();
    truncate(&compact, 160)
}

fn preview_tool_output(output: &moa_core::ToolOutput) -> String {
    let joined = output
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.clone(),
            // Tools that return only structured payloads (e.g. JSON
            // search results) would otherwise render as "(no text
            // output)" — show a compact JSON preview instead.
            ToolContent::Json { data } => serde_json::to_string(data).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if joined.is_empty() {
        return "(no output)".to_string();
    }
    truncate(&joined, 2000)
}

fn truncate(input: &str, limit: usize) -> String {
    if input.chars().count() <= limit {
        input.to_string()
    } else {
        let short: String = input.chars().take(limit).collect();
        format!("{short}…")
    }
}
