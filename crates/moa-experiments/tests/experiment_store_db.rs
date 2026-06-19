use moa_core::{
    MemoryScope, ModelId, Result, ScopeContext, ScopedConn, SessionId, UserId, WorkspaceId,
};
use moa_experiments::{
    model::{
        ExperimentRunStatus, ExperimentScorecard, ExperimentSimulatorConfig, ExperimentTarget,
        ExperimentTargetKind, ExperimentTrialStatus, ExperimentTrialStopReason, ExperimentVariant,
        NewExperimentRun as NewExperiment, NewExperimentTrial,
    },
    store::ExperimentStore,
};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static DB_TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn workspace_scoped_run_insert_load_round_trip_db() -> Result<()> {
    // Pins: workspace-scoped experiment metadata persists and loads through the scoped store.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let scope = workspace_scope("experiment-round-trip");
    let artifact_revision_uid = insert_artifact_revision(test_db.store().pool(), &scope).await?;
    let new_run = new_experiment(
        "round-trip",
        Some("round-trip-key"),
        vec![artifact_revision_uid],
    );

    let inserted = store.insert_run(&scope, new_run).await?;
    let loaded = store
        .load_run(&scope, inserted.run_uid)
        .await?
        .expect("inserted experiment should load in same workspace");

    assert_eq!(loaded.scope, scope);
    assert_eq!(loaded.status, ExperimentRunStatus::Accepted);
    assert_eq!(loaded.name, "round-trip");
    assert_eq!(loaded.scorecard.score_names, ["task_success"]);
    assert_eq!(loaded.artifact_revision_uids, [artifact_revision_uid]);
    assert_eq!(loaded.idempotency_key.as_deref(), Some("round-trip-key"));
    assert_eq!(loaded.created_by_identity["id"], "experimenter");
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
    let scope = workspace_scope("experiment-missing-artifact-revision");
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
    let workspace_a = workspace_scope("experiment-a");
    let workspace_b = workspace_scope("experiment-b");

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
async fn idempotency_key_deduplicates_within_scope_not_across_workspaces_db() -> Result<()> {
    // Pins: scoped idempotency returns the existing row only inside the same workspace.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = workspace_scope("experiment-idem-a");
    let workspace_b = workspace_scope("experiment-idem-b");

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
    let scope = workspace_scope("experiment-idem-links");
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
async fn score_run_id_from_another_workspace_rejects_insert_db() -> Result<()> {
    // Pins: experiment runs cannot attach to an existing score-run parent from another workspace.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = workspace_scope("experiment-score-run-a");
    let workspace_b = workspace_scope("experiment-score-run-b");
    let score_run_id = Uuid::now_v7();
    insert_score_run(test_db.store().pool(), &workspace_b, score_run_id).await?;
    let mut new_run = new_experiment("cross-score-run", None, Vec::new());
    new_run.score_run_id = score_run_id;

    let error = store
        .insert_run(&workspace_a, new_run)
        .await
        .expect_err("cross-workspace score run should reject experiment insert");

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
async fn cross_workspace_artifact_revision_rejects_insert_db() -> Result<()> {
    // Pins: experiment artifact links must target revisions visible from the requested scope.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = workspace_scope("experiment-artifact-a");
    let workspace_b = workspace_scope("experiment-artifact-b");
    let workspace_b_revision_uid =
        insert_artifact_revision(test_db.store().pool(), &workspace_b).await?;
    let new_run = new_experiment(
        "cross-workspace-revision",
        None,
        vec![workspace_b_revision_uid],
    );
    let score_run_id = new_run.score_run_id;

    let error = store
        .insert_run(&workspace_a, new_run)
        .await
        .expect_err("cross-workspace artifact revision should reject experiment insert");

    assert!(
        error.to_string().contains("artifact revision"),
        "expected artifact revision visibility error, got {error}"
    );
    assert_score_run_absent(test_db.store().pool(), score_run_id).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn workflow_run_and_session_links_persist_db() -> Result<()> {
    // Pins: session and workflow artifact-run links persist on experiment records.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let experiment_store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_id = WorkspaceId::new(format!("workspace-{}", Uuid::now_v7()));
    let user_id = UserId::new(format!("user-{}", Uuid::now_v7()));
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let session_id =
        insert_session_for_experiment_fk(test_db.store().pool(), &workspace_id, &user_id).await?;
    let workflow_run_uid = insert_workflow_run(test_db.store().pool(), &scope, session_id).await?;
    let inserted = experiment_store
        .insert_run(&scope, new_experiment("links", None, Vec::new()))
        .await?;

    experiment_store
        .attach_session(&scope, inserted.run_uid, session_id)
        .await?
        .expect("session link update should return the run");
    let linked = experiment_store
        .attach_workflow_run(&scope, inserted.run_uid, workflow_run_uid)
        .await?
        .expect("workflow link update should return the run");

    assert_eq!(linked.session_id, Some(session_id));
    assert_eq!(linked.workflow_run_uid, Some(workflow_run_uid));
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
    let scope = workspace_scope("run-terminal-guard");
    let run = store
        .insert_run(
            &scope,
            new_experiment("run-terminal-guard", None, Vec::new()),
        )
        .await?;
    let completed_at = chrono::Utc::now();
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
            Some(chrono::Utc::now()),
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
    let scope = workspace_scope("trial-round-trip");
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
    let scope = workspace_scope("trial-idem");
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
async fn trial_rejects_cross_workspace_score_run_db() -> Result<()> {
    // Pins: trial-level score runs cannot be attached across workspace scope boundaries.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = workspace_scope("trial-score-a");
    let workspace_b = workspace_scope("trial-score-b");
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
        .expect_err("cross-workspace score run should reject trial insert");

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
async fn trial_rejects_cross_workspace_artifact_revision_db() -> Result<()> {
    // Pins: trial artifact pins must be visible from the requested experiment scope.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_a = workspace_scope("trial-artifact-a");
    let workspace_b = workspace_scope("trial-artifact-b");
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
        .expect_err("cross-workspace artifact revision should reject trial insert");

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
    // Pins: trial session/workflow/trace links, turn counts, and terminal status persist.
    let _guard = DB_TEST_LOCK.lock().await;
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = ExperimentStore::new(test_db.store().pool().clone());
    let workspace_id = WorkspaceId::new(format!("workspace-{}", Uuid::now_v7()));
    let user_id = UserId::new(format!("user-{}", Uuid::now_v7()));
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let session_id =
        insert_session_for_experiment_fk(test_db.store().pool(), &workspace_id, &user_id).await?;
    let workflow_run_uid = insert_workflow_run(test_db.store().pool(), &scope, session_id).await?;
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

    store
        .attach_trial_session(&scope, trial.trial_uid, session_id)
        .await?
        .expect("session link update should return the trial");
    store
        .attach_trial_workflow_run(&scope, trial.trial_uid, workflow_run_uid)
        .await?
        .expect("workflow link update should return the trial");
    store
        .attach_trial_trace(&scope, trial.trial_uid, "trace-trial-123".to_string())
        .await?
        .expect("trace link update should return the trial");
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
            Some(chrono::Utc::now()),
        )
        .await?
        .expect("status update should return the trial");

    assert_eq!(incremented.turn_count, 2);
    assert_eq!(completed.session_id, Some(session_id));
    assert_eq!(completed.workflow_run_uid, Some(workflow_run_uid));
    assert_eq!(completed.trace_id.as_deref(), Some("trace-trial-123"));
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
    let scope = workspace_scope("trial-cancel-active");
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
            Some(chrono::Utc::now()),
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
            Some(chrono::Utc::now()),
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
async fn concurrent_trial_creation_uses_unique_workspace_ids_db() -> Result<()> {
    // Pins: trial creation is parallel-safe when callers use unique workspace IDs.
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

fn workspace_scope(label: &str) -> MemoryScope {
    MemoryScope::Workspace {
        workspace_id: WorkspaceId::new(format!("{label}-{}", Uuid::now_v7())),
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
            session_id: None,
            model: ModelId::new("gpt-5.1"),
            attachments: Vec::new(),
        },
        variant: ExperimentVariant {
            name: "baseline".to_string(),
            model: Some(ModelId::new("gpt-5.1")),
            artifact_revision_uids: artifact_revision_uids.clone(),
            skill_refs: vec!["skill://experiment-baseline".to_string()],
            workflow_ref: None,
            metadata: json!({ "cohort": "db" }),
        },
        scorecard: ExperimentScorecard {
            score_names: vec!["task_success".to_string()],
            evaluator_metadata: json!({ "judge": "offline" }),
        },
        score_run_id: Uuid::now_v7(),
        session_id: None,
        workflow_run_uid: None,
        artifact_revision_uids,
        idempotency_key: idempotency_key.map(ToOwned::to_owned),
        created_by_identity: json!({
            "type": "user",
            "id": "experimenter"
        }),
    }
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
            model: ModelId::new("gpt-5.1-mini"),
            temperature: Some(0.0),
            max_turns: 6,
            token_budget: Some(4_000),
            metadata: json!({ "fixture": "db" }),
        },
        target_model: Some(ModelId::new("gpt-5.1")),
        seed: Some("seed-fixture".to_string()),
        score_run_id: Uuid::now_v7(),
    }
}

async fn create_run_and_trial(pool: sqlx::PgPool, label: &'static str) -> Result<(Uuid, Uuid)> {
    let store = ExperimentStore::new(pool.clone());
    let scope = workspace_scope(label);
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

async fn insert_workflow_run(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    session_id: SessionId,
) -> Result<Uuid> {
    let workspace_id = scope
        .workspace_id()
        .expect("workflow link test uses a workspace scope")
        .to_string();
    let run_uid = Uuid::now_v7();
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_run (
            run_uid, workspace_id, session_id, workflow_ref, status, input, state
        )
        VALUES ($1, $2, $3, $4, 'queued', $5, $6)
        "#,
    )
    .bind(run_uid)
    .bind(workspace_id)
    .bind(session_id.0)
    .bind("workflow://experiment-link")
    .bind(json!({ "case": "link" }))
    .bind(json!({}))
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(run_uid)
}

async fn insert_artifact_revision(pool: &sqlx::PgPool, scope: &MemoryScope) -> Result<Uuid> {
    let workspace_id = scope
        .workspace_id()
        .expect("artifact revision test uses a workspace scope")
        .to_string();
    let artifact_uid = Uuid::now_v7();
    let revision_uid = Uuid::now_v7();
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, workspace_id, kind, name, description
        )
        VALUES ($1, $2, 'skill', $3, 'experiment fixture')
        "#,
    )
    .bind(artifact_uid)
    .bind(&workspace_id)
    .bind(format!("experiment-fixture-{artifact_uid}"))
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
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
    .bind(json!({ "kind": "skill", "name": "experiment fixture" }))
    .bind(vec![1_u8; 32])
    .bind(br#"{"kind":"skill","name":"experiment fixture"}"#.to_vec())
    .bind(json!({}))
    .execute(conn.as_mut())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(revision_uid)
}

async fn assert_score_run_exists(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
) -> Result<()> {
    assert_score_run_exists_with_source(pool, scope, score_run_id, "experiment_run").await
}

async fn assert_score_run_exists_with_source(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
    source: &str,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM analytics.score_run
            WHERE run_id = $1
              AND source = $5
              AND scope = $2
              AND workspace_id IS NOT DISTINCT FROM $3
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    assert!(!exists, "score-run parent row should not exist");
    Ok(())
}

async fn assert_scoped_experiment_count_for_score_run(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
    expected: i64,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.experiment_run
        WHERE score_run_id = $1
          AND scope = $2
          AND workspace_id IS NOT DISTINCT FROM $3
          AND user_id IS NOT DISTINCT FROM $4
        "#,
    )
    .bind(score_run_id)
    .bind(parts.0)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    assert_eq!(count, expected);
    Ok(())
}

async fn assert_scoped_trial_count_for_score_run(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
    expected: i64,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.experiment_trial
        WHERE score_run_id = $1
          AND scope = $2
          AND workspace_id IS NOT DISTINCT FROM $3
          AND user_id IS NOT DISTINCT FROM $4
        "#,
    )
    .bind(score_run_id)
    .bind(parts.0)
    .bind(parts.1.as_deref())
    .bind(parts.2.as_deref())
    .fetch_one(conn.as_mut())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    assert_eq!(table, None);
    Ok(())
}

async fn insert_score_run(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
) -> Result<()> {
    insert_score_run_with_source(pool, scope, score_run_id, "experiment_run").await
}

async fn insert_score_run_with_source(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    score_run_id: Uuid,
    source: &str,
) -> Result<()> {
    let parts = scope_parts(scope);
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
    sqlx::query(
        r#"
        INSERT INTO analytics.score_run (
            run_id, workspace_id, user_id, source
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    Ok(())
}

async fn assert_artifact_revision_links(
    pool: &sqlx::PgPool,
    scope: &MemoryScope,
    run_uid: Uuid,
    expected_revision_uids: &[Uuid],
) -> Result<()> {
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone())).await?;
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    conn.commit().await?;
    let mut expected = expected_revision_uids.to_vec();
    revision_uids.sort_unstable();
    expected.sort_unstable();
    assert_eq!(revision_uids, expected);
    Ok(())
}

async fn insert_session_for_experiment_fk(
    pool: &sqlx::PgPool,
    workspace_id: &WorkspaceId,
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
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let session_id = SessionId::new();
    let target_table = target_table.unwrap_or_else(|| "sessions".to_string());
    sqlx::query(&format!(
        r#"
        INSERT INTO {target_table} (
            id, workspace_id, user_id, status, platform, model
        )
        VALUES ($1, $2, $3, 'created', 'api', $4)
        "#
    ))
    .bind(session_id.0)
    .bind(workspace_id.to_string())
    .bind(user_id.to_string())
    .bind("gpt-5.1")
    .execute(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(session_id)
}

fn scope_parts(scope: &MemoryScope) -> (&'static str, Option<String>, Option<String>) {
    match scope {
        MemoryScope::Global => ("global", None, None),
        MemoryScope::Workspace { workspace_id } => {
            ("workspace", Some(workspace_id.to_string()), None)
        }
        MemoryScope::User {
            workspace_id,
            user_id,
        } => (
            "user",
            Some(workspace_id.to_string()),
            Some(user_id.to_string()),
        ),
    }
}
