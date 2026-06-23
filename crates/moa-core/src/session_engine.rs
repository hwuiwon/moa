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
            | Event::GuardrailCheck { .. }
            | Event::MemoryWrite { .. }
            | Event::HandDestroyed { .. }
            | Event::HandError { .. }
            | Event::Checkpoint { .. } => None,
            Event::UserMessage { .. }
            | Event::QueuedMessage { .. }
            | Event::ToolResult { .. }
            | Event::ToolError { .. }
            | Event::ToolCall { .. } => Some(true),
            // Action reviews are workspace-admin state and do not resume the turn loop by themselves.
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
        Event, EventRecord, EventType, GuardrailDirection, GuardrailMode, ModelId, SessionId,
        SessionMeta,
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
            },
        )];

        assert!(!session_requires_processing(&session, &events));
    }
}
