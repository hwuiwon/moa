//! DB-backed coverage for agent revision simulation comparison.

use anyhow::Result;
use chrono::Utc;
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{AgentRevisionSimulationCompareRequest, AgentRevisionSimulationVariant};
use moa_core::{ActionRuleScope, ModelId, TenantId};
use moa_db::ScopedConn;
use moa_experiments::model::{
    ExperimentScorecard, ExperimentSimulatorConfig, ExperimentTarget, ExperimentTrialStatus,
    ExperimentTrialStopReason, ExperimentVariant, NewExperimentRun as NewExperiment,
    NewExperimentTrial,
};
use moa_experiments::store::ExperimentStore;
use moa_memory_types::ScopeContext;
use moa_orchestrator::services::experiments::compare_agent_revision_simulation_inner;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn compare_agent_revision_simulation_groups_trials_by_exact_revision_db_memory() -> Result<()>
{
    // Pins: simulation compare groups real trial rows by exact agent revision without deployment changes.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let experiment_store = ExperimentStore::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let base_revision_uid = Uuid::now_v7();
    let candidate_revision_uid = Uuid::now_v7();

    let run = experiment_store
        .insert_run(
            &scope,
            new_experiment(
                "agent revision comparison",
                plan_revision_uid,
                base_revision_uid,
                candidate_revision_uid,
            ),
        )
        .await?;
    let base_trial = experiment_store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "base-1", "base", plan_revision_uid),
        )
        .await?;
    let candidate_trial = experiment_store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "candidate-1", "candidate", plan_revision_uid),
        )
        .await?;
    experiment_store
        .update_trial_status(
            &scope,
            base_trial.trial_uid,
            ExperimentTrialStatus::Completed,
            Some(ExperimentTrialStopReason::Success),
            None,
            Some(Utc::now()),
        )
        .await?;
    experiment_store
        .update_trial_status(
            &scope,
            candidate_trial.trial_uid,
            ExperimentTrialStatus::Failed,
            Some(ExperimentTrialStopReason::Error),
            Some("candidate policy blocked tool".to_string()),
            Some(Utc::now()),
        )
        .await?;

    let compared = map_handler_error(
        compare_agent_revision_simulation_inner(
            pool,
            AgentRevisionSimulationCompareRequest {
                tenant_id,
                run_uid: run.run_uid,
                base_variant_key: "base".to_string(),
                candidate_variant_keys: Vec::new(),
            },
        )
        .await,
    )?;

    assert_eq!(compared.tenant_id, tenant_id);
    assert_eq!(compared.run_uid, run.run_uid);
    assert_eq!(compared.base_variant_key, "base");
    assert_eq!(compared.variants.len(), 2);
    let base = compared
        .variants
        .iter()
        .find(|variant| variant.variant_key == "base")
        .expect("base variant should be present");
    assert_eq!(base.revision_uid, base_revision_uid);
    assert_eq!(base.trial_count, 1);
    assert_eq!(base.completed_count, 1);
    assert_eq!(base.stop_reason_counts.get("success"), Some(&1));

    let candidate = compared
        .variants
        .iter()
        .find(|variant| variant.variant_key == "candidate")
        .expect("candidate variant should be present");
    assert_eq!(candidate.revision_uid, candidate_revision_uid);
    assert_eq!(candidate.trial_count, 1);
    assert_eq!(candidate.failed_count, 1);
    assert_eq!(candidate.stop_reason_counts.get("error"), Some(&1));
    assert_eq!(candidate.errors, vec!["candidate policy blocked tool"]);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

fn new_experiment(
    name: &str,
    plan_revision_uid: Uuid,
    base_revision_uid: Uuid,
    candidate_revision_uid: Uuid,
) -> NewExperiment {
    let variants = vec![
        AgentRevisionSimulationVariant {
            variant_key: "base".to_string(),
            revision_uid: base_revision_uid,
        },
        AgentRevisionSimulationVariant {
            variant_key: "candidate".to_string(),
            revision_uid: candidate_revision_uid,
        },
    ];
    NewExperiment {
        name: name.to_string(),
        target: ExperimentTarget::AgentLoop {
            prompt: "Measure this behavior.".to_string(),
            session_id: None,
            agent: None,
            model: ModelId::new("gpt-5.1"),
            attachments: Vec::new(),
        },
        variant: ExperimentVariant {
            name: "agent-revisions".to_string(),
            model: Some(ModelId::new("gpt-5.1")),
            artifact_revision_uids: vec![plan_revision_uid],
            skill_refs: Vec::new(),
            workflow_ref: None,
            metadata: json!({ "agent_revision_variants": variants }),
        },
        scorecard: ExperimentScorecard {
            score_names: vec!["task_success".to_string()],
            evaluator_metadata: json!({ "judge": "offline" }),
        },
        score_run_id: Uuid::now_v7(),
        session_id: None,
        workflow_run_uid: None,
        artifact_revision_uids: vec![plan_revision_uid],
        idempotency_key: None,
        created_by_identity: identity_json(),
    }
}

fn new_trial(
    run_uid: Uuid,
    trial_key: &str,
    variant_key: &str,
    plan_revision_uid: Uuid,
) -> NewExperimentTrial {
    NewExperimentTrial {
        run_uid,
        trial_key: trial_key.to_string(),
        target_kind: ExperimentTargetKind::AgentLoop,
        variant_key: variant_key.to_string(),
        plan_revision_uid,
        scenario_id: Some("scenario-a".to_string()),
        persona_id: Some("persona-a".to_string()),
        profile_id: None,
        data_bundle_ids: Vec::new(),
        artifact_revision_uids: vec![plan_revision_uid],
        simulator: ExperimentSimulatorConfig {
            model: ModelId::new("gpt-5.1-mini"),
            temperature: Some(0.0),
            max_turns: 4,
            token_budget: Some(2_000),
            metadata: json!({ "fixture": "agent_revision_simulation_db_memory" }),
        },
        target_model: Some(ModelId::new("gpt-5.1")),
        seed: Some(format!("{variant_key}-seed")),
        score_run_id: Uuid::now_v7(),
    }
}

async fn insert_artifact_revision(pool: &sqlx::PgPool, scope: &ActionRuleScope) -> Result<Uuid> {
    let ActionRuleScope::Tenant { tenant_id } = scope else {
        anyhow::bail!("simulation comparison test uses tenant scope");
    };
    let workspace_id = tenant_id.to_string();
    let artifact_uid = Uuid::now_v7();
    let revision_uid = Uuid::now_v7();
    let mut conn = ScopedConn::begin(pool, &ScopeContext::tenant(*tenant_id)).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, workspace_id, kind, name, description
        )
        VALUES ($1, $2, 'experiment_plan', $3, 'simulation fixture')
        "#,
    )
    .bind(artifact_uid)
    .bind(&workspace_id)
    .bind(format!("simulation-fixture-{artifact_uid}"))
    .execute(conn.as_mut())
    .await
    .map_err(|error| anyhow::anyhow!(error))?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_revision (
            revision_uid, artifact_uid, workspace_id, definition, canonical_hash,
            source_format, source_text, status, validation_report, version, published_at
        )
        VALUES ($1, $2, $3, $4, $5, 'json', $6, 'published', $7, 1, now())
        "#,
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(&workspace_id)
    .bind(json!({ "kind": "experiment_plan", "name": "simulation fixture" }))
    .bind(vec![2_u8; 32])
    .bind(br#"{"kind":"experiment_plan","name":"simulation fixture"}"#.to_vec())
    .bind(json!({}))
    .execute(conn.as_mut())
    .await
    .map_err(|error| anyhow::anyhow!(error))?;
    conn.commit().await?;
    Ok(revision_uid)
}

fn identity_json() -> serde_json::Value {
    let identity = Identity {
        identity_type: IdentityType::User,
        id: Uuid::now_v7(),
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    json!({
        "type": "user",
        "id": identity.id,
        "tenant_id": identity.tenant_id
    })
}

fn map_handler_error<T>(
    result: std::result::Result<T, restate_sdk::errors::HandlerError>,
) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{error:?}"))
}
