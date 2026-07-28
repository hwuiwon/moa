//! Unit tests for root turn-execution routing, guards, and canned messages.

use std::collections::{BTreeSet, HashMap};

use serde_json::json;

use super::*;

fn run_turn_request(trigger: TurnTrigger) -> RunTurnRequest {
    RunTurnRequest {
        session_id: "session-1".to_string(),
        turn_id: "turn-1".to_string(),
        identity: moa_core::traits::Identity {
            identity_type: moa_core::traits::IdentityType::Service,
            id: uuid::Uuid::from_u128(1),
            tenant_id: moa_core::types::identifiers::TenantId::from(uuid::Uuid::from_u128(2)),
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        contact: None,
        generation: 1,
        user_message: "Inspect every account".to_string(),
        attachments: Vec::new(),
        model: None,
        max_turns: None,
        trigger,
        child_signal_id: None,
        execution_template: None,
        action_review: None,
    }
}

fn action_review_continuation() -> moa_core::types::action_policy::ActionReviewContinuation {
    use moa_core::types::action_policy::{
        ActionReviewContinuation, ActionReviewOutcome, ActionReviewOwner, ActionReviewReceipt,
        ActionReviewTerminalEvent,
    };

    let review_id = uuid::Uuid::from_u128(0x13_0001);
    ActionReviewContinuation {
        review_id,
        receipt: ActionReviewReceipt {
            review_id,
            owner: ActionReviewOwner::Coordinator {
                session_id: moa_core::types::identifiers::SessionId::new(),
                turn_id: "turn-1".to_string(),
                generation: 1,
            },
            tool_name: "bash".to_string(),
            requested_tool_call_id: moa_core::types::identifiers::ToolCallId::new(),
            executed_tool_call_id: Some(moa_core::types::identifiers::ToolCallId::new()),
            outcome: ActionReviewOutcome::ClearedSuccess {
                summary: "reviewed".to_string(),
                assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
            },
            terminal_events: vec![
                ActionReviewTerminalEvent::Decided,
                ActionReviewTerminalEvent::ToolResult,
            ],
        },
    }
}

#[test]
fn user_message_origin_is_trigger_based_for_zero_based_events() {
    // Pins: event sequence zero is a valid first user message; system
    // continuations remain distinguishable by their explicit trigger.
    let mut request = run_turn_request(TurnTrigger::UserMessage);
    assert!(has_user_message_origin(&request));

    for trigger in [
        TurnTrigger::ChildSignal,
        TurnTrigger::WorkerResults,
        TurnTrigger::ExecutionSynthesis,
        TurnTrigger::ActionReview,
    ] {
        request.trigger = trigger;
        assert!(
            !has_user_message_origin(&request),
            "system continuation must not gain user-message origin: {trigger:?}"
        );
    }
}

#[test]
fn action_review_continuation_turn_skips_routing_and_never_reopens_planning() {
    // Pins: the exact continuation matrix at the routing seam. A resolved review
    // continues on a bounded Respond path only: it must not spend a classifier call,
    // must not gain user-message origin (which is what authorizes durable execution
    // and pinned templates), and must not be able to consume a Durable upgrade.
    let mut request = run_turn_request(TurnTrigger::ActionReview);
    request.action_review = Some(action_review_continuation());

    assert!(is_action_review_turn(&request));
    assert!(!is_execution_synthesis_turn(&request));
    assert!(!has_user_message_origin(&request));

    let inline_route = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Inline,
        rationale: "unused".to_string(),
    };
    assert!(
        !DurableUpgradeGuard::new(&request, &inline_route).allows_tool_signal(),
        "a review continuation must not be able to upgrade itself to durable execution"
    );
}

#[test]
fn trigger_and_continuation_context_must_agree_exactly() {
    // Pins: the typed pairing is enforced, not inferred. An ActionReview turn without a
    // receipt has nothing to render, and a receipt riding an ordinary user turn would
    // inject review state into work that never raised a review.
    let mut missing = run_turn_request(TurnTrigger::ActionReview);
    missing.action_review = None;
    assert_eq!(
        missing.action_review_continuation(),
        Err(moa_wire::turn::TurnTriggerContextError::MissingContinuation)
    );

    let mut unexpected = run_turn_request(TurnTrigger::UserMessage);
    unexpected.action_review = Some(action_review_continuation());
    assert_eq!(
        unexpected.action_review_continuation(),
        Err(moa_wire::turn::TurnTriggerContextError::UnexpectedContinuation)
    );

    let mut paired = run_turn_request(TurnTrigger::ActionReview);
    paired.action_review = Some(action_review_continuation());
    assert!(
        paired
            .action_review_continuation()
            .expect("a paired trigger and receipt is valid")
            .is_some()
    );
}

#[test]
fn turn_cap_reached_message_states_the_effective_cap() {
    // Pins: the cap-stop message reports the cap actually in force, so an escalated
    // delegation turn tells the user the delegation cap (12), not the base cap (6).
    let escalated = driver_model_loop::TurnCapEscalation::new(6, 12);
    let base_message = turn_cap_reached_message(escalated.effective_max_turns());
    assert!(
        base_message.contains("(6)"),
        "a non-delegated turn reports the base cap: {base_message}"
    );

    let mut escalated = escalated;
    escalated.record_delegation();
    let delegation_message = turn_cap_reached_message(escalated.effective_max_turns());
    assert!(
        delegation_message.contains("(12)"),
        "an escalated turn reports the delegation cap: {delegation_message}"
    );
}

#[test]
fn durable_upgrade_guard_is_root_inline_byte_exact_and_single_use() {
    // Pins: only an initial root Execute/Inline turn can consume one bounded Durable
    // upgrade for its byte-identical persisted objective; a second transition is rejected.
    let mut request = run_turn_request(TurnTrigger::UserMessage);
    let inline_route = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Inline,
        rationale: "The inspection can begin in a bounded interactive loop.".to_string(),
    };
    let signal = moa_core::types::execution_planning::DurableUpgradeSignal {
        objective: request.user_message.clone(),
        rationale: "The discovered account workflow must continue durably.".to_string(),
        evidence: vec![
            moa_core::types::execution_planning::ExecutionPlanningEvidence {
                source: "tool:inventory".to_string(),
                summary: "500 independent accounts".to_string(),
                value: json!({"count": 500}),
            },
        ],
    };
    let mut guard = DurableUpgradeGuard::new(&request, &inline_route);
    assert!(guard.allows_tool_signal());
    assert_eq!(
        guard
            .consume(&request.user_message, signal.clone())
            .expect("authorized first transition should produce a route")
            .routing
            .decision,
        ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Durable,
            rationale: signal.rationale.clone(),
        }
    );
    assert!(!guard.allows_tool_signal());
    assert_eq!(
        guard.consume(&request.user_message, signal.clone()),
        Err(DurableUpgradeTransitionError::AlreadyConsumed)
    );

    let mut changed_objective_guard = DurableUpgradeGuard::new(&request, &inline_route);
    let mut changed = signal.clone();
    changed.objective.push(' ');
    assert_eq!(
        changed_objective_guard.consume(&request.user_message, changed),
        Err(DurableUpgradeTransitionError::ObjectiveChanged)
    );

    for trigger in [
        TurnTrigger::WorkerResults,
        TurnTrigger::ChildSignal,
        TurnTrigger::ExecutionSynthesis,
        TurnTrigger::ActionReview,
    ] {
        request.trigger = trigger;
        assert!(
            !DurableUpgradeGuard::new(&request, &inline_route).allows_tool_signal(),
            "system trigger must not gain Durable-upgrade authority: {trigger:?}"
        );
    }

    request.trigger = TurnTrigger::UserMessage;
    let durable_route = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Durable,
        rationale: "The account workflow must run durably.".to_string(),
    };
    assert!(
        !DurableUpgradeGuard::new(&request, &durable_route).allows_tool_signal(),
        "an initially Durable route cannot transition back through Inline"
    );
}

#[test]
fn selected_skill_names_ignores_invalid_values_and_deduplicates() {
    // Pins: skill selection metadata from the context pipeline becomes stable segment evidence.
    let mut metadata = HashMap::new();
    metadata.insert(
        SELECTED_SKILL_NAMES_METADATA_KEY.to_string(),
        json!(["rust", "", "incident-triage", "rust", 42, null]),
    );

    assert_eq!(
        selected_skill_names(&metadata),
        vec!["incident-triage".to_string(), "rust".to_string()]
    );
}

#[test]
fn provider_tool_list_excludes_reserved_controls() {
    // Pins: operator lifecycle tools and the workflow-owned Durable control cannot enter
    // a provider request unless the root Inline workflow injects its authoritative schema.
    let mut request = moa_core::types::completion::CompletionRequest::new("work");
    request.tools = [
        "execution_runs_list",
        "execution_run_start",
        "execution_run_status",
        "execution_run_cancel",
        "execution_review_decide",
        "execution_signal",
        "request_durable_execution",
        "file_read",
    ]
    .into_iter()
    .map(|name| json!({"name": name, "input_schema": {"type": "object"}}))
    .collect();

    crate::turn::util::exclude_reserved_control_tool_schemas(&mut request);

    assert_eq!(
        crate::turn::util::allowed_tool_names(&request),
        BTreeSet::from(["file_read".to_string()])
    );
}

#[test]
fn planning_provider_failure_keeps_raw_detail_out_of_the_user_reply() {
    // Pins: a durable planner provider failure records the raw provider detail in a
    // recoverable Error event, replies with a fixed user-safe message that never leaks
    // the provider string, and fails the turn — so the raw "provider error: ..." string
    // can never become the assistant's answer.
    let detail = "Terminal error [500]: validation error: Responses requests require at \
                  least one non-system message";
    let failure = planning_provider_failure_outcome(detail);

    assert_eq!(failure.outcome, TurnOutcomeKind::Failed);
    assert_eq!(failure.user_message, PLANNING_PROVIDER_FAILURE_USER_MESSAGE);
    assert!(!failure.user_message.contains(detail));
    assert!(!failure.user_message.to_lowercase().contains("provider"));
    assert!(!failure.user_message.contains("validation error"));
    match failure.error_event {
        Event::Error {
            message,
            recoverable,
        } => {
            assert!(recoverable, "planner provider failure is user-retryable");
            assert!(
                message.contains(detail),
                "durable error must retain the raw provider detail for operators"
            );
        }
        other => panic!("expected Event::Error, got {other:?}"),
    }
}
