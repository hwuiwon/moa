use chrono::Utc;
use moa_core::{Attachment, MemoryScope, ModelId, SessionId, WorkspaceId};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentSimulatorConfig,
    ExperimentTarget, ExperimentTargetKind, ExperimentTrialRecord, ExperimentTrialStatus,
    ExperimentTrialStopReason, ExperimentVariant,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn agent_loop_target_round_trips_through_public_model_offline() {
    // Pins: agent-loop experiments preserve prompts, model choice, and attachments.
    let session_id = SessionId::new();
    let target = ExperimentTarget::AgentLoop {
        prompt: "Check whether the answer cites the provided source.".to_string(),
        session_id: Some(session_id),
        model: ModelId::new("gpt-5.1"),
        attachments: vec![Attachment {
            name: "source.md".to_string(),
            mime_type: Some("text/markdown".to_string()),
            url: None,
            path: None,
            size_bytes: Some(42),
        }],
    };
    let record = record_for_target(
        ExperimentTargetKind::AgentLoop,
        target,
        Some(session_id),
        None,
    );

    let encoded = serde_json::to_string(&record).expect("agent loop record serializes");
    let decoded: ExperimentRunRecord =
        serde_json::from_str(&encoded).expect("agent loop record deserializes");

    assert_eq!(decoded.target_kind, ExperimentTargetKind::AgentLoop);
    assert_eq!(decoded.target.kind(), ExperimentTargetKind::AgentLoop);
    assert_eq!(decoded.session_id, Some(session_id));
    assert_eq!(decoded.workflow_run_uid, None);
    assert_eq!(decoded, record);
}

#[test]
fn workflow_target_round_trips_through_public_model_offline() {
    // Pins: workflow experiments preserve workflow refs, inputs, and idempotency.
    let workflow_run_uid = Uuid::now_v7();
    let target = ExperimentTarget::Workflow {
        workflow_ref: "workflow://damaged-food-order".to_string(),
        input: json!({ "order_id": "order-123", "priority": "high" }),
        session_id: None,
        idempotency_key: Some("experiment-live-workflow-123".to_string()),
    };
    let record = record_for_target(
        ExperimentTargetKind::Workflow,
        target,
        None,
        Some(workflow_run_uid),
    );

    let encoded = serde_json::to_value(&record).expect("workflow record serializes");
    let decoded: ExperimentRunRecord =
        serde_json::from_value(encoded).expect("workflow record deserializes");

    assert_eq!(decoded.target_kind, ExperimentTargetKind::Workflow);
    assert_eq!(decoded.target.kind(), ExperimentTargetKind::Workflow);
    assert_eq!(decoded.session_id, None);
    assert_eq!(decoded.workflow_run_uid, Some(workflow_run_uid));
    assert_eq!(
        decoded.idempotency_key.as_deref(),
        Some("experiment-live-workflow-123")
    );
    assert_eq!(decoded, record);
}

#[test]
fn scorecard_carries_expected_scores_without_evaluator_execution_offline() {
    // Pins: scorecards define expected score names and evaluator metadata only.
    let scorecard = ExperimentScorecard {
        score_names: vec!["grounding".to_string(), "task_success".to_string()],
        evaluator_metadata: json!({
            "judge": "offline-replay",
            "rubric_version": "2026-06-16"
        }),
    };

    let encoded = serde_json::to_string(&scorecard).expect("scorecard serializes");
    let decoded: ExperimentScorecard =
        serde_json::from_str(&encoded).expect("scorecard deserializes");

    assert_eq!(decoded.score_names, ["grounding", "task_success"]);
    assert_eq!(decoded, scorecard);
}

#[test]
fn storage_enum_conversions_reject_unknown_database_values_offline() {
    // Pins: storage conversion helpers accept only the durable database vocabulary.
    assert_eq!(ExperimentRunStatus::Accepted.as_str(), "accepted");
    assert_eq!(
        ExperimentRunStatus::from_db("waiting_approval"),
        Some(ExperimentRunStatus::WaitingApproval)
    );
    assert_eq!(ExperimentRunStatus::from_db("queued"), None);
    assert_eq!(ExperimentTargetKind::AgentLoop.as_str(), "agent_loop");
    assert_eq!(
        ExperimentTargetKind::from_db("workflow"),
        Some(ExperimentTargetKind::Workflow)
    );
    assert_eq!(ExperimentTargetKind::from_db("dataset"), None);
    assert_eq!(ExperimentTrialStatus::Dispatched.as_str(), "dispatched");
    assert_eq!(
        ExperimentTrialStatus::from_db("dispatched"),
        Some(ExperimentTrialStatus::Dispatched)
    );
    assert_eq!(ExperimentTrialStatus::Running.as_str(), "running");
    assert_eq!(
        ExperimentTrialStatus::from_db("waiting_approval"),
        Some(ExperimentTrialStatus::WaitingApproval)
    );
    assert_eq!(ExperimentTrialStatus::from_db("queued"), None);
    assert_eq!(ExperimentTrialStopReason::MaxTurns.as_str(), "max_turns");
    assert_eq!(
        ExperimentTrialStopReason::from_db("approval_wait"),
        Some(ExperimentTrialStopReason::ApprovalWait)
    );
    assert_eq!(ExperimentTrialStopReason::from_db("timeout"), None);
}

#[test]
fn trial_record_round_trips_through_public_model_offline() {
    // Pins: trial records preserve simulator config, artifact pins, links, and stop reason.
    let now = Utc::now();
    let session_id = SessionId::new();
    let workflow_run_uid = Uuid::now_v7();
    let trial = ExperimentTrialRecord {
        scope: MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-test"),
        },
        trial_uid: Uuid::now_v7(),
        run_uid: Uuid::now_v7(),
        trial_key: "scenario-a/persona-b/baseline".to_string(),
        status: ExperimentTrialStatus::Completed,
        target_kind: ExperimentTargetKind::AgentLoop,
        variant_key: "baseline".to_string(),
        plan_revision_uid: Uuid::now_v7(),
        persona_id: Some("careful-shopper".to_string()),
        profile_id: None,
        scenario_id: Some("checkout-delay".to_string()),
        data_bundle_ids: vec!["orders-fixture".to_string()],
        artifact_revision_uids: vec![Uuid::now_v7()],
        simulator: ExperimentSimulatorConfig {
            model: ModelId::new("gpt-5.1-mini"),
            temperature: Some(0.2),
            max_turns: 8,
            token_budget: Some(8_000),
            metadata: json!({ "style": "terse" }),
        },
        target_model: Some(ModelId::new("gpt-5.1")),
        seed: Some("seed-123".to_string()),
        session_id: Some(session_id),
        workflow_run_uid: Some(workflow_run_uid),
        score_run_id: Uuid::now_v7(),
        turn_count: 4,
        stop_reason: Some(ExperimentTrialStopReason::Success),
        error: None,
        trace_id: Some("trace-abc".to_string()),
        started_at: Some(now),
        completed_at: Some(now),
        created_at: now,
        updated_at: now,
    };

    let encoded = serde_json::to_value(&trial).expect("trial record serializes");
    let decoded: ExperimentTrialRecord =
        serde_json::from_value(encoded).expect("trial record deserializes");

    assert_eq!(decoded.trial_key, "scenario-a/persona-b/baseline");
    assert_eq!(decoded.simulator.model, ModelId::new("gpt-5.1-mini"));
    assert_eq!(decoded.session_id, Some(session_id));
    assert_eq!(decoded.workflow_run_uid, Some(workflow_run_uid));
    assert_eq!(
        decoded.stop_reason,
        Some(ExperimentTrialStopReason::Success)
    );
    assert_eq!(decoded, trial);
}

fn record_for_target(
    target_kind: ExperimentTargetKind,
    target: ExperimentTarget,
    session_id: Option<SessionId>,
    workflow_run_uid: Option<Uuid>,
) -> ExperimentRunRecord {
    let now = Utc::now();

    ExperimentRunRecord {
        scope: MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-test"),
        },
        run_uid: Uuid::now_v7(),
        name: "experiment run".to_string(),
        target_kind,
        status: ExperimentRunStatus::Accepted,
        target,
        variant: ExperimentVariant {
            name: "baseline".to_string(),
            model: Some(ModelId::new("gpt-5.1")),
            artifact_revision_uids: vec![Uuid::now_v7()],
            skill_refs: vec!["skill://citation-checker".to_string()],
            workflow_ref: Some("workflow://damaged-food-order".to_string()),
            metadata: json!({ "cohort": "offline" }),
        },
        scorecard: ExperimentScorecard {
            score_names: vec!["grounding".to_string()],
            evaluator_metadata: json!({ "judge": "offline-replay" }),
        },
        score_run_id: Uuid::now_v7(),
        session_id,
        workflow_run_uid,
        artifact_revision_uids: vec![Uuid::now_v7()],
        idempotency_key: match target_kind {
            ExperimentTargetKind::AgentLoop => None,
            ExperimentTargetKind::Workflow => Some("experiment-live-workflow-123".to_string()),
        },
        created_by_identity: json!({
            "type": "user",
            "id": "experimenter"
        }),
        error: None,
        created_at: now,
        started_at: None,
        completed_at: None,
        updated_at: now,
    }
}
