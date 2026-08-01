//! Experiment service helper coverage.

#[path = "support/mod.rs"]
mod support;

use chrono::{TimeZone, Utc};
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_artifacts::validation::validate_for_status;
use moa_core::types::experiments::{
    ExperimentScorecard, ScorecardEffect, ScorecardFinding, ScorecardGroupRollup,
    ScorecardRequirement, ScorecardSupportSummary, ScorecardValueType,
};
use moa_core::{
    types::action_policy::ActionRuleScope, types::execution_planning::PinnedExecutionTemplateRef,
    types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_experiments::app::{
    ExperimentLearningProposalEvidence, ExperimentRunScorecards, TrialScorecardAssessment,
    build_experiment_learning_candidate,
};
use moa_experiments::eligibility::{ScorecardAssessment, ScorecardEligibility};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentSimulatorConfig, ExperimentTarget,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentVariant,
};
use moa_experiments::scores::{ScenarioScoreSummary, TrialScoreSummary};
use moa_wire::artifacts::ArtifactSummary;
use moa_wire::experiments::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentCompareRow, ExperimentGeneratePlanRequest,
    ExperimentGeneratePlanResponse, ExperimentListRequest, ExperimentListResponse,
    ExperimentPlanListRequest, ExperimentPlanListResponse, ExperimentProposeImprovementsRequest,
    ExperimentProposeImprovementsResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentRunStatusRequest, ExperimentRunStatusResponse, ExperimentScenarioScoreDeltaRow,
    ExperimentScenarioScoreSummary, ExperimentScoreSummaryRow, ExperimentScoresRequest,
    ExperimentScoresResponse, ExperimentTrialScoreSummary, ExperimentTrialStatusRequest,
    ExperimentTrialStatusResponse, ExperimentTrialSummary, ExperimentTrialsRequest,
    ExperimentTrialsResponse, ExperimentVariantScoreDeltaRow,
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
    assert_experiment_type::<ExperimentPlanListRequest>();
    assert_experiment_type::<ExperimentPlanListResponse>();
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
        scorecard: Some(
            ExperimentScorecard::new(vec![ScorecardRequirement {
                evaluator_id: "target_completed".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Blocking,
            }])
            .expect("fixture scorecard is valid"),
        ),
        score_run_id: None,
        idempotency_key: Some("run-key".to_string()),
        agent_revision_variants: Vec::new(),
        release_evaluation: None,
    });
    assert_has_tenant_id(ExperimentGeneratePlanRequest {
        tenant_id,
        description: "Simulate damaged-food-order support behavior.".to_string(),
        model: Some("gpt-5.4".to_string()),
        artifact_refs: vec!["skill://damaged-food-order".to_string()],
        simulator_policy_uid: fixture_uuid(3),
        simulator_policy_revision: 1,
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
        execution_run_uid: None,
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
        execution_run_uid: None,
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
    assert_has_tenant_id(ExperimentPlanListRequest {
        tenant_id,
        scope: Some(ActionRuleScope::Tenant { tenant_id }),
        status: Some("published".to_string()),
    });
    assert_has_tenant_id(ExperimentPlanListResponse {
        tenant_id,
        plans: vec![ArtifactSummary {
            artifact_uid: fixture_uuid(1),
            revision_uid: fixture_uuid(2),
            scope: "tenant".to_string(),
            kind: "experiment_plan".to_string(),
            name: "support-behavior".to_string(),
            description: "Support behavior plan".to_string(),
            tags: vec!["behavior-lab".to_string()],
            status: "published".to_string(),
            version: 1,
            updated_at: fixture_time(),
        }],
    });
    let trial_summary = ExperimentTrialSummary {
        tenant_id,
        run_uid: fixture_uuid(1),
        trial_uid: fixture_uuid(2),
        status: "completed".to_string(),
        target_kind: "execution_template".to_string(),
        trial_key: "scenario-a/persona-a/profile-a/candidate/0".to_string(),
        variant_key: "candidate".to_string(),
        scenario_id: Some(fixture_uuid(3).to_string()),
        score_run_id: fixture_uuid(4),
        session_id: Some(SessionId(fixture_uuid(5))),
        execution_run_uid: Some(fixture_uuid(6)),
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
        execution_run_uid: trial_summary.execution_run_uid,
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
        trial_rollup_rows: vec![score_row("task.completed", "boolean", 2, 0.5)],
        trials: vec![ExperimentTrialScoreSummary {
            trial_uid: fixture_uuid(5),
            trial_key: "scenario-a/baseline/0".to_string(),
            score_run_id: fixture_uuid(6),
            variant_key: "baseline".to_string(),
            scenario_id: Some(fixture_uuid(7).to_string()),
            rows: vec![score_row("task.completed", "boolean", 1, 1.0)],
            eligibility: ScorecardEligibility::Eligible,
            eligibility_findings: Vec::new(),
        }],
        scenarios: vec![ExperimentScenarioScoreSummary {
            scenario_id: Some(fixture_uuid(7).to_string()),
            rows: vec![score_row("task.completed", "boolean", 2, 0.5)],
        }],
        run_scorecard: scorecard_rollup("run", ScorecardEligibility::Eligible, 1),
        scenario_scorecards: vec![scorecard_rollup(
            &fixture_uuid(7).to_string(),
            ScorecardEligibility::Eligible,
            1,
        )],
        variant_scorecards: vec![scorecard_rollup(
            "baseline",
            ScorecardEligibility::Eligible,
            1,
        )],
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
fn experiment_score_responses_serialize_typed_trial_and_scenario_breakdowns() {
    // Pins: Experiments/scores exposes typed aggregate, trial, and scenario score APIs.
    let tenant_id = tenant_id_fixture();
    let scenario_id = fixture_uuid(7);
    let response = ExperimentScoresResponse {
        tenant_id,
        run_uid: fixture_uuid(1),
        score_run_id: fixture_uuid(2),
        trial_rollup_rows: vec![score_row("quality", "numeric", 4, 0.8)],
        trials: vec![ExperimentTrialScoreSummary {
            trial_uid: fixture_uuid(3),
            trial_key: "scenario-a/candidate/0".to_string(),
            score_run_id: fixture_uuid(4),
            variant_key: "candidate".to_string(),
            scenario_id: Some(scenario_id.to_string()),
            rows: vec![score_row("quality", "numeric", 1, 0.9)],
            eligibility: ScorecardEligibility::Incomplete,
            eligibility_findings: vec![ScorecardFinding {
                score_name: "target_completed".to_string(),
                detail: "no provenance-backed score row is visible yet".to_string(),
            }],
        }],
        scenarios: vec![ExperimentScenarioScoreSummary {
            scenario_id: Some(scenario_id.to_string()),
            rows: vec![score_row("quality", "numeric", 2, 0.85)],
        }],
        run_scorecard: scorecard_rollup("run", ScorecardEligibility::Incomplete, 1),
        scenario_scorecards: vec![scorecard_rollup(
            &scenario_id.to_string(),
            ScorecardEligibility::Incomplete,
            1,
        )],
        variant_scorecards: vec![scorecard_rollup(
            "candidate",
            ScorecardEligibility::Incomplete,
            1,
        )],
    };

    let encoded = serde_json::to_value(response).expect("scores response should serialize");

    // Run-level score rows are gone from this response on purpose: trial score
    // runs are authoritative, and nothing in the trial path ever wrote a
    // run-level row, so the old `rows` field could only ever hold seeded data.
    assert!(encoded.get("rows").is_none());
    assert_eq!(encoded["trial_rollup_rows"][0]["mean_or_rate"], 0.8);
    assert_eq!(encoded["trials"][0]["variant_key"], "candidate");
    assert_eq!(encoded["trials"][0]["scenario_id"], scenario_id.to_string());
    assert_eq!(encoded["trials"][0]["eligibility"], "incomplete");
    assert_eq!(
        encoded["scenarios"][0]["scenario_id"],
        scenario_id.to_string()
    );
    assert_eq!(encoded["run_scorecard"]["eligibility"], "incomplete");
    assert_eq!(encoded["run_scorecard"]["support"]["independent_units"], 1);
    assert_eq!(
        encoded["run_scorecard"]["support"]["required_independent_units"],
        moa_experiments::eligibility::group_support_floor()
    );
    assert_eq!(
        encoded["run_scorecard"]["support"]["status"],
        "insufficient_independent_units"
    );
    assert_eq!(encoded["scenario_scorecards"][0]["trials"], 1);
    assert_eq!(encoded["variant_scorecards"][0]["key"], "candidate");
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
    let execution_run_uid = fixture_uuid(6);

    let response = ExperimentTrialsResponse {
        tenant_id,
        run_uid,
        trials: vec![ExperimentTrialSummary {
            tenant_id,
            run_uid,
            trial_uid,
            status: "completed".to_string(),
            target_kind: "execution_template".to_string(),
            trial_key: "scenario/persona/profile/candidate/0".to_string(),
            variant_key: "candidate".to_string(),
            scenario_id: Some(scenario_id.to_string()),
            score_run_id,
            session_id: Some(session_id),
            execution_run_uid: Some(execution_run_uid),
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
    assert_eq!(encoded["trials"][0]["target_kind"], "execution_template");
    assert_eq!(encoded["trials"][0]["variant_key"], "candidate");
    assert_eq!(encoded["trials"][0]["scenario_id"], scenario_id.to_string());
    assert_eq!(
        encoded["trials"][0]["score_run_id"],
        score_run_id.to_string()
    );
    assert_eq!(encoded["trials"][0]["session_id"], session_id.0.to_string());
    assert_eq!(
        encoded["trials"][0]["execution_run_uid"],
        execution_run_uid.to_string()
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

// Authorization for the Experiments service is exercised behaviorally, not by source-grep:
// `experiment_agent_loop_e2e::experiments_run_denies_caller_without_tenant_operator` calls
// `Experiments/run` over the real Restate + OpenFGA stack as a caller with no Tenant:Operator
// grant and asserts a 403 denial. Every Experiments handler authorizes tenant
// operator/admin access as its first statement, so that e2e is the template for
// the remaining read/mutate handlers.

#[test]
fn experiment_proposal_payload_carries_evidence_and_stays_proposed() {
    // Pins: proposal candidates preserve experiment evidence without promoting learned state.
    let storage_partition_id = StoragePartitionId::new("workspace-a");
    let tenant_id = TenantId::new();
    let run = completed_run_record(storage_partition_id.clone());
    let trials = vec![completed_trial_record(run.run_uid)];
    let summary_rows = vec![moa_scoring::ScoreSummaryRow {
        name: "target_completed".to_string(),
        value_type: ScorecardValueType::Boolean,
        n: 1,
        mean_or_rate: Some(1.0),
    }];
    let scorecards = ExperimentRunScorecards {
        run: ScorecardGroupRollup {
            key: run.run_uid.to_string(),
            eligibility: ScorecardEligibility::Eligible,
            trials: 1,
            support: ScorecardSupportSummary::from_counts(
                moa_experiments::eligibility::group_support_floor(),
                moa_experiments::eligibility::group_support_floor(),
            ),
        },
        scenarios: Vec::new(),
        variants: Vec::new(),
        trials: vec![TrialScorecardAssessment {
            trial_uid: trials[0].trial_uid,
            score_run_id: trials[0].score_run_id,
            trial_key: trials[0].trial_key.clone(),
            variant_key: trials[0].variant_key.clone(),
            scenario_id: trials[0].scenario_id.clone(),
            persona_id: trials[0].persona_id.clone(),
            profile_id: trials[0].profile_id.clone(),
            assessment: ScorecardAssessment {
                eligibility: ScorecardEligibility::Eligible,
                findings: Vec::new(),
            },
        }],
    };
    let trial_score_summary = TrialScoreSummary {
        trial_uid: trials[0].trial_uid,
        trial_key: trials[0].trial_key.clone(),
        score_run_id: trials[0].score_run_id,
        variant_key: trials[0].variant_key.clone(),
        scenario_id: trials[0].scenario_id.clone(),
        rows: summary_rows.clone(),
    };
    let scenario_score_summary = ScenarioScoreSummary {
        scenario_id: trials[0].scenario_id.clone(),
        rows: summary_rows.clone(),
    };

    let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
        tenant_id,
        run: &run,
        completed_trials: &trials,
        scorecards: &scorecards,
        trial_rollup_rows: &summary_rows,
        trial_score_summaries: std::slice::from_ref(&trial_score_summary),
        scenario_score_summaries: std::slice::from_ref(&scenario_score_summary),
        plan_revision_uid: fixture_uuid(20),
        draft_artifact_revision_uids: &[],
        idempotency_key: Some("proposal-key"),
        now: fixture_time(),
    });

    assert_eq!(candidate.status.as_str(), "needs_authoring");
    assert_eq!(candidate.tenant_id, tenant_id);
    assert_eq!(candidate.candidate_type.as_str(), "skill");
    assert_eq!(candidate.payload["kind"], "experiment_learning_proposal");
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
        candidate.payload["evidence_refs"]["execution_run_uids"][0],
        trials[0]
            .execution_run_uid
            .expect("fixture should include execution run")
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
    let value_type = value_type.into();
    ExperimentScoreSummaryRow {
        name: name.into(),
        value_type: ScorecardValueType::from_db(&value_type)
            .expect("score summary fixture value type should be supported"),
        n,
        mean_or_rate: Some(mean_or_rate),
    }
}

fn completed_run_record(storage_partition_id: StoragePartitionId) -> ExperimentRunRecord {
    let template = pinned_execution_template(22);
    ExperimentRunRecord {
        plan_artifact_uid: None,
        resource_envelope: fixture_experiment_envelope(),
        simulator_policy: None,
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::new(),
        },
        run_uid: fixture_uuid(1),
        name: format!("proposal fixture {storage_partition_id}"),
        target_kind: ExperimentTargetKind::ExecutionTemplate,
        status: ExperimentRunStatus::Completed,
        target: ExperimentTarget::ExecutionTemplate {
            template: template.clone(),
            objective: "Improve support behavior with the pinned support flow.".to_string(),
            input: json!({"ticket_id": "ticket-42"}),
            session_id: Some(SessionId(fixture_uuid(2))),
            idempotency_key: Some("template-run-key".to_string()),
        },
        variant: ExperimentVariant {
            name: "candidate".to_string(),
            model: None,
            artifact_revision_uids: vec![fixture_uuid(20), fixture_uuid(21), template.revision_uid],
            skill_refs: Vec::new(),
            execution_template: Some(template),
            metadata: json!({"plan_revision_uid": fixture_uuid(20)}),
        },
        scorecard: ExperimentScorecard::new(vec![ScorecardRequirement {
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Blocking,
        }])
        .expect("fixture scorecard is valid"),
        score_run_id: fixture_uuid(3),
        session_id: Some(SessionId(fixture_uuid(4))),
        execution_run_uid: Some(fixture_uuid(5)),
        artifact_revision_uids: vec![fixture_uuid(20), fixture_uuid(21)],
        idempotency_key: Some("run-key".to_string()),
        created_by_identity: json!({"type": "operator"}),
        error: None,
        created_at: fixture_time(),
        started_at: Some(fixture_time()),
        completed_at: Some(fixture_time()),
        updated_at: fixture_time(),
    }
}

fn completed_trial_record(run_uid: Uuid) -> ExperimentTrialRecord {
    ExperimentTrialRecord {
        resource_envelope: fixture_experiment_envelope().trial_envelope(),
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::new(),
        },
        trial_uid: fixture_uuid(6),
        run_uid,
        trial_key: "scenario/persona/profile/candidate/0".to_string(),
        status: ExperimentTrialStatus::Completed,
        target_kind: ExperimentTargetKind::ExecutionTemplate,
        variant_key: "candidate".to_string(),
        plan_revision_uid: fixture_uuid(20),
        persona_id: Some(fixture_uuid(7).to_string()),
        profile_id: Some(fixture_uuid(8).to_string()),
        scenario_id: Some(fixture_uuid(9).to_string()),
        data_bundle_ids: vec![fixture_uuid(10).to_string()],
        artifact_revision_uids: vec![fixture_uuid(21)],
        simulator: ExperimentSimulatorConfig {
            policy: support::simulator_policy::fixture("gpt-5.4"),
            max_turns: 3,
            token_budget: Some(1000),
        },
        target_model: Some(ModelId::new("gpt-5.4")),
        seed: Some("seed".to_string()),
        session_id: Some(SessionId(fixture_uuid(11))),
        execution_run_uid: Some(fixture_uuid(12)),
        score_run_id: fixture_uuid(13),
        final_evidence_hash: Some(vec![7; 32]),
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

fn pinned_execution_template(last_byte: u8) -> PinnedExecutionTemplateRef {
    PinnedExecutionTemplateRef {
        skill_ref: "skill://support-flow".to_string(),
        revision_uid: fixture_uuid(last_byte),
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
                "simulator_policy": {
                    "policy_uid": "10000000-0000-0000-0000-000000000001",
                    "revision": 1
                },
                "parallelism": 1,
                "trials_per_combination": 1,
                "budget": { "max_total_cents": 1000 },
                "scorecard": {
                    "requirements": [{
                        "evaluator_id": "target_completed",
                        "evaluator_version": "v1",
                        "config": {},
                        "effect": "blocking"
                    }]
                }
            }
        }
    })
    .to_string()
}

/// Builds one wire scorecard rollup for DTO-shape assertions.
fn scorecard_rollup(
    key: &str,
    eligibility: ScorecardEligibility,
    trials: usize,
) -> ScorecardGroupRollup {
    let required = moa_experiments::eligibility::group_support_floor();
    ScorecardGroupRollup {
        key: key.to_string(),
        eligibility,
        trials,
        support: if eligibility == ScorecardEligibility::Eligible {
            ScorecardSupportSummary::from_counts(required, required)
        } else {
            ScorecardSupportSummary::from_counts(required.saturating_sub(1), required)
        },
    }
}

/// Bounded experiment envelope for fixtures in this test binary.
///
/// Stated locally rather than pulled from a platform ceiling so a change to a
/// production limit cannot silently retune what these tests exercise.
fn fixture_experiment_envelope() -> moa_experiments::model::ExperimentResourceEnvelope {
    let limits = moa_core::types::resource::ResourceAmounts {
        cost_micro_usd: 1_000_000,
        tokens: 100_000,
        turns: 8,
        model_calls: 16,
        tool_calls: 32,
    };
    moa_experiments::model::ExperimentResourceEnvelope::new(
        limits,
        limits,
        moa_test_support::fixtures::pg_now(),
    )
}
