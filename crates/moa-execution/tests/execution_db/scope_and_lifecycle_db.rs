//! Execution-run scope, lifecycle, terminal, and database-guard persistence contracts.

use super::support::*;

#[tokio::test]
async fn queued_run_admission_persists_initial_activation_before_returning_db() -> TestResult {
    // Pins: the real create_run path accepts the current five-key plan contract, and a
    // serving-process crash after admission cannot strand the queued run because its canonical
    // generation-one, wake-one activation commits atomically, matching scheduled admission.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "initial-run-activation",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;

    assert_eq!(run.activation_state, ExecutionActivationState::Queued);
    assert_eq!(run.controller_generation, 1);
    assert_eq!(run.wake_epoch, 1);
    assert_eq!(run.processed_wake_epoch, 0);
    let current_plan_shape: (bool, bool) = sqlx::query_as(
        "SELECT moa.execution_plan_snapshot_is_current(initial_plan), \
                moa.execution_plan_snapshot_is_current(active_plan) \
         FROM moa.execution_run WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(current_plan_shape, (true, true));
    let persisted: (Uuid, i64, i64, String, String, String) = sqlx::query_as(
        "SELECT dispatch.dispatch_uid, dispatch.controller_generation, dispatch.wake_epoch, \
                dispatch.dispatch_kind, \
                dispatch.state, run.activation_state \
         FROM moa.execution_dispatch_outbox AS dispatch \
         JOIN moa.execution_run AS run USING (run_uid) \
         WHERE dispatch.run_uid = $1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_ne!(persisted.0, Uuid::nil());
    assert_eq!(persisted.1, 1);
    assert_eq!(persisted.2, 1);
    assert_eq!(persisted.3, "run_activation");
    assert_eq!(persisted.4, "pending");
    assert_eq!(persisted.5, "queued");
    Ok(())
}

#[tokio::test]
async fn session_fenced_run_reads_hide_cross_session_rows_db() -> TestResult {
    // Pins: after caller authorization, a guessed run UID from another parent session is
    // excluded by the protected SQL read itself, including the scheduler snapshot path.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let owning_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let mut run = new_run(
        tenant_id,
        None,
        "session-fenced-read",
        ExecutionRunStatus::Queued,
        budget(1),
    );
    run.session_id = owning_session_id;
    let run = create_run(&repository, scope, run).await?;

    assert_eq!(
        repository
            .load_run_for_session(scope, run.run_uid, owning_session_id)
            .await?,
        Some(run.clone())
    );
    assert_eq!(
        repository
            .load_run_for_session(scope, run.run_uid, other_session_id)
            .await?,
        None,
        "a same-tenant session must not observe another session's run"
    );
    assert!(
        repository
            .load_planning_context_for_session(scope, run.planning_context_uid, owning_session_id,)
            .await?
            .is_some()
    );
    assert_eq!(
        repository
            .load_planning_context_for_session(scope, run.planning_context_uid, other_session_id,)
            .await?,
        None,
        "execution start must not materialize another session's planning context"
    );
    assert_eq!(
        repository
            .load_run_by_idempotency_key_for_session(
                scope,
                tenant_id,
                None,
                owning_session_id,
                "session-fenced-read",
            )
            .await?,
        Some(run.clone())
    );
    assert_eq!(
        repository
            .load_run_by_idempotency_key_for_session(
                scope,
                tenant_id,
                None,
                other_session_id,
                "session-fenced-read",
            )
            .await?,
        None,
        "execution start replay must not materialize another session's idempotent run"
    );
    assert!(
        repository
            .load_run_for_session(scope, run.run_uid, owning_session_id)
            .await?
            .is_some()
    );
    assert_eq!(
        repository
            .load_run_for_session(scope, run.run_uid, other_session_id)
            .await?,
        None,
        "the run read must enforce the same parent-session fence"
    );
    Ok(())
}

#[tokio::test]
async fn originating_user_sequence_num_round_trips_to_execution_run_db() -> TestResult {
    // Pins: execution admission persists the exact user-event sequence as immutable run
    // provenance instead of deriving it from current session state.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new_run = new_run(
        tenant_id,
        None,
        "originating-sequence",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new_run.originating_user_sequence_num = 41;

    let created = create_run(&repository, scope, new_run).await?;

    assert_eq!(created.originating_user_sequence_num, 41);
    assert_ne!(created.planning_context_uid, Uuid::nil());
    let planning_context = repository
        .load_planning_context(scope, created.planning_context_uid)
        .await?
        .expect("run fixture must persist its normalized planning context");
    assert_eq!(
        created.planning_context_hash,
        planning_context.planning_context_hash
    );
    Ok(())
}

#[tokio::test]
async fn execution_analytics_metadata_round_trips_normalized_source_and_terminal_fields_db()
-> TestResult {
    // Pins: the repository writes normalized execution dimensions and advances the
    // sequence-backed analytics cursor on later state changes without mining provenance JSON.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let new_run = new_run(
        tenant_id,
        None,
        "execution-analytics-metadata",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    let (expected_template_ref, expected_template_revision_uid) = match &new_run.source_provenance {
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (skill_template_ref.clone(), *skill_template_revision_uid),
        provenance => panic!("unexpected analytics fixture provenance: {provenance:?}"),
    };
    let run = create_run(&repository, scope, new_run).await?;
    let initial_change_seq: i64 =
        sqlx::query_scalar("SELECT analytics_change_seq FROM moa.execution_run WHERE run_uid = $1")
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;

    let running =
        claim_running_controller(&repository, scope, &ExecutionConfig::default(), &run).await?;
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Completed {
        output: json!({ "status": "complete" }),
    };
    let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    assert!(matches!(
        repository
            .finalize_run(
                scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: running.plan_revision,
                    expected_wake_epoch: running.wake_epoch,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence: evidence,
                    terminal_reason,
                },
            )
            .await?,
        FinalizationOutcome::Finalized(_)
    ));

    let row: (i64, String, Option<String>, Option<Uuid>, Option<String>) = sqlx::query_as(
        r#"
            SELECT analytics_change_seq, source_kind, skill_template_ref,
                   skill_template_revision_uid, terminal_reason
            FROM moa.execution_run
            WHERE run_uid = $1
            "#,
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert!(row.0 > initial_change_seq);
    assert_eq!(row.1, "skill_template");
    assert_eq!(row.2.as_deref(), Some(expected_template_ref.as_str()));
    assert_eq!(row.3, Some(expected_template_revision_uid));
    assert_eq!(row.4.as_deref(), Some("completed"));
    let persisted = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("finalized analytics fixture must round trip through the repository");
    assert_eq!(
        persisted.source_provenance,
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: expected_template_ref.clone(),
            skill_template_revision_uid: expected_template_revision_uid,
        }
    );
    assert_eq!(persisted.source_kind, ExecutionSourceKind::SkillTemplate);
    assert_eq!(
        persisted.skill_template_ref.as_deref(),
        Some(expected_template_ref.as_str())
    );
    assert_eq!(
        persisted.skill_template_revision_uid,
        Some(expected_template_revision_uid)
    );
    assert_eq!(
        persisted.terminal_reason,
        Some(ExecutionTerminalReason::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn tenant_contact_and_control_plane_scopes_are_isolated_db() -> TestResult {
    // Pins: apply_contact_rls exposes exactly control-plane, tenant-null-contact, and matching-contact rows.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let contact_a = ContactId::new();
    let peer = ContactId::new();
    let tenant_scope = ExecutionScope::Tenant {
        tenant_id: tenant_a,
    };
    let contact_scope = ExecutionScope::Contact {
        tenant_id: tenant_a,
        contact_id: contact_a,
    };

    let tenant_run = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(
            tenant_a,
            None,
            "tenant-a",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let contact_run = create_run(
        &repository,
        ExecutionScope::Contact {
            tenant_id: tenant_a,
            contact_id: contact_a,
        },
        new_run(
            tenant_a,
            Some(contact_a),
            "contact-a",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let tenant_b_run = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_b,
        },
        new_run(
            tenant_b,
            None,
            "tenant-b",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    let tenant_tasks = (0..3)
        .map(|index| {
            logical_task(
                tenant_run.run_uid,
                "tenant",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    let contact_tasks = (0..3)
        .map(|index| {
            logical_task(
                contact_run.run_uid,
                "contact",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    let tenant_b_tasks = (0..3)
        .map(|index| {
            logical_task(
                tenant_b_run.run_uid,
                "tenant-b",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(tenant_scope, tenant_run.run_uid, 1, tenant_tasks.clone())
        .await?;
    repository
        .materialize_tasks(contact_scope, contact_run.run_uid, 1, contact_tasks.clone())
        .await?;
    repository
        .materialize_tasks(
            ExecutionScope::Tenant {
                tenant_id: tenant_b,
            },
            tenant_b_run.run_uid,
            1,
            tenant_b_tasks.clone(),
        )
        .await?;

    assert!(
        repository
            .load_run(tenant_scope, tenant_run.run_uid)
            .await?
            .is_some()
    );
    assert!(
        repository
            .load_run(tenant_scope, contact_run.run_uid)
            .await?
            .is_none()
    );
    assert_task_pages(
        &repository,
        tenant_scope,
        tenant_run.run_uid,
        &tenant_tasks,
        2,
    )
    .await?;
    assert!(
        repository
            .list_tasks(
                tenant_scope,
                contact_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );
    assert!(
        repository
            .load_run(tenant_scope, tenant_b_run.run_uid)
            .await?
            .is_none()
    );
    assert_task_pages(
        &repository,
        contact_scope,
        contact_run.run_uid,
        &contact_tasks,
        1,
    )
    .await?;
    assert!(
        repository
            .list_tasks(
                contact_scope,
                tenant_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );
    assert!(
        repository
            .list_tasks(
                ExecutionScope::Contact {
                    tenant_id: tenant_a,
                    contact_id: peer,
                },
                contact_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );

    assert!(
        repository
            .load_run(contact_scope, contact_run.run_uid)
            .await?
            .is_some()
    );
    assert!(
        repository
            .load_run(contact_scope, tenant_run.run_uid)
            .await?
            .is_none()
    );
    assert!(
        repository
            .load_run(
                ExecutionScope::Contact {
                    tenant_id: tenant_a,
                    contact_id: peer,
                },
                contact_run.run_uid,
            )
            .await?
            .is_none()
    );

    for run_uid in [
        tenant_run.run_uid,
        contact_run.run_uid,
        tenant_b_run.run_uid,
    ] {
        assert!(
            repository
                .load_run(ExecutionScope::ControlPlane, run_uid)
                .await?
                .is_some(),
            "control plane must see {run_uid}"
        );
    }
    assert_task_pages(
        &repository,
        ExecutionScope::ControlPlane,
        tenant_b_run.run_uid,
        &tenant_b_tasks,
        2,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn terminal_run_rows_require_typed_cause_and_requirement_counts_db() -> TestResult {
    // Pins: the final execution schema requires terminal cause, status-compatible
    // reason, and requirement coverage evidence to be persisted atomically.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "terminal-evidence-required",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;

    let missing_evidence = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(missing_evidence, "execution_run_terminal_evidence");

    for cause in [
        json!({"kind":"internal_failure","extra":true}),
        json!({"kind":"cancellation"}),
        json!({"kind":"limit_stop","reason":"not_a_limit"}),
    ] {
        let malformed = sqlx::query(
            "UPDATE moa.execution_run SET status = 'failed', terminal_cause = $2, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 0, completed_at = NOW() WHERE run_uid = $1",
        )
        .bind(run.run_uid)
        .bind(cause)
        .execute(test_db.store().pool())
        .await;
        assert_db_error_contains(malformed, "execution_run_terminal_evidence");
    }
    let invalid_counts = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"internal_failure\"}'::JSONB, terminal_satisfied_requirement_count = 2, terminal_requirement_count = 1, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(invalid_counts, "execution_run_terminal_evidence");
    let partial_evidence = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"internal_failure\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = NULL, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(partial_evidence, "execution_run_terminal_evidence");

    let unsupported_cause_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "unsupported-terminal-evidence-shape",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let unsupported_cause = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"removed_runtime\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 0, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(unsupported_cause_run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(unsupported_cause, "execution_run_terminal_evidence");
    let unchanged = repository
        .load_run(scope, unsupported_cause_run.run_uid)
        .await?
        .expect("rejected terminal mutation leaves the run visible");
    assert_eq!(unchanged.status, ExecutionRunStatus::Queued);
    assert!(unchanged.terminal_evidence.is_none());
    Ok(())
}

#[tokio::test]
async fn terminal_finalization_rejects_a_projection_changed_after_evaluation_db() -> TestResult {
    // Pins: cause and counts computed before a scheduling-relevant task mutation
    // cannot be committed as evidence for the newer locked projection.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "stale-terminal-projection",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let running =
        claim_running_controller(&repository, scope, &ExecutionConfig::default(), &run).await?;
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal_evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let terminal = TerminalProjection::Completed {
        output: json!({"result": "complete"}),
    };
    let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![logical_task(run.run_uid, "late-task", "", estimate(1))],
        )
        .await?;
    assert_eq!(
        repository
            .finalize_run(
                scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: 1,
                    expected_wake_epoch: running.wake_epoch,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence,
                    terminal_reason,
                },
            )
            .await?,
        FinalizationOutcome::Conflict
    );
    assert_eq!(
        repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible")
            .status,
        ExecutionRunStatus::Running
    );
    Ok(())
}

#[tokio::test]
async fn directly_queued_run_uses_one_database_timestamp_for_created_and_queued_db() -> TestResult {
    // Pins: a direct queued start cannot acquire an application-clock queue
    // timestamp distinct from the database-owned creation timestamp.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "direct-queued-at",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let (created_at, queued_at): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT created_at, queued_at FROM moa.execution_run WHERE run_uid = $1")
            .bind(run.run_uid)
            .fetch_one(test_db.store().pool())
            .await?;
    assert_eq!(created_at, queued_at);
    Ok(())
}

#[tokio::test]
async fn idempotency_is_scoped_and_null_contact_is_not_distinct_db() -> TestResult {
    // Pins: the partial unique key treats null contact as one tenant scope without crossing tenants or contacts.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let contact = ContactId::new();
    let key = "same-key";

    let first = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(tenant_a, None, key, ExecutionRunStatus::Queued, budget(10)),
    )
    .await?;
    let duplicate = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(tenant_a, None, key, ExecutionRunStatus::Queued, budget(20)),
    )
    .await?;
    let other_tenant = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_b,
        },
        new_run(tenant_b, None, key, ExecutionRunStatus::Queued, budget(10)),
    )
    .await?;
    let contact_scope = ExecutionScope::Contact {
        tenant_id: tenant_a,
        contact_id: contact,
    };
    let contact_run = create_run(
        &repository,
        contact_scope,
        new_run(
            tenant_a,
            Some(contact),
            key,
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    assert_eq!(duplicate.run_uid, first.run_uid);
    assert_eq!(duplicate.approved_budget, first.approved_budget);
    assert_ne!(other_tenant.run_uid, first.run_uid);
    assert_ne!(contact_run.run_uid, first.run_uid);
    Ok(())
}

#[tokio::test]
async fn database_rejects_illegal_run_and_task_transition_matrices_db() -> TestResult {
    // Pins: every run/task status class is enforced by PostgreSQL, not repository convention.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    const RUN_STATUSES: [&str; 13] = [
        "awaiting_confirmation",
        "queued",
        "running",
        "waiting_input",
        "waiting_review",
        "waiting_replan",
        "compensating",
        "completed",
        "partial",
        "blocked",
        "unsupported",
        "failed",
        "cancelled",
    ];
    const TASK_STATUSES: [&str; 9] = [
        "pending",
        "reserved",
        "running",
        "waiting_input",
        "waiting_replan",
        "completed",
        "skipped",
        "failed",
        "cancelled",
    ];

    let mut rejected_run_edges = 0;
    for source in RUN_STATUSES {
        for target in RUN_STATUSES {
            if source == target || run_transition_allowed(source, target) {
                continue;
            }
            let initial_status = if source == "awaiting_confirmation" {
                ExecutionRunStatus::AwaitingConfirmation
            } else {
                ExecutionRunStatus::Queued
            };
            let run = create_run(
                &repository,
                scope,
                new_run(
                    tenant_id,
                    None,
                    &format!("run-{source}-to-{target}"),
                    initial_status,
                    budget(10),
                ),
            )
            .await?;
            set_run_status_path(test_db.store().pool(), run.run_uid, run_setup_path(source))
                .await?;
            let error = sqlx::query("UPDATE moa.execution_run SET status = $2 WHERE run_uid = $1")
                .bind(run.run_uid)
                .bind(target)
                .execute(test_db.store().pool())
                .await
                .expect_err("contract-disallowed run transition must fail");
            assert!(
                error
                    .to_string()
                    .contains("invalid execution run status transition"),
                "unexpected error for run {source} -> {target}: {error}"
            );
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT status FROM moa.execution_run WHERE run_uid = $1",
                )
                .bind(run.run_uid)
                .fetch_one(test_db.store().pool())
                .await?,
                source,
                "rejected run transition must not mutate the row"
            );
            rejected_run_edges += 1;
        }
    }
    assert_eq!(rejected_run_edges, 111);

    let task_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "task-transition-matrix",
            ExecutionRunStatus::Queued,
            budget(100),
        ),
    )
    .await?;
    let mut task_cases = Vec::new();
    for source in TASK_STATUSES {
        for target in TASK_STATUSES {
            if source == target || task_transition_allowed(source, target) {
                continue;
            }
            let task = logical_task(
                task_run.run_uid,
                "transition",
                &format!("{source}-to-{target}"),
                estimate(1),
            );
            task_cases.push((source, target, task));
        }
    }
    repository
        .materialize_tasks(
            scope,
            task_run.run_uid,
            1,
            task_cases.iter().map(|(_, _, task)| task.clone()).collect(),
        )
        .await?;
    for (source, target, task) in &task_cases {
        set_task_status_path(
            test_db.store().pool(),
            task.task_id,
            task_setup_path(source),
        )
        .await?;
        let error = sqlx::query("UPDATE moa.execution_task SET status = $2 WHERE task_id = $1")
            .bind(task.task_id.as_uuid())
            .bind(target)
            .execute(test_db.store().pool())
            .await
            .expect_err("contract-disallowed task transition must fail");
        assert!(
            error
                .to_string()
                .contains("invalid execution task status transition"),
            "unexpected error for task {source} -> {target}: {error}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM moa.execution_task WHERE task_id = $1",
            )
            .bind(task.task_id.as_uuid())
            .fetch_one(test_db.store().pool())
            .await?,
            *source,
            "rejected task transition must not mutate the row"
        );
    }
    assert_eq!(task_cases.len(), 59);
    Ok(())
}

#[tokio::test]
async fn database_enforces_task_counter_history_and_immutable_field_guards_db() -> TestResult {
    // Pins: task counters, histories, and immutable fields are enforced by PostgreSQL.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let task_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "task-update-guards",
            ExecutionRunStatus::Queued,
            budget(100),
        ),
    )
    .await?;
    let guard_tasks = [
        logical_task(task_run.run_uid, "guard", "retry", estimate(1)),
        logical_task(task_run.run_uid, "guard", "resume", estimate(1)),
        logical_task(task_run.run_uid, "guard", "counter", estimate(1)),
        logical_task(task_run.run_uid, "guard", "immutable", estimate(1)),
        logical_task(task_run.run_uid, "guard", "history", estimate(1)),
    ];
    repository
        .materialize_tasks(scope, task_run.run_uid, 1, guard_tasks.to_vec())
        .await?;
    set_task_status_path(
        test_db.store().pool(),
        guard_tasks[0].task_id,
        task_setup_path("running"),
    )
    .await?;
    let retry_before =
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id).await?;
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET attempt = attempt + 1, \
                 generation = generation + 2, \
                 generation_history = generation_history || jsonb_build_array( \
                     jsonb_build_object( \
                         'kind', 'retry', \
                         'attempt', attempt + 1, \
                         'generation', generation + 2 \
                     ) \
                 ) \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[0].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution retry must increment attempt and generation together",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id,).await?,
        retry_before,
        "malformed retry counters and history must roll back together"
    );
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET attempt = attempt + 1, \
                 generation = generation + 1, \
                 generation_history = '[]'::JSONB \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[0].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution task histories are append-only",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id,).await?,
        retry_before,
        "paired retry counters cannot replace generation history"
    );
    set_task_status_path(
        test_db.store().pool(),
        guard_tasks[1].task_id,
        task_setup_path("waiting_input"),
    )
    .await?;
    let resume_before =
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[1].task_id).await?;
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET status = 'running', \
                 attempt = attempt + 1, \
                 generation = generation + 1, \
                 generation_history = generation_history || jsonb_build_array( \
                     jsonb_build_object( \
                         'kind', 'input_resume', \
                         'attempt', attempt + 1, \
                         'generation', generation + 1 \
                     ) \
                 ) \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[1].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution input resume must increment only generation",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[1].task_id,).await?,
        resume_before,
        "input resume attempt mutation must roll back the full task projection"
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_task SET generation = generation + 1 WHERE task_id = $1")
            .bind(guard_tasks[2].task_id.as_uuid())
            .execute(test_db.store().pool())
            .await,
        "execution task counters changed outside retry or input resume",
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_task SET node_id = 'changed' WHERE task_id = $1")
            .bind(guard_tasks[3].task_id.as_uuid())
            .execute(test_db.store().pool())
            .await,
        "execution task immutable fields cannot change",
    );
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task SET generation_history = '[]'::JSONB WHERE task_id = $1",
        )
        .bind(guard_tasks[4].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution task histories are append-only",
    );
    Ok(())
}

#[tokio::test]
async fn database_enforces_run_immutable_and_plan_update_guards_db() -> TestResult {
    // Pins: run immutable fields and fenced plan updates are enforced by PostgreSQL.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run_guard = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "run-update-guards",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_run SET input = '{}'::JSONB WHERE run_uid = $1")
            .bind(run_guard.run_uid)
            .execute(test_db.store().pool())
            .await,
        "execution run immutable fields cannot change",
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_run SET active_plan = '{}'::JSONB WHERE run_uid = $1")
            .bind(run_guard.run_uid)
            .execute(test_db.store().pool())
            .await,
        "execution run plan changes require one fenced history append",
    );
    Ok(())
}
