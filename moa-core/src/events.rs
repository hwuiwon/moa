//! Session event definitions and helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{
    ApprovalDecision, ApprovalPrompt, Attachment, CacheReport, EventType, ModelId, ModelTier,
    RiskLevel, SessionStatus, ToolCallId, ToolOutput, UserId, WorkspaceId,
};

/// Append-only session event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    /// Session was created.
    SessionCreated {
        /// Workspace identifier.
        workspace_id: WorkspaceId,
        /// User identifier.
        user_id: UserId,
        /// Model identifier.
        model: ModelId,
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
        /// Provider-specific thought signature that should be replayed on the next turn when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        /// Model identifier.
        model: ModelId,
        /// Routing tier that produced this response.
        #[serde(default = "default_main_model_tier")]
        model_tier: ModelTier,
        /// Input tokens billed at the provider's standard uncached rate.
        #[serde(default, alias = "input_tokens")]
        input_tokens_uncached: usize,
        /// Input tokens billed to create or refresh a cache entry.
        #[serde(default)]
        input_tokens_cache_write: usize,
        /// Input tokens served from cache.
        #[serde(default)]
        input_tokens_cache_read: usize,
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
        tool_id: ToolCallId,
        /// Provider-specific tool-use identifier, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Provider-specific thought signature that must be replayed with this tool call when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_thought_signature: Option<String>,
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
        tool_id: ToolCallId,
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
        tool_id: ToolCallId,
        /// Provider-specific tool-use identifier, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Tool name.
        #[serde(default)]
        tool_name: String,
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
        prompt: ApprovalPrompt,
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
    /// Memory ingest operation.
    MemoryIngest {
        /// Human-readable source name.
        source_name: String,
        /// Created source page path.
        source_path: String,
        /// Pages created or updated during ingest.
        affected_pages: Vec<String>,
        /// Contradictions detected in the source text.
        contradictions: Vec<String>,
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
        /// Model identifier used to generate the summary.
        #[serde(default)]
        model: ModelId,
        /// Routing tier that produced this checkpoint.
        #[serde(default = "default_auxiliary_model_tier")]
        model_tier: ModelTier,
        /// Input token count used to generate the summary.
        #[serde(default)]
        input_tokens: usize,
        /// Output token count used to generate the summary.
        #[serde(default)]
        output_tokens: usize,
        /// Cost in cents attributed to the summary generation.
        #[serde(default)]
        cost_cents: u32,
    },
    /// Durable cache-planning and cache-usage report for one provider request.
    CacheReport {
        /// Structured cache audit payload.
        report: CacheReport,
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
            Self::MemoryIngest { .. } => EventType::MemoryIngest,
            Self::HandProvisioned { .. } => EventType::HandProvisioned,
            Self::HandDestroyed { .. } => EventType::HandDestroyed,
            Self::HandError { .. } => EventType::HandError,
            Self::Checkpoint { .. } => EventType::Checkpoint,
            Self::CacheReport { .. } => EventType::CacheReport,
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
            Self::MemoryIngest { .. } => "MemoryIngest",
            Self::HandProvisioned { .. } => "HandProvisioned",
            Self::HandDestroyed { .. } => "HandDestroyed",
            Self::HandError { .. } => "HandError",
            Self::Checkpoint { .. } => "Checkpoint",
            Self::CacheReport { .. } => "CacheReport",
            Self::Error { .. } => "Error",
            Self::Warning { .. } => "Warning",
        }
    }

    /// Returns input tokens attributed to the event.
    pub fn input_tokens(&self) -> usize {
        match self {
            Self::BrainResponse {
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                ..
            } => input_tokens_uncached + input_tokens_cache_write + input_tokens_cache_read,
            Self::Checkpoint { input_tokens, .. } => *input_tokens,
            _ => 0,
        }
    }

    /// Returns uncached input tokens attributed to the event.
    pub fn input_tokens_uncached(&self) -> usize {
        match self {
            Self::BrainResponse {
                input_tokens_uncached,
                ..
            }
            | Self::Checkpoint {
                input_tokens: input_tokens_uncached,
                ..
            } => *input_tokens_uncached,
            _ => 0,
        }
    }

    /// Returns cache-write input tokens attributed to the event.
    pub fn input_tokens_cache_write(&self) -> usize {
        match self {
            Self::BrainResponse {
                input_tokens_cache_write,
                ..
            } => *input_tokens_cache_write,
            _ => 0,
        }
    }

    /// Returns cache-read input tokens attributed to the event.
    pub fn input_tokens_cache_read(&self) -> usize {
        match self {
            Self::BrainResponse {
                input_tokens_cache_read,
                ..
            } => *input_tokens_cache_read,
            _ => 0,
        }
    }

    /// Returns output tokens attributed to the event.
    pub fn output_tokens(&self) -> usize {
        match self {
            Self::BrainResponse { output_tokens, .. } | Self::Checkpoint { output_tokens, .. } => {
                *output_tokens
            }
            _ => 0,
        }
    }

    /// Returns cost in cents attributed to the event.
    pub fn cost_cents(&self) -> u32 {
        match self {
            Self::BrainResponse { cost_cents, .. } | Self::Checkpoint { cost_cents, .. } => {
                *cost_cents
            }
            _ => 0,
        }
    }

    /// Returns token count attributed to the event body.
    pub fn token_count(&self) -> usize {
        match self {
            Self::BrainThinking { token_count, .. } | Self::Checkpoint { token_count, .. } => {
                *token_count
            }
            Self::CacheReport { report } => report.total_tokens_estimate,
            Self::BrainResponse { output_tokens, .. } => self.input_tokens() + output_tokens,
            _ => 0,
        }
    }
}

fn default_main_model_tier() -> ModelTier {
    ModelTier::Main
}

fn default_auxiliary_model_tier() -> ModelTier {
    ModelTier::Auxiliary
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn sample_approval_prompt(
        request_id: Uuid,
        tool_name: &str,
        input_summary: &str,
        risk_level: RiskLevel,
    ) -> ApprovalPrompt {
        ApprovalPrompt {
            request: crate::types::ApprovalRequest {
                request_id,
                tool_name: tool_name.to_string(),
                input_summary: input_summary.to_string(),
                risk_level: risk_level.clone(),
            },
            pattern: input_summary.to_string(),
            parameters: vec![crate::types::ApprovalField {
                label: "Path".to_string(),
                value: input_summary.to_string(),
            }],
            file_diffs: vec![crate::types::ApprovalFileDiff {
                path: input_summary.to_string(),
                before: String::new(),
                after: "hello\n".to_string(),
                language_hint: Some("md".to_string()),
            }],
        }
    }

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
    fn cache_report_roundtrip() {
        let event = Event::CacheReport {
            report: CacheReport {
                provider: "anthropic".to_string(),
                model: ModelId::new("claude-sonnet-4-6"),
                message_count: 3,
                tool_count: 2,
                cache_breakpoints: vec![2],
                tool_tokens_estimate: 100,
                stable_message_tokens_estimate: 200,
                stable_total_tokens_estimate: 300,
                total_tokens_estimate: 360,
                dynamic_tokens_estimate: 60,
                cache_ratio_estimate: 0.833,
                stable_prefix_fingerprint: 123,
                full_request_fingerprint: 456,
                stable_prefix_reused: true,
                input_tokens: 40,
                cached_input_tokens: 25,
                output_tokens: 8,
                cached_vs_stable_estimate_ratio: 0.083,
            },
        };

        let json = serde_json::to_string(&event).expect("cache report serializes");
        let parsed: Event = serde_json::from_str(&json).expect("cache report deserializes");
        assert_eq!(event, parsed);
        assert!(json.contains("CacheReport"));
    }

    #[test]
    fn brain_response_event_has_cost_fields() {
        let event = Event::BrainResponse {
            text: "Hi there".to_string(),
            thought_signature: None,
            model: ModelId::new("claude-sonnet-4-6"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: 100,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 50,
            cost_cents: 2,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("cost_cents"));
        assert!(json.contains("input_tokens_uncached"));
    }

    #[test]
    fn brain_response_deserializes_pre_step_79_payload() {
        let payload = serde_json::json!({
            "type": "BrainResponse",
            "data": {
                "text": "done",
                "model": "claude-sonnet-4-6",
                "input_tokens": 42,
                "output_tokens": 9,
                "cost_cents": 3,
                "duration_ms": 120
            }
        });

        let parsed: Event =
            serde_json::from_value(payload).expect("legacy BrainResponse should deserialize");

        match parsed {
            Event::BrainResponse {
                model_tier,
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                ..
            } => {
                assert_eq!(model_tier, ModelTier::Main);
                assert_eq!(input_tokens_uncached, 42);
                assert_eq!(input_tokens_cache_write, 0);
                assert_eq!(input_tokens_cache_read, 0);
                assert_eq!(output_tokens, 9);
            }
            other => panic!("expected BrainResponse, got {other:?}"),
        }
    }

    #[test]
    fn all_event_types_serialize() {
        let events = vec![
            Event::SessionCreated {
                workspace_id: WorkspaceId::new("ws1"),
                user_id: UserId::new("u1"),
                model: ModelId::new("test"),
            },
            Event::UserMessage {
                text: "hi".into(),
                attachments: vec![],
            },
            Event::ToolCall {
                tool_id: ToolCallId::new(),
                provider_tool_use_id: Some("toolu_123".into()),
                provider_thought_signature: None,
                tool_name: "bash".into(),
                input: json!({}),
                hand_id: None,
            },
            Event::ApprovalRequested {
                request_id: Uuid::nil(),
                tool_name: "bash".into(),
                input_summary: "ls".into(),
                risk_level: RiskLevel::Low,
                prompt: sample_approval_prompt(Uuid::nil(), "bash", "ls", RiskLevel::Low),
            },
            Event::Checkpoint {
                summary: "test".into(),
                events_summarized: 10,
                token_count: 500,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: ModelTier::Auxiliary,
                input_tokens: 120,
                output_tokens: 45,
                cost_cents: 1,
            },
            Event::MemoryIngest {
                source_name: "RFC 0042".into(),
                source_path: "sources/rfc-0042.md".into(),
                affected_pages: vec!["sources/rfc-0042.md".into()],
                contradictions: vec![],
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
        let request_id = Uuid::now_v7();
        let event = Event::ApprovalRequested {
            request_id,
            tool_name: "file_write".to_string(),
            input_summary: "notes/today.md".to_string(),
            risk_level: RiskLevel::Medium,
            prompt: sample_approval_prompt(
                request_id,
                "file_write",
                "notes/today.md",
                RiskLevel::Medium,
            ),
        };

        let json = serde_json::to_string(&event).expect("serialize approval request");
        let decoded: Event = serde_json::from_str(&json).expect("deserialize approval request");
        assert_eq!(decoded, event);
    }

    #[test]
    fn tool_result_event_deserializes_without_provider_tool_use_id() {
        let tool_id = ToolCallId::new();
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
}
