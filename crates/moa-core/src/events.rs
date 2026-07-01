//! Session event definitions and helpers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{
    ActionEnvelope, ActionReviewDecision, ActionReviewPreview, AgentSignalId, Attachment,
    CacheReport, Channel, ChildSignalKind, ContactId, GuardrailDirection, GuardrailMode,
    InputAudience, ModelId, ModelTier, NarrationSegment, NarrationSource, SegmentId,
    SessionActorRef, SessionChannelBindingId, SessionStatus, SignalSeverity, TenantId, ToolCallId,
    ToolOutput, WorkerId, WorkerState, WorkerTerminalResult,
};

/// Append-only session event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, strum::EnumDiscriminants)]
#[serde(tag = "type", content = "data")]
#[strum_discriminants(name(EventType))]
#[strum_discriminants(derive(
    std::hash::Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::IntoStaticStr,
    strum::EnumString
))]
#[strum_discriminants(serde(rename_all = "snake_case"))]
#[strum_discriminants(doc = "Event type discriminator used for filtering and indexing.")]
#[strum_discriminants(
    doc = "The strum IntoStaticStr/EnumString derives intentionally use the verbatim"
)]
#[strum_discriminants(
    doc = "PascalCase variant names, which are the persisted database representation."
)]
pub enum Event {
    /// Session was created.
    SessionCreated {
        /// Tenant runtime boundary that owns the session.
        tenant_id: TenantId,
        /// Contact attached to the session, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contact_id: Option<ContactId>,
        /// Actor that created the session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_by: Option<SessionActorRef>,
        /// Model identifier.
        model: ModelId,
        /// Initial delivery channel.
        #[serde(default)]
        channel: Channel,
    },
    /// Session status changed.
    SessionStatusChanged {
        /// Previous status.
        from: SessionStatus,
        /// New status.
        to: SessionStatus,
    },
    /// Session communication route changed.
    SessionChannelChanged {
        /// Previous delivery channel.
        from: Channel,
        /// New delivery channel.
        to: Channel,
        /// Contact associated with the session route, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contact_id: Option<ContactId>,
        /// Previous active route binding, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_binding_id: Option<SessionChannelBindingId>,
        /// New active route binding, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_binding_id: Option<SessionChannelBindingId>,
        /// Actor that requested or applied the change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed_by: Option<SessionActorRef>,
        /// Optional reason supplied by caller or workflow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Session completed successfully.
    SessionCompleted {
        /// Human-readable summary.
        summary: String,
        /// Number of turns completed.
        total_turns: u32,
    },
    /// A new task segment started within the session.
    SegmentStarted {
        /// Segment identifier.
        segment_id: SegmentId,
        /// Zero-based segment index within the session.
        segment_index: u32,
        /// Best-effort task summary for the segment.
        task_summary: Option<String>,
        /// Previous segment identifier, when present.
        previous_segment_id: Option<SegmentId>,
    },
    /// The current task segment completed.
    SegmentCompleted {
        /// Segment identifier.
        segment_id: SegmentId,
        /// Zero-based segment index within the session.
        segment_index: u32,
        /// Best-effort task summary for the segment.
        task_summary: Option<String>,
        /// Number of turns attributed to the segment.
        turn_count: u32,
        /// Tool names used during the segment.
        tools_used: Vec<String>,
        /// Skill names activated during the segment.
        skills_activated: Vec<String>,
        /// Token cost attributed to the segment.
        token_cost: u64,
        /// Segment duration in milliseconds.
        duration_ms: u64,
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
        /// Attached files or media.
        attachments: Vec<Attachment>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        /// Model identifier.
        model: ModelId,
        /// Routing tier that produced this response.
        model_tier: ModelTier,
        /// Input tokens billed at the provider's standard uncached rate.
        input_tokens_uncached: usize,
        /// Input tokens billed to create or refresh a cache entry.
        input_tokens_cache_write: usize,
        /// Input tokens served from cache.
        input_tokens_cache_read: usize,
        /// Output token count.
        output_tokens: usize,
        /// Cost in cents.
        cost_cents: u32,
        /// Duration in milliseconds.
        duration_ms: u64,
    },
    /// Durable user-visible progress update for a running turn.
    ProgressUpdate {
        /// Stable turn identifier and workflow key.
        turn_id: String,
        /// Current durable turn phase.
        phase: String,
        /// Short safe progress summary.
        summary: String,
        /// Elapsed turn runtime in milliseconds.
        elapsed_ms: u64,
    },
    /// A guardrail judge evaluated user or assistant text.
    GuardrailCheck {
        /// Direction of text that was evaluated.
        direction: GuardrailDirection,
        /// Guardrail enforcement mode used for the check.
        mode: GuardrailMode,
        /// Whether the judge accepted the text.
        passed: bool,
        /// Whether this check was eligible to block the turn.
        enforced: bool,
        /// Short safe reason from the judge; must not include guarded text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Judge model used for the check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
        /// Pinned policy hash that selected this guardrail check.
        policy_hash: String,
        /// Input tokens billed at the provider's standard uncached rate.
        #[serde(default)]
        input_tokens_uncached: usize,
        /// Input tokens billed to create or refresh a cache entry.
        #[serde(default)]
        input_tokens_cache_write: usize,
        /// Input tokens served from cache.
        #[serde(default)]
        input_tokens_cache_read: usize,
        /// Output token count.
        #[serde(default)]
        output_tokens: usize,
        /// Cost in cents.
        #[serde(default)]
        cost_cents: u32,
        /// Duration in milliseconds.
        #[serde(default)]
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
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Full tool output.
        output: ToolOutput,
        /// Approximate token count before router-level truncation, when truncation occurred.
        #[serde(skip_serializing_if = "Option::is_none")]
        original_output_tokens: Option<u32>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Tool name.
        tool_name: String,
        /// Error message.
        error: String,
        /// Whether the failure is retryable.
        retryable: bool,
    },
    /// A tool call was queued for tenant-admin action review.
    ActionReviewRequested {
        /// Tenant-admin review identifier.
        review_id: Uuid,
        /// Durable policy-facing action envelope.
        envelope: ActionEnvelope,
        /// Human-readable review preview.
        preview: ActionReviewPreview,
    },
    /// A tenant-admin action review was decided.
    ActionReviewDecided {
        /// Tenant-admin review identifier.
        review_id: Uuid,
        /// Review decision.
        decision: ActionReviewDecision,
        /// User who decided the review.
        decided_by: String,
        /// Decision timestamp.
        decided_at: DateTime<Utc>,
    },
    /// A child worker was spawned by the root session coordinator.
    WorkerSpawned {
        /// Child worker identifier.
        worker_id: WorkerId,
        /// Stable model-visible child path.
        path: String,
        /// Delegated task text.
        task: String,
        /// Reserved token budget for the child.
        budget_tokens: u64,
    },
    /// A parent sent a follow-up or steering message to a child worker.
    WorkerMessageSent {
        /// Child worker identifier.
        worker_id: WorkerId,
        /// Input request answered by this message, when it is a `provide_worker_input`
        /// reply rather than a general follow-up.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_request_id: Option<String>,
        /// Message text sent to the child.
        text: String,
    },
    /// A child worker lifecycle state changed.
    WorkerStatusChanged {
        /// Child worker identifier.
        worker_id: WorkerId,
        /// Previous known state, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<WorkerState>,
        /// New state.
        to: WorkerState,
        /// Optional status summary.
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// A child worker terminal notification was delivered to the parent session log.
    WorkerNotificationDelivered {
        /// Child worker identifier.
        worker_id: WorkerId,
        /// Terminal state delivered.
        state: WorkerState,
        /// Short result or error summary.
        summary: String,
    },
    /// A root coordinator bundled completed auto-delegated worker results for synthesis.
    WorkerResultBundle {
        /// User-message sequence that triggered the auto-delegated workers.
        user_sequence_num: u64,
        /// Terminal worker results in the original scheduled order.
        results: Vec<WorkerTerminalResult>,
    },
    /// A coordinator synthesis turn was requested for a completed worker result bundle.
    WorkerResultSynthesisRequested {
        /// User-message sequence whose worker bundle should be synthesized.
        user_sequence_num: u64,
        /// Coordinator turn id dispatched for synthesis.
        turn_id: String,
        /// System-visible instruction for the synthesis turn.
        reason: String,
    },
    /// Per-turn coordination / replay / latency telemetry, appended at turn end when metrics
    /// persistence is enabled (`MOA_PERSIST_TURN_METRICS`). Purely informational: it is not shown
    /// to the model, does not require processing, and is skipped by history compilation and
    /// compaction (all handled by their catch-all match arms). It exists so per-turn tool-call /
    /// round-trip / replay cost is reconstructable post-hoc from the durable event log — the
    /// substrate for the conversation-cost analyzer and the deterministic coordination tests.
    TurnMetrics {
        /// Turn id this summary describes.
        turn_id: String,
        /// Actor whose turn this was ("coordinator" or "worker").
        actor: String,
        /// Blocking Session-VO round-trips during the turn.
        #[serde(default)]
        session_vo_calls: u64,
        /// Blocking Worker-VO round-trips during the turn.
        #[serde(default)]
        worker_vo_calls: u64,
        /// Fire-and-forget VO dispatches during the turn.
        #[serde(default)]
        vo_sends: u64,
        /// Durable event appends during the turn.
        #[serde(default)]
        durable_appends: u64,
        /// `get_events` replay reads during the turn.
        #[serde(default)]
        get_events_calls: u64,
        /// Bytes deserialized across replay reads.
        #[serde(default)]
        events_bytes: u64,
        /// LLM-call wall-clock for the turn (ms).
        #[serde(default)]
        llm_ms: u64,
        /// Tool-dispatch wall-clock for the turn (ms).
        #[serde(default)]
        tool_ms: u64,
        /// Event-persist wall-clock for the turn (ms).
        #[serde(default)]
        persist_ms: u64,
    },
    /// A control-plane attention signal from a child was recorded on the coordinator.
    WorkerSignalReceived {
        /// Stable identifier for the recorded signal.
        signal_id: AgentSignalId,
        /// Child worker that raised the signal.
        worker_id: WorkerId,
        /// Kind of attention requested.
        kind: ChildSignalKind,
        /// Relative urgency of the signal.
        severity: SignalSeverity,
        /// Short, safe summary of the signal.
        summary: String,
        /// Awakeable id the child is blocked on; `Some` only for `NeedsInput`.
        ///
        /// Persisted on the event (not only the compact VO projection) so that any
        /// later coordinator turn rendered from the history window — including a
        /// plain `UserMessage` turn, not just a guarded `ChildSignal` resume — can
        /// answer the request via `provide_worker_input`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_request_id: Option<String>,
        /// Who should answer the request; `Some` only for `NeedsInput`.
        ///
        /// `User` means the question must be surfaced to the human; `Coordinator`
        /// means the coordinator may answer it autonomously.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_audience: Option<InputAudience>,
    },
    /// A child signal triggered a guarded coordinator auto-resume turn.
    WorkerParentResumeRequested {
        /// Signal that triggered the resume.
        signal_id: AgentSignalId,
        /// Child worker associated with the resume.
        worker_id: WorkerId,
        /// Coordinator turn id dispatched for the resume.
        turn_id: String,
        /// Short reason the resume was requested.
        reason: String,
    },
    /// A child's heartbeat was detected stale by the watchdog.
    WorkerHeartbeatStale {
        /// Child worker whose heartbeat went stale.
        worker_id: WorkerId,
        /// Last heartbeat timestamp observed before the staleness was detected.
        last_heartbeat_at: DateTime<Utc>,
        /// Stale threshold, in milliseconds, that was exceeded.
        threshold_ms: u64,
    },
    /// One durable, rate-limited natural-language progress narration for the session.
    ///
    /// Emitted by the per-session narrator: one merged update per period covering
    /// all active workers (and the active coordinator step). Carries
    /// `model`/`tokens_used` for cost observability.
    ProgressNarrated {
        /// Source attributed to the merged narration (`Coordinator` for the merge).
        source: NarrationSource,
        /// Merged human-readable update streamed to the user.
        text: String,
        /// Optional per-source breakdown produced by the same single call.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        segments: Vec<NarrationSegment>,
        /// Model used for the narration call (`"none"` for the 0-call short-circuit).
        model: String,
        /// Tokens consumed by the narration call (`0` for the short-circuit).
        tokens_used: u32,
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
        model: ModelId,
        /// Routing tier that produced this checkpoint.
        model_tier: ModelTier,
        /// Input token count used to generate the summary.
        input_tokens: usize,
        /// Output token count used to generate the summary.
        output_tokens: usize,
        /// Cost in cents attributed to the summary generation.
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
        EventType::from(self)
    }

    /// Returns a stable type name for storage.
    pub fn type_name(&self) -> &'static str {
        self.event_type().as_str()
    }

    /// Returns input tokens attributed to the event.
    pub fn input_tokens(&self) -> usize {
        match self {
            Self::BrainResponse {
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                ..
            }
            | Self::GuardrailCheck {
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
            | Self::GuardrailCheck {
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
            }
            | Self::GuardrailCheck {
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
            }
            | Self::GuardrailCheck {
                input_tokens_cache_read,
                ..
            } => *input_tokens_cache_read,
            _ => 0,
        }
    }

    /// Returns output tokens attributed to the event.
    pub fn output_tokens(&self) -> usize {
        match self {
            Self::BrainResponse { output_tokens, .. }
            | Self::GuardrailCheck { output_tokens, .. }
            | Self::Checkpoint { output_tokens, .. } => *output_tokens,
            _ => 0,
        }
    }

    /// Returns cost in cents attributed to the event.
    pub fn cost_cents(&self) -> u32 {
        match self {
            Self::BrainResponse { cost_cents, .. }
            | Self::GuardrailCheck { cost_cents, .. }
            | Self::Checkpoint { cost_cents, .. } => *cost_cents,
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

impl EventType {
    /// Returns the stable database representation.
    ///
    /// This is the verbatim PascalCase variant name (the persisted form), which
    /// is intentionally distinct from the snake_case serde/JSON representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn sample_action_envelope(
        review_id: Uuid,
        tool_name: &str,
        input_summary: &str,
        risk_level: crate::types::RiskLevel,
    ) -> ActionEnvelope {
        ActionEnvelope {
            review_id,
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            requested_by: SessionActorRef::Identity {
                id: Uuid::from_u128(2),
            },
            session_id: Some(crate::types::SessionId::new()),
            worker_id: None,
            tool_call_id: ToolCallId::from(review_id),
            tool_name: tool_name.to_string(),
            normalized_input: input_summary.to_string(),
            input_summary: input_summary.to_string(),
            risk_level,
            action_class: crate::types::ActionClass::LocalWrite,
            origin_kind: None,
            origin_id: None,
            origin_step_id: None,
            idempotency_key: None,
            created_at: Utc::now(),
        }
    }

    fn sample_action_review_preview(input_summary: &str) -> ActionReviewPreview {
        ActionReviewPreview {
            fields: vec![crate::types::ActionReviewField {
                label: "Path".to_string(),
                value: input_summary.to_string(),
            }],
            file_diffs: vec![crate::types::ActionReviewFileDiff {
                path: input_summary.to_string(),
                before: String::new(),
                after: "hello\n".to_string(),
                language_hint: Some("md".to_string()),
            }],
        }
    }

    #[test]
    fn action_review_requested_event_round_trips_full_payload() {
        // Pins: tenant-admin action-review events preserve policy envelope and preview
        // details and keep the persisted PascalCase discriminator stable.
        let review_id = Uuid::now_v7();
        let event = Event::ActionReviewRequested {
            review_id,
            envelope: sample_action_envelope(
                review_id,
                "file_write",
                "notes/today.md",
                crate::types::RiskLevel::Medium,
            ),
            preview: sample_action_review_preview("notes/today.md"),
        };

        let json = serde_json::to_string(&event).expect("serialize action review request");
        assert!(
            json.contains("\"type\":\"ActionReviewRequested\""),
            "expected stable PascalCase discriminator in {json}"
        );
        let decoded: Event =
            serde_json::from_str(&json).expect("deserialize action review request");
        assert_eq!(decoded, event);
    }

    #[test]
    fn guardrail_check_decodes_legacy_json_missing_default_token_fields() {
        // Pins: a frozen legacy GuardrailCheck payload written before the cost/token
        // accounting fields existed still decodes, with the `#[serde(default)]` token
        // fields (events.rs) and absent optional reason/model filling in as zero/None.
        // A rename or removal of these defaults would break replay of historic logs.
        let legacy_json = r#"{
            "type": "GuardrailCheck",
            "data": {
                "direction": "input",
                "mode": "enforce",
                "passed": false,
                "enforced": true,
                "policy_hash": "policy-sha256:legacy"
            }
        }"#;

        let decoded: Event =
            serde_json::from_str(legacy_json).expect("legacy guardrail JSON should decode");

        match decoded {
            Event::GuardrailCheck {
                direction,
                mode,
                passed,
                enforced,
                reason,
                model,
                policy_hash,
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                cost_cents,
                duration_ms,
            } => {
                assert_eq!(direction, GuardrailDirection::Input);
                assert_eq!(mode, GuardrailMode::Enforce);
                assert!(!passed);
                assert!(enforced);
                assert_eq!(reason, None);
                assert_eq!(model, None);
                assert_eq!(policy_hash, "policy-sha256:legacy");
                assert_eq!(input_tokens_uncached, 0);
                assert_eq!(input_tokens_cache_write, 0);
                assert_eq!(input_tokens_cache_read, 0);
                assert_eq!(output_tokens, 0);
                assert_eq!(cost_cents, 0);
                assert_eq!(duration_ms, 0);
            }
            other => panic!("expected GuardrailCheck, got {other:?}"),
        }
    }

    #[test]
    fn session_created_decodes_legacy_json_missing_default_channel_and_optionals() {
        // Pins: a frozen legacy SessionCreated payload lacking the `#[serde(default)]`
        // channel and the optional contact_id/created_by fields still decodes, with the
        // channel falling back to its Default and the optionals to None.
        let legacy_json = r#"{
            "type": "SessionCreated",
            "data": {
                "tenant_id": "00000000-0000-0000-0000-000000000001",
                "model": "anthropic:claude-sonnet-4-6"
            }
        }"#;

        let decoded: Event =
            serde_json::from_str(legacy_json).expect("legacy session-created JSON should decode");

        match decoded {
            Event::SessionCreated {
                tenant_id,
                contact_id,
                created_by,
                model,
                channel,
            } => {
                assert_eq!(tenant_id, TenantId::from(Uuid::from_u128(1)));
                assert_eq!(contact_id, None);
                assert!(created_by.is_none());
                assert_eq!(model, ModelId::new("anthropic:claude-sonnet-4-6"));
                assert_eq!(channel, Channel::default());
            }
            other => panic!("expected SessionCreated, got {other:?}"),
        }
    }

    #[test]
    fn progress_update_event_round_trips_minimal_payload() {
        // Pins: durable progress updates stay a small event-log payload.
        let event = Event::ProgressUpdate {
            turn_id: "turn-123".to_string(),
            phase: "Tooling".to_string(),
            summary: "Running tool: bash".to_string(),
            elapsed_ms: 12_500,
        };

        assert_eq!(event.event_type(), EventType::ProgressUpdate);
        assert_eq!(event.type_name(), "ProgressUpdate");
        assert_eq!(event.token_count(), 0);

        let json = serde_json::to_string(&event).expect("serialize progress update");
        assert!(json.contains("\"type\":\"ProgressUpdate\""));
        assert!(json.contains("\"turn_id\":\"turn-123\""));
        assert!(json.contains("\"phase\":\"Tooling\""));
        assert!(json.contains("\"summary\":\"Running tool: bash\""));
        assert!(json.contains("\"elapsed_ms\":12500"));

        let decoded: Event = serde_json::from_str(&json).expect("deserialize progress update");
        assert_eq!(decoded, event);
    }

    #[test]
    fn worker_lifecycle_events_use_stable_type_names() {
        // Pins: worker lifecycle events have stable event-log discriminators.
        let events = [
            (
                Event::WorkerSpawned {
                    worker_id: "child-1".to_string(),
                    path: "/root/research".to_string(),
                    task: "research".to_string(),
                    budget_tokens: 512,
                },
                EventType::WorkerSpawned,
                "WorkerSpawned",
            ),
            (
                Event::WorkerMessageSent {
                    worker_id: "child-1".to_string(),
                    input_request_id: None,
                    text: "continue".to_string(),
                },
                EventType::WorkerMessageSent,
                "WorkerMessageSent",
            ),
            (
                Event::WorkerStatusChanged {
                    worker_id: "child-1".to_string(),
                    from: Some(WorkerState::Running),
                    to: WorkerState::Completed,
                    summary: Some("done".to_string()),
                },
                EventType::WorkerStatusChanged,
                "WorkerStatusChanged",
            ),
            (
                Event::WorkerNotificationDelivered {
                    worker_id: "child-1".to_string(),
                    state: WorkerState::Completed,
                    summary: "done".to_string(),
                },
                EventType::WorkerNotificationDelivered,
                "WorkerNotificationDelivered",
            ),
            (
                Event::WorkerResultBundle {
                    user_sequence_num: 42,
                    results: vec![crate::WorkerTerminalResult {
                        state: WorkerState::Completed,
                        result: crate::WorkerResult {
                            worker_id: "child-1".to_string(),
                            success: true,
                            output: "done".to_string(),
                            tokens_used: 17,
                            tools_invoked: 2,
                            error: None,
                        },
                    }],
                },
                EventType::WorkerResultBundle,
                "WorkerResultBundle",
            ),
            (
                Event::WorkerResultSynthesisRequested {
                    user_sequence_num: 42,
                    turn_id: "turn-1".to_string(),
                    reason: "bundle complete".to_string(),
                },
                EventType::WorkerResultSynthesisRequested,
                "WorkerResultSynthesisRequested",
            ),
            (
                Event::WorkerSignalReceived {
                    signal_id: AgentSignalId::new(),
                    worker_id: "child-1".to_string(),
                    kind: ChildSignalKind::Blocked,
                    severity: SignalSeverity::Warning,
                    summary: "blocked on input".to_string(),
                    input_request_id: None,
                    input_audience: None,
                },
                EventType::WorkerSignalReceived,
                "WorkerSignalReceived",
            ),
            (
                Event::WorkerParentResumeRequested {
                    signal_id: AgentSignalId::new(),
                    worker_id: "child-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    reason: "child blocked".to_string(),
                },
                EventType::WorkerParentResumeRequested,
                "WorkerParentResumeRequested",
            ),
            (
                Event::WorkerHeartbeatStale {
                    worker_id: "child-1".to_string(),
                    last_heartbeat_at: Utc::now(),
                    threshold_ms: 30_000,
                },
                EventType::WorkerHeartbeatStale,
                "WorkerHeartbeatStale",
            ),
            (
                Event::ProgressNarrated {
                    source: NarrationSource::Coordinator,
                    text: "Searching the pricing docs".to_string(),
                    segments: Vec::new(),
                    model: "none".to_string(),
                    tokens_used: 0,
                },
                EventType::ProgressNarrated,
                "ProgressNarrated",
            ),
        ];

        for (event, expected_type, expected_name) in events {
            assert_eq!(event.event_type(), expected_type);
            assert_eq!(event.type_name(), expected_name);
        }
    }

    #[test]
    fn worker_signal_received_round_trips_needs_input_routing() {
        // Pins: NeedsInput signals persist the awakeable id and audience on the event
        // so a later coordinator turn can answer via `provide_worker_input`, and a
        // payload that omits those optional input fields decodes to `None` for both.
        let event = Event::WorkerSignalReceived {
            signal_id: AgentSignalId::new(),
            worker_id: "child-7".to_string(),
            kind: ChildSignalKind::NeedsInput,
            severity: SignalSeverity::Warning,
            summary: "needs the staging API key".to_string(),
            input_request_id: Some("req-42".to_string()),
            input_audience: Some(InputAudience::User),
        };

        let encoded = serde_json::to_string(&event).expect("serialize signal event");
        assert_eq!(
            serde_json::from_str::<Event>(&encoded).expect("deserialize signal event"),
            event
        );

        let without_input_fields = serde_json::json!({
            "type": "WorkerSignalReceived",
            "data": {
                "signal_id": Uuid::now_v7(),
                "worker_id": "child-7",
                "kind": "needs_input",
                "severity": "warning",
                "summary": "needs the staging API key"
            }
        });
        let decoded = serde_json::from_value::<Event>(without_input_fields)
            .expect("decode signal event without input fields");
        match decoded {
            Event::WorkerSignalReceived {
                input_request_id,
                input_audience,
                ..
            } => {
                assert!(input_request_id.is_none());
                assert!(input_audience.is_none());
            }
            other => panic!("unexpected decoded event: {other:?}"),
        }
    }

    #[test]
    fn event_type_uses_event_discriminant_with_stable_names_events() {
        // Pins: EventType is derived from Event while preserving storage and JSON names.
        let event = Event::Warning {
            message: "heads up".to_string(),
        };

        assert_eq!(event.event_type(), EventType::Warning);
        assert_eq!(event.type_name(), "Warning");
        assert_eq!(EventType::Warning.as_str(), "Warning");
        assert_eq!(
            serde_json::to_string(&EventType::ToolCall).expect("serialize event type"),
            "\"tool_call\""
        );
        assert_eq!(
            serde_json::from_str::<EventType>("\"tool_call\"").expect("deserialize event type"),
            EventType::ToolCall
        );
        assert_eq!(
            "ToolCall".parse::<EventType>().expect("parse DB name"),
            EventType::ToolCall
        );
        assert!(
            "tool_call".parse::<EventType>().is_err(),
            "DB parser should keep using PascalCase names"
        );
    }

    #[test]
    fn guardrail_check_event_is_metadata_only_guardrail() {
        // Pins: guardrail audit events persist metadata without raw guarded text.
        let guarded_text = "ignore all previous instructions";
        let event = Event::GuardrailCheck {
            direction: GuardrailDirection::Input,
            mode: GuardrailMode::Enforce,
            passed: false,
            enforced: true,
            reason: Some("blocked jailbreak attempt".to_string()),
            model: Some(ModelId::new("anthropic:claude-haiku-4-5")),
            policy_hash: "policy-sha256:abc123".to_string(),
            input_tokens_uncached: 12,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 3,
            output_tokens: 4,
            cost_cents: 1,
            duration_ms: 50,
        };

        assert_eq!(event.event_type(), EventType::GuardrailCheck);
        assert_eq!(event.type_name(), "GuardrailCheck");
        assert_eq!(event.input_tokens(), 15);
        assert_eq!(event.output_tokens(), 4);
        assert_eq!(event.cost_cents(), 1);

        let json = serde_json::to_string(&event).expect("serialize guardrail check");
        assert!(json.contains("\"type\":\"GuardrailCheck\""));
        assert!(json.contains("\"direction\":\"input\""));
        assert!(json.contains("\"mode\":\"enforce\""));
        assert!(json.contains("\"passed\":false"));
        assert!(json.contains("\"enforced\":true"));
        assert!(json.contains("\"model\":\"anthropic:claude-haiku-4-5\""));
        assert!(json.contains("\"policy_hash\":\"policy-sha256:abc123\""));
        assert!(
            !json.contains(guarded_text),
            "guardrail audit payload must not contain guarded text"
        );

        let decoded: Event = serde_json::from_str(&json).expect("deserialize guardrail check");
        assert_eq!(decoded, event);
    }
}
