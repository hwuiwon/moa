//! DB-backed coverage for agent revision simulation comparison.

#[path = "support/mod.rs"]
mod support;

use anyhow::Result;
use moa_agents::AgentResolver;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_artifacts::validation::validate_for_status;
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::experiments::{ExperimentScorecard, ScorecardEffect, ScorecardRequirement};
use moa_core::types::memory::RlsContext;
use moa_core::{
    types::action_policy::ActionRuleScope, types::identifiers::ModelId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use moa_experiments::model::{
    ExperimentSimulatorConfig, ExperimentTarget, ExperimentTrialStatus, ExperimentTrialStopReason,
    ExperimentVariant, NewExperimentRun as NewExperiment, NewExperimentTrial,
};
use moa_experiments::store::ExperimentStore;
use moa_orchestrator::services::experiments::{
    compare_agent_revision_simulation_inner, list_plans_inner, run_agent_revision_simulation_inner,
};
use moa_wire::experiments::{
    AgentRevisionSimulationCompareRequest, AgentRevisionSimulationRunRequest,
    AgentRevisionSimulationVariant, ExperimentPlanListRequest,
};
use serde_json::{Value, json};
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
    let (plan_artifact_uid, plan_revision_uid) = insert_artifact_revision(&pool, &scope).await?;
    let base_revision_uid = Uuid::now_v7();
    let candidate_revision_uid = Uuid::now_v7();

    let run = experiment_store
        .insert_run(
            &scope,
            new_experiment(
                "agent revision comparison",
                plan_artifact_uid,
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
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?;
    experiment_store
        .update_trial_status(
            &scope,
            candidate_trial.trial_uid,
            ExperimentTrialStatus::Failed,
            Some(ExperimentTrialStopReason::Error),
            Some("candidate policy blocked tool".to_string()),
            Some(moa_test_support::fixtures::pg_now()),
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

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn list_plans_returns_only_visible_experiment_plan_artifacts_db_memory() -> Result<()> {
    // Pins: Behavior Lab plan catalog lists stored experiment_plan artifacts, not eval files or other artifacts.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let suffix = Uuid::now_v7();
    let published_name = format!("support-plan-{suffix}");
    let draft_name = format!("draft-plan-{suffix}");
    let skill_name = format!("non-plan-{suffix}");
    let other_tenant_name = format!("other-tenant-plan-{suffix}");

    let published =
        publish_artifact(&registry, &scope, experiment_plan_doc(&published_name)).await?;
    create_draft_artifact(&registry, &scope, experiment_plan_doc(&draft_name)).await?;
    publish_artifact(&registry, &scope, skill_doc(&skill_name)).await?;
    publish_artifact(
        &registry,
        &ActionRuleScope::Tenant {
            tenant_id: TenantId::new(),
        },
        experiment_plan_doc(&other_tenant_name),
    )
    .await?;

    let response = map_handler_error(
        list_plans_inner(
            pool,
            ExperimentPlanListRequest {
                tenant_id,
                scope: None,
                status: Some("published".to_string()),
            },
        )
        .await,
    )?;

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.plans.len(), 1);
    assert_eq!(response.plans[0].revision_uid, published.revision_uid);
    assert_eq!(response.plans[0].name, published_name);
    assert_eq!(response.plans[0].kind, "experiment_plan");
    assert_eq!(response.plans[0].status, "published");
    assert!(response.plans.iter().all(|plan| plan.name != draft_name
        && plan.name != skill_name
        && plan.name != other_tenant_name));

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn run_preserves_agent_revision_variants_for_workflow_db_memory() -> Result<()> {
    // Pins: agent-revision simulation admission preserves exact revision variants for workflow fanout.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let experiment_store = ExperimentStore::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let suffix = Uuid::now_v7();
    support::simulator_policy::seed_certified(
        &pool,
        tenant_id,
        Uuid::parse_str("10000000-0000-0000-0000-000000000001")?,
        "gpt-5.1-mini",
        "fixture-provider",
    )
    .await?;
    let plan = publish_artifact(
        &registry,
        &scope,
        experiment_plan_doc(&format!("agent-revision-plan-{suffix}")),
    )
    .await?;
    let base_agent = publish_artifact(
        &registry,
        &scope,
        agent_doc(&format!("base-agent-{suffix}"), "Base Agent", "file_read"),
    )
    .await?;
    let candidate_agent = publish_artifact(
        &registry,
        &scope,
        agent_doc(
            &format!("candidate-agent-{suffix}"),
            "Candidate Agent",
            "memory_search",
        ),
    )
    .await?;
    let base = AgentRevisionSimulationVariant {
        variant_key: "base".to_string(),
        revision_uid: base_agent.revision_uid,
    };
    let candidate = AgentRevisionSimulationVariant {
        variant_key: "candidate".to_string(),
        revision_uid: candidate_agent.revision_uid,
    };

    let accepted = map_handler_error(
        run_agent_revision_simulation_inner(
            pool,
            AgentRevisionSimulationRunRequest {
                tenant_id,
                name: format!("agent revision simulation {suffix}"),
                plan_revision_uid: plan.revision_uid,
                base: base.clone(),
                candidates: vec![candidate.clone()],
                idempotency_key: Some(format!("simulation-{suffix}")),
            },
            identity_for_tenant(tenant_id),
        )
        .await,
    )?;
    let workflow_request = accepted.workflow_request();
    let variants = vec![base, candidate];

    assert_eq!(workflow_request.tenant_id, tenant_id);
    assert_eq!(workflow_request.agent_revision_variants, variants);

    let run = experiment_store
        .load_run(&scope, workflow_request.run_uid)
        .await?
        .expect("accepted simulation run should be persisted");
    assert_eq!(
        run.variant.metadata["plan_revision_uid"],
        plan.revision_uid.to_string()
    );
    assert_eq!(
        run.variant.metadata["agent_revision_variants"],
        serde_json::to_value(&workflow_request.agent_revision_variants)?
    );
    let mut expected_revisions = vec![
        plan.revision_uid,
        base_agent.revision_uid,
        candidate_agent.revision_uid,
    ];
    expected_revisions.sort_unstable();
    let mut actual_revisions = run.artifact_revision_uids.clone();
    actual_revisions.sort_unstable();
    assert_eq!(actual_revisions, expected_revisions);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

fn new_experiment(
    name: &str,
    plan_artifact_uid: Uuid,
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
        plan_artifact_uid,
        expected_trials: 1,
        resource_envelope: fixture_experiment_envelope(),
        name: name.to_string(),
        target: ExperimentTarget::AgentLoop {
            prompt: "Measure this behavior.".to_string(),
            agent: None,
            model: ModelId::new("gpt-5.1"),
            attachments: Vec::new(),
        },
        variant: ExperimentVariant {
            name: "agent-revisions".to_string(),
            model: Some(ModelId::new("gpt-5.1")),
            artifact_revision_uids: vec![plan_revision_uid],
            skill_refs: Vec::new(),
            execution_template: None,
            metadata: json!({
                "agent_revision_variants": variants,
                "plan_revision_uid": plan_revision_uid,
            }),
        },
        scorecard: ExperimentScorecard::new(vec![ScorecardRequirement {
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            config: json!({}),
            effect: ScorecardEffect::Blocking,
        }])
        .expect("fixture scorecard is valid"),
        score_run_id: Uuid::now_v7(),
        session_id: None,
        execution_run_uid: None,
        artifact_revision_uids: vec![plan_revision_uid],
        idempotency_key: None,
        created_by_identity: identity_json(),
        simulator_policy: support::simulator_policy::fixture("gpt-5.1"),
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
            policy: support::simulator_policy::fixture("gpt-5.1-mini"),
            max_turns: 4,
            token_budget: Some(2_000),
        },
        target_model: Some(ModelId::new("gpt-5.1")),
        seed: Some(format!("{variant_key}-seed")),
        score_run_id: Uuid::now_v7(),
    }
}

async fn insert_artifact_revision(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
) -> Result<(Uuid, Uuid)> {
    let tenant_id = scope.tenant_id();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let user_id = scope.contact_id().map(|contact_id| contact_id.to_string());
    let artifact_uid = Uuid::now_v7();
    let revision_uid = Uuid::now_v7();
    let rls_context = match scope {
        ActionRuleScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => RlsContext::contact(*tenant_id, *contact_id),
    };
    let mut conn = ScopedConn::begin(pool, &rls_context).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, storage_partition_id, user_id, kind, name, description
        )
        VALUES ($1, $2, $3, 'experiment_plan', $4, 'simulation fixture')
        "#,
    )
    .bind(artifact_uid)
    .bind(&storage_partition_id)
    .bind(&user_id)
    .bind(format!("simulation-fixture-{artifact_uid}"))
    .execute(conn.as_mut())
    .await
    .map_err(|error| anyhow::anyhow!(error))?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_revision (
            revision_uid, artifact_uid, storage_partition_id, user_id, definition, canonical_hash,
            source_format, source_text, status, validation_report, version, published_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'json', $7, 'published', $8, 1, now())
        "#,
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(&storage_partition_id)
    .bind(&user_id)
    .bind(json!({ "kind": "experiment_plan", "name": "simulation fixture" }))
    .bind(vec![2_u8; 32])
    .bind(br#"{"kind":"experiment_plan","name":"simulation fixture"}"#.to_vec())
    .bind(json!({}))
    .execute(conn.as_mut())
    .await
    .map_err(|error| anyhow::anyhow!(error))?;
    conn.commit().await?;
    Ok((artifact_uid, revision_uid))
}

async fn create_draft_artifact(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    document: ArtifactDocument,
) -> Result<StoredArtifactRevision> {
    let source = document.to_json()?;
    Ok(registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "json",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?)
}

/// Makes a revision resolvable through its owning activation path.
async fn publish_artifact(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    document: ArtifactDocument,
) -> Result<StoredArtifactRevision> {
    let draft = create_draft_artifact(registry, scope, document.clone()).await?;
    if !moa_artifacts::release::ActivationTargetClass::is_release_gated(&draft.kind) {
        return Ok(registry
            .publish_unserved_revision(
                scope,
                draft.revision_uid,
                &validate_for_status(&document, ArtifactStatus::Published),
            )
            .await?);
    }
    let release_scope = moa_artifacts::release::TenantScope::from_action_rule_scope(scope)?;
    let target = if draft.kind == moa_artifacts::document::ArtifactKind::Agent {
        let installation_uid = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO moa.agent_installation (
                installation_uid, storage_partition_id, artifact_uid, definition_ref, display_name,
                status, current_revision_uid, serving_pointer_version
            )
            VALUES ($1, $2, $3, $4, $5, 'inactive', NULL, 0)
            "#,
        )
        .bind(installation_uid)
        .bind(release_scope.storage_partition_id().to_string())
        .bind(draft.artifact_uid)
        .bind(format!("agent://{}", draft.name))
        .bind(&draft.name)
        .execute(registry.pool())
        .await?;
        moa_artifacts::release::ActivationTarget::AgentDeployment {
            artifact_uid: draft.artifact_uid,
            installation_uid,
        }
    } else {
        moa_artifacts::release::ActivationTarget::for_kind(&draft.kind, draft.artifact_uid, None)?
    };
    if draft.kind == moa_artifacts::document::ArtifactKind::Agent {
        let lock = AgentResolver::new(registry.pool().clone())
            .resolve_release_candidate(scope, draft.revision_uid)
            .await?
            .revision_lock;
        moa_artifacts::test_fixtures::activate_agent_revision(
            registry.pool(),
            release_scope,
            target,
            draft.revision_uid,
            lock,
        )
        .await?;
    } else {
        moa_artifacts::test_fixtures::activate_revision(
            registry.pool(),
            release_scope,
            target,
            draft.revision_uid,
        )
        .await?;
    }
    registry
        .load_revision(scope, draft.revision_uid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("activated revision vanished"))
}

fn experiment_plan_doc(name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "experiment_plan",
        "metadata": {
            "name": name,
            "description": "Support behavior-lab plan",
            "tags": ["behavior-lab"]
        },
        "definition": {
            "type": "experiment_plan",
            "spec": {
                "simulation": {
                    "scenarios": [{
                        "id": "support-followup",
                        "initial_situation": "The user asks for help with a delayed order.",
                        "goals": ["Get a concrete next step."],
                        "success_criteria": ["The target gives a concrete next step."],
                        "max_turns": 3
                    }],
                    "personas": [{
                        "id": "careful-customer",
                        "voice": "Concise and specific.",
                        "goals": ["Resolve the order issue."],
                        "stop_behavior": "Stop after a concrete next step."
                    }],
                    "profiles": [{
                        "id": "standard-account",
                        "facts": { "account_tier": "standard" }
                    }]
                },
                "target_variants": [{
                    "key": "agent-loop",
                    "kind": "agent_loop",
                    "config": { "prompt": "Start the support simulation." }
                }],
                "simulator_policy": {
                    "policy_uid": "10000000-0000-0000-0000-000000000001",
                    "revision": 1
                },
                "target_model": "gpt-5.1",
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
    }))
    .expect("experiment plan artifact fixture is valid")
}

fn skill_doc(name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": name,
            "description": "Non-plan artifact fixture"
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "inputs": { "type": "object" },
                "outputs": { "type": "object" }
            }
        }
    }))
    .expect("skill artifact fixture is valid")
}

fn agent_doc(name: &str, display_name: &str, allowed_tool: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": {
            "name": name,
            "description": "Agent revision fixture"
        },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": display_name,
                "purpose": {
                    "summary": "Triage support requests.",
                    "default_task": "Classify support requests.",
                    "expected_outputs": ["classification", "next action"]
                },
                "instruction_policy": {
                    "system_prompt": format!("You are {display_name}.")
                },
                "tool_policy": {
                    "mode": "allowlist",
                    "tools": [allowed_tool]
                },
                "metadata": Value::Null
            }
        }
    }))
    .expect("agent artifact fixture is valid")
}

fn identity_json() -> serde_json::Value {
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    json!({
        "type": "operator",
        "id": identity.id,
        "tenant_id": identity.tenant_id
    })
}

fn identity_for_tenant(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn map_handler_error<T>(
    result: std::result::Result<T, restate_sdk::errors::HandlerError>,
) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{error:?}"))
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
        moa_test_support::fixtures::pg_now() + chrono::Duration::hours(24),
    )
}
