//! Shared session-lifecycle rules used by multiple orchestrator adapters.

use crate::{Event, EventRecord, SessionMeta, SessionStatus};

/// Returns whether the persisted session log indicates more work is required.
pub fn session_requires_processing(session: &SessionMeta, events: &[EventRecord]) -> bool {
    if matches!(session.status, SessionStatus::Cancelled) {
        return false;
    }

    events
        .iter()
        .rev()
        .find_map(|record| match record.event {
            Event::SessionStatusChanged { .. }
            | Event::Warning { .. }
            | Event::ProgressUpdate { .. }
            | Event::GuardrailCheck { .. }
            | Event::MemoryWrite { .. }
            | Event::HandDestroyed { .. }
            | Event::HandError { .. }
            | Event::Checkpoint { .. }
            | Event::QueuedMessage { .. }
            | Event::WorkerStatusChanged { .. }
            | Event::WorkerNotificationDelivered { .. }
            | Event::WorkerSignalReceived { .. }
            | Event::WorkerResultBundle { .. } => None,
            Event::UserMessage { .. }
            | Event::ToolResult { .. }
            | Event::ToolError { .. }
            | Event::ToolCall { .. }
            // A guarded coordinator resume seeds its instruction via this control event
            // (not a fake user message), so a trailing resume request must drive the loop.
            | Event::WorkerParentResumeRequested { .. }
            // Completed deterministic auto-delegation similarly seeds a synthesis turn via
            // this control event, not another fake user message.
            | Event::WorkerResultSynthesisRequested { .. } => Some(true),
            // Action reviews are tenant-admin state and do not resume the turn loop by themselves.
            Event::ActionReviewRequested { .. } | Event::ActionReviewDecided { .. } => Some(false),
            _ => Some(false),
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::{
        AgentSignalId, ChildSignalKind, Event, EventRecord, EventType, GuardrailDirection,
        GuardrailMode, InputAudience, ModelId, SessionId, SessionMeta, SignalSeverity, WorkerState,
    };

    use super::session_requires_processing;

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        let event_type = event.event_type();
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[test]
    fn guardrail_check_does_not_mask_pending_user_message_guardrail() {
        // Pins: guardrail audit events are informational and do not change turn scheduling.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "hello".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                Event::GuardrailCheck {
                    direction: GuardrailDirection::Input,
                    mode: GuardrailMode::Shadow,
                    passed: true,
                    enforced: false,
                    reason: Some("accepted".to_string()),
                    model: Some(ModelId::new("anthropic:claude-haiku-4-5")),
                    policy_hash: "policy-sha256:abc123".to_string(),
                    input_tokens_uncached: 0,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 0,
                    cost_cents: 0,
                    duration_ms: 0,
                },
            ),
        ];

        assert!(session_requires_processing(&session, &events));
        assert_eq!(events[1].event_type, EventType::GuardrailCheck);
    }

    #[test]
    fn lone_guardrail_check_does_not_require_processing_guardrail() {
        // Pins: guardrail audit events alone do not resume the turn loop.
        let session = SessionMeta::default();
        let events = vec![record(
            1,
            Event::GuardrailCheck {
                direction: GuardrailDirection::Output,
                mode: GuardrailMode::Enforce,
                passed: false,
                enforced: true,
                reason: Some("blocked".to_string()),
                model: None,
                policy_hash: "policy-sha256:def456".to_string(),
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
            },
        )];

        assert!(!session_requires_processing(&session, &events));
    }

    #[test]
    fn progress_update_does_not_mask_pending_user_message_progress() {
        // Pins: durable progress updates are replay metadata and must not complete pending user work.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "please continue".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                Event::ProgressUpdate {
                    turn_id: "turn-123".to_string(),
                    phase: "Compiling".to_string(),
                    summary: "Working on it".to_string(),
                    elapsed_ms: 14,
                },
            ),
        ];

        assert!(session_requires_processing(&session, &events));
        assert_eq!(events[1].event_type, EventType::ProgressUpdate);
    }

    #[test]
    fn queued_message_waits_for_drained_user_message() {
        // Pins: queued-message events are replay breadcrumbs; the drained UserMessage drives work.
        let session = SessionMeta::default();
        let events = vec![record(
            1,
            Event::QueuedMessage {
                text: "later".to_string(),
                attachments: Vec::new(),
                queued_at: Utc::now(),
            },
        )];

        assert!(!session_requires_processing(&session, &events));
    }

    #[test]
    fn worker_result_synthesis_request_drives_processing() {
        // Pins: deterministic auto-delegation completion dispatches a system-triggered
        // synthesis turn, so the request event must compile a model call.
        let session = SessionMeta::default();
        let events = vec![record(
            1,
            Event::WorkerResultSynthesisRequested {
                user_sequence_num: 1,
                turn_id: "turn-1".to_string(),
                reason: "worker bundle ready".to_string(),
            },
        )];

        assert!(session_requires_processing(&session, &events));
    }

    #[test]
    fn worker_result_synthesis_request_survives_late_worker_lifecycle_events() {
        // Pins: a late terminal notification from one worker must not mask the already
        // dispatched coordinator synthesis turn for the completed auto-delegation bundle.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::WorkerResultSynthesisRequested {
                    user_sequence_num: 1,
                    turn_id: "turn-1".to_string(),
                    reason: "worker bundle ready".to_string(),
                },
            ),
            record(
                2,
                Event::WorkerStatusChanged {
                    worker_id: "worker-1".to_string(),
                    from: Some(WorkerState::Running),
                    to: WorkerState::Completed,
                    summary: Some("done".to_string()),
                },
            ),
            record(
                3,
                Event::WorkerNotificationDelivered {
                    worker_id: "worker-1".to_string(),
                    state: WorkerState::Completed,
                    summary: "done".to_string(),
                },
            ),
            record(
                4,
                Event::WorkerSignalReceived {
                    signal_id: AgentSignalId::new(),
                    worker_id: "worker-1".to_string(),
                    kind: ChildSignalKind::Finding,
                    severity: SignalSeverity::Info,
                    summary: "done".to_string(),
                    input_request_id: None,
                    input_audience: Some(InputAudience::Coordinator),
                },
            ),
        ];

        assert!(session_requires_processing(&session, &events));
    }
}
