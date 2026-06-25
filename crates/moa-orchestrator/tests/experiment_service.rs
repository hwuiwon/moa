//! Experiment service helper coverage.

use chrono::{TimeZone, Utc};
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_artifacts::validation::validate_for_status;
use moa_core::wire::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentCompareRow, ExperimentGeneratePlanRequest,
    ExperimentGeneratePlanResponse, ExperimentListRequest, ExperimentListResponse,
    ExperimentProposeImprovementsRequest, ExperimentProposeImprovementsResponse,
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, ExperimentScenarioScoreDeltaRow, ExperimentScenarioScoreSummary,
    ExperimentScoreSummaryRow, ExperimentScoresRequest, ExperimentScoresResponse,
    ExperimentTrialScoreSummary, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse,
    ExperimentTrialSummary, ExperimentTrialsRequest, ExperimentTrialsResponse,
    ExperimentVariantScoreDeltaRow,
};
use moa_core::{ActionRuleScope, ModelId, SessionId, StoragePartitionId, TenantId};
use moa_experiments::app::{
    ExperimentLearningProposalEvidence, build_experiment_learning_candidate,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentSimulatorConfig,
    ExperimentTarget, ExperimentTrialRecord, ExperimentTrialStatus, ExperimentVariant,
};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

#[test]
fn experiment_wire_dtos_use_experiment_names_and_include_tenant_id() {
    // Pins: the public experiment surface is separate from EvalRunRequest and remains tenant-scoped.
    let tenant_id = tenant_id_fixture();
    assert_experiment_type::<ExperimentRunRequest>();
    assert_experiment_type::<ExperimentRunResponse>();
    assert_experiment_type::<ExperimentGeneratePlanRequest>();
    assert_experiment_type::<ExperimentGeneratePlanResponse>();
    assert_experiment_type::<ExperimentRunStatusRequest>();
    assert_experiment_type::<ExperimentRunStatusResponse>();
    assert_experiment_type::<ExperimentListRequest>();
    assert_experiment_type::<ExperimentListResponse>();
    assert_experiment_type::<ExperimentTrialsRequest>();
    assert_experiment_type::<ExperimentTrialsResponse>();
    assert_experiment_type::<ExperimentTrialStatusRequest>();
    assert_experiment_type::<ExperimentTrialStatusResponse>();
    assert_experiment_type::<ExperimentCancelRequest>();
    assert_experiment_type::<ExperimentCancelResponse>();
    assert_experiment_type::<ExperimentProposeImprovementsRequest>();
    assert_experiment_type::<ExperimentProposeImprovementsResponse>();
    assert_experiment_type::<ExperimentScoresRequest>();
    assert_experiment_type::<ExperimentScoresResponse>();
    assert_experiment_type::<ExperimentCompareRequest>();
    assert_experiment_type::<ExperimentCompareResponse>();

    assert_has_tenant_id(ExperimentRunRequest {
        tenant_id,
        name: "live behavior smoke".to_string(),
        plan_revision_uid: None,
        target: Some(json!({"kind": "agent_loop", "prompt": "summarize"})),
        variant: Some(json!({"name": "candidate"})),
        scorecard: json!({"score_names": ["task.completed"]}),
        score_run_id: None,
        idempotency_key: Some("run-key".to_string()),
        agent_revision_variants: Vec::new(),
    });
    assert_has_tenant_id(ExperimentGeneratePlanRequest {
        tenant_id,
        description: "Simulate damaged-food-order support behavior.".to_string(),
        model: Some("gpt-5.4".to_string()),
        artifact_refs: vec!["workflow://damaged-food-order".to_string()],
    });
    assert_has_tenant_id(ExperimentGeneratePlanResponse {
        tenant_id,
        artifact_uid: fixture_uuid(1),
        revision_uid: fixture_uuid(2),
        status: "draft".to_string(),
        source_format: "json".to_string(),
        source_text: minimal_valid_generated_plan(),
        document: json!({"kind": "experiment_plan"}),
        validation_report: json!({"errors": []}),
    });
    assert_has_tenant_id(ExperimentRunResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        status: "accepted".to_string(),
        score_run_id: fixture_uuid(2),
        session_id: None,
        workflow_run_uid: None,
    });
    assert_has_tenant_id(ExperimentRunStatusRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
    });
    assert_has_tenant_id(ExperimentRunStatusResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        status: "accepted".to_string(),
        target_kind: Some("agent_loop".to_string()),
        score_run_id: Some(fixture_uuid(2)),
        session_id: None,
        workflow_run_uid: None,
        error: None,
        run: json!({}),
    });
    assert_has_tenant_id(ExperimentListRequest {
        tenant_id,
        status: Some("accepted".to_string()),
        limit: Some(20),
    });
    assert_has_tenant_id(ExperimentListResponse {
        tenant_id,
        runs: vec![json!({"run_uid": fixture_uuid(1)})],
    });
    let trial_summary = ExperimentTrialSummary {
        tenant_id,
        run_uid: fixture_uuid(1),
        trial_uid: fixture_uuid(2),
        status: "completed".to_string(),
        target_kind: "agent_loop".to_string(),
        trial_key: "scenario-a/persona-a/profile-a/candidate/0".to_string(),
        variant_key: "candidate".to_string(),
        scenario_id: Some(fixture_uuid(3).to_string()),
        score_run_id: fixture_uuid(4),
        session_id: Some(SessionId(fixture_uuid(5))),
        workflow_run_uid: Some(fixture_uuid(6)),
        trace_id: Some("trace-fixture".to_string()),
        stop_reason: Some("success".to_string()),
        error: None,
        turn_count: 3,
    };
    assert_has_tenant_id(ExperimentTrialsRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
        status: Some("completed".to_string()),
        limit: Some(20),
    });
    assert_has_tenant_id(ExperimentTrialsResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        trials: vec![trial_summary.clone()],
    });
    assert_has_tenant_id(ExperimentTrialStatusRequest {
        tenant_id,
        trial_uid: fixture_uuid(2),
    });
    assert_has_tenant_id(ExperimentTrialStatusResponse {
        tenant_id: trial_summary.tenant_id,
        run_uid: trial_summary.run_uid,
        trial_uid: trial_summary.trial_uid,
        status: trial_summary.status,
        target_kind: trial_summary.target_kind,
        trial_key: trial_summary.trial_key,
        variant_key: trial_summary.variant_key,
        scenario_id: trial_summary.scenario_id,
        score_run_id: trial_summary.score_run_id,
        session_id: trial_summary.session_id,
        workflow_run_uid: trial_summary.workflow_run_uid,
        trace_id: trial_summary.trace_id,
        stop_reason: trial_summary.stop_reason,
        error: trial_summary.error,
        turn_count: trial_summary.turn_count,
    });
    assert_has_tenant_id(ExperimentCancelRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
        reason: Some("operator request".to_string()),
    });
    assert_has_tenant_id(ExperimentCancelResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        cancelled: true,
        status: "cancelled".to_string(),
        reason: "operator request".to_string(),
    });
    assert_has_tenant_id(ExperimentProposeImprovementsRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
        idempotency_key: Some("proposal-key".to_string()),
    });
    assert_has_tenant_id(ExperimentProposeImprovementsResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        candidate_ids: vec![fixture_uuid(2)],
        draft_artifact_revision_uids: Vec::new(),
    });
    assert_has_tenant_id(ExperimentScoresRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
    });
    assert_has_tenant_id(ExperimentScoresResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        score_run_id: fixture_uuid(2),
        rows: vec![score_row("task.completed", "boolean", 1, 1.0)],
        trial_rollup_rows: vec![score_row("task.completed", "boolean", 2, 0.5)],
        trials: vec![ExperimentTrialScoreSummary {
            trial_uid: fixture_uuid(5),
            trial_key: "scenario-a/baseline/0".to_string(),
            score_run_id: fixture_uuid(6),
            variant_key: "baseline".to_string(),
            scenario_id: Some(fixture_uuid(7).to_string()),
            rows: vec![score_row("task.completed", "boolean", 1, 1.0)],
        }],
        scenarios: vec![ExperimentScenarioScoreSummary {
            scenario_id: Some(fixture_uuid(7).to_string()),
            rows: vec![score_row("task.completed", "boolean", 2, 0.5)],
        }],
    });
    assert_has_tenant_id(ExperimentCompareRequest {
        tenant_id,
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
    });
    assert_has_tenant_id(ExperimentCompareResponse {
        tenant_id,
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
        base_score_run_id: fixture_uuid(3),
        new_score_run_id: fixture_uuid(4),
        rows: vec![ExperimentCompareRow {
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.8),
            delta: Some(0.1),
        }],
        scenario_deltas: vec![ExperimentScenarioScoreDeltaRow {
            scenario_id: Some(fixture_uuid(7).to_string()),
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.8),
            delta: Some(0.1),
        }],
        variant_deltas: vec![ExperimentVariantScoreDeltaRow {
            variant_key: "candidate".to_string(),
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.8),
            delta: Some(0.1),
        }],
    });
}

#[test]
fn experiment_score_dtos_use_run_uids_without_legacy_score_run_fields() {
    // Pins: public experiment score APIs address experiment runs, not internal score-run IDs.
    let tenant_id = tenant_id_fixture();
    let scores = serde_json::to_value(ExperimentScoresRequest {
        tenant_id,
        run_uid: fixture_uuid(1),
    })
    .expect("scores request should serialize");
    assert!(scores.get("run_uid").is_some());
    assert!(scores.get("run_id").is_none());

    let compare = serde_json::to_value(ExperimentCompareRequest {
        tenant_id,
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
    })
    .expect("compare request should serialize");
    assert!(compare.get("base_run_uid").is_some());
    assert!(compare.get("new_run_uid").is_some());
    assert!(compare.get("base_run").is_none());
    assert!(compare.get("new_run").is_none());

    let compare_response = serde_json::to_value(ExperimentCompareResponse {
        tenant_id,
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
        base_score_run_id: fixture_uuid(3),
        new_score_run_id: fixture_uuid(4),
        rows: Vec::new(),
        scenario_deltas: Vec::new(),
        variant_deltas: Vec::new(),
    })
    .expect("compare response should serialize");
    assert!(compare_response.get("base_score_run_id").is_some());
    assert!(compare_response.get("new_score_run_id").is_some());
    assert!(compare_response.get("base_run").is_none());
    assert!(compare_response.get("new_run").is_none());
}

#[test]
fn experiment_score_responses_serialize_typed_trial_and_scenario_breakdowns() {
    // Pins: Experiments/scores exposes typed aggregate, trial, and scenario score APIs.
    let tenant_id = tenant_id_fixture();
    let scenario_id = fixture_uuid(7);
    let response = ExperimentScoresResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        score_run_id: fixture_uuid(2),
        rows: vec![score_row("quality", "numeric", 2, 0.75)],
        trial_rollup_rows: vec![score_row("quality", "numeric", 4, 0.8)],
        trials: vec![ExperimentTrialScoreSummary {
            trial_uid: fixture_uuid(3),
            trial_key: "scenario-a/candidate/0".to_string(),
            score_run_id: fixture_uuid(4),
            variant_key: "candidate".to_string(),
            scenario_id: Some(scenario_id.to_string()),
            rows: vec![score_row("quality", "numeric", 1, 0.9)],
        }],
        scenarios: vec![ExperimentScenarioScoreSummary {
            scenario_id: Some(scenario_id.to_string()),
            rows: vec![score_row("quality", "numeric", 2, 0.85)],
        }],
    };

    let encoded = serde_json::to_value(response).expect("scores response should serialize");

    assert_eq!(encoded["rows"][0]["name"], "quality");
    assert_eq!(encoded["trial_rollup_rows"][0]["mean_or_rate"], 0.8);
    assert_eq!(encoded["trials"][0]["variant_key"], "candidate");
    assert_eq!(encoded["trials"][0]["scenario_id"], scenario_id.to_string());
    assert_eq!(
        encoded["scenarios"][0]["scenario_id"],
        scenario_id.to_string()
    );
}

#[test]
fn experiment_trial_responses_serialize_typed_ui_drilldown_fields() {
    // Pins: trial list/status APIs expose typed UI fields without raw record payloads.
    let tenant_id = tenant_id_fixture();
    let run_uid = fixture_uuid(1);
    let trial_uid = fixture_uuid(2);
    let scenario_id = fixture_uuid(3);
    let score_run_id = fixture_uuid(4);
    let session_id = SessionId(fixture_uuid(5));
    let workflow_run_uid = fixture_uuid(6);

    let response = ExperimentTrialsResponse {
        tenant_id,
        run_uid,
        trials: vec![ExperimentTrialSummary {
            tenant_id,
            run_uid,
            trial_uid,
            status: "completed".to_string(),
            target_kind: "agent_loop".to_string(),
            trial_key: "scenario/persona/profile/candidate/0".to_string(),
            variant_key: "candidate".to_string(),
            scenario_id: Some(scenario_id.to_string()),
            score_run_id,
            session_id: Some(session_id),
            workflow_run_uid: Some(workflow_run_uid),
            trace_id: Some("trace-fixture".to_string()),
            stop_reason: Some("success".to_string()),
            error: None,
            turn_count: 2,
        }],
    };

    let encoded = serde_json::to_value(response).expect("trials response should serialize");

    assert_eq!(encoded["run_uid"], run_uid.to_string());
    assert_eq!(encoded["trials"][0]["tenant_id"], tenant_id.to_string());
    assert_eq!(encoded["trials"][0]["trial_uid"], trial_uid.to_string());
    assert_eq!(encoded["trials"][0]["target_kind"], "agent_loop");
    assert_eq!(encoded["trials"][0]["variant_key"], "candidate");
    assert_eq!(encoded["trials"][0]["scenario_id"], scenario_id.to_string());
    assert_eq!(
        encoded["trials"][0]["score_run_id"],
        score_run_id.to_string()
    );
    assert_eq!(encoded["trials"][0]["session_id"], session_id.0.to_string());
    assert_eq!(
        encoded["trials"][0]["workflow_run_uid"],
        workflow_run_uid.to_string()
    );
    assert_eq!(encoded["trials"][0]["trace_id"], "trace-fixture");
    assert_eq!(encoded["trials"][0]["stop_reason"], "success");
    assert_eq!(encoded["trials"][0]["turn_count"], 2);
    assert!(encoded["trials"][0].get("run").is_none());
}

#[test]
fn experiment_compare_response_serializes_scenario_and_variant_deltas() {
    // Pins: Experiments/compare exposes typed scenario and variant deltas for trial rollups.
    let tenant_id = tenant_id_fixture();
    let scenario_id = fixture_uuid(7);
    let response = ExperimentCompareResponse {
        tenant_id,
        base_run_uid: fixture_uuid(1),
        new_run_uid: fixture_uuid(2),
        base_score_run_id: fixture_uuid(3),
        new_score_run_id: fixture_uuid(4),
        rows: vec![ExperimentCompareRow {
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.9),
            delta: Some(0.2),
        }],
        scenario_deltas: vec![ExperimentScenarioScoreDeltaRow {
            scenario_id: Some(scenario_id.to_string()),
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.9),
            delta: Some(0.2),
        }],
        variant_deltas: vec![ExperimentVariantScoreDeltaRow {
            variant_key: "candidate".to_string(),
            name: "quality".to_string(),
            base_mean: Some(0.7),
            new_mean: Some(0.9),
            delta: Some(0.2),
        }],
    };

    let encoded = serde_json::to_value(response).expect("compare response should serialize");

    assert_eq!(encoded["rows"][0]["delta"], 0.2);
    assert_eq!(
        encoded["scenario_deltas"][0]["scenario_id"],
        scenario_id.to_string()
    );
    assert_eq!(encoded["variant_deltas"][0]["variant_key"], "candidate");
    assert_eq!(encoded["variant_deltas"][0]["delta"], 0.2);
}

#[test]
fn experiments_service_declares_required_tenant_relations() {
    // Pins: experiment service handlers authorize tenant runtime access before protected work.
    let source = include_str!("../src/services/experiments.rs");
    assert!(
        source.contains("ObjectType::Tenant"),
        "experiment service must authorize tenant objects"
    );
    for method in [
        "generate_plan",
        "run",
        "status",
        "list",
        "trials",
        "trial_status",
        "cancel",
        "propose_improvements",
        "scores",
        "compare",
        "run_agent_revision_simulation",
        "compare_agent_revisions",
        "compare_agent_revision_simulation",
    ] {
        assert_method_requires_relation(source, method, "Relation::Operator");
    }
}

#[test]
fn experiments_exposes_propose_improvements_without_candidate_read_endpoint() {
    // Pins: Experiments owns proposal writes only; Analytics remains the learning-candidate read path.
    let experiments_source = normalized_source(include_str!("../src/services/experiments.rs"));
    let analytics_source = include_str!("../src/services/analytics.rs");

    assert!(
        experiments_source.contains(&normalized_source(
            "async fn propose_improvements(
                 request: Json<ExperimentProposeImprovementsRequest>,
             ) -> Result<Json<ExperimentProposeImprovementsResponse>, HandlerError>;"
        )),
        "Experiments should expose the explicit proposal operation"
    );
    assert!(
        experiments_source
            .contains("annotate_restate_handler_span(\"Experiments\", \"propose_improvements\")"),
        "proposal writes should have their own Experiments handler span"
    );
    assert!(
        !experiments_source.contains("async fn learning_candidates("),
        "Experiments must not grow a learning-candidate read endpoint"
    );
    assert!(
        analytics_source.contains("async fn learning_candidates("),
        "Analytics should remain the read surface for learning candidates"
    );
}

#[test]
fn experiment_proposal_payload_carries_evidence_and_stays_proposed() {
    // Pins: proposal candidates preserve experiment evidence without promoting learned state.
    let storage_partition_id = StoragePartitionId::new("workspace-a");
    let tenant_id = TenantId::new();
    let run = completed_run_record(storage_partition_id.clone());
    let trials = vec![completed_trial_record(run.run_uid)];
    let score_summary = moa_scoring::ScoreSummary {
        tenant_id,
        run_id: run.score_run_id,
        rows: vec![moa_scoring::ScoreSummaryRow {
            name: "quality".to_string(),
            value_type: "numeric".to_string(),
            n: 1,
            mean_or_rate: Some(0.92),
        }],
    };
    let trial_score_summary = moa_scoring::TrialScoreSummary {
        trial_uid: trials[0].trial_uid,
        trial_key: trials[0].trial_key.clone(),
        score_run_id: trials[0].score_run_id,
        variant_key: trials[0].variant_key.clone(),
        scenario_id: trials[0].scenario_id.clone(),
        rows: score_summary.rows.clone(),
    };
    let scenario_score_summary = moa_scoring::ScenarioScoreSummary {
        scenario_id: trials[0].scenario_id.clone(),
        rows: score_summary.rows.clone(),
    };

    let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
        tenant_id,
        run: &run,
        completed_trials: &trials,
        run_score_summary: &score_summary,
        trial_rollup_rows: &score_summary.rows,
        trial_score_summaries: std::slice::from_ref(&trial_score_summary),
        scenario_score_summaries: std::slice::from_ref(&scenario_score_summary),
        plan_revision_uid: fixture_uuid(20),
        draft_artifact_revision_uids: &[],
        idempotency_key: Some("proposal-key"),
        now: fixture_time(),
    });

    assert_eq!(candidate.status.as_str(), "proposed");
    assert_eq!(candidate.tenant_id, tenant_id);
    assert_eq!(candidate.candidate_type.as_str(), "workflow");
    assert_eq!(candidate.payload["kind"], "workflow_learning_proposal");
    assert_eq!(
        candidate.promotion_requirements,
        vec![
            "human_review".to_string(),
            "explicit_candidate_evaluation".to_string(),
            "no_automatic_artifact_publish".to_string(),
        ]
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["experiment_run_uid"],
        run.run_uid.to_string()
    );
    assert_eq!(candidate.payload["tenant_id"], tenant_id.to_string());
    assert_eq!(
        candidate.payload["evidence_refs"]["run_score_run_id"],
        run.score_run_id.to_string()
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["trial_score_run_ids"][0],
        trials[0].score_run_id.to_string()
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["artifact_revision_refs"]["scenario_ids"][0].as_str(),
        trials[0].scenario_id.as_deref()
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["artifact_revision_refs"]["persona_ids"][0].as_str(),
        trials[0].persona_id.as_deref()
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["session_ids"][0],
        trials[0]
            .session_id
            .expect("fixture should include session")
            .to_string()
    );
    assert_eq!(
        candidate.payload["evidence_refs"]["workflow_run_uids"][0],
        trials[0]
            .workflow_run_uid
            .expect("fixture should include workflow run")
            .to_string()
    );
    assert_eq!(
        candidate.payload["suggested_changes"]["draft_artifact_revision_uids"]
            .as_array()
            .expect("draft list should be an array")
            .len(),
        0,
        "proposal evidence currently has no meaningful artifact patch payload to draft"
    );
}

#[test]
fn experiment_proposal_path_rejects_incomplete_or_ungated_evidence() {
    // Pins: proposal writes must not rely on parent rows without completed trials and score rows.
    let source = normalized_source(include_str!("../../moa-experiments/src/app.rs"));

    assert!(
        source.contains("require_completed_run(&run)?;"),
        "proposal path should reject non-completed experiment runs"
    );
    assert!(
        source.contains("definition.learning_proposals.enabled"),
        "proposal path should enforce the plan learning_proposals.enabled gate"
    );
    assert!(
        source.contains("ExperimentTrialStatus::Completed"),
        "proposal path should load completed trials explicitly"
    );
    assert!(
        source.contains("completed_trials.is_empty()"),
        "proposal path should reject runs without completed trials"
    );
    assert!(
        source.contains("run_score_summary.rows.is_empty()"),
        "proposal path should reject missing run score rows"
    );
    assert!(
        source.contains("require_trial_score_rows(&completed_trials, &trial_breakdown.trials)?"),
        "proposal path should reject completed trials without actual score rows"
    );
}

#[test]
fn experiment_proposal_path_does_not_publish_or_promote() {
    // Pins: proposal execution appends proposed candidates only; review is required before publishing artifacts or learned state.
    let service_source = include_str!("../src/services/experiments.rs");
    let app_source = include_str!("../../moa-experiments/src/app.rs");

    assert!(
        service_source.contains("append_learning_candidate(&proposal.candidate)"),
        "proposal path should append a learning candidate for review"
    );
    assert!(
        app_source.contains("LearningCandidateStatus::Proposed"),
        "proposal path should create candidates that wait for review"
    );
    assert!(
        !service_source.contains("update_learning_candidate_status")
            && !app_source.contains("update_learning_candidate_status"),
        "proposal path must not transition candidate status"
    );
    assert!(
        !service_source.contains("LearningCandidateStatus::Promoted")
            && !app_source.contains("LearningCandidateStatus::Promoted"),
        "proposal path must not promote candidates"
    );
    assert!(
        !service_source.contains("publish_revision(") && !app_source.contains("publish_revision("),
        "proposal path must not publish artifact revisions"
    );
    assert!(
        !service_source.contains("bootstrap_global") && !app_source.contains("bootstrap_global"),
        "proposal path must not import or publish skills"
    );
    assert!(
        !service_source.contains("WorkflowRuntime::new")
            && !app_source.contains("WorkflowRuntime::new"),
        "proposal path must not start or publish workflows"
    );
}

#[test]
fn experiments_generate_plan_uses_provider_json_schema_and_stores_draft_artifact_only() {
    // Pins: plan generation is model-backed draft artifact creation, not durable execution.
    let service_source = normalized_source(include_str!("../src/services/experiments.rs"));
    let app_source = normalized_source(include_str!("../../moa-experiments/src/app.rs"));

    assert!(
        service_source.contains(&normalized_source(
            "async fn generate_plan(
                 request: Json<ExperimentGeneratePlanRequest>,
             ) -> Result<Json<ExperimentGeneratePlanResponse>, HandlerError>;"
        )),
        "Experiments should expose a generate_plan service method"
    );
    assert!(
        service_source.contains("LLMGatewayImpl::new(runtime.provider_registry())"),
        "generate_plan should use the configured provider registry through the LLM gateway"
    );
    assert!(
        app_source.contains("JsonResponseFormat::strict_json_schema"),
        "generate_plan should ask providers for structured experiment_plan JSON"
    );
    assert!(
        app_source.contains("document.kind != ArtifactKind::ExperimentPlan"),
        "generate_plan should reject non-experiment_plan generated artifacts"
    );
    assert!(
        app_source.contains("validate_for_status(document, ArtifactStatus::Draft)"),
        "generate_plan should draft-validate generated artifacts"
    );
    assert!(
        app_source.contains("NewArtifactDraft"),
        "generate_plan should store through ArtifactRegistry::create_draft"
    );
    assert!(
        app_source.contains("source_format: GENERATED_PLAN_SOURCE_FORMAT"),
        "generated artifact source format should be json"
    );
}

#[test]
fn experiments_generate_plan_rejects_invalid_generated_plan_before_storage() {
    // Pins: invalid LLM output fails validation before ArtifactRegistry::create_draft can persist it.
    let source = normalized_source(include_str!("../../moa-experiments/src/app.rs"));
    let validation_index = source
        .find("require_valid_generated_plan(&document)?;")
        .expect("generate_plan should validate the generated document");
    let storage_index = source
        .find(".create_draft(")
        .expect("generate_plan should store valid output through create_draft");

    assert!(
        validation_index < storage_index,
        "generated plans must be validated before storage"
    );
    assert!(
        source.contains("return Err(generated_plan_validation_error("),
        "invalid generated plans should return a validation error instead of storing"
    );
}

#[test]
fn generated_plan_draft_document_uses_normal_artifact_validation_shape() {
    // Pins: generated drafts are ordinary experiment_plan artifacts that artifact APIs can validate.
    let document = ArtifactDocument::from_json(&minimal_valid_generated_plan())
        .expect("generated plan fixture should parse as an artifact document");
    let report = validate_for_status(&document, ArtifactStatus::Draft);
    let exported = document
        .to_json()
        .expect("generated plan should export as canonical json");
    let reparsed = ArtifactDocument::from_json(&exported)
        .expect("exported generated plan should parse through artifact import path");

    assert_eq!(document.kind, ArtifactKind::ExperimentPlan);
    assert!(
        report.is_ok(),
        "generated draft should validate: {report:?}"
    );
    assert_eq!(document, reparsed);
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
    // Pins: agent-loop experiments enter the normal Session path and do not bypass action policy.
    let source = experiment_run_workflow_source();

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
        !source.contains("ActionReviewDecision::Cleared"),
        "ExperimentRun must not clear action reviews"
    );
}

#[test]
fn experiment_run_workflow_has_no_plan_generation_logic() {
    // Pins: Task 5 keeps prompt generation out of durable experiment execution.
    let source = experiment_run_workflow_source();

    assert!(
        !source.contains("generate_plan"),
        "ExperimentRun workflow should not expose plan generation"
    );
    assert!(
        !source.contains("JsonResponseFormat"),
        "ExperimentRun workflow should not request structured provider JSON"
    );
    assert!(
        !source.contains("LLMGateway"),
        "ExperimentRun workflow should not call the LLM for plan drafting"
    );
    assert!(
        !source.contains("ExperimentGeneratePlan"),
        "ExperimentRun workflow should not depend on generate-plan DTOs"
    );
}

#[test]
fn experiment_run_workflow_expands_plan_and_dispatches_bounded_trial_workflows() {
    // Pins: Task 7 parent workflow expands plan rows and dispatches child trials by stable key.
    let source = experiment_run_workflow_source();

    assert!(
        source.contains("plan_revision_uid: Option<Uuid>"),
        "ExperimentRun workflow request should carry the pinned plan revision"
    );
    assert!(
        source.contains(".name(\"experiment_plan_expand\")"),
        "plan artifact loading and matrix expansion should be journaled"
    );
    assert!(
        source.contains(".name(\"experiment_plan_create_trials\")"),
        "trial-row creation should be journaled"
    );
    assert!(
        source.contains("ExperimentTrialRunClient"),
        "parent run workflow should dispatch ExperimentTrialRun children"
    );
    assert!(
        source.contains("trial_workflow_key(request.run_uid, &trial.trial.trial_key)"),
        "child workflow identity should use deterministic trial keys"
    );
    assert!(
        source.contains("active_plan_trial_count(&aggregate.trials)"),
        "dispatch should respect the plan parallelism bound"
    );
    assert!(
        source.contains(".name(\"experiment_plan_claim_trial_dispatch\")"),
        "dispatch should re-read and claim accepted trials before sending child workflows"
    );
}

#[test]
fn experiment_cancellation_marks_active_trial_rows() {
    // Pins: cancelling a parent run prevents remaining accepted/running work from looking active.
    let service_source = include_str!("../src/services/experiments.rs");
    let app_source = include_str!("../../moa-experiments/src/app.rs");
    let workflow_source = experiment_run_workflow_source();
    let store_source = include_str!("../../moa-experiments/src/store.rs");

    assert!(
        service_source.contains("cancel_run(pool, request)"),
        "service cancellation should dispatch to the experiment app boundary"
    );
    assert!(
        app_source.contains(".cancel_active_trials(&scope, request.run_uid, reason.clone())"),
        "experiment app cancellation should mark active child trials immediately"
    );
    assert!(
        workflow_source.contains(".name(\"experiment_plan_cancel_active_trials\")"),
        "parent workflow cancellation reconciliation should be journaled"
    );
    assert!(
        store_source.contains("AND status IN ('accepted', 'dispatched', 'running')"),
        "store helper should only cancel non-terminal trial rows"
    );
}

#[test]
fn experiment_trial_run_workflow_is_bound_and_expected_by_readiness() {
    // Pins: Task 6 adds a real Restate workflow, not a helper-only executor.
    let endpoint_source = include_str!("../src/runtime/endpoint.rs");
    let workflows_source = include_str!("../src/workflows/mod.rs");

    assert!(
        workflows_source.contains("pub mod experiment_trial_run;"),
        "workflow module should expose ExperimentTrialRun"
    );
    assert!(
        endpoint_source
            .contains("experiment_trial_run::{ExperimentTrialRun, ExperimentTrialRunImpl}"),
        "orchestrator endpoint builder should import the ExperimentTrialRun workflow"
    );
    assert!(
        endpoint_source.contains(".bind(ExperimentTrialRunImpl.serve())"),
        "orchestrator endpoint should bind ExperimentTrialRun"
    );
    assert!(
        endpoint_source.contains("\"ExperimentTrialRun\""),
        "readiness expected services should include ExperimentTrialRun"
    );
}

#[test]
fn experiment_trial_run_queues_simulator_messages_without_target_tools_or_review_clearance() {
    // Pins: simulator trials enter the target through Session/queue_message and never bypass action policy.
    let source = experiment_trial_run_workflow_source();

    assert!(
        source.contains("object_client::<SessionClient>"),
        "ExperimentTrialRun should call the Session VO"
    );
    assert!(
        source.contains(".queue_message("),
        "ExperimentTrialRun should submit simulator user messages through Session/queue_message"
    );
    assert!(
        source.contains("request.tools = Vec::new();"),
        "simulator provider calls should not expose target tools"
    );
    assert!(
        !source.contains("ActionReviewDecision::Cleared"),
        "simulator trials should not clear action reviews"
    );
    assert!(
        !source.contains("TurnExecutionClient"),
        "ExperimentTrialRun must not invoke TurnExecution directly"
    );
    assert!(
        !source.contains("ActionReviewDecision::Cleared"),
        "ExperimentTrialRun must not auto-clear reviewed actions"
    );
}

#[test]
fn experiment_trial_run_journals_external_work_and_uses_trial_key_idempotency() {
    // Pins: pinned artifact loads, DB writes, and simulator LLM calls stay inside ctx.run activities.
    let source_text = experiment_trial_run_workflow_source();
    let source = normalized_source(&source_text);

    assert!(
        source.contains(&normalized_source(
            "ExperimentStore::new(pool) .insert_trial(&scope, trial)"
        )),
        "trial workflow should rely on ExperimentStore insert_trial idempotency"
    );
    assert!(
        source.contains("fn trial_workflow_key(run_uid: Uuid, trial_key: &str) -> String"),
        "workflow key should include the deterministic trial key"
    );
    assert!(
        source.contains(".name(\"experiment_trial_load_plan\")"),
        "pinned artifact loading should be journaled"
    );
    assert!(
        source.contains(".name(\"simulation_user_model_call\")"),
        "simulator provider calls should be journaled"
    );
    assert!(
        source.contains(".name(\"experiment_trial_update_status\")"),
        "trial status persistence should be journaled"
    );
}

#[test]
fn experiment_trial_run_attaches_current_trace_before_target_execution() {
    // Pins: trial records carry the trace ID needed to debug a failing simulator run.
    let source_text = experiment_trial_run_workflow_source();
    let source = normalized_source(&source_text);
    let insert_index = source
        .find("insert_or_load_trial(ctx, request.tenant_id, request.trial.clone()).await?")
        .expect("trial workflow should insert or load a durable trial row");
    let attach_index = source
        .find("attach_current_trial_trace(ctx, request.tenant_id, trial.trial_uid).await?")
        .expect("trial workflow should attach the active trace to the durable trial row");
    let agent_loop_index = source
        .find("run_agent_loop_trial(ctx, request, trial, simulator_context).await")
        .expect("trial workflow should run agent-loop targets through the simulator path");
    let workflow_index = source
        .find("run_workflow_trial(ctx, request, trial).await")
        .expect("trial workflow should run workflow targets through the workflow path");

    assert!(
        insert_index < attach_index,
        "trace attachment should happen after the stable trial_uid is known"
    );
    assert!(
        attach_index < agent_loop_index && attach_index < workflow_index,
        "trace attachment should happen before target execution starts"
    );
    assert!(
        source.contains(
            "ExperimentStore::new(pool) .attach_trial_trace(&scope, trial_uid, trace_id)"
        ),
        "trace attachment should use the ExperimentStore attach_trial_trace path"
    );
    assert!(
        source.contains("current_trace_id()"),
        "trace attachment should derive the active OpenTelemetry trace id"
    );
}

#[test]
fn experiment_observability_uses_trace_attributes_for_drilldown_ids_only() {
    // Pins: drilldown IDs are trace attributes; Prometheus labels stay bounded.
    let trial_source = experiment_trial_run_workflow_source();
    let run_source = experiment_run_workflow_source();
    let metrics_source = include_str!("../../moa-observability/src/runtime_metrics.rs");

    for required in [
        "moa.experiment.run_uid",
        "moa.experiment.trial_uid",
        "moa.experiment.session_id",
        "moa.experiment.workflow_run_uid",
        "moa.experiment.score_run_id",
    ] {
        assert!(
            trial_source.contains(required) || run_source.contains(required),
            "experiment traces should expose drilldown attribute {required}"
        );
    }

    let experiment_metrics_source = metrics_source
        .split("pub fn record_experiment_run")
        .nth(1)
        .expect("experiment metric helpers should exist")
        .split("#[cfg(tokio_unstable)]")
        .next()
        .expect("experiment metric helper section should end before runtime publisher");
    for forbidden in [
        "run_uid",
        "trial_uid",
        "session_id",
        "workflow_run_uid",
        "score_run_id",
        "trial_key",
        "artifact_revision",
        "prompt",
        "profile",
        "persona",
        "scenario",
        "transcript",
        "connector",
        "model_output",
    ] {
        assert!(
            !experiment_metrics_source.contains(forbidden),
            "experiment metric helper labels must not contain `{forbidden}`"
        );
    }
}

#[test]
fn experiment_trial_run_supports_current_workflow_runtime_without_fake_stepping() {
    // Pins: workflow-target trials start WorkflowRuntime and stop at the current artifact-run state.
    let source = experiment_trial_run_workflow_source();

    assert!(
        source.contains("WorkflowRuntime::new(ArtifactRegistry::new(pool))"),
        "workflow target trials should use WorkflowRuntime"
    );
    assert!(
        source.contains(".start("),
        "workflow target trials should start an artifact workflow run"
    );
    assert!(
        source.contains("ArtifactRunStatus"),
        "workflow target trials should map artifact-run statuses"
    );
    assert!(
        !source.contains("current_node_id = Some"),
        "trial workflow should not fake workflow node stepping"
    );
}

#[test]
fn experiment_score_handlers_resolve_run_uids_through_scoped_experiment_runs() {
    // Pins: score APIs reject cross-tenant experiment IDs by resolving run_uid through a scoped experiment load.
    let service_source = normalized_source(include_str!("../src/services/experiments.rs"));
    let app_source = normalized_source(include_str!("../../moa-experiments/src/app.rs"));

    assert!(
        service_source.contains("scores(pool, request)"),
        "scores handler should dispatch to the experiment app boundary"
    );
    assert!(
        app_source.contains(&normalized_source(
            "let scope = tenant_scope(request.tenant_id);
             let run = load_required_run(&ExperimentStore::new(pool.clone()), &scope, request.run_uid).await?;"
        )),
        "experiment app must load the experiment run in the requested tenant before reading scores"
    );
    assert!(
        app_source.contains(&normalized_source("run_id: run.score_run_id")),
        "experiment app must query analytics scores by resolved score_run_id"
    );
    assert!(
        !service_source.contains(&normalized_source("run_id: request.run_uid"))
            && !app_source.contains(&normalized_source("run_id: request.run_uid")),
        "score handling must not treat experiment run_uid as a score run id"
    );

    assert!(
        service_source.contains("compare_runs(pool, request)"),
        "compare handler should dispatch to the experiment app boundary"
    );
    assert!(
        app_source.contains(&normalized_source(
            "let scope = tenant_scope(request.tenant_id);
             let store = ExperimentStore::new(pool.clone());
             let base_run = load_required_run(&store, &scope, request.base_run_uid).await?;
             let new_run = load_required_run(&store, &scope, request.new_run_uid).await?;"
        )),
        "experiment app must load both experiment runs in the requested tenant"
    );
    assert!(
        app_source.contains(&normalized_source("base_run: base_run.score_run_id")),
        "experiment app must pass the resolved baseline score_run_id to the shared score helper"
    );
    assert!(
        app_source.contains(&normalized_source("new_run: new_run.score_run_id")),
        "experiment app must pass the resolved new score_run_id to the shared score helper"
    );
    assert!(
        !service_source.contains(&normalized_source("base_run: request.base_run_uid"))
            && !app_source.contains(&normalized_source("base_run: request.base_run_uid")),
        "compare handling must not treat baseline experiment run_uid as a score run id"
    );
    assert!(
        !service_source.contains(&normalized_source("new_run: request.new_run_uid"))
            && !app_source.contains(&normalized_source("new_run: request.new_run_uid")),
        "compare handling must not treat new experiment run_uid as a score run id"
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

fn assert_has_tenant_id<T: Serialize>(value: T) {
    let encoded = serde_json::to_value(value).expect("wire DTO should serialize");
    assert!(
        encoded.get("tenant_id").is_some(),
        "wire DTO should include tenant_id: {encoded}"
    );
}

fn assert_method_requires_relation(source: &str, method: &str, relation: &str) {
    let needle = format!("async fn {method}(");
    let start = source
        .match_indices(&needle)
        .last()
        .map(|(index, _)| index)
        .unwrap_or_else(|| panic!("Experiments::{method} implementation should exist"));
    let tail = &source[start..];
    let end = tail
        .find("\n    #[tracing::instrument")
        .unwrap_or(tail.len());
    let method_body = &tail[..end];

    assert!(
        method_body.contains(&format!(
            "authorize_tenant(&ctx, request.tenant_id, {relation})"
        )),
        "Experiments::{method} should require {relation}"
    );
}

fn fixture_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn tenant_id_fixture() -> TenantId {
    TenantId::from(fixture_uuid(42))
}

fn score_row(
    name: impl Into<String>,
    value_type: impl Into<String>,
    n: u64,
    mean_or_rate: f64,
) -> ExperimentScoreSummaryRow {
    ExperimentScoreSummaryRow {
        name: name.into(),
        value_type: value_type.into(),
        n,
        mean_or_rate: Some(mean_or_rate),
    }
}

fn completed_run_record(storage_partition_id: StoragePartitionId) -> ExperimentRunRecord {
    ExperimentRunRecord {
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::new(),
        },
        run_uid: fixture_uuid(1),
        name: format!("proposal fixture {storage_partition_id}"),
        target_kind: ExperimentTargetKind::AgentLoop,
        status: ExperimentRunStatus::Completed,
        target: ExperimentTarget::AgentLoop {
            prompt: "Improve support behavior.".to_string(),
            session_id: Some(SessionId(fixture_uuid(2))),
            agent: None,
            model: ModelId::new("gpt-5.4"),
            attachments: Vec::new(),
        },
        variant: ExperimentVariant {
            name: "candidate".to_string(),
            model: Some(ModelId::new("gpt-5.4")),
            artifact_revision_uids: vec![fixture_uuid(20), fixture_uuid(21)],
            skill_refs: vec!["skill://support-style".to_string()],
            workflow_ref: Some("workflow://support-flow".to_string()),
            metadata: json!({"plan_revision_uid": fixture_uuid(20)}),
        },
        scorecard: ExperimentScorecard {
            score_names: vec!["quality".to_string()],
            evaluator_metadata: json!({"judge": "fixture"}),
        },
        score_run_id: fixture_uuid(3),
        session_id: Some(SessionId(fixture_uuid(4))),
        workflow_run_uid: Some(fixture_uuid(5)),
        artifact_revision_uids: vec![fixture_uuid(20), fixture_uuid(21)],
        idempotency_key: Some("run-key".to_string()),
        created_by_identity: json!({"type": "user"}),
        error: None,
        created_at: fixture_time(),
        started_at: Some(fixture_time()),
        completed_at: Some(fixture_time()),
        updated_at: fixture_time(),
    }
}

fn completed_trial_record(run_uid: Uuid) -> ExperimentTrialRecord {
    ExperimentTrialRecord {
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::new(),
        },
        trial_uid: fixture_uuid(6),
        run_uid,
        trial_key: "scenario/persona/profile/candidate/0".to_string(),
        status: ExperimentTrialStatus::Completed,
        target_kind: ExperimentTargetKind::AgentLoop,
        variant_key: "candidate".to_string(),
        plan_revision_uid: fixture_uuid(20),
        persona_id: Some(fixture_uuid(7).to_string()),
        profile_id: Some(fixture_uuid(8).to_string()),
        scenario_id: Some(fixture_uuid(9).to_string()),
        data_bundle_ids: vec![fixture_uuid(10).to_string()],
        artifact_revision_uids: vec![fixture_uuid(21)],
        simulator: ExperimentSimulatorConfig {
            model: ModelId::new("gpt-5.4"),
            temperature: Some(0.2),
            max_turns: 3,
            token_budget: Some(1000),
            metadata: json!({"fixture": true}),
        },
        target_model: Some(ModelId::new("gpt-5.4")),
        seed: Some("seed".to_string()),
        session_id: Some(SessionId(fixture_uuid(11))),
        workflow_run_uid: Some(fixture_uuid(12)),
        score_run_id: fixture_uuid(13),
        turn_count: 2,
        stop_reason: None,
        error: None,
        trace_id: Some("trace-fixture".to_string()),
        started_at: Some(fixture_time()),
        completed_at: Some(fixture_time()),
        created_at: fixture_time(),
        updated_at: fixture_time(),
    }
}

fn fixture_time() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0)
        .single()
        .expect("fixture datetime should be valid")
}

fn minimal_valid_generated_plan() -> String {
    json!({
        "api_version": "moa.artifact/v1",
        "kind": "experiment_plan",
        "metadata": {
            "name": "damaged-food-order-behavior",
            "description": "Generated behavior-lab draft."
        },
        "status": "draft",
        "definition": {
            "type": "experiment_plan",
            "spec": {
                "simulation": {
                    "scenarios": [{
                        "id": "damaged-food-order",
                        "initial_situation": "The user reports a damaged food delivery.",
                        "goals": ["Get a concrete replacement or refund-review next step."],
                        "success_criteria": ["The target asks for enough evidence before resolving."],
                        "max_turns": 3
                    }],
                    "personas": [{
                        "id": "concerned-customer",
                        "voice": "Concerned and concise.",
                        "goals": ["Resolve the damaged order."],
                        "stop_behavior": "Stop after the target gives a concrete next step."
                    }],
                    "profiles": [{
                        "id": "loyal-customer",
                        "facts": { "account_tier": "loyal" }
                    }]
                },
                "target_variants": [
                    { "key": "agent-loop", "kind": "agent_loop" }
                ],
                "simulator_model": "gpt-5.4-mini",
                "parallelism": 1,
                "trials_per_combination": 1,
                "budget": { "max_total_cents": 1000 }
            }
        }
    })
    .to_string()
}

fn experiment_run_workflow_source() -> String {
    [
        include_str!("../src/workflows/experiment_run.rs"),
        include_str!("../src/workflows/experiment_run/plan_expansion.rs"),
        include_str!("../src/workflows/experiment_run/status.rs"),
        include_str!("../src/workflows/experiment_run/target_execution.rs"),
    ]
    .join("\n")
}

fn experiment_trial_run_workflow_source() -> String {
    [
        include_str!("../src/workflows/experiment_trial_run.rs"),
        include_str!("../src/workflows/experiment_trial_run/status.rs"),
        include_str!("../src/workflows/experiment_trial_run/target_execution.rs"),
        include_str!("../src/workflows/experiment_trial_run/trial_simulator.rs"),
    ]
    .join("\n")
}

fn normalized_source(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}
