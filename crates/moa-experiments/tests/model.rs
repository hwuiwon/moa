use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::types::experiments::{ExperimentScorecard, ScorecardEffect, ScorecardRequirement};
use moa_core::{
    types::action_policy::ActionRuleScope, types::channel::Attachment,
    types::execution_planning::PinnedExecutionTemplateRef, types::identifiers::ModelId,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentSimulatorConfig, ExperimentTarget,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentTrialStopReason, ExperimentVariant,
};
use serde_json::json;
use std::collections::BTreeSet;
use uuid::Uuid;

#[test]
fn agent_loop_target_round_trips_through_public_model_offline() {
    // Pins: agent-loop experiments preserve prompts, model choice, and attachments.
    let session_id = SessionId::new();
    let target = ExperimentTarget::AgentLoop {
        prompt: "Check whether the answer cites the provided source.".to_string(),
        agent: None,
        model: ModelId::new("gpt-5.1"),
        attachments: vec![Attachment {
            id: None,
            name: "source.md".to_string(),
            mime_type: Some("text/markdown".to_string()),
            sha256: None,
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
    assert_eq!(decoded.execution_run_uid, None);
    assert_eq!(decoded, record);
}

#[test]
fn execution_template_target_round_trips_through_public_model_offline() {
    // Pins: execution-template experiments preserve the exact revision, objective, input, and link.
    let execution_run_uid = Uuid::now_v7();
    let template = PinnedExecutionTemplateRef {
        skill_ref: "skill://damaged-food-order".to_string(),
        revision_uid: Uuid::now_v7(),
    };
    let target = ExperimentTarget::ExecutionTemplate {
        template: template.clone(),
        objective: "Resolve the damaged order without widening contact scope.".to_string(),
        input: json!({ "order_id": "order-123", "priority": "high" }),
        session_id: None,
        idempotency_key: Some("experiment-live-execution-template-123".to_string()),
    };
    let record = record_for_target(
        ExperimentTargetKind::ExecutionTemplate,
        target,
        None,
        Some(execution_run_uid),
    );

    let encoded = serde_json::to_value(&record).expect("execution-template record serializes");
    assert_eq!(encoded["target"]["kind"], "execution_template");
    assert_eq!(
        encoded["target"]["template"]["skill_ref"],
        template.skill_ref
    );
    assert_eq!(
        encoded["target"]["template"]["revision_uid"],
        template.revision_uid.to_string()
    );
    let decoded: ExperimentRunRecord =
        serde_json::from_value(encoded).expect("execution-template record deserializes");

    assert_eq!(decoded.target_kind, ExperimentTargetKind::ExecutionTemplate);
    assert_eq!(
        decoded.target.kind(),
        ExperimentTargetKind::ExecutionTemplate
    );
    assert_eq!(decoded.session_id, None);
    assert_eq!(decoded.execution_run_uid, Some(execution_run_uid));
    assert_eq!(
        decoded.idempotency_key.as_deref(),
        Some("experiment-live-execution-template-123")
    );
    assert_eq!(decoded, record);
}

#[test]
fn scorecard_round_trips_evaluator_linkage_without_output_metadata_offline() {
    // Pins: scorecards identify evaluators; the registry owns their output name and type.
    let scorecard = ExperimentScorecard::new(vec![
        ScorecardRequirement {
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Blocking,
        },
        ScorecardRequirement {
            evaluator_id: "scenario_quality".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Informational,
        },
    ])
    .expect("scorecard is structurally valid");

    let encoded = serde_json::to_value(&scorecard).expect("scorecard serializes");
    for requirement in encoded["requirements"]
        .as_array()
        .expect("scorecard requirements serialize as an array")
    {
        assert!(requirement.get("score_name").is_none());
        assert!(requirement.get("value_type").is_none());
    }
    let decoded: ExperimentScorecard =
        serde_json::from_value(encoded).expect("scorecard deserializes");

    assert_eq!(
        decoded
            .requirements()
            .iter()
            .map(|requirement| requirement.evaluator_id.as_str())
            .collect::<Vec<_>>(),
        ["target_completed", "scenario_quality"]
    );
    assert_eq!(decoded.blocking_requirements().count(), 1);
    assert_eq!(decoded, scorecard);
}

#[test]
fn storage_enum_conversions_reject_unknown_database_values_offline() {
    // Pins: storage conversion helpers accept only the durable database vocabulary.
    assert_eq!(ExperimentRunStatus::Accepted.as_str(), "accepted");
    assert_eq!(ExperimentRunStatus::from_db("queued"), None);
    assert_eq!(ExperimentTargetKind::AgentLoop.as_str(), "agent_loop");
    assert_eq!(
        ExperimentTargetKind::from_db("execution_template"),
        Some(ExperimentTargetKind::ExecutionTemplate)
    );
    assert_eq!(ExperimentTargetKind::from_db("dataset"), None);
    assert_eq!(ExperimentTrialStatus::Dispatched.as_str(), "dispatched");
    assert_eq!(
        ExperimentTrialStatus::from_db("dispatched"),
        Some(ExperimentTrialStatus::Dispatched)
    );
    assert_eq!(ExperimentTrialStatus::Running.as_str(), "running");
    assert_eq!(ExperimentTrialStatus::from_db("queued"), None);
    assert_eq!(ExperimentTrialStopReason::MaxTurns.as_str(), "max_turns");
    assert_eq!(ExperimentTrialStopReason::from_db("timeout"), None);
}

#[test]
fn action_policy_migration_allows_current_trial_stop_reasons_offline() {
    // Pins: the forward action-policy migration must accept every stop reason the Rust store persists.
    const MIGRATION: &str = include_str!(
        "../../moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql"
    );
    let migration_values = trial_stop_reason_values_from_migration(MIGRATION);
    let model_values = current_trial_stop_reason_values();

    assert_eq!(
        migration_values, model_values,
        "V000302 experiment_trial_stop_reason_check must match ExperimentTrialStopReason::as_str"
    );
}

#[test]
fn trial_record_round_trips_through_public_model_offline() {
    // Pins: trial records preserve simulator config, artifact pins, links, and stop reason.
    let now = moa_test_support::fixtures::pg_now();
    let session_id = SessionId::new();
    let execution_run_uid = Uuid::now_v7();
    let trial = ExperimentTrialRecord {
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::from(Uuid::now_v7()),
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
        execution_run_uid: Some(execution_run_uid),
        score_run_id: Uuid::now_v7(),
        final_evidence_hash: Some(vec![7; 32]),
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
    assert_eq!(decoded.execution_run_uid, Some(execution_run_uid));
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
    execution_run_uid: Option<Uuid>,
) -> ExperimentRunRecord {
    let now = moa_test_support::fixtures::pg_now();

    ExperimentRunRecord {
        scope: ActionRuleScope::Tenant {
            tenant_id: TenantId::from(Uuid::now_v7()),
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
            execution_template: Some(PinnedExecutionTemplateRef {
                skill_ref: "skill://damaged-food-order".to_string(),
                revision_uid: Uuid::now_v7(),
            }),
            metadata: json!({ "cohort": "offline" }),
        },
        scorecard: ExperimentScorecard::new(vec![ScorecardRequirement {
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Blocking,
        }])
        .expect("fixture scorecard is valid"),
        score_run_id: Uuid::now_v7(),
        session_id,
        execution_run_uid,
        artifact_revision_uids: vec![Uuid::now_v7()],
        idempotency_key: match target_kind {
            ExperimentTargetKind::AgentLoop => None,
            ExperimentTargetKind::ExecutionTemplate => {
                Some("experiment-live-execution-template-123".to_string())
            }
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

fn current_trial_stop_reason_values() -> BTreeSet<&'static str> {
    [
        ExperimentTrialStopReason::Success,
        ExperimentTrialStopReason::Failure,
        ExperimentTrialStopReason::MaxTurns,
        ExperimentTrialStopReason::BudgetCap,
        ExperimentTrialStopReason::SimulatorDone,
        ExperimentTrialStopReason::TargetTerminal,
        ExperimentTrialStopReason::Error,
        ExperimentTrialStopReason::Cancelled,
    ]
    .into_iter()
    .map(ExperimentTrialStopReason::as_str)
    .collect()
}

fn trial_stop_reason_values_from_migration(sql: &'static str) -> BTreeSet<&'static str> {
    let constraint = sql
        .split("ADD CONSTRAINT experiment_trial_stop_reason_check")
        .nth(1)
        .expect("V000302 should recreate experiment_trial_stop_reason_check");
    let values = constraint
        .split("stop_reason IN (")
        .nth(1)
        .expect("stop reason constraint should use IN list")
        .split(')')
        .next()
        .expect("stop reason IN list should be closed");

    values
        .lines()
        .filter_map(|line| {
            let value = line.trim().trim_end_matches(',');
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .collect()
}
