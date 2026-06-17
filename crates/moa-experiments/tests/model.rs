use chrono::Utc;
use moa_core::{Attachment, MemoryScope, ModelId, SessionId, WorkspaceId};
use moa_experiments::model::{
    ExperimentRunKind, ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard,
    ExperimentTarget, ExperimentTargetKind, ExperimentVariant,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn agent_loop_target_round_trips_through_public_model_offline() {
    // Pins: agent-loop experiments preserve prompts, model choice, attachments, and run kind.
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
        ExperimentRunKind::RegressionEval,
        ExperimentTargetKind::AgentLoop,
        target,
        Some(session_id),
        None,
    );

    let encoded = serde_json::to_string(&record).expect("agent loop record serializes");
    let decoded: ExperimentRunRecord =
        serde_json::from_str(&encoded).expect("agent loop record deserializes");

    assert_eq!(decoded.run_kind, ExperimentRunKind::RegressionEval);
    assert_eq!(decoded.target_kind, ExperimentTargetKind::AgentLoop);
    assert_eq!(decoded.target.kind(), ExperimentTargetKind::AgentLoop);
    assert_eq!(decoded.session_id, Some(session_id));
    assert_eq!(decoded.workflow_run_uid, None);
    assert_eq!(decoded, record);
}

#[test]
fn workflow_target_round_trips_through_public_model_offline() {
    // Pins: workflow experiments preserve workflow refs, inputs, idempotency, and live run kind.
    let workflow_run_uid = Uuid::now_v7();
    let target = ExperimentTarget::Workflow {
        workflow_ref: "workflow://damaged-food-order".to_string(),
        input: json!({ "order_id": "order-123", "priority": "high" }),
        session_id: None,
        idempotency_key: Some("experiment-live-workflow-123".to_string()),
    };
    let record = record_for_target(
        ExperimentRunKind::LiveBehaviorExperiment,
        ExperimentTargetKind::Workflow,
        target,
        None,
        Some(workflow_run_uid),
    );

    let encoded = serde_json::to_value(&record).expect("workflow record serializes");
    let decoded: ExperimentRunRecord =
        serde_json::from_value(encoded).expect("workflow record deserializes");

    assert_eq!(decoded.run_kind, ExperimentRunKind::LiveBehaviorExperiment);
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
}

fn record_for_target(
    run_kind: ExperimentRunKind,
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
        run_kind,
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
