//! Streaming IPC event payloads forwarded to the frontend.

use moa_core::{ApprovalPrompt, RiskLevel, RuntimeEvent, ToolCardStatus};
use serde::Serialize;

/// Tagged stream event sent over a Tauri channel.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "event", content = "data")]
pub enum StreamEvent {
    /// The assistant has started streaming a new response.
    AssistantStarted,
    /// One streamed text delta from the assistant.
    AssistantDelta {
        /// Incremental assistant text.
        text: String,
    },
    /// The assistant finished streaming.
    AssistantFinished {
        /// Final assistant text.
        text: String,
    },
    /// Tool card update for the current turn.
    ToolUpdate {
        /// Tool call identifier.
        call_id: String,
        /// Tool name.
        tool_name: String,
        /// Simplified status label.
        status: String,
        /// Concise one-line summary when available.
        summary: Option<String>,
    },
    /// Approval is required before a tool may proceed.
    ApprovalRequired {
        /// Approval request identifier.
        request_id: String,
        /// Tool name awaiting approval.
        tool_name: String,
        /// Risk level label.
        risk_level: String,
        /// Human-readable tool input summary.
        input_summary: String,
        /// Compact diff preview for the first proposed file edit.
        diff_preview: Option<String>,
    },
    /// Aggregate token usage changed during the turn.
    UsageUpdated {
        /// Total input + output tokens observed so far.
        total_tokens: usize,
    },
    /// Informational notice from the runtime.
    Notice {
        /// Human-readable status message.
        message: String,
    },
    /// The runtime finished the turn.
    TurnCompleted,
    /// The runtime failed during streaming.
    Error {
        /// Error message.
        message: String,
    },
}

impl From<RuntimeEvent> for StreamEvent {
    fn from(event: RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::AssistantStarted => Self::AssistantStarted,
            RuntimeEvent::AssistantDelta(delta) => Self::AssistantDelta {
                text: delta.to_string(),
            },
            RuntimeEvent::AssistantFinished { text } => Self::AssistantFinished { text },
            RuntimeEvent::ToolUpdate(update) => Self::ToolUpdate {
                call_id: update.tool_id.to_string(),
                tool_name: update.tool_name,
                status: tool_status_label(update.status).to_string(),
                summary: Some(update.summary),
            },
            RuntimeEvent::ApprovalRequested(prompt) => {
                let diff_preview = diff_preview(&prompt);
                Self::ApprovalRequired {
                    request_id: prompt.request.request_id.to_string(),
                    tool_name: prompt.request.tool_name,
                    risk_level: risk_level_label(&prompt.request.risk_level).to_string(),
                    input_summary: prompt.request.input_summary,
                    diff_preview,
                }
            }
            RuntimeEvent::UsageUpdated { total_tokens } => Self::UsageUpdated { total_tokens },
            RuntimeEvent::Notice(message) => Self::Notice { message },
            RuntimeEvent::TurnCompleted => Self::TurnCompleted,
            RuntimeEvent::Error(message) => Self::Error { message },
        }
    }
}

fn tool_status_label(status: ToolCardStatus) -> &'static str {
    match status {
        ToolCardStatus::Pending | ToolCardStatus::WaitingApproval => "pending",
        ToolCardStatus::Running => "running",
        ToolCardStatus::Succeeded => "done",
        ToolCardStatus::Failed => "error",
    }
}

fn risk_level_label(level: &RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

fn diff_preview(prompt: &ApprovalPrompt) -> Option<String> {
    let first = prompt.file_diffs.first()?;
    let mut preview = format!(
        "{}\n--- before ---\n{}\n--- after ---\n{}",
        first.path, first.before, first.after
    );
    if preview.len() > 1_000 {
        preview.truncate(997);
        preview.push_str("...");
    }
    Some(preview)
}
