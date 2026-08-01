mod support;

use chrono::Utc;
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::experiments::{
    ExperimentCancelSignal, ExperimentScorecard, ScorecardEffect, ScorecardRequirement,
    ScorecardSupportStatus, ScorecardValueType,
};
use moa_core::types::memory::RlsContext;
use moa_core::types::resource::ResourceAmounts;
use moa_core::{
    error::{MoaError, Result},
    types::action_policy::ActionRuleScope,
    types::contact::ContactId,
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::identifiers::UserId,
};
use moa_db::ScopedConn;
use moa_experiments::{
    eligibility::ScorecardEligibility,
    model::{
        ExperimentResourceAdmission, ExperimentResourceComponent, ExperimentResourceEnvelope,
        ExperimentResourceReservationRequest, ExperimentResourceUsage, ExperimentRunStatus,
        ExperimentSimulatorConfig, ExperimentTarget, ExperimentTrialStatus,
        ExperimentTrialStopReason, ExperimentVariant, NewExperimentRun as NewExperiment,
        NewExperimentTrial,
    },
    plan::admission::DEFAULT_MAX_ARTIFACT_ACTIVE_TRIALS,
    scores::{
        ExperimentRunCompareRef, ExperimentRunScoreRef, ScenarioScoreDeltaRow, TrialScoreSummary,
        VariantScoreDeltaRow, compare_experiment_score_breakdown_for_tenant,
        experiment_score_breakdown_for_tenant,
    },
    store::ExperimentStore,
};
use moa_scoring::ScoreSummaryRow;
use moa_wire::experiments::ExperimentCancelRequest;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static DB_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn cancel_signal(tenant_id: TenantId, reason: &str) -> ExperimentCancelSignal {
    ExperimentCancelSignal {
        reason: reason.to_string(),
        identity: Identity {
            identity_type: IdentityType::Service,
            id: Uuid::now_v7(),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        },
    }
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn tenant_scoped_run_insert_load_round_trip_db() -> Result<()> {
    // Pins: tenant-scoped experiment metadata persists and loads through the scoped store.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("experiment-round-trip");
    let artifact_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let mut new_run = new_experiment(
        "round-trip",
        Some("round-trip-key"),
        vec![artifact_revision_uid],
    );
    let simulator_policy = support::simulator_policy("gpt-5.1-mini");
    new_run.simulator_policy = Some(simulator_policy.clone());

    let inserted = store.insert_run(&scope, new_run).await?;
    let loaded = store
        .load_run(&scope, inserted.run_uid)
        .await?
        .expect("inserted experiment should load in same workspace");

    assert_eq!(loaded.scope, scope);
    assert_eq!(loaded.status, ExperimentRunStatus::Accepted);
    assert_eq!(loaded.name, "round-trip");
    assert_eq!(
        loaded
            .scorecard
            .requirements()
            .iter()
            .map(|requirement| requirement.evaluator_id.as_str())
            .collect::<Vec<_>>(),
        ["target_completed"]
    );
    assert_eq!(loaded.artifact_revision_uids, [artifact_revision_uid]);
    assert_eq!(loaded.idempotency_key.as_deref(), Some("round-trip-key"));
    assert_eq!(loaded.created_by_identity["id"], "experimenter");
    assert_eq!(loaded.simulator_policy, Some(simulator_policy));
    assert_eq!(loaded.created_at, inserted.created_at);
    assert_score_run_exists(test_db.store().pool(), &scope, loaded.score_run_id).await?;
    assert_artifact_revision_links(
        test_db.store().pool(),
        &scope,
        loaded.run_uid,
        &[artifact_revision_uid],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn missing_artifact_revision_rejects_experiment_insert_db() -> Result<()> {
    // Pins: experiment artifact revision links are backed by enforced artifact_revision FKs.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("experiment-missing-artifact-revision");
    let missing_revision_uid = Uuid::now_v7();

    let error = store
        .insert_run(
            &scope,
            new_experiment("missing-revision", None, vec![missing_revision_uid]),
        )
        .await
        .expect_err("missing artifact revision should reject experiment insert");

    assert!(
        error.to_string().contains("artifact revision"),
        "expected link-table FK failure, got {error}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn workspace_a_cannot_load_workspace_b_run_db() -> Result<()> {
    // Pins: exact scoped loads cannot cross workspace boundaries.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("experiment-a");
    let workspace_b = tenant_scope("experiment-b");

    let inserted = store
        .insert_run(
            &workspace_b,
            new_experiment("workspace-b", None, Vec::new()),
        )
        .await?;

    let visible_to_a = store.load_run(&workspace_a, inserted.run_uid).await?;
    let visible_to_b = store.load_run(&workspace_b, inserted.run_uid).await?;

    assert_eq!(visible_to_a, None);
    assert_eq!(
        visible_to_b
            .expect("workspace b should load its run")
            .run_uid,
        inserted.run_uid
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn contact_scoped_run_insert_load_round_trip_db() -> Result<()> {
    // Pins: contact-scoped experiment metadata persists with a personal scope and does not leak to the tenant or peer contacts.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId(Uuid::now_v7());
    let peer_contact_id = ContactId(Uuid::now_v7());
    let personal_scope = contact_scope(tenant_id, contact_id);
    let peer_scope = contact_scope(tenant_id, peer_contact_id);
    let tenant_scope = ActionRuleScope::Tenant { tenant_id };

    let inserted = store
        .insert_run(
            &personal_scope,
            new_experiment("contact-round-trip", Some("contact-key"), Vec::new()),
        )
        .await?;
    let loaded = store
        .load_run(&personal_scope, inserted.run_uid)
        .await?
        .expect("inserted contact experiment should load in the same contact scope");

    assert_eq!(loaded.scope, personal_scope);
    assert_eq!(loaded.name, "contact-round-trip");
    assert_eq!(loaded.idempotency_key.as_deref(), Some("contact-key"));
    assert_score_run_exists(test_db.store().pool(), &personal_scope, loaded.score_run_id).await?;
    assert!(
        store
            .load_run(&tenant_scope, inserted.run_uid)
            .await?
            .is_none(),
        "tenant-scope reads must not collapse contact experiments into tenant rows"
    );
    assert!(
        store
            .load_run(&peer_scope, inserted.run_uid)
            .await?
            .is_none(),
        "peer contacts must not load another contact's experiment row"
    );
    assert_eq!(
        store
            .list_runs(&personal_scope, Some(ExperimentRunStatus::Accepted), 10)
            .await?
            .into_iter()
            .filter(|run| run.run_uid == inserted.run_uid)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn idempotency_key_deduplicates_within_scope_not_across_workspaces_db() -> Result<()> {
    // Pins: scoped idempotency returns the existing row only inside the same workspace.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("experiment-idem-a");
    let workspace_b = tenant_scope("experiment-idem-b");

    let first = store
        .insert_run(
            &workspace_a,
            new_experiment("first", Some("shared-key"), Vec::new()),
        )
        .await?;
    let duplicate = store
        .insert_run(
            &workspace_a,
            new_experiment("second", Some("shared-key"), Vec::new()),
        )
        .await?;
    let other_workspace = store
        .insert_run(
            &workspace_b,
            new_experiment("third", Some("shared-key"), Vec::new()),
        )
        .await?;

    assert_eq!(duplicate.run_uid, first.run_uid);
    assert_eq!(duplicate.name, "first");
    assert_ne!(other_workspace.run_uid, first.run_uid);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn idempotency_duplicate_does_not_add_artifact_revision_links_db() -> Result<()> {
    // Pins: scoped idempotency returns before duplicate requests mutate revision links or score runs.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("experiment-idem-links");
    let first_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let duplicate_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let first = store
        .insert_run(
            &scope,
            new_experiment("first", Some("shared-link-key"), vec![first_revision_uid]),
        )
        .await?;
    let duplicate_request = new_experiment(
        "duplicate",
        Some("shared-link-key"),
        vec![duplicate_revision_uid],
    );
    let duplicate_score_run_id = duplicate_request.score_run_id;

    let duplicate = store.insert_run(&scope, duplicate_request).await?;

    assert_eq!(duplicate.run_uid, first.run_uid);
    assert_eq!(duplicate.artifact_revision_uids, [first_revision_uid]);
    assert_artifact_revision_links(
        test_db.store().pool(),
        &scope,
        first.run_uid,
        &[first_revision_uid],
    )
    .await?;
    assert_score_run_absent(test_db.store().pool(), duplicate_score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn score_run_id_from_another_tenant_rejects_insert_db() -> Result<()> {
    // Pins: experiment runs cannot attach to an existing score-run parent from another tenant.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("experiment-score-run-a");
    let workspace_b = tenant_scope("experiment-score-run-b");
    let score_run_id = Uuid::now_v7();
    insert_score_run(test_db.store().pool(), &workspace_b, score_run_id).await?;
    let mut new_run = new_experiment("cross-score-run", None, Vec::new());
    new_run.score_run_id = score_run_id;

    let error = store
        .insert_run(&workspace_a, new_run)
        .await
        .expect_err("cross-tenant score run should reject experiment insert");

    assert!(
        error.to_string().contains("score_run"),
        "expected score-run scope error, got {error}"
    );
    assert_scoped_experiment_count_for_score_run(
        test_db.store().pool(),
        &workspace_a,
        score_run_id,
        0,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn cross_tenant_artifact_revision_rejects_insert_db() -> Result<()> {
    // Pins: experiment artifact links must target revisions visible from the requested scope.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("experiment-artifact-a");
    let workspace_b = tenant_scope("experiment-artifact-b");
    let workspace_b_revision_uid =
        insert_artifact_revision(test_db.store().pool(), &workspace_b).await?;
    let new_run = new_experiment(
        "cross-tenant-revision",
        None,
        vec![workspace_b_revision_uid],
    );
    let score_run_id = new_run.score_run_id;

    let error = store
        .insert_run(&workspace_a, new_run)
        .await
        .expect_err("cross-tenant artifact revision should reject experiment insert");

    assert!(
        error.to_string().contains("artifact revision"),
        "expected artifact revision visibility error, got {error}"
    );
    assert_score_run_absent(test_db.store().pool(), score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn execution_run_and_session_links_persist_db() -> Result<()> {
    // Pins: session and execution-run links persist on experiment records.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let experiment_store = ExperimentStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let user_id = UserId::new(format!("user-{}", Uuid::now_v7()));
    let scope = ActionRuleScope::Tenant { tenant_id };
    let session_id =
        insert_session_for_experiment_fk(test_db.store().pool(), &storage_partition_id, &user_id)
            .await?;
    let inserted = experiment_store
        .insert_run(&scope, new_experiment("links", None, Vec::new()))
        .await?;
    let execution_run_uid = insert_execution_run(
        test_db.store().pool(),
        &scope,
        session_id,
        inserted.run_uid,
        inserted.score_run_id,
        None,
    )
    .await?;

    experiment_store
        .attach_session(&scope, inserted.run_uid, session_id)
        .await?
        .expect("session link update should return the run");
    let linked = experiment_store
        .attach_execution_run(&scope, inserted.run_uid, execution_run_uid)
        .await?
        .expect("execution-run link update should return the run");

    assert_eq!(linked.session_id, Some(session_id));
    assert_eq!(linked.execution_run_uid, Some(execution_run_uid));
    assert_score_run_exists(test_db.store().pool(), &scope, linked.score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn terminal_run_status_cannot_be_overwritten_db() -> Result<()> {
    // Pins: terminal runs keep their final status, error, and completed_at across late updates.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("run-terminal-guard");
    let run = store
        .insert_run(
            &scope,
            new_experiment("run-terminal-guard", None, Vec::new()),
        )
        .await?;
    let completed_at = moa_test_support::fixtures::pg_now();
    let completed = store
        .update_run_status(
            &scope,
            run.run_uid,
            ExperimentRunStatus::Completed,
            None,
            Some(completed_at),
        )
        .await?
        .expect("terminal update should return run");

    let cancelled = store
        .update_run_status(
            &scope,
            run.run_uid,
            ExperimentRunStatus::Cancelled,
            Some("late cancel".to_string()),
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?;
    let loaded = store
        .load_run(&scope, run.run_uid)
        .await?
        .expect("run should still load");

    assert_eq!(cancelled, None);
    assert_eq!(loaded.status, ExperimentRunStatus::Completed);
    assert_eq!(loaded.error, completed.error);
    assert_eq!(loaded.completed_at, completed.completed_at);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn trial_insert_load_list_round_trip_db() -> Result<()> {
    // Pins: trial rows persist through scoped store paths without introducing a trial event table.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("trial-round-trip");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let variant_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let run = store
        .insert_run(&scope, new_experiment("trial-parent", None, Vec::new()))
        .await?;
    let mut new_trial = new_trial(
        run.run_uid,
        "scenario-a/persona-a/baseline",
        plan_revision_uid,
        vec![variant_revision_uid],
    );
    new_trial.persona_id = Some("careful-shopper".to_string());
    new_trial.scenario_id = Some("checkout-delay".to_string());
    let trial = store.insert_trial(&scope, new_trial).await?;

    let loaded = store
        .load_trial(&scope, trial.trial_uid)
        .await?
        .expect("inserted trial should load in same workspace");
    let listed = store
        .list_trials(
            &scope,
            run.run_uid,
            Some(ExperimentTrialStatus::Accepted),
            10,
        )
        .await?;

    assert_eq!(loaded.scope, scope);
    assert_eq!(loaded.run_uid, run.run_uid);
    assert_eq!(loaded.trial_key, "scenario-a/persona-a/baseline");
    assert_eq!(loaded.status, ExperimentTrialStatus::Accepted);
    assert_eq!(loaded.persona_id.as_deref(), Some("careful-shopper"));
    assert_eq!(loaded.scenario_id.as_deref(), Some("checkout-delay"));
    assert_eq!(loaded.plan_revision_uid, plan_revision_uid);
    assert_eq!(loaded.artifact_revision_uids, [variant_revision_uid]);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].trial_uid, trial.trial_uid);
    assert_score_run_exists_with_source(
        test_db.store().pool(),
        &scope,
        loaded.score_run_id,
        "experiment_trial",
    )
    .await?;
    assert_no_experiment_trial_event_table(test_db.store().pool()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn trial_key_deduplicates_within_run_db() -> Result<()> {
    // Pins: run-scoped trial keys are idempotent and duplicate requests do not create score parents.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("trial-idem");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("trial-idem-parent", None, Vec::new()),
        )
        .await?;
    let first = store
        .insert_trial(
            &scope,
            new_trial(
                run.run_uid,
                "shared-trial-key",
                plan_revision_uid,
                Vec::new(),
            ),
        )
        .await?;
    let duplicate_request = new_trial(
        run.run_uid,
        "shared-trial-key",
        plan_revision_uid,
        Vec::new(),
    );
    let duplicate_score_run_id = duplicate_request.score_run_id;

    let duplicate = store.insert_trial(&scope, duplicate_request).await?;

    assert_eq!(duplicate.trial_uid, first.trial_uid);
    assert_eq!(duplicate.score_run_id, first.score_run_id);
    assert_score_run_absent(test_db.store().pool(), duplicate_score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn experiment_score_queries_preserve_breakdowns_comparisons_and_tenant_scope_db() -> Result<()>
{
    // Pins: experiment-owned trial joins preserve exact grouping, comparison order, and tenant isolation.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("experiment-score-query");
    let other_scope = tenant_scope("experiment-score-query-other");
    let tenant_id = scope_tenant_id(&scope);
    let other_tenant_id = scope_tenant_id(&other_scope);
    let plan_revision_uid = insert_artifact_revision(pool, &scope).await?;
    let base_run = store
        .insert_run(&scope, new_experiment("score-base", None, Vec::new()))
        .await?;
    let new_run = store
        .insert_run(&scope, new_experiment("score-new", None, Vec::new()))
        .await?;

    insert_scored_trial(
        &store,
        pool,
        &scope,
        base_run.run_uid,
        plan_revision_uid,
        "base-baseline",
        "baseline",
        "scenario-a",
        0.25,
        None,
    )
    .await?;
    insert_scored_trial(
        &store,
        pool,
        &scope,
        base_run.run_uid,
        plan_revision_uid,
        "base-candidate",
        "candidate",
        "scenario-b",
        0.5,
        None,
    )
    .await?;
    let new_baseline = insert_scored_trial(
        &store,
        pool,
        &scope,
        new_run.run_uid,
        plan_revision_uid,
        "new-baseline",
        "baseline",
        "scenario-a",
        0.5,
        Some(true),
    )
    .await?;
    let new_candidate = insert_scored_trial(
        &store,
        pool,
        &scope,
        new_run.run_uid,
        plan_revision_uid,
        "new-candidate",
        "candidate",
        "scenario-b",
        1.0,
        Some(false),
    )
    .await?;

    // A score sharing the tenant-A trial score-run ID but owned by tenant B
    // must not become visible through the experiment join.
    insert_score(
        pool,
        &other_scope,
        new_baseline.score_run_id,
        "quality",
        "numeric",
        Some(10.0),
        None,
    )
    .await?;

    let breakdown = experiment_score_breakdown_for_tenant(
        pool,
        ExperimentRunScoreRef {
            tenant_id,
            run_uid: new_run.run_uid,
        },
    )
    .await
    .expect("read tenant experiment score breakdown");
    assert_eq!(
        breakdown.trial_rollup_rows,
        vec![
            score_summary("quality", "numeric", 2, 0.75),
            score_summary("success", "boolean", 2, 0.5),
        ]
    );
    assert_eq!(
        breakdown.trials,
        vec![
            TrialScoreSummary {
                trial_uid: new_baseline.trial_uid,
                trial_key: "new-baseline".to_string(),
                score_run_id: new_baseline.score_run_id,
                variant_key: "baseline".to_string(),
                scenario_id: Some("scenario-a".to_string()),
                rows: vec![
                    score_summary("quality", "numeric", 1, 0.5),
                    score_summary("success", "boolean", 1, 1.0),
                ],
            },
            TrialScoreSummary {
                trial_uid: new_candidate.trial_uid,
                trial_key: "new-candidate".to_string(),
                score_run_id: new_candidate.score_run_id,
                variant_key: "candidate".to_string(),
                scenario_id: Some("scenario-b".to_string()),
                rows: vec![
                    score_summary("quality", "numeric", 1, 1.0),
                    score_summary("success", "boolean", 1, 0.0),
                ],
            },
        ]
    );
    assert_eq!(breakdown.scenarios.len(), 2);
    assert_eq!(
        breakdown.scenarios[0].scenario_id.as_deref(),
        Some("scenario-a")
    );
    assert_eq!(
        breakdown.scenarios[1].scenario_id.as_deref(),
        Some("scenario-b")
    );

    let comparison = compare_experiment_score_breakdown_for_tenant(
        pool,
        ExperimentRunCompareRef {
            tenant_id,
            base_run_uid: base_run.run_uid,
            new_run_uid: new_run.run_uid,
        },
    )
    .await
    .expect("compare tenant experiment scores");
    assert_eq!(
        comparison.scenario_deltas,
        vec![
            ScenarioScoreDeltaRow {
                scenario_id: Some("scenario-a".to_string()),
                name: "quality".to_string(),
                base_mean: Some(0.25),
                new_mean: Some(0.5),
                delta: Some(0.25),
            },
            ScenarioScoreDeltaRow {
                scenario_id: Some("scenario-b".to_string()),
                name: "quality".to_string(),
                base_mean: Some(0.5),
                new_mean: Some(1.0),
                delta: Some(0.5),
            },
        ]
    );
    assert_eq!(
        comparison.variant_deltas,
        vec![
            VariantScoreDeltaRow {
                variant_key: "baseline".to_string(),
                name: "quality".to_string(),
                base_mean: Some(0.25),
                new_mean: Some(0.5),
                delta: Some(0.25),
            },
            VariantScoreDeltaRow {
                variant_key: "candidate".to_string(),
                name: "quality".to_string(),
                base_mean: Some(0.5),
                new_mean: Some(1.0),
                delta: Some(0.5),
            },
        ]
    );

    let invisible = experiment_score_breakdown_for_tenant(
        pool,
        ExperimentRunScoreRef {
            tenant_id: other_tenant_id,
            run_uid: new_run.run_uid,
        },
    )
    .await
    .expect("read other tenant experiment score breakdown");
    assert!(invisible.trial_rollup_rows.is_empty());
    assert!(invisible.trials.is_empty());
    assert!(invisible.scenarios.is_empty());

    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn trial_rejects_cross_tenant_score_run_db() -> Result<()> {
    // Pins: trial-level score runs cannot be attached across tenant scope boundaries.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("trial-score-a");
    let workspace_b = tenant_scope("trial-score-b");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &workspace_a).await?;
    let run = store
        .insert_run(
            &workspace_a,
            new_experiment("trial-score-parent", None, Vec::new()),
        )
        .await?;
    let score_run_id = Uuid::now_v7();
    insert_score_run_with_source(
        test_db.store().pool(),
        &workspace_b,
        score_run_id,
        "experiment_trial",
    )
    .await?;
    let mut trial = new_trial(run.run_uid, "cross-score", plan_revision_uid, Vec::new());
    trial.score_run_id = score_run_id;

    let error = store
        .insert_trial(&workspace_a, trial)
        .await
        .expect_err("cross-tenant score run should reject trial insert");

    assert!(
        error.to_string().contains("score_run"),
        "expected score-run scope error, got {error}"
    );
    assert_scoped_trial_count_for_score_run(test_db.store().pool(), &workspace_a, score_run_id, 0)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn trial_rejects_cross_tenant_artifact_revision_db() -> Result<()> {
    // Pins: trial artifact pins must be visible from the requested experiment scope.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = tenant_scope("trial-artifact-a");
    let workspace_b = tenant_scope("trial-artifact-b");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &workspace_a).await?;
    let run = store
        .insert_run(
            &workspace_a,
            new_experiment("trial-artifact-parent", None, Vec::new()),
        )
        .await?;
    let workspace_b_revision_uid =
        insert_artifact_revision(test_db.store().pool(), &workspace_b).await?;
    let trial = new_trial(
        run.run_uid,
        "cross-artifact",
        plan_revision_uid,
        vec![workspace_b_revision_uid],
    );
    let score_run_id = trial.score_run_id;

    let error = store
        .insert_trial(&workspace_a, trial)
        .await
        .expect_err("cross-tenant artifact revision should reject trial insert");

    assert!(
        error.to_string().contains("artifact revision"),
        "expected artifact revision visibility error, got {error}"
    );
    assert_score_run_absent(test_db.store().pool(), score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn trial_links_trace_status_and_turns_persist_db() -> Result<()> {
    // Pins: trial session/execution/trace links, immutable final evidence,
    // turn counts, and terminal status persist.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let user_id = UserId::new(format!("user-{}", Uuid::now_v7()));
    let scope = ActionRuleScope::Tenant { tenant_id };
    let session_id =
        insert_session_for_experiment_fk(test_db.store().pool(), &storage_partition_id, &user_id)
            .await?;
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("trial-links-parent", None, Vec::new()),
        )
        .await?;
    let trial = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "trial-links", plan_revision_uid, Vec::new()),
        )
        .await?;
    let execution_run_uid = insert_execution_run(
        test_db.store().pool(),
        &scope,
        session_id,
        run.run_uid,
        trial.score_run_id,
        Some(trial.trial_uid),
    )
    .await?;

    store
        .attach_trial_session(&scope, trial.trial_uid, session_id)
        .await?
        .expect("session link update should return the trial");
    store
        .attach_trial_execution_run(&scope, trial.trial_uid, execution_run_uid)
        .await?
        .expect("execution-run link update should return the trial");
    store
        .attach_trial_trace(&scope, trial.trial_uid, "trace-trial-123".to_string())
        .await?
        .expect("trace link update should return the trial");
    let evidence_hash = vec![7_u8; 32];
    store
        .set_trial_final_evidence_hash(&scope, trial.trial_uid, &evidence_hash)
        .await?
        .expect("first final evidence hash write should return the trial");
    store
        .set_trial_final_evidence_hash(&scope, trial.trial_uid, &evidence_hash)
        .await?
        .expect("an identical replay should return the trial");
    assert!(
        store
            .set_trial_final_evidence_hash(&scope, trial.trial_uid, &[8_u8; 32])
            .await?
            .is_none(),
        "a replay must not replace finalized evidence with a different digest"
    );
    store
        .increment_trial_turn(&scope, trial.trial_uid)
        .await?
        .expect("first turn increment should return the trial");
    let incremented = store
        .increment_trial_turn(&scope, trial.trial_uid)
        .await?
        .expect("second turn increment should return the trial");
    let completed = store
        .update_trial_status(
            &scope,
            trial.trial_uid,
            ExperimentTrialStatus::Completed,
            Some(ExperimentTrialStopReason::Success),
            None,
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("status update should return the trial");

    assert_eq!(incremented.turn_count, 2);
    assert_eq!(completed.session_id, Some(session_id));
    assert_eq!(completed.execution_run_uid, Some(execution_run_uid));
    assert_eq!(completed.trace_id.as_deref(), Some("trace-trial-123"));
    assert_eq!(completed.final_evidence_hash, Some(evidence_hash));
    assert_eq!(completed.status, ExperimentTrialStatus::Completed);
    assert_eq!(
        completed.stop_reason,
        Some(ExperimentTrialStopReason::Success)
    );
    let reopened = store
        .update_trial_status(
            &scope,
            trial.trial_uid,
            ExperimentTrialStatus::Running,
            None,
            None,
            None,
        )
        .await?;
    let loaded = store
        .load_trial(&scope, trial.trial_uid)
        .await?
        .expect("trial should still load");
    assert_eq!(reopened, None);
    assert_eq!(loaded.status, ExperimentTrialStatus::Completed);
    assert_eq!(loaded.stop_reason, Some(ExperimentTrialStopReason::Success));
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn cancel_active_trials_marks_remaining_work_without_mutating_terminal_trials_db()
-> Result<()> {
    // Pins: parent run cancellation marks pending/dispatched/running trials clearly but preserves partial results.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("trial-cancel-active");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("trial-cancel-active-parent", None, Vec::new()),
        )
        .await?;
    let accepted = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "accepted", plan_revision_uid, Vec::new()),
        )
        .await?;
    let running = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "running", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            running.trial_uid,
            ExperimentTrialStatus::Running,
            None,
            None,
            None,
        )
        .await?
        .expect("running status update should return trial");
    let dispatched = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "dispatched", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            dispatched.trial_uid,
            ExperimentTrialStatus::Dispatched,
            None,
            None,
            None,
        )
        .await?
        .expect("dispatched status update should return trial");
    let completed = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "completed", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            completed.trial_uid,
            ExperimentTrialStatus::Completed,
            Some(ExperimentTrialStopReason::Success),
            None,
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("completed status update should return trial");
    let failed = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "failed", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            failed.trial_uid,
            ExperimentTrialStatus::Failed,
            Some(ExperimentTrialStopReason::Error),
            Some("target failed".to_string()),
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("failed status update should return trial");

    let cancelled = store
        .cancel_active_trials(&scope, run.run_uid, "operator cancelled".to_string())
        .await?;
    let trials = store.list_trials(&scope, run.run_uid, None, 10).await?;

    assert_eq!(cancelled.len(), 3);
    assert_trial_status(
        &trials,
        accepted.trial_uid,
        ExperimentTrialStatus::Cancelled,
        Some(ExperimentTrialStopReason::Cancelled),
    );
    assert_trial_status(
        &trials,
        running.trial_uid,
        ExperimentTrialStatus::Cancelled,
        Some(ExperimentTrialStopReason::Cancelled),
    );
    assert_trial_status(
        &trials,
        dispatched.trial_uid,
        ExperimentTrialStatus::Cancelled,
        Some(ExperimentTrialStopReason::Cancelled),
    );
    assert_trial_status(
        &trials,
        completed.trial_uid,
        ExperimentTrialStatus::Completed,
        Some(ExperimentTrialStopReason::Success),
    );
    assert_trial_status(
        &trials,
        failed.trial_uid,
        ExperimentTrialStatus::Failed,
        Some(ExperimentTrialStopReason::Error),
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn cancel_run_and_active_trials_reconciles_behind_already_cancelled_parent_db() -> Result<()>
{
    // Pins (F19): the combined cancel reconciles stranded active trials even when the
    // parent run is already terminal-cancelled, atomically in one transaction, without
    // disturbing already-terminal trials or overwriting the original cancellation error.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("cancel-reconcile");
    let plan_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("cancel-reconcile-parent", None, Vec::new()),
        )
        .await?;
    let running = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "running", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            running.trial_uid,
            ExperimentTrialStatus::Running,
            None,
            None,
            None,
        )
        .await?
        .expect("running status update should return trial");
    let completed = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "completed", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            completed.trial_uid,
            ExperimentTrialStatus::Completed,
            Some(ExperimentTrialStopReason::Success),
            None,
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("completed status update should return trial");

    // Simulate a first cancel that updated the run projection but crashed before
    // reconciling trials: the parent is Cancelled while the running trial is stranded.
    store
        .update_run_status(
            &scope,
            run.run_uid,
            ExperimentRunStatus::Cancelled,
            Some("first attempt".to_string()),
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("run cancel should return run");

    let (reconciled_run, cancelled_trials) = store
        .cancel_run_and_active_trials(
            &scope,
            run.run_uid,
            cancel_signal(scope_tenant_id(&scope), "operator cancelled"),
        )
        .await?;

    let reconciled_run =
        reconciled_run.expect("already-cancelled run is still cancellable for reconciliation");
    assert_eq!(reconciled_run.status, ExperimentRunStatus::Cancelled);
    assert_eq!(
        reconciled_run.error.as_deref(),
        Some("first attempt"),
        "reconciliation preserves the original cancellation error"
    );
    assert_eq!(
        cancelled_trials.len(),
        1,
        "only the stranded running trial is reconciled"
    );
    let persisted_signal = store
        .load_run_cancel_signal(&scope, run.run_uid)
        .await?
        .expect("cancellation fence should carry the authorized caller");
    assert_eq!(persisted_signal.reason, "operator cancelled");
    assert_eq!(persisted_signal.identity.tenant_id, scope_tenant_id(&scope));
    assert_eq!(cancelled_trials[0].trial_uid, running.trial_uid);

    let trials = store.list_trials(&scope, run.run_uid, None, 10).await?;
    assert_trial_status(
        &trials,
        running.trial_uid,
        ExperimentTrialStatus::Cancelled,
        Some(ExperimentTrialStopReason::Cancelled),
    );
    assert_trial_status(
        &trials,
        completed.trial_uid,
        ExperimentTrialStatus::Completed,
        Some(ExperimentTrialStopReason::Success),
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn cancel_run_and_active_trials_does_not_override_completed_run_db() -> Result<()> {
    // Pins (F19): the combined cancel never flips a genuinely completed run to cancelled.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = tenant_scope("cancel-completed");
    let run = store
        .insert_run(
            &scope,
            new_experiment("cancel-completed-parent", None, Vec::new()),
        )
        .await?;
    store
        .update_run_status(
            &scope,
            run.run_uid,
            ExperimentRunStatus::Completed,
            None,
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("run completion should return run");

    let (reconciled_run, cancelled_trials) = store
        .cancel_run_and_active_trials(
            &scope,
            run.run_uid,
            cancel_signal(scope_tenant_id(&scope), "late cancel"),
        )
        .await?;

    assert!(
        reconciled_run.is_none(),
        "a completed run must not be transitioned to cancelled"
    );
    assert!(cancelled_trials.is_empty());
    let reloaded = store
        .load_run(&scope, run.run_uid)
        .await?
        .expect("run persists");
    assert_eq!(reloaded.status, ExperimentRunStatus::Completed);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn cancel_run_app_reconciles_active_trials_behind_terminal_cancelled_parent_db() -> Result<()>
{
    // Pins (F19): a retried Experiments/cancel behind an already-cancelled parent no
    // longer short-circuits on the terminal-parent early return; it still reconciles
    // stranded active trial rows and reports the retry idempotently.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("app-cancel-retry");
    let ActionRuleScope::Tenant { tenant_id } = scope else {
        unreachable!("tenant_scope builds a tenant scope");
    };
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("app-cancel-retry-parent", None, Vec::new()),
        )
        .await?;
    let running = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "running", plan_revision_uid, Vec::new()),
        )
        .await?;
    store
        .update_trial_status(
            &scope,
            running.trial_uid,
            ExperimentTrialStatus::Running,
            None,
            None,
            None,
        )
        .await?
        .expect("running status update should return trial");
    // Strand: the run projection is already cancelled but the trial is still active.
    store
        .update_run_status(
            &scope,
            run.run_uid,
            ExperimentRunStatus::Cancelled,
            Some("first attempt".to_string()),
            Some(moa_test_support::fixtures::pg_now()),
        )
        .await?
        .expect("run cancel should return run");

    let response = moa_experiments::app::cancel_run(
        pool.clone(),
        ExperimentCancelRequest {
            tenant_id,
            run_uid: run.run_uid,
            reason: Some("retry".to_string()),
        },
        cancel_signal(tenant_id, "retry").identity,
    )
    .await
    .expect("retried cancel behind a cancelled parent should succeed");

    assert!(
        !response.cancelled,
        "an idempotent retry behind an already-cancelled parent reports cancelled=false"
    );
    assert_eq!(response.status, "cancelled");
    let trials = store.list_trials(&scope, run.run_uid, None, 10).await?;
    assert_trial_status(
        &trials,
        running.trial_uid,
        ExperimentTrialStatus::Cancelled,
        Some(ExperimentTrialStopReason::Cancelled),
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn concurrent_trial_creation_uses_unique_storage_partitions_db() -> Result<()> {
    // Pins: trial creation is parallel-safe when callers use unique storage partitions.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();

    let (first, second) = tokio::join!(
        create_run_and_trial(pool.clone(), "trial-concurrent-a"),
        create_run_and_trial(pool, "trial-concurrent-b")
    );
    let first = first?;
    let second = second?;

    assert_ne!(first.0, second.0);
    assert_ne!(first.1, second.1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn pre_expansion_admission_reserves_projected_trial_count_db() -> Result<()> {
    // Pins: an accepted run consumes its full projected trial quota before child rows exist.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("experiment-projected-admission");
    let plan_revision_uid = insert_artifact_revision(pool, &scope).await?;
    let plan_artifact_uid = artifact_uid_for_revision(pool, &scope, plan_revision_uid).await?;

    let mut first = new_experiment("projected-admission-first", None, vec![plan_revision_uid]);
    first.plan_artifact_uid = Some(plan_artifact_uid);
    first.expected_trials = DEFAULT_MAX_ARTIFACT_ACTIVE_TRIALS;
    let admitted = store.insert_run(&scope, first).await?;

    let child_count = scoped_trial_count(pool, &scope, admitted.run_uid).await?;
    assert_eq!(
        child_count, 0,
        "the quota assertion must run before plan expansion mints child rows"
    );

    let mut second = new_experiment("projected-admission-second", None, vec![plan_revision_uid]);
    second.plan_artifact_uid = Some(plan_artifact_uid);
    second.expected_trials = 1;
    let error = store
        .insert_run(&scope, second)
        .await
        .expect_err("the prior run's projected matrix should fill the artifact trial quota");

    match error {
        MoaError::ValidationError(message) => {
            assert!(
                message.contains(
                    "plan_artifact trials quota is full (5000 active + 1 requested exceeds 5000)"
                ),
                "unexpected admission refusal: {message}"
            );
        }
        other => panic!("expected an admission validation error, got {other}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn reservation_rejects_trial_owned_by_another_run_db() -> Result<()> {
    // Pins: a reservation cannot pair a run with another run's trial.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("experiment-reservation-run-trial-integrity");
    let plan_revision_uid = insert_artifact_revision(pool, &scope).await?;

    let owner = store
        .insert_run(
            &scope,
            new_experiment("reservation-trial-owner", None, vec![plan_revision_uid]),
        )
        .await?;
    let other = store
        .insert_run(
            &scope,
            new_experiment("reservation-other-run", None, vec![plan_revision_uid]),
        )
        .await?;
    let trial = store
        .insert_trial(
            &scope,
            new_trial(
                owner.run_uid,
                "owned-trial",
                plan_revision_uid,
                vec![plan_revision_uid],
            ),
        )
        .await?;

    let error = store
        .try_reserve_resources(
            &scope,
            ExperimentResourceReservationRequest {
                run_uid: other.run_uid,
                trial_uid: Some(trial.trial_uid),
                reservation_key: "mismatched-run-trial".to_string(),
                component: ExperimentResourceComponent::Target,
                worst_case: ResourceAmounts {
                    tokens: 1,
                    ..ResourceAmounts::ZERO
                },
            },
            Utc::now(),
        )
        .await
        .expect_err("the composite run/trial foreign key should reject the reservation");

    match error {
        MoaError::StorageError(message) => assert!(
            message.contains("experiment_resource_reservation_run_trial_fkey"),
            "unexpected storage error: {message}"
        ),
        other => panic!("expected a foreign-key storage error, got {other}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn reconciliation_rejects_inconsistent_token_split_without_settling_db() -> Result<()> {
    // Pins: actual input/output tokens must equal the token total committed to the ledger.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("experiment-reconciliation-token-split");
    let run = store
        .insert_run(
            &scope,
            new_experiment("reconciliation-token-split", None, Vec::new()),
        )
        .await?;
    let request = ExperimentResourceReservationRequest {
        run_uid: run.run_uid,
        trial_uid: None,
        reservation_key: "invalid-token-split".to_string(),
        component: ExperimentResourceComponent::Target,
        worst_case: ResourceAmounts {
            tokens: 2,
            ..ResourceAmounts::ZERO
        },
    };
    let admission = store
        .try_reserve_resources(&scope, request.clone(), Utc::now())
        .await?;
    assert!(
        matches!(admission, ExperimentResourceAdmission::Granted(_)),
        "the valid reservation should be open before reconciliation"
    );

    let error = store
        .reconcile_resources(
            &scope,
            run.run_uid,
            &request.reservation_key,
            ExperimentResourceUsage {
                input_tokens: 1,
                output_tokens: 0,
                amounts: ResourceAmounts {
                    tokens: 2,
                    ..ResourceAmounts::ZERO
                },
            },
        )
        .await
        .expect_err("an inconsistent token split must not be committed");
    assert!(
        matches!(error, MoaError::ValidationError(_)),
        "expected token accounting validation error, got {error}"
    );

    let retry = store
        .try_reserve_resources(&scope, request, Utc::now())
        .await?;
    assert!(
        matches!(retry, ExperimentResourceAdmission::Granted(_)),
        "failed reconciliation must leave the reservation open"
    );
    Ok(())
}

fn tenant_scope(_label: &str) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    }
}

fn contact_scope(tenant_id: TenantId, contact_id: ContactId) -> ActionRuleScope {
    ActionRuleScope::Contact {
        tenant_id,
        contact_id,
    }
}

fn new_experiment(
    name: &str,
    idempotency_key: Option<&str>,
    artifact_revision_uids: Vec<Uuid>,
) -> NewExperiment {
    NewExperiment {
        name: name.to_string(),
        target: ExperimentTarget::AgentLoop {
            prompt: "Measure this behavior.".to_string(),
            agent: None,
            model: ModelId::new("gpt-5.1"),
            attachments: Vec::new(),
        },
        variant: ExperimentVariant {
            name: "baseline".to_string(),
            model: Some(ModelId::new("gpt-5.1")),
            artifact_revision_uids: artifact_revision_uids.clone(),
            skill_refs: vec!["skill://experiment-baseline".to_string()],
            execution_template: None,
            metadata: json!({ "cohort": "db" }),
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
        artifact_revision_uids,
        idempotency_key: idempotency_key.map(ToOwned::to_owned),
        created_by_identity: json!({
            "type": "user",
            "id": "experimenter"
        }),
        plan_artifact_uid: None,
        expected_trials: 1,
        resource_envelope: fixture_experiment_envelope(),
        simulator_policy: None,
    }
}

/// A bounded envelope for store round-trip fixtures.
///
/// Stated explicitly rather than borrowed from a production ceiling: these tests
/// pin persistence, so the numbers must not move when a platform limit does.
fn fixture_experiment_envelope() -> ExperimentResourceEnvelope {
    let limits = ResourceAmounts {
        cost_micro_usd: 1_000_000,
        tokens: 100_000,
        turns: 8,
        model_calls: 16,
        tool_calls: 32,
    };
    ExperimentResourceEnvelope::new(
        limits,
        limits,
        moa_test_support::fixtures::pg_now() + chrono::Duration::hours(1),
    )
}

fn new_trial(
    run_uid: Uuid,
    trial_key: &str,
    plan_revision_uid: Uuid,
    artifact_revision_uids: Vec<Uuid>,
) -> NewExperimentTrial {
    NewExperimentTrial {
        run_uid,
        trial_key: trial_key.to_string(),
        target_kind: ExperimentTargetKind::AgentLoop,
        variant_key: "baseline".to_string(),
        plan_revision_uid,
        scenario_id: None,
        persona_id: None,
        profile_id: None,
        data_bundle_ids: Vec::new(),
        artifact_revision_uids,
        simulator: ExperimentSimulatorConfig {
            policy: support::simulator_policy("gpt-5.1-mini"),
            max_turns: 6,
            token_budget: Some(4_000),
        },
        target_model: Some(ModelId::new("gpt-5.1")),
        seed: Some("seed-fixture".to_string()),
        score_run_id: Uuid::now_v7(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_scored_trial(
    store: &ExperimentStore,
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    trial_key: &str,
    variant_key: &str,
    scenario_id: &str,
    quality: f64,
    success: Option<bool>,
) -> Result<moa_experiments::model::ExperimentTrialRecord> {
    let mut trial = new_trial(run_uid, trial_key, plan_revision_uid, Vec::new());
    trial.variant_key = variant_key.to_string();
    trial.scenario_id = Some(scenario_id.to_string());
    let trial = store.insert_trial(scope, trial).await?;
    insert_score(
        pool,
        scope,
        trial.score_run_id,
        "quality",
        "numeric",
        Some(quality),
        None,
    )
    .await?;
    if let Some(success) = success {
        insert_score(
            pool,
            scope,
            trial.score_run_id,
            "success",
            "boolean",
            None,
            Some(success),
        )
        .await?;
    }
    Ok(trial)
}

#[allow(clippy::too_many_arguments)]
async fn insert_score(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    run_id: Uuid,
    name: &str,
    value_type: &str,
    value_numeric: Option<f64>,
    value_boolean: Option<bool>,
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO analytics.scores (
            score_id, ts, storage_partition_id, user_id, target_kind, run_id,
            name, value_type, value_numeric, value_boolean, source, model_or_evaluator
        )
        VALUES ($1, now(), $2, $3, 'agent_loop', $4, $5, $6, $7, $8, 'test', 'offline')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(scope_storage_partition_id(scope))
    .bind(scope_user_id(scope))
    .bind(run_id)
    .bind(name)
    .bind(value_type)
    .bind(value_numeric)
    .bind(value_boolean)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(())
}

fn score_summary(name: &str, value_type: &str, n: u64, mean_or_rate: f64) -> ScoreSummaryRow {
    ScoreSummaryRow {
        name: name.to_string(),
        value_type: ScorecardValueType::from_db(value_type)
            .expect("score summary fixture value type should be supported"),
        n,
        mean_or_rate: Some(mean_or_rate),
    }
}

async fn create_run_and_trial(pool: sqlx::PgPool, label: &'static str) -> Result<(Uuid, Uuid)> {
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope(label);
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let run = store
        .insert_run(&scope, new_experiment(label, None, Vec::new()))
        .await?;
    let trial = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, label, plan_revision_uid, Vec::new()),
        )
        .await?;
    Ok((run.run_uid, trial.trial_uid))
}

fn assert_trial_status(
    trials: &[moa_experiments::model::ExperimentTrialRecord],
    trial_uid: Uuid,
    status: ExperimentTrialStatus,
    stop_reason: Option<ExperimentTrialStopReason>,
) {
    let trial = trials
        .iter()
        .find(|trial| trial.trial_uid == trial_uid)
        .expect("trial should be listed after cancellation");
    assert_eq!(trial.status, status);
    assert_eq!(trial.stop_reason, stop_reason);
}

async fn insert_execution_run(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    session_id: SessionId,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Option<Uuid>,
) -> Result<Uuid> {
    let tenant_id = scope_tenant_id(scope);
    let contact_id = scope_contact_id(scope);
    let owner_user_id = scope_user_id(scope).unwrap_or_else(|| "experiment-owner".to_string());
    let planning_context_uid = Uuid::now_v7();
    let run_uid = Uuid::now_v7();
    let planning_hash = "1".repeat(64);
    let plan_hash = "2".repeat(64);
    // The typed columns and the provenance JSON must agree: `source_kind` carries a
    // CHECK that an `experiment_template` run names a canonical skill template, so a
    // fixture that set only the JSON would be rejected by the schema.
    let skill_template_ref = "skill://experiment-link";
    let skill_template_revision_uid = Uuid::now_v7();
    let source_provenance = json!({
        "kind": "experiment_template",
        "skill_template_ref": skill_template_ref,
        "skill_template_revision_uid": skill_template_revision_uid,
        "experiment_run_uid": experiment_run_uid,
        "score_run_id": score_run_id,
        "trial_uid": trial_uid,
    });
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_planning_context (
            planning_context_uid, tenant_id, contact_id, session_id,
            originating_user_sequence_num, originating_user_event_hash,
            owner_user_id, planning_context_hash, snapshot
        )
        VALUES ($1, $2, $3, $4, 1, $5, $6, $5, '{}'::JSONB)
        "#,
    )
    .bind(planning_context_uid)
    .bind(tenant_id.0)
    .bind(contact_id.map(|id| id.0))
    .bind(session_id.0)
    .bind(&planning_hash)
    .bind(&owner_user_id)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, contact_id, session_id,
            originating_user_sequence_num, planning_context_uid, planning_context_hash,
            owner_user_id, goal_contract, initial_plan, active_plan,
            initial_plan_hash, active_plan_hash, capability_catalog,
            authorization_envelope, source_provenance, input, status,
            source_kind, skill_template_ref, skill_template_revision_uid
        )
        VALUES (
            $1, $2, $3, $4, 1, $5, $6, $7,
            '{}'::JSONB, '{}'::JSONB, '{}'::JSONB, $8, $8,
            '{"schema_version":1}'::JSONB,
            '{"capability_refs":[],"skill_refs":[]}'::JSONB,
            $9, '{}'::JSONB, 'queued',
            'experiment_template', $10, $11
        )
        "#,
    )
    .bind(run_uid)
    .bind(tenant_id.0)
    .bind(contact_id.map(|id| id.0))
    .bind(session_id.0)
    .bind(planning_context_uid)
    .bind(&planning_hash)
    .bind(&owner_user_id)
    .bind(&plan_hash)
    .bind(source_provenance)
    .bind(skill_template_ref)
    .bind(skill_template_revision_uid)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(run_uid)
}

async fn insert_artifact_revision(pool: &sqlx::PgPool, scope: &ActionRuleScope) -> Result<Uuid> {
    let tenant_id = scope_tenant_id(scope);
    let storage_partition_id = scope_storage_partition_id(scope);
    let user_id = scope_user_id(scope);
    let artifact_uid = Uuid::now_v7();
    let revision_uid = Uuid::now_v7();
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, tenant_id, storage_partition_id, user_id, kind, name, description
        )
        VALUES ($1, $2, $3, $4, 'skill', $5, 'experiment fixture')
        "#,
    )
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(user_id.as_deref())
    .bind(format!("experiment-fixture-{artifact_uid}"))
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_revision (
            revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition,
            canonical_hash, source_format, source_text, status, validation_report, version,
            published_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'json', $8, 'ready', $9, 1, now())
        "#,
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(user_id.as_deref())
    .bind(json!({ "kind": "skill", "name": "experiment fixture" }))
    .bind(vec![1_u8; 32])
    .bind(br#"{"kind":"skill","name":"experiment fixture"}"#.to_vec())
    .bind(json!({}))
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(revision_uid)
}

async fn artifact_uid_for_revision(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<Uuid> {
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let artifact_uid = sqlx::query_scalar(
        "SELECT artifact_uid FROM moa.artifact_revision WHERE revision_uid = $1",
    )
    .bind(revision_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(artifact_uid)
}

async fn scoped_trial_count(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> Result<i64> {
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let count = sqlx::query_scalar("SELECT count(*) FROM moa.experiment_trial WHERE run_uid = $1")
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(count)
}

async fn assert_score_run_exists(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
) -> Result<()> {
    assert_score_run_exists_with_source(pool, scope, score_run_id, "experiment_run").await
}

async fn assert_score_run_exists_with_source(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    source: &str,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM analytics.score_run
            WHERE run_id = $1
              AND source = $5
              AND scope = $2
              AND storage_partition_id IS NOT DISTINCT FROM $3
              AND user_id IS NOT DISTINCT FROM $4
        )
        "#,
    )
    .bind(score_run_id)
    .bind(parts.0)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .bind(source)
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    assert!(exists, "score-run parent row should exist");
    Ok(())
}

async fn assert_score_run_absent(pool: &sqlx::PgPool, score_run_id: Uuid) -> Result<()> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM analytics.score_run
            WHERE run_id = $1
        )
        "#,
    )
    .bind(score_run_id)
    .fetch_one(pool)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    assert!(!exists, "score-run parent row should not exist");
    Ok(())
}

async fn assert_scoped_experiment_count_for_score_run(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    expected: i64,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.experiment_run
        WHERE score_run_id = $1
          AND scope = $2
          AND storage_partition_id IS NOT DISTINCT FROM $3
          AND user_id IS NOT DISTINCT FROM $4
        "#,
    )
    .bind(score_run_id)
    .bind(parts.0)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    assert_eq!(count, expected);
    Ok(())
}

async fn assert_scoped_trial_count_for_score_run(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    expected: i64,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.experiment_trial
        WHERE score_run_id = $1
          AND scope = $2
          AND storage_partition_id IS NOT DISTINCT FROM $3
          AND user_id IS NOT DISTINCT FROM $4
        "#,
    )
    .bind(score_run_id)
    .bind(parts.0)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    assert_eq!(count, expected);
    Ok(())
}

async fn assert_no_experiment_trial_event_table(pool: &sqlx::PgPool) -> Result<()> {
    let table = sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT to_regclass('moa.experiment_trial_event')::TEXT
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    assert_eq!(table, None);
    Ok(())
}

async fn insert_score_run(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
) -> Result<()> {
    insert_score_run_with_source(pool, scope, score_run_id, "experiment_run").await
}

async fn insert_score_run_with_source(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    source: &str,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO analytics.score_run (
            run_id, storage_partition_id, user_id, source
        )
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(score_run_id)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .bind(source)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(())
}

async fn assert_artifact_revision_links(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    expected_revision_uids: &[Uuid],
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    let mut revision_uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT revision_uid
        FROM moa.experiment_run_artifact_revision
        WHERE run_uid = $1
        ORDER BY revision_uid
        "#,
    )
    .bind(run_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    let mut expected = expected_revision_uids.to_vec();
    revision_uids.sort_unstable();
    expected.sort_unstable();
    assert_eq!(revision_uids, expected);
    Ok(())
}

async fn insert_session_for_experiment_fk(
    pool: &sqlx::PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
) -> Result<SessionId> {
    let target_table = sqlx::query_scalar::<_, String>(
        r#"
        SELECT confrelid::regclass::text
        FROM pg_constraint
        WHERE conrelid = 'moa.experiment_run'::regclass
          AND conname = 'experiment_run_session_id_fkey'
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    let session_id = SessionId::new();
    let target_table = target_table.unwrap_or_else(|| "sessions".to_string());
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(&format!(
        r#"
        INSERT INTO {target_table} (
            id, storage_partition_id, user_id, status, channel, model
        )
        VALUES ($1, $2, $3, 'created', 'chat', $4)
        "#
    ))
    .bind(session_id.0)
    .bind(storage_partition_id.to_string())
    .bind(user_id.to_string())
    .bind("gpt-5.1")
    .execute(&mut *transaction)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO session_agent_context (
            session_id, storage_partition_id, user_id, agent_definition_ref,
            agent_revision_uid, policy_hash, display_name, policy_snapshot,
            artifact_dependencies, tool_dependencies
        )
        VALUES (
            $1, $2, $3, 'agent://system-default',
            '00000000-0000-4000-8000-000000000a02',
            'system-default-agent-v1', 'MOA Default Agent',
            '{"instructions":[],"tool_policy":{"mode":"auto","tools":[],"denied_tools":[]}}'::JSONB,
            '[]'::JSONB, '[]'::JSONB
        )
        "#,
    )
    .bind(session_id.0)
    .bind(storage_partition_id.to_string())
    .bind(user_id.to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(session_id)
}

fn scope_parts(scope: &ActionRuleScope) -> (&'static str, Option<String>, Option<String>) {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => ("tenant", Some(tenant_id.to_string()), None),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => (
            "contact",
            Some(tenant_id.to_string()),
            Some(contact_id.to_string()),
        ),
    }
}

fn scope_storage_partition_id(scope: &ActionRuleScope) -> String {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => tenant_id.to_string(),
        ActionRuleScope::Contact { tenant_id, .. } => tenant_id.to_string(),
    }
}

fn scope_tenant_id(scope: &ActionRuleScope) -> TenantId {
    match scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
            *tenant_id
        }
    }
}

fn scope_contact_id(scope: &ActionRuleScope) -> Option<ContactId> {
    match scope {
        ActionRuleScope::Tenant { .. } => None,
        ActionRuleScope::Contact { contact_id, .. } => Some(*contact_id),
    }
}

fn scope_user_id(scope: &ActionRuleScope) -> Option<String> {
    match scope {
        ActionRuleScope::Tenant { .. } => None,
        ActionRuleScope::Contact { contact_id, .. } => Some(contact_id.to_string()),
    }
}

fn scope_context(scope: &ActionRuleScope) -> RlsContext {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => RlsContext::contact(*tenant_id, *contact_id),
    }
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn seeded_score_rows_without_provenance_never_satisfy_the_scorecard_db() -> Result<()> {
    // Pins the task's headline claim on the real read path: score rows that were
    // seeded straight into `analytics.scores` prove query mechanics and nothing
    // else. The exact-row query inner-joins provenance, so a seeded row is
    // structurally invisible to the gate and the trial reads Incomplete — not
    // Eligible, and not "no rows found, nothing to check".
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("scorecard-seeded");
    let artifact_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("seeded", None, vec![artifact_revision_uid]),
        )
        .await?;
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let trial = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "seeded/0", plan_revision_uid, Vec::new()),
        )
        .await?;
    let session_id = SessionId(Uuid::now_v7());
    attach_trial_session_row(
        &pool,
        &scope,
        trial.trial_uid,
        session_id,
        artifact_revision_uid,
    )
    .await?;

    // A score row with exactly the right name and value, and no provenance.
    insert_score(
        &pool,
        &scope,
        trial.score_run_id,
        "target_completed",
        "boolean",
        None,
        Some(true),
    )
    .await?;

    let response = moa_experiments::app::scores(
        pool.clone(),
        moa_wire::experiments::ExperimentScoresRequest {
            tenant_id: scope_tenant_id(&scope),
            run_uid: run.run_uid,
        },
    )
    .await
    .expect("scores read should succeed");

    assert_eq!(
        response.run_scorecard.eligibility,
        ScorecardEligibility::Incomplete,
        "a seeded score row must not make a run eligible"
    );
    assert_eq!(response.run_scorecard.trials, 1);
    let trial_summary = response
        .trials
        .iter()
        .find(|summary| summary.trial_uid == trial.trial_uid)
        .expect("the scored trial should appear in the breakdown");
    assert_eq!(trial_summary.eligibility, ScorecardEligibility::Incomplete);
    assert!(
        trial_summary
            .eligibility_findings
            .iter()
            .any(|finding| finding.detail.contains("no provenance-backed score row")),
        "the finding must name the missing provenance: {:?}",
        trial_summary.eligibility_findings
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn provenance_backed_rows_drive_run_scenario_and_variant_eligibility_db() -> Result<()> {
    // Pins the full eligibility ladder on the real read path: a correctly linked
    // trial is individually Eligible, but groups with no modeled-case identity
    // remain Incomplete. Removing another trial's required result also keeps its
    // groups Incomplete.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("scorecard-provenance");
    let artifact_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("provenance", None, vec![artifact_revision_uid]),
        )
        .await?;
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;

    let mut complete = new_trial(run.run_uid, "complete/0", plan_revision_uid, Vec::new());
    complete.scenario_id = Some("scenario-a".to_string());
    complete.variant_key = "variant-a".to_string();
    let complete = store.insert_trial(&scope, complete).await?;
    let mut missing = new_trial(run.run_uid, "missing/0", plan_revision_uid, Vec::new());
    missing.scenario_id = Some("scenario-b".to_string());
    missing.variant_key = "variant-b".to_string();
    let missing = store.insert_trial(&scope, missing).await?;

    let complete_session = SessionId(Uuid::now_v7());
    let missing_session = SessionId(Uuid::now_v7());
    attach_trial_session_row(
        &pool,
        &scope,
        complete.trial_uid,
        complete_session,
        artifact_revision_uid,
    )
    .await?;
    attach_trial_session_row(
        &pool,
        &scope,
        missing.trial_uid,
        missing_session,
        artifact_revision_uid,
    )
    .await?;
    insert_provenance_backed_score(
        &pool,
        &scope,
        &complete,
        run.run_uid,
        plan_revision_uid,
        complete_session,
        true,
    )
    .await?;

    let response = moa_experiments::app::scores(
        pool.clone(),
        moa_wire::experiments::ExperimentScoresRequest {
            tenant_id: scope_tenant_id(&scope),
            run_uid: run.run_uid,
        },
    )
    .await
    .expect("scores read should succeed");

    let by_trial = response
        .trials
        .iter()
        .map(|summary| (summary.trial_uid, summary.eligibility.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(by_trial.get(&complete.trial_uid).copied(), Some("eligible"));
    assert_eq!(
        by_trial.get(&missing.trial_uid).copied(),
        Some("incomplete")
    );

    let scenarios = response
        .scenario_scorecards
        .iter()
        .map(|rollup| (rollup.key.as_str(), rollup.eligibility.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(scenarios.get("scenario-a").copied(), Some("incomplete"));
    assert_eq!(scenarios.get("scenario-b").copied(), Some("incomplete"));

    let variants = response
        .variant_scorecards
        .iter()
        .map(|rollup| (rollup.key.as_str(), rollup.eligibility.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(variants.get("variant-a").copied(), Some("incomplete"));
    assert_eq!(variants.get("variant-b").copied(), Some("incomplete"));

    assert_eq!(
        response.run_scorecard.eligibility,
        ScorecardEligibility::Incomplete,
        "one unproven trial must keep the whole run from being eligible"
    );
    assert_eq!(response.run_scorecard.trials, 2);
    assert_eq!(
        response.run_scorecard.support.status,
        ScorecardSupportStatus::CaseIdentityUnavailable
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn a_provenance_backed_row_from_another_trial_never_satisfies_the_gate_db() -> Result<()> {
    // Pins that borrowing evidence is refused end to end: a fully valid score row
    // that belongs to a NEIGHBOURING trial in the same run does not count for this
    // trial. Without the trial-linkage check, one scored trial would make every
    // trial in the run look eligible.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = ExperimentStore::new(pool.clone());
    let scope = tenant_scope("scorecard-borrowed");
    let artifact_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let run = store
        .insert_run(
            &scope,
            new_experiment("borrowed", None, vec![artifact_revision_uid]),
        )
        .await?;
    let plan_revision_uid = insert_artifact_revision(&pool, &scope).await?;
    let owner = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "owner/0", plan_revision_uid, Vec::new()),
        )
        .await?;
    let borrower = store
        .insert_trial(
            &scope,
            new_trial(run.run_uid, "borrower/0", plan_revision_uid, Vec::new()),
        )
        .await?;
    let owner_session = SessionId(Uuid::now_v7());
    let borrower_session = SessionId(Uuid::now_v7());
    attach_trial_session_row(
        &pool,
        &scope,
        owner.trial_uid,
        owner_session,
        artifact_revision_uid,
    )
    .await?;
    attach_trial_session_row(
        &pool,
        &scope,
        borrower.trial_uid,
        borrower_session,
        artifact_revision_uid,
    )
    .await?;
    insert_provenance_backed_score(
        &pool,
        &scope,
        &owner,
        run.run_uid,
        plan_revision_uid,
        owner_session,
        true,
    )
    .await?;

    let response = moa_experiments::app::scores(
        pool.clone(),
        moa_wire::experiments::ExperimentScoresRequest {
            tenant_id: scope_tenant_id(&scope),
            run_uid: run.run_uid,
        },
    )
    .await
    .expect("scores read should succeed");

    let borrower_summary = response
        .trials
        .iter()
        .find(|summary| summary.trial_uid == borrower.trial_uid)
        .expect("the unscored trial should still appear");
    assert_eq!(
        borrower_summary.eligibility,
        ScorecardEligibility::Incomplete,
        "a neighbouring trial's score must not satisfy this trial's requirement"
    );
    Ok(())
}

/// Creates one target session and links a trial to it, as the trial workflow does.
async fn attach_trial_session_row(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    trial_uid: Uuid,
    session_id: SessionId,
    agent_revision_uid: Uuid,
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO sessions (id, storage_partition_id, user_id, tenant_id, status, model)
        VALUES ($1, $2, $3, $4, 'completed', 'gpt-5.1')
        "#,
    )
    .bind(session_id.0)
    .bind(scope_storage_partition_id(scope))
    .bind(scope_user_id(scope).unwrap_or_else(|| "system".to_string()))
    .bind(scope_tenant_id(scope).0)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    // Every session carries an agent context; a deferred trigger refuses a
    // session without one at commit, so the fixture must build a real session
    // rather than a bare row.
    sqlx::query(
        r#"
        INSERT INTO session_agent_context (
            session_id, storage_partition_id, user_id, tenant_id, agent_definition_ref,
            agent_revision_uid, policy_hash, display_name, policy_snapshot
        )
        VALUES ($1, $2, $3, $4, 'agent://experiment-fixture', $5, 'fixture-hash',
                'Experiment fixture', '{}'::jsonb)
        "#,
    )
    .bind(session_id.0)
    .bind(scope_storage_partition_id(scope))
    .bind(scope_user_id(scope).unwrap_or_else(|| "system".to_string()))
    .bind(scope_tenant_id(scope).0)
    .bind(agent_revision_uid)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query("UPDATE moa.experiment_trial SET session_id = $1 WHERE trial_uid = $2")
        .bind(session_id.0)
        .bind(trial_uid)
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(())
}

/// Writes one `target_completed` score with the provenance row that explains it.
///
/// This mirrors exactly what the lineage sink drain writes, so the read path
/// under test sees the same shape production produces.
#[allow(clippy::too_many_arguments)]
async fn insert_provenance_backed_score(
    pool: &sqlx::PgPool,
    scope: &ActionRuleScope,
    trial: &moa_experiments::model::ExperimentTrialRecord,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    session_id: SessionId,
    value: bool,
) -> Result<()> {
    let score_id = Uuid::now_v7();
    let score_ts = Utc::now();
    let mut conn = ScopedConn::begin(pool, &scope_context(scope)).await?;
    sqlx::query(
        r#"
        INSERT INTO analytics.scores (
            score_id, ts, storage_partition_id, user_id, target_kind, session_id, run_id,
            name, value_type, value_boolean, source, model_or_evaluator
        )
        VALUES ($1, $2, $3, $4, 'session', $5, $6, 'target_completed', 'boolean', $7,
                'product_evaluator', 'target_completed@v1')
        "#,
    )
    .bind(score_id)
    .bind(score_ts)
    .bind(scope_storage_partition_id(scope))
    .bind(scope_user_id(scope))
    .bind(session_id.0)
    .bind(trial.score_run_id)
    .bind(value)
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.experiment_score_provenance (
            score_id, score_ts, storage_partition_id, user_id, score_run_id, experiment_run_uid,
            plan_revision_uid, trial_uid, target_session_id, target_execution_run_uid,
            evaluator_id, evaluator_version, score_name, value_type, evidence_ref, evidence_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL, 'target_completed', 'v1',
                'target_completed', 'boolean', 'session:fixture#seq=1', $10)
        "#,
    )
    .bind(score_id)
    .bind(score_ts)
    .bind(scope_storage_partition_id(scope))
    .bind(scope_user_id(scope))
    .bind(trial.score_run_id)
    .bind(run_uid)
    .bind(plan_revision_uid)
    .bind(trial.trial_uid)
    .bind(session_id.0)
    .bind(vec![3_u8; 32])
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(())
}
