//! Experiment service helper coverage.

use moa_core::WorkspaceId;
use moa_core::wire::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentListRequest, ExperimentListResponse, ExperimentRunRequest,
    ExperimentRunResponse, ExperimentRunStatusRequest, ExperimentRunStatusResponse,
    ExperimentScoresRequest, ExperimentScoresResponse,
};
use moa_orchestrator::services::score_queries::{COMPARE_NUMERIC_RUNS_SQL, SCORES_BY_RUN_SQL};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

#[test]
fn experiment_wire_dtos_use_experiment_names_and_include_workspace_id() {
    // Pins: the public experiment surface is separate from EvalRunRequest and remains workspace-scoped.
    assert_experiment_type::<ExperimentRunRequest>();
    assert_experiment_type::<ExperimentRunResponse>();
    assert_experiment_type::<ExperimentRunStatusRequest>();
    assert_experiment_type::<ExperimentRunStatusResponse>();
    assert_experiment_type::<ExperimentListRequest>();
    assert_experiment_type::<ExperimentListResponse>();
    assert_experiment_type::<ExperimentCancelRequest>();
    assert_experiment_type::<ExperimentCancelResponse>();
    assert_experiment_type::<ExperimentScoresRequest>();
    assert_experiment_type::<ExperimentScoresResponse>();
    assert_experiment_type::<ExperimentCompareRequest>();
    assert_experiment_type::<ExperimentCompareResponse>();

    assert_has_workspace_id(ExperimentRunRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        name: "live behavior smoke".to_string(),
        target: json!({"kind": "agent_loop", "prompt": "summarize"}),
        variant: json!({"name": "candidate"}),
        scorecard: json!({"score_names": ["task.completed"]}),
        score_run_id: None,
        idempotency_key: Some("run-key".to_string()),
    });
    assert_has_workspace_id(ExperimentRunResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
        status: "accepted".to_string(),
        score_run_id: fixture_uuid(2),
        session_id: None,
        workflow_run_uid: None,
    });
    assert_has_workspace_id(ExperimentRunStatusRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
    });
    assert_has_workspace_id(ExperimentRunStatusResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
        status: "accepted".to_string(),
        target_kind: Some("agent_loop".to_string()),
        score_run_id: Some(fixture_uuid(2)),
        session_id: None,
        workflow_run_uid: None,
        error: None,
        run: json!({}),
    });
    assert_has_workspace_id(ExperimentListRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        status: Some("accepted".to_string()),
        limit: Some(20),
    });
    assert_has_workspace_id(ExperimentListResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        runs: vec![json!({"run_uid": fixture_uuid(1)})],
    });
    assert_has_workspace_id(ExperimentCancelRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
        reason: Some("operator request".to_string()),
    });
    assert_has_workspace_id(ExperimentCancelResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
        cancelled: true,
        status: "cancelled".to_string(),
        reason: "operator request".to_string(),
    });
    assert_has_workspace_id(ExperimentScoresRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
    });
    assert_has_workspace_id(ExperimentScoresResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
        score_run_id: fixture_uuid(2),
        rows: vec![json!({"name": "task.completed"})],
    });
    assert_has_workspace_id(ExperimentCompareRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
    });
    assert_has_workspace_id(ExperimentCompareResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
        base_score_run_id: fixture_uuid(3),
        new_score_run_id: fixture_uuid(4),
        rows: vec![json!({"name": "task.completed"})],
    });
}

#[test]
fn experiment_score_dtos_use_run_uids_without_legacy_score_run_fields() {
    // Pins: public experiment score APIs address experiment runs, not internal score-run IDs.
    let scores = serde_json::to_value(ExperimentScoresRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        run_uid: fixture_uuid(1),
    })
    .expect("scores request should serialize");
    assert!(scores.get("run_uid").is_some());
    assert!(scores.get("run_id").is_none());

    let compare = serde_json::to_value(ExperimentCompareRequest {
        workspace_id: WorkspaceId::new("workspace-a"),
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
    })
    .expect("compare request should serialize");
    assert!(compare.get("base_run_uid").is_some());
    assert!(compare.get("new_run_uid").is_some());
    assert!(compare.get("base_run").is_none());
    assert!(compare.get("new_run").is_none());

    let compare_response = serde_json::to_value(ExperimentCompareResponse {
        workspace_id: WorkspaceId::new("workspace-a"),
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
        base_score_run_id: fixture_uuid(3),
        new_score_run_id: fixture_uuid(4),
        rows: Vec::new(),
    })
    .expect("compare response should serialize");
    assert!(compare_response.get("base_score_run_id").is_some());
    assert!(compare_response.get("new_score_run_id").is_some());
    assert!(compare_response.get("base_run").is_none());
    assert!(compare_response.get("new_run").is_none());
}

#[test]
fn experiments_service_declares_required_workspace_relations() {
    // Pins: experiment service handlers keep the planned Workspace relation requirements.
    let source = include_str!("../src/services/experiments.rs");
    assert_eq!(
        source.matches("Relation::Editor").count(),
        2,
        "run and cancel should require workspace editor"
    );
    assert_eq!(
        source.matches("Relation::Member").count(),
        4,
        "status, list, scores, and compare should require workspace member"
    );
}

#[test]
fn experiments_run_dispatches_agent_loop_and_workflow_targets_to_experiment_run_workflow() {
    // Pins: Task 7 starts both live experiment target kinds through the ExperimentRun workflow.
    let source = include_str!("../src/services/experiments.rs");

    assert!(
        source.contains("ExperimentRunClient"),
        "Experiments/run should dispatch the ExperimentRun workflow for executable targets"
    );
    assert!(
        !source.contains("ExperimentTargetKind::AgentLoop"),
        "workflow experiment targets should be dispatched instead of remaining accepted forever"
    );
    assert!(
        source.contains("workflow_request = ExperimentRunWorkflowRequest"),
        "Experiments/run should build a required ExperimentRun workflow request for stored runs"
    );
}

#[test]
fn experiment_run_workflow_drives_session_without_eval_or_direct_turn_execution() {
    // Pins: agent-loop experiments enter the normal Session path and do not bypass approvals.
    let source = include_str!("../src/workflows/experiment_run.rs");

    assert!(
        source.contains("object_client::<SessionClient>"),
        "ExperimentRun should call the Session VO"
    );
    assert!(
        source.contains(".queue_message("),
        "ExperimentRun should queue the experiment prompt through Session"
    );
    assert!(
        !source.contains("TurnExecutionClient"),
        "ExperimentRun must not invoke TurnExecution directly"
    );
    assert!(
        !source.contains("run_streamed_turn"),
        "ExperimentRun must not call moa-eval streamed turn execution"
    );
    assert!(
        !source.contains("ApprovalDecision::Allow"),
        "ExperimentRun must not auto-approve tools"
    );
}

#[test]
fn shared_score_queries_scope_every_run_id_by_workspace() {
    // Pins: experiments reuse score SQL that constrains each requested run by the authorized workspace.
    assert!(
        SCORES_BY_RUN_SQL.contains("WHERE run_id = $1 AND workspace_id = $2"),
        "scores query must scope the run id by workspace"
    );
    assert!(
        COMPARE_NUMERIC_RUNS_SQL.contains("WHERE run_id = $1 AND workspace_id = $3"),
        "compare base run must be scoped by workspace"
    );
    assert!(
        COMPARE_NUMERIC_RUNS_SQL.contains("WHERE run_id = $2 AND workspace_id = $3"),
        "compare new run must be scoped by workspace"
    );
    assert_eq!(
        COMPARE_NUMERIC_RUNS_SQL
            .matches("workspace_id = $3")
            .count(),
        2,
        "compare SQL must constrain both run IDs by the same authorized workspace"
    );
}

#[test]
fn experiment_score_handlers_resolve_run_uids_through_scoped_experiment_runs() {
    // Pins: score APIs reject cross-workspace experiment IDs by resolving run_uid through a scoped experiment load.
    let source = normalized_source(include_str!("../src/services/experiments.rs"));

    assert!(
        source.contains(&normalized_source(
            "let scope = workspace_scope(request.workspace_id.clone());
             let run = load_required_run(&ExperimentStore::new(pool.clone()), &scope, request.run_uid).await?;"
        )),
        "scores handler must load the experiment run in the requested workspace before reading scores"
    );
    assert!(
        source.contains(&normalized_source("run_id: run.score_run_id")),
        "scores handler must query analytics scores by resolved score_run_id"
    );
    assert!(
        !source.contains(&normalized_source("run_id: request.run_uid")),
        "scores handler must not treat experiment run_uid as a score run id"
    );

    assert!(
        source.contains(&normalized_source(
            "let scope = workspace_scope(request.workspace_id.clone());
             let store = ExperimentStore::new(pool.clone());
             let base_run = load_required_run(&store, &scope, request.base_run_uid).await?;
             let new_run = load_required_run(&store, &scope, request.new_run_uid).await?;"
        )),
        "compare handler must load both experiment runs in the requested workspace"
    );
    assert!(
        source.contains(&normalized_source("base_run: base_run.score_run_id")),
        "compare handler must pass the resolved baseline score_run_id to the shared score helper"
    );
    assert!(
        source.contains(&normalized_source("new_run: new_run.score_run_id")),
        "compare handler must pass the resolved new score_run_id to the shared score helper"
    );
    assert!(
        !source.contains(&normalized_source("base_run: request.base_run_uid")),
        "compare handler must not treat baseline experiment run_uid as a score run id"
    );
    assert!(
        !source.contains(&normalized_source("new_run: request.new_run_uid")),
        "compare handler must not treat new experiment run_uid as a score run id"
    );
}

#[test]
fn experiment_compare_rows_are_ordered_by_score_name() {
    // Pins: experiment compare inherits stable score-name ordering from the shared score helper.
    assert!(
        normalized_source(COMPARE_NUMERIC_RUNS_SQL).ends_with("ORDER BY name"),
        "compare rows should be returned in score-name order"
    );
}

fn assert_experiment_type<T: 'static>() {
    let type_name = std::any::type_name::<T>();
    let short_name = type_name
        .rsplit("::")
        .next()
        .expect("type name should include a final segment");
    assert!(
        short_name.starts_with("Experiment"),
        "{short_name} should use the Experiment prefix"
    );
    assert!(
        !short_name.starts_with("Eval"),
        "{short_name} should not use the Eval prefix"
    );
}

fn assert_has_workspace_id<T: Serialize>(value: T) {
    let encoded = serde_json::to_value(value).expect("wire DTO should serialize");
    assert!(
        encoded.get("workspace_id").is_some(),
        "wire DTO should include workspace_id: {encoded}"
    );
}

fn fixture_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn normalized_source(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}
