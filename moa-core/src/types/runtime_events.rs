//! Live runtime event types used by local UI surfaces.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ApprovalPrompt;

/// Inline tool card lifecycle state used by the local UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCardStatus {
    /// The tool call is known but not yet executed.
    Pending,
    /// The tool is waiting for approval.
    WaitingApproval,
    /// The tool is actively executing.
    Running,
    /// The tool completed successfully.
    Succeeded,
    /// The tool failed or was denied.
    Failed,
}

/// Update payload for a single inline tool card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUpdate {
    /// Stable tool call identifier.
    pub tool_id: Uuid,
    /// Tool name.
    pub tool_name: String,
    /// Current tool card status.
    pub status: ToolCardStatus,
    /// Concise single-line summary.
    pub summary: String,
    /// Optional detail shown below the summary.
    pub detail: Option<String>,
}

/// Live runtime update emitted by the local orchestrator for UI and CLI rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// A new assistant message started streaming.
    AssistantStarted,
    /// One streamed character from the assistant.
    AssistantDelta(char),
    /// A streamed assistant message finished.
    AssistantFinished {
        /// Final text for the completed assistant message.
        text: String,
    },
    /// A tool card should be inserted or updated.
    ToolUpdate(ToolUpdate),
    /// Human approval is required before a tool can execute.
    ApprovalRequested(ApprovalPrompt),
    /// Session token totals changed.
    UsageUpdated {
        /// Aggregate input and output token count for the current session.
        total_tokens: usize,
    },
    /// Informational status line from the runtime.
    Notice(String),
    /// The turn finished without more pending work.
    TurnCompleted,
    /// The runtime hit an error while processing the turn.
    Error(String),
}

impl RuntimeEvent {
    /// Returns the stable SSE event name for this runtime update.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::AssistantStarted => "assistant_started",
            Self::AssistantDelta(_) => "assistant_delta",
            Self::AssistantFinished { .. } => "assistant_finished",
            Self::ToolUpdate(_) => "tool_update",
            Self::ApprovalRequested(_) => "approval_requested",
            Self::UsageUpdated { .. } => "usage_updated",
            Self::Notice(_) => "notice",
            Self::TurnCompleted => "turn_completed",
            Self::Error(_) => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{RuntimeEvent, ToolCardStatus, ToolUpdate};

    #[test]
    fn runtime_event_type_uses_stable_sse_names() {
        assert_eq!(
            RuntimeEvent::AssistantStarted.event_type(),
            "assistant_started"
        );
        assert_eq!(
            RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: Uuid::now_v7(),
                tool_name: "bash".to_string(),
                status: ToolCardStatus::Pending,
                summary: "pending".to_string(),
                detail: None,
            })
            .event_type(),
            "tool_update"
        );
        assert_eq!(RuntimeEvent::TurnCompleted.event_type(), "turn_completed");
    }
}
