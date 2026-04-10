//! Session event definitions and helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{
    ApprovalDecision, ApprovalPrompt, Attachment, EventType, RiskLevel, SessionStatus, ToolOutput,
};

/// Append-only session event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    /// Session was created.
    SessionCreated {
        /// Workspace identifier.
        workspace_id: String,
        /// User identifier.
        user_id: String,
        /// Model identifier.
        model: String,
    },
    /// Session status changed.
    SessionStatusChanged {
        /// Previous status.
        from: SessionStatus,
        /// New status.
        to: SessionStatus,
    },
    /// Session completed successfully.
    SessionCompleted {
        /// Human-readable summary.
        summary: String,
        /// Number of turns completed.
        total_turns: u32,
    },
    /// A user authored message.
    UserMessage {
        /// Message text.
        text: String,
        /// Attached files or media.
        attachments: Vec<Attachment>,
    },
    /// A user message was queued for later processing.
    QueuedMessage {
        /// Queued message text.
        text: String,
        /// Queue timestamp.
        queued_at: DateTime<Utc>,
    },
    /// The brain emitted a short thinking summary.
    BrainThinking {
        /// Summary text.
        summary: String,
        /// Tokens used for the internal reasoning summary.
        token_count: usize,
    },
    /// The brain emitted a visible response.
    BrainResponse {
        /// Response text.
        text: String,
        /// Model identifier.
        model: String,
        /// Input token count.
        input_tokens: usize,
        /// Output token count.
        output_tokens: usize,
        /// Cost in cents.
        cost_cents: u32,
        /// Duration in milliseconds.
        duration_ms: u64,
    },
    /// A tool call was issued.
    ToolCall {
        /// Unique tool call identifier.
        tool_id: Uuid,
        /// Provider-specific tool-use identifier, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Tool name.
        tool_name: String,
        /// Full tool input.
        input: Value,
        /// Hand identifier, when applicable.
        hand_id: Option<String>,
    },
    /// A tool call completed.
    ToolResult {
        /// Matching tool call identifier.
        tool_id: Uuid,
        /// Provider-specific tool-use identifier, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Full tool output.
        output: ToolOutput,
        /// Whether execution succeeded.
        success: bool,
        /// Duration in milliseconds.
        duration_ms: u64,
    },
    /// A tool call failed.
    ToolError {
        /// Matching tool call identifier.
        tool_id: Uuid,
        /// Error message.
        error: String,
        /// Whether the failure is retryable.
        retryable: bool,
    },
    /// A tool call needs approval.
    ApprovalRequested {
        /// Approval request identifier.
        request_id: Uuid,
        /// Tool name.
        tool_name: String,
        /// Concise input summary.
        input_summary: String,
        /// Assigned risk level.
        risk_level: RiskLevel,
        /// Full approval prompt with parsed parameters, diffs, and allow pattern.
        ///
        /// TODO: Use a claim-check pattern for large diffs in cloud mode.
        #[serde(default)]
        prompt: Option<ApprovalPrompt>,
    },
    /// An approval request was decided.
    ApprovalDecided {
        /// Approval request identifier.
        request_id: Uuid,
        /// User decision.
        decision: ApprovalDecision,
        /// User who decided.
        decided_by: String,
        /// Decision timestamp.
        decided_at: DateTime<Utc>,
    },
    /// Memory read operation.
    MemoryRead {
        /// Logical page path.
        path: String,
        /// Scope identifier.
        scope: String,
    },
    /// Memory write operation.
    MemoryWrite {
        /// Logical page path.
        path: String,
        /// Scope identifier.
        scope: String,
        /// Human-readable summary.
        summary: String,
    },
    /// Hand was provisioned.
    HandProvisioned {
        /// Hand identifier.
        hand_id: String,
        /// Provider name.
        provider: String,
        /// Sandbox tier name.
        tier: String,
    },
    /// Hand was destroyed.
    HandDestroyed {
        /// Hand identifier.
        hand_id: String,
        /// Reason for destruction.
        reason: String,
    },
    /// Hand encountered an error.
    HandError {
        /// Hand identifier.
        hand_id: String,
        /// Error message.
        error: String,
    },
    /// Checkpoint event used for compaction.
    Checkpoint {
        /// Summary text.
        summary: String,
        /// Number of events summarized.
        events_summarized: u64,
        /// Tokens in the summary.
        token_count: usize,
    },
    /// Recoverable or fatal error.
    Error {
        /// Error message.
        message: String,
        /// Whether the error is recoverable.
        recoverable: bool,
    },
    /// Warning event.
    Warning {
        /// Warning message.
        message: String,
    },
}

impl Event {
    /// Returns the event discriminator.
    pub fn event_type(&self) -> EventType {
        match self {
            Self::SessionCreated { .. } => EventType::SessionCreated,
            Self::SessionStatusChanged { .. } => EventType::SessionStatusChanged,
            Self::SessionCompleted { .. } => EventType::SessionCompleted,
            Self::UserMessage { .. } => EventType::UserMessage,
            Self::QueuedMessage { .. } => EventType::QueuedMessage,
            Self::BrainThinking { .. } => EventType::BrainThinking,
            Self::BrainResponse { .. } => EventType::BrainResponse,
            Self::ToolCall { .. } => EventType::ToolCall,
            Self::ToolResult { .. } => EventType::ToolResult,
            Self::ToolError { .. } => EventType::ToolError,
            Self::ApprovalRequested { .. } => EventType::ApprovalRequested,
            Self::ApprovalDecided { .. } => EventType::ApprovalDecided,
            Self::MemoryRead { .. } => EventType::MemoryRead,
            Self::MemoryWrite { .. } => EventType::MemoryWrite,
            Self::HandProvisioned { .. } => EventType::HandProvisioned,
            Self::HandDestroyed { .. } => EventType::HandDestroyed,
            Self::HandError { .. } => EventType::HandError,
            Self::Checkpoint { .. } => EventType::Checkpoint,
            Self::Error { .. } => EventType::Error,
            Self::Warning { .. } => EventType::Warning,
        }
    }

    /// Returns a stable type name for storage.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "SessionCreated",
            Self::SessionStatusChanged { .. } => "SessionStatusChanged",
            Self::SessionCompleted { .. } => "SessionCompleted",
            Self::UserMessage { .. } => "UserMessage",
            Self::QueuedMessage { .. } => "QueuedMessage",
            Self::BrainThinking { .. } => "BrainThinking",
            Self::BrainResponse { .. } => "BrainResponse",
            Self::ToolCall { .. } => "ToolCall",
            Self::ToolResult { .. } => "ToolResult",
            Self::ToolError { .. } => "ToolError",
            Self::ApprovalRequested { .. } => "ApprovalRequested",
            Self::ApprovalDecided { .. } => "ApprovalDecided",
            Self::MemoryRead { .. } => "MemoryRead",
            Self::MemoryWrite { .. } => "MemoryWrite",
            Self::HandProvisioned { .. } => "HandProvisioned",
            Self::HandDestroyed { .. } => "HandDestroyed",
            Self::HandError { .. } => "HandError",
            Self::Checkpoint { .. } => "Checkpoint",
            Self::Error { .. } => "Error",
            Self::Warning { .. } => "Warning",
        }
    }

    /// Returns input tokens attributed to the event.
    pub fn input_tokens(&self) -> usize {
        match self {
            Self::BrainResponse { input_tokens, .. } => *input_tokens,
            _ => 0,
        }
    }

    /// Returns output tokens attributed to the event.
    pub fn output_tokens(&self) -> usize {
        match self {
            Self::BrainResponse { output_tokens, .. } => *output_tokens,
            _ => 0,
        }
    }

    /// Returns cost in cents attributed to the event.
    pub fn cost_cents(&self) -> u32 {
        match self {
            Self::BrainResponse { cost_cents, .. } => *cost_cents,
            _ => 0,
        }
    }

    /// Returns token count attributed to the event body.
    pub fn token_count(&self) -> usize {
        match self {
            Self::BrainThinking { token_count, .. } | Self::Checkpoint { token_count, .. } => {
                *token_count
            }
            Self::BrainResponse {
                input_tokens,
                output_tokens,
                ..
            } => input_tokens + output_tokens,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn event_serialization_roundtrip() {
        let event = Event::UserMessage {
            text: "Hello".to_string(),
            attachments: vec![],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, parsed);
        assert!(json.contains("UserMessage"));
    }

    #[test]
    fn brain_response_event_has_cost_fields() {
        let event = Event::BrainResponse {
            text: "Hi there".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cost_cents: 2,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("cost_cents"));
        assert!(json.contains("input_tokens"));
    }

    #[test]
    fn all_event_types_serialize() {
        let events = vec![
            Event::SessionCreated {
                workspace_id: "ws1".into(),
                user_id: "u1".into(),
                model: "test".into(),
            },
            Event::UserMessage {
                text: "hi".into(),
                attachments: vec![],
            },
            Event::ToolCall {
                tool_id: Uuid::new_v4(),
                provider_tool_use_id: Some("toolu_123".into()),
                tool_name: "bash".into(),
                input: json!({}),
                hand_id: None,
            },
            Event::ApprovalRequested {
                request_id: Uuid::new_v4(),
                tool_name: "bash".into(),
                input_summary: "ls".into(),
                risk_level: RiskLevel::Low,
                prompt: None,
            },
            Event::Checkpoint {
                summary: "test".into(),
                events_summarized: 10,
                token_count: 500,
            },
            Event::Error {
                message: "oops".into(),
                recoverable: true,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event);
            assert!(json.is_ok(), "Failed to serialize: {:?}", event);
        }
    }

    #[test]
    fn approval_requested_event_round_trips_full_prompt() {
        let request_id = Uuid::new_v4();
        let event = Event::ApprovalRequested {
            request_id,
            tool_name: "file_write".to_string(),
            input_summary: "notes/today.md".to_string(),
            risk_level: RiskLevel::Medium,
            prompt: Some(ApprovalPrompt {
                request: crate::types::ApprovalRequest {
                    request_id,
                    tool_name: "file_write".to_string(),
                    input_summary: "notes/today.md".to_string(),
                    risk_level: RiskLevel::Medium,
                },
                pattern: "notes/today.md".to_string(),
                parameters: vec![crate::types::ApprovalField {
                    label: "Path".to_string(),
                    value: "notes/today.md".to_string(),
                }],
                file_diffs: vec![crate::types::ApprovalFileDiff {
                    path: "notes/today.md".to_string(),
                    before: String::new(),
                    after: "hello\n".to_string(),
                    language_hint: Some("md".to_string()),
                }],
            }),
        };

        let json = serde_json::to_string(&event).expect("serialize approval request");
        let decoded: Event = serde_json::from_str(&json).expect("deserialize approval request");
        assert_eq!(decoded, event);
    }

    #[test]
    fn tool_result_event_deserializes_without_provider_tool_use_id() {
        let tool_id = Uuid::new_v4();
        let json = serde_json::json!({
            "type": "ToolResult",
            "data": {
                "tool_id": tool_id,
                "output": {
                    "content": [
                        {
                            "type": "text",
                            "text": "ok"
                        }
                    ],
                    "is_error": false,
                    "structured": null,
                    "duration": {
                        "secs": 0,
                        "nanos": 0
                    }
                },
                "success": true,
                "duration_ms": 5
            }
        });

        let decoded: Event = serde_json::from_value(json).expect("deserialize legacy tool result");
        match decoded {
            Event::ToolResult {
                tool_id: decoded_id,
                provider_tool_use_id,
                output,
                success,
                duration_ms,
            } => {
                assert_eq!(decoded_id, tool_id);
                assert_eq!(provider_tool_use_id, None);
                assert_eq!(output.to_text(), "ok");
                assert!(success);
                assert_eq!(duration_ms, 5);
            }
            other => panic!("expected tool result event, got {other:?}"),
        }
    }

    #[test]
    fn approval_requested_event_deserializes_without_prompt() {
        let request_id = Uuid::new_v4();
        let json = serde_json::json!({
            "type": "ApprovalRequested",
            "data": {
                "request_id": request_id,
                "tool_name": "bash",
                "input_summary": "pwd",
                "risk_level": "high"
            }
        });

        let decoded: Event =
            serde_json::from_value(json).expect("deserialize legacy approval request");
        match decoded {
            Event::ApprovalRequested {
                request_id: decoded_id,
                tool_name,
                input_summary,
                risk_level,
                prompt,
            } => {
                assert_eq!(decoded_id, request_id);
                assert_eq!(tool_name, "bash");
                assert_eq!(input_summary, "pwd");
                assert_eq!(risk_level, RiskLevel::High);
                assert!(prompt.is_none());
            }
            other => panic!("expected approval request event, got {other:?}"),
        }
    }
}
