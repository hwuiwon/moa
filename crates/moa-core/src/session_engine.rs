//! Shared session-lifecycle rules used by multiple orchestrator adapters.

use crate::{
    events::ProcessingEffect, types::events_stream::EventRecord, types::session::SessionMeta,
    types::session::SessionStatus,
};

/// Returns whether the persisted session log indicates more work is required.
///
/// The tail is scanned newest-first and each event is classified by
/// [`crate::events::Event::processing_effect`]. [`ProcessingEffect::Neutral`] events —
/// passive telemetry, liveness, enrichment, and lifecycle breadcrumbs, several
/// of which are appended asynchronously off the turn path — are skipped so they
/// cannot mask an older trigger. The first non-neutral event decides:
/// [`ProcessingEffect::Trigger`] means a model turn is pending;
/// [`ProcessingEffect::Terminal`] means the loop has concluded or is suspended.
/// A tail of only neutral events (or an empty tail) requires no processing.
pub fn session_requires_processing(session: &SessionMeta, events: &[EventRecord]) -> bool {
    if matches!(session.status, SessionStatus::Cancelled) {
        return false;
    }

    events
        .iter()
        .rev()
        .find_map(|record| match record.event.processing_effect() {
            ProcessingEffect::Neutral => None,
            ProcessingEffect::Trigger => Some(true),
            ProcessingEffect::Terminal => Some(false),
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use std::time::Duration;

    use crate::{
        events::Event,
        events::EventType,
        types::events_stream::EventRecord,
        types::execution_planning::{ExecutionRunAdmissionStatus, ExecutionRunStarted},
        types::guardrails::GuardrailDirection,
        types::guardrails::GuardrailMode,
        types::identifiers::ModelId,
        types::identifiers::SessionId,
        types::identifiers::ToolCallId,
        types::provider::ModelTier,
        types::session::SessionMeta,
        types::tools::ToolOutput,
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

    fn turn_failed_event(actor: crate::events::TurnFailureActor) -> Event {
        Event::TurnFailed {
            actor,
            turn_id: "turn-1".to_string(),
            class: crate::events::TurnFailureClass::ModelCall,
            summary: crate::events::TurnFailureClass::ModelCall
                .summary()
                .to_string(),
        }
    }

    fn tool_result_event() -> Event {
        Event::tool_result(
            ToolCallId::new(),
            None,
            crate::types::tools::SecuredToolOutput::assessed_safe(
                ToolOutput::text("done", Duration::from_millis(5)),
                crate::types::security::ToolCapabilityId::BuiltIn {
                    tool: "noop".to_string(),
                },
            ),
        )
    }

    fn brain_response_event() -> Event {
        Event::BrainResponse {
            text: "all set".to_string(),
            thought_signature: None,
            model: ModelId::new("anthropic:claude-sonnet-4-6"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: 1,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 1,
            cost_cents: 0,
            duration_ms: 1,
            llm_ttft_ms: None,
        }
    }

    fn stale_heartbeat_event() -> Event {
        Event::WorkerHeartbeatStale {
            worker_id: "worker-1".to_string(),
            last_heartbeat_at: Utc::now(),
            threshold_ms: 30_000,
        }
    }

    #[test]
    fn stale_heartbeat_does_not_mask_pending_tool_result() {
        // Pins: F04 — the async watchdog `WorkerHeartbeatStale` is a second asynchronously
        // appended vector and must not mask a pending `ToolResult`.
        let session = SessionMeta::default();
        let events = vec![
            record(1, tool_result_event()),
            record(2, stale_heartbeat_event()),
        ];

        assert!(session_requires_processing(&session, &events));
        assert_eq!(events[1].event_type, EventType::WorkerHeartbeatStale);
    }

    #[test]
    fn execution_run_started_does_not_mask_pending_user_work() {
        // Pins: the admission breadcrumb is neutral scheduling evidence and may
        // not consume or terminate the user message that precedes it.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "start an execution run".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                Event::ExecutionRunStarted(ExecutionRunStarted {
                    run_uid: Uuid::now_v7(),
                    originating_user_sequence_num: 1,
                    plan_revision: 1,
                    status: ExecutionRunAdmissionStatus::Queued,
                    confirmation: None,
                }),
            ),
        ];

        assert!(session_requires_processing(&session, &events));
    }

    #[test]
    fn execution_delivery_effects_drive_only_input_and_synthesis_processing_boundaries() {
        // Pins: terminal run status is a transparent projection, input parks the current turn,
        // and only the guarded synthesis request starts new model work.
        use crate::events::{
            ExecutionInputRequired, ExecutionRunEvidenceRef, ExecutionSynthesisRequested,
            ExecutionTaskResultsRef, ExecutionTerminalSummary,
        };

        let session = SessionMeta::default();
        let run_uid = Uuid::from_u128(61);
        let terminal = ExecutionTerminalSummary {
            run_uid,
            originating_user_sequence_num: 1,
            output: None,
            output_hash: [5; 32],
            citation_ids: Vec::new(),
            failures: Vec::new(),
            gaps: Vec::new(),
            task_results: ExecutionTaskResultsRef::ExecutionTaskTable { run_uid },
        };
        let user = record(
            1,
            Event::UserMessage {
                text: "run it".to_string(),
                attachments: Vec::new(),
            },
        );

        assert!(session_requires_processing(
            &session,
            &[
                user.clone(),
                record(2, Event::ExecutionCompleted(terminal.clone()))
            ]
        ));
        assert!(!session_requires_processing(
            &session,
            &[
                user,
                record(
                    2,
                    Event::ExecutionInputRequired(ExecutionInputRequired {
                        run_uid,
                        originating_user_sequence_num: 1,
                        task_id: Uuid::from_u128(62),
                        generation: 2,
                        question: "Which account?".to_string(),
                    }),
                ),
            ]
        ));
        assert!(session_requires_processing(
            &session,
            &[record(
                3,
                Event::ExecutionSynthesisRequested(ExecutionSynthesisRequested {
                    run_uid,
                    originating_user_sequence_num: 1,
                    turn_id: "execution-synthesis-61-1".to_string(),
                    terminal,
                    run_evidence: ExecutionRunEvidenceRef::ExecutionRun { run_uid },
                }),
            )]
        ));
    }

    #[test]
    fn terminal_brain_response_requires_no_processing() {
        // Pins: a completed assistant response ends the turn loop even when a passive
        // liveness event is appended after it.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "hello".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(2, brain_response_event()),
            record(3, stale_heartbeat_event()),
        ];

        assert!(!session_requires_processing(&session, &events));
    }

    #[test]
    fn neutral_only_tail_requires_no_processing() {
        // Pins: a tail of only passive liveness/telemetry events requires no turn, so a
        // late async append on an otherwise-idle session does not resurrect the loop.
        let session = SessionMeta::default();
        let events = vec![record(1, stale_heartbeat_event())];

        assert!(!session_requires_processing(&session, &events));
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
    fn a_worker_turn_failure_cannot_mask_pending_root_work() {
        // Pins the CONSEQUENCE of classifying a worker failure as scheduling-neutral,
        // not just the classification itself. A child's failure lands in the shared
        // session log, so if it were scheduling-terminal the reverse tail scan would
        // stop at it and conclude the root loop has nothing pending — stalling a
        // coordinator that still owes the user a reply.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "run it".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                turn_failed_event(crate::events::TurnFailureActor::Worker {
                    worker_id: "worker-7".to_string(),
                }),
            ),
        ];

        assert!(
            session_requires_processing(&session, &events),
            "the root's pending user message must still drive a turn after a child failed"
        );
    }

    #[test]
    fn a_coordinator_turn_failure_concludes_the_root_loop() {
        // Pins the opposite half: the coordinator's own failure is the end of its
        // turn loop. Classifying it neutral would let the scan fall through to the
        // user message it already failed on and re-trigger the same turn forever.
        let session = SessionMeta::default();
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "run it".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                turn_failed_event(crate::events::TurnFailureActor::Coordinator),
            ),
        ];

        assert!(
            !session_requires_processing(&session, &events),
            "a failed coordinator turn concludes the loop instead of re-triggering itself"
        );
    }
}
