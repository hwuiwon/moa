//! Database contract coverage for recurring durable execution schedules.

use chrono::{Duration, Utc};
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCancelPolicy, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionPlanDefinition, RetryPolicy,
};
use moa_config::ExecutionConfig;
use moa_core::{
    traits::{Identity, IdentityType},
    types::{
        execution_planning::{
            ExecutionScheduleCreateRequest, ExecutionScheduleDstPolicy,
            ExecutionScheduleMissedFirePolicy, ExecutionScheduleOrigin,
            ExecutionScheduleOriginSource, ExecutionScheduleOverlapPolicy, ExecutionSchedulePolicy,
            ExecutionScheduleStatus, ExecutionScheduleTemplate, ExecutionScheduleUpdateRequest,
            ExecutionSourceProvenance, execution_schedule_template_hash,
        },
        identifiers::{SessionId, TenantId},
    },
};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    schedule::{
        ExecutionScheduleCreateOutcome, ExecutionScheduleMutationOutcome,
        ExecutionScheduleOccurrence, ExecutionScheduleRunAdmission,
        ExecutionScheduleRunAdmissionOutcome, ExecutionScheduleRunBlueprint,
        execution_schedule_run_blueprint,
    },
};
use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash,
    },
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn schedule_create_pause_resume_is_tenant_scoped_and_generation_fenced_db() -> TestResult {
    // Pins: one committed schedule carries exact immutable identity/provenance, an occurrence
    // trigger and outbox row commit together, and pause/resume invalidate the old incarnation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let other_tenant_id = TenantId::new();
    let schedule_uid = Uuid::now_v7();
    let scope = ExecutionScope::Tenant { tenant_id };
    let now = moa_test_support::fixtures::pg_now();
    let first = ExecutionScheduleOccurrence {
        at: now + Duration::minutes(5),
        local: (now + Duration::minutes(5)).naive_utc(),
    };
    let mut request = schedule_request(tenant_id, schedule_uid, now);
    request.policy.start_at = now - Duration::minutes(1);

    let created = repository
        .create_schedule(
            scope,
            &moa_config::ExecutionConfig::default(),
            request.clone(),
            Some(first),
        )
        .await?;
    let ExecutionScheduleCreateOutcome::Created { schedule, trigger } = created else {
        panic!("fresh schedule must be created");
    };
    assert_eq!(schedule.template, request.template);
    assert_eq!(schedule.run_as_identity, request.run_as_identity);
    assert_eq!(schedule.origin, request.origin);
    assert_eq!(schedule.schedule_incarnation, 1);
    let blueprint = execution_schedule_run_blueprint(&schedule)?;
    sqlx::query(
        "INSERT INTO moa.execution_planning_context (\
             planning_context_uid, tenant_id, session_id, originating_user_sequence_num, \
             originating_user_event_hash, owner_user_id, planning_context_hash, snapshot\
         ) VALUES ($1, $2, $3, $4, $5, $6, $5, '{}'::JSONB)",
    )
    .bind(blueprint.planning_context_uid)
    .bind(tenant_id.0)
    .bind(blueprint.session_id.0)
    .bind(i64::try_from(blueprint.originating_user_sequence_num)?)
    .bind(blueprint.planning_context_hash.to_string())
    .bind(schedule.run_as_identity.id.to_string())
    .execute(&pool)
    .await?;
    let trigger = trigger.expect("active schedule must atomically arm a trigger");
    assert_eq!(trigger.trigger.schedule_incarnation, Some(1));
    assert_eq!(trigger.trigger.occurrence_sequence, Some(1));
    assert_eq!(
        trigger.dispatch.trigger_uid,
        Some(trigger.trigger.trigger_uid)
    );
    assert!(
        repository
            .load_schedule(
                ExecutionScope::Tenant {
                    tenant_id: other_tenant_id,
                },
                other_tenant_id,
                schedule_uid,
            )
            .await?
            .is_none(),
        "forced tenant RLS must hide the schedule from another tenant"
    );

    let paused = repository
        .pause_schedule(
            scope,
            &moa_config::ExecutionConfig::default(),
            tenant_id,
            schedule_uid,
        )
        .await?;
    let ExecutionScheduleMutationOutcome::Updated {
        schedule: paused, ..
    } = paused
    else {
        panic!("active schedule must pause");
    };
    assert_eq!(paused.status, ExecutionScheduleStatus::Paused);
    assert_eq!(paused.schedule_incarnation, 2);
    assert_eq!(paused.next_occurrence_at, None);

    let next = ExecutionScheduleOccurrence {
        at: now - Duration::seconds(1),
        local: (now - Duration::seconds(1)).naive_utc(),
    };
    let resumed = repository
        .resume_schedule(
            scope,
            &moa_config::ExecutionConfig::default(),
            tenant_id,
            schedule_uid,
            Some(next),
        )
        .await?;
    let ExecutionScheduleMutationOutcome::Updated {
        schedule: resumed,
        trigger: Some(resumed_trigger),
    } = resumed
    else {
        panic!("paused schedule must resume and arm a new trigger");
    };
    assert_eq!(resumed.status, ExecutionScheduleStatus::Active);
    assert_eq!(resumed.schedule_incarnation, 3);
    assert_eq!(resumed_trigger.trigger.schedule_incarnation, Some(3));
    assert_ne!(
        resumed_trigger.trigger.trigger_uid,
        trigger.trigger.trigger_uid
    );
    let occurrence_run = blueprint.instantiate(&resumed, next, 1, 30 * 24 * 60 * 60)?;
    assert_eq!(
        occurrence_run.approved_budget.deadline_at,
        Some(next.at + Duration::hours(2)),
        "a resumed recurrence must receive a fresh occurrence-relative deadline"
    );

    let old_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(trigger.trigger.trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(old_state, "superseded");

    let admission = repository
        .admit_schedule_occurrence(
            scope,
            &moa_config::ExecutionConfig::default(),
            ExecutionScheduleRunAdmission {
                tenant_id,
                schedule_uid,
                schedule_incarnation: resumed.schedule_incarnation,
                occurrence_sequence: 1,
                trigger_uid: resumed_trigger.trigger.trigger_uid,
                trigger_dispatch_uid: resumed_trigger.dispatch.dispatch_uid,
                occurrence: next,
                run: occurrence_run.clone(),
                next_occurrence: None,
            },
        )
        .await?;
    let ExecutionScheduleRunAdmissionOutcome::Admitted {
        run, activation, ..
    } = admission
    else {
        panic!("due resumed occurrence must admit one fresh run");
    };
    assert_eq!(run.controller_generation, 1);
    assert_eq!(run.wake_epoch, 1);
    assert_eq!(run.processed_wake_epoch, 0);
    assert_eq!(activation.controller_generation, Some(1));
    assert_eq!(activation.wake_epoch, Some(1));
    let seeded_node: (String, i64, String) = sqlx::query_as(
        "SELECT node_id, remaining_dependency_count, node_status \
         FROM moa.execution_node_state WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        seeded_node,
        ("output".to_string(), 0, "pending".to_string())
    );
    let deadline_shape: (String, chrono::DateTime<Utc>, String, String) = sqlx::query_as(
        "SELECT trigger.state, trigger.due_at, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid=trigger.trigger_uid \
          AND dispatch.dispatch_kind='trigger_delivery' \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.trigger_uid=trigger.trigger_uid \
          AND capacity.resource_dimension='scheduled_triggers' \
         WHERE trigger.run_uid=$1 AND trigger.trigger_kind='run_deadline'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(deadline_shape.0, "pending");
    assert_eq!(deadline_shape.1, next.at + Duration::hours(2));
    assert_eq!(deadline_shape.2, "pending");
    assert_eq!(deadline_shape.3, "reserved");

    let replay = repository
        .admit_schedule_occurrence(
            scope,
            &moa_config::ExecutionConfig::default(),
            ExecutionScheduleRunAdmission {
                tenant_id,
                schedule_uid,
                schedule_incarnation: resumed.schedule_incarnation,
                occurrence_sequence: 1,
                trigger_uid: resumed_trigger.trigger.trigger_uid,
                trigger_dispatch_uid: resumed_trigger.dispatch.dispatch_uid,
                occurrence: next,
                run: occurrence_run,
                next_occurrence: None,
            },
        )
        .await?;
    assert_eq!(
        replay,
        ExecutionScheduleRunAdmissionOutcome::Replayed {
            run_uid: Some(run.run_uid),
            activation_dispatch_uid: Some(activation.dispatch_uid),
        },
        "replay after completion clears mutable next fields but remains an accepted no-op"
    );
    Ok(())
}

#[tokio::test]
async fn schedule_update_binds_every_mutable_field_to_its_named_column_db() -> TestResult {
    // Pins: schedule policy replacement must bind each SQL parameter exactly once so changes to
    // the name cannot shift timezone, calendar, policy, concurrency, budget, or time boundaries.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let now = moa_test_support::fixtures::pg_now();
    let original_occurrence = ExecutionScheduleOccurrence {
        at: now + Duration::minutes(5),
        local: (now + Duration::minutes(5)).naive_utc(),
    };
    let create = schedule_request(tenant_id, Uuid::now_v7(), now);
    let ExecutionScheduleCreateOutcome::Created {
        schedule,
        trigger: Some(original_trigger),
    } = repository
        .create_schedule(
            scope,
            &ExecutionConfig::default(),
            create,
            Some(original_occurrence),
        )
        .await?
    else {
        panic!("update fixture must create one active schedule");
    };
    let next = ExecutionScheduleOccurrence {
        at: now + Duration::hours(3),
        local: (now + Duration::hours(3)).naive_utc(),
    };
    let updated_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(2_001),
        max_tokens: Some(3_002),
        max_tasks: Some(4),
        max_tool_calls: Some(5),
        max_retrieved_bytes: Some(6_003),
        deadline_at: None,
    };
    let update = ExecutionScheduleUpdateRequest {
        tenant_id,
        schedule_uid: schedule.schedule_uid,
        expected_incarnation: schedule.schedule_incarnation,
        name: "monthly close report".to_string(),
        policy: ExecutionSchedulePolicy {
            timezone: "America/New_York".to_string(),
            calendar_expression: "0 30 17 1 * *".to_string(),
            start_at: now + Duration::hours(1),
            end_at: Some(now + Duration::days(90)),
            missed_fire_policy: ExecutionScheduleMissedFirePolicy::Skip,
            overlap_policy: ExecutionScheduleOverlapPolicy::Allow,
            dst_policy: ExecutionScheduleDstPolicy::Latest,
            maximum_concurrent_runs: 7,
            occurrence_budget: serde_json::to_value(&updated_budget)?,
        },
    };

    let ExecutionScheduleMutationOutcome::Updated {
        schedule: updated,
        trigger: Some(updated_trigger),
    } = repository
        .update_schedule(
            scope,
            &ExecutionConfig::default(),
            update.clone(),
            Some(next),
        )
        .await?
    else {
        panic!("exact current incarnation must accept the policy replacement");
    };
    assert_eq!(updated.name, update.name);
    assert_eq!(updated.policy, update.policy);
    assert_eq!(
        updated.schedule_incarnation,
        schedule.schedule_incarnation + 1
    );
    assert_eq!(updated.next_occurrence_at, Some(next.at));
    assert_eq!(updated.next_occurrence_local, Some(next.local));
    assert_eq!(updated.template, schedule.template);
    assert_eq!(updated.run_as_identity, schedule.run_as_identity);
    assert_eq!(updated.origin, schedule.origin);
    assert_eq!(
        updated_trigger.trigger.schedule_incarnation,
        Some(updated.schedule_incarnation)
    );
    let original_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(original_trigger.trigger.trigger_uid)
            .fetch_one(test_db.store().pool())
            .await?;
    assert_eq!(original_state, "superseded");
    Ok(())
}

#[tokio::test]
async fn concurrent_schedule_update_and_occurrence_fire_share_capacity_first_lock_order_db()
-> TestResult {
    // Pins: schedule CRUD and occurrence delivery must acquire ScheduledTriggers capacity before
    // the schedule row, so either mutation wins atomically without a schedule/trigger deadlock.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let due_at = moa_test_support::fixtures::pg_now() - Duration::seconds(1);
    let due = ExecutionScheduleOccurrence {
        at: due_at,
        local: due_at.naive_utc(),
    };
    let create = schedule_request(tenant_id, Uuid::now_v7(), due_at);
    let ExecutionScheduleCreateOutcome::Created {
        schedule,
        trigger: Some(trigger),
    } = repository
        .create_schedule(scope, &config, create, Some(due))
        .await?
    else {
        panic!("concurrency fixture must create one due schedule");
    };
    let run = execution_schedule_run_blueprint(&schedule)?.instantiate(
        &schedule,
        due,
        1,
        config.maximum_horizon_seconds,
    )?;
    let next = ExecutionScheduleOccurrence {
        at: due_at + Duration::hours(1),
        local: (due_at + Duration::hours(1)).naive_utc(),
    };
    let update = ExecutionScheduleUpdateRequest {
        tenant_id,
        schedule_uid: schedule.schedule_uid,
        expected_incarnation: schedule.schedule_incarnation,
        name: "concurrent replacement".to_string(),
        policy: ExecutionSchedulePolicy {
            end_at: Some(due_at + Duration::days(60)),
            ..schedule.policy.clone()
        },
    };
    let admission = ExecutionScheduleRunAdmission {
        tenant_id,
        schedule_uid: schedule.schedule_uid,
        schedule_incarnation: schedule.schedule_incarnation,
        occurrence_sequence: 1,
        trigger_uid: trigger.trigger.trigger_uid,
        trigger_dispatch_uid: trigger.dispatch.dispatch_uid,
        occurrence: due,
        run,
        next_occurrence: None,
    };

    let (update_result, fire_result) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                repository.update_schedule(scope, &config, update, Some(next)),
                repository.admit_schedule_occurrence(scope, &config, admission),
            )
        })
        .await
        .expect("capacity-first lock order must complete both contenders without deadlock");
    let update_result = update_result?;
    let fire_result = fire_result?;
    assert!(
        matches!(
            (&update_result, &fire_result),
            (
                ExecutionScheduleMutationOutcome::Updated { .. },
                ExecutionScheduleRunAdmissionOutcome::Stale
            ) | (
                ExecutionScheduleMutationOutcome::Stale,
                ExecutionScheduleRunAdmissionOutcome::Admitted { .. }
            )
        ),
        "exactly one schedule-row mutation must win: update={update_result:?}, fire={fire_result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn schedule_occurrence_respects_joint_active_and_parked_resident_ceiling_db() -> TestResult {
    // Pins: schedule-owned admission cannot bypass the joint resident-run ceiling; saturation
    // consumes the occurrence once and leaves no run, activation, or capacity receipt to replay.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = moa_config::ExecutionConfig {
        max_tenant_active_runs: 1,
        max_fleet_active_runs: 1,
        max_tenant_parked_runs: 1,
        max_fleet_parked_runs: 1,
        ..moa_config::ExecutionConfig::default()
    };
    config.validate()?;
    let due_at = moa_test_support::fixtures::pg_now() - Duration::seconds(1);
    let due = ExecutionScheduleOccurrence {
        at: due_at,
        local: due_at.naive_utc(),
    };

    let first_request = schedule_request(tenant_id, Uuid::now_v7(), due.at);
    let ExecutionScheduleCreateOutcome::Created {
        schedule: first_schedule,
        trigger: Some(first_trigger),
    } = repository
        .create_schedule(scope, &config, first_request, Some(due))
        .await?
    else {
        panic!("first resident schedule must arm its occurrence");
    };
    let first_blueprint = execution_schedule_run_blueprint(&first_schedule)?;
    let first_run =
        first_blueprint.instantiate(&first_schedule, due, 1, config.maximum_horizon_seconds)?;
    insert_schedule_planning_context(
        &pool,
        tenant_id,
        first_schedule.run_as_identity.id,
        &first_blueprint,
    )
    .await?;
    let ExecutionScheduleRunAdmissionOutcome::Admitted {
        run: admitted_run, ..
    } = repository
        .admit_schedule_occurrence(
            scope,
            &config,
            ExecutionScheduleRunAdmission {
                tenant_id,
                schedule_uid: first_schedule.schedule_uid,
                schedule_incarnation: first_schedule.schedule_incarnation,
                occurrence_sequence: 1,
                trigger_uid: first_trigger.trigger.trigger_uid,
                trigger_dispatch_uid: first_trigger.dispatch.dispatch_uid,
                occurrence: due,
                run: first_run,
                next_occurrence: None,
            },
        )
        .await?
    else {
        panic!("first occurrence must consume the sole resident entitlement");
    };

    let second_request = schedule_request(tenant_id, Uuid::now_v7(), due.at);
    let ExecutionScheduleCreateOutcome::Created {
        schedule: second_schedule,
        trigger: Some(second_trigger),
    } = repository
        .create_schedule(scope, &config, second_request, Some(due))
        .await?
    else {
        panic!("second schedule must arm before its resident admission check");
    };
    let second_blueprint = execution_schedule_run_blueprint(&second_schedule)?;
    let second_run =
        second_blueprint.instantiate(&second_schedule, due, 1, config.maximum_horizon_seconds)?;
    insert_schedule_planning_context(
        &pool,
        tenant_id,
        second_schedule.run_as_identity.id,
        &second_blueprint,
    )
    .await?;
    let replay_run = second_run.clone();
    let saturated_request = ExecutionScheduleRunAdmission {
        tenant_id,
        schedule_uid: second_schedule.schedule_uid,
        schedule_incarnation: second_schedule.schedule_incarnation,
        occurrence_sequence: 1,
        trigger_uid: second_trigger.trigger.trigger_uid,
        trigger_dispatch_uid: second_trigger.dispatch.dispatch_uid,
        occurrence: due,
        run: second_run,
        next_occurrence: None,
    };
    let saturated = repository
        .admit_schedule_occurrence(scope, &config, saturated_request)
        .await?;
    assert!(
        matches!(
            saturated,
            ExecutionScheduleRunAdmissionOutcome::Skipped { .. }
        ),
        "joint resident saturation must consume and skip the occurrence"
    );

    let second_run_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moa.execution_run WHERE schedule_uid=$1")
            .bind(second_schedule.schedule_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(second_run_count, 0);
    let resident_receipts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT resource_dimension, count(*) \
         FROM moa.execution_capacity_reservation \
         WHERE tenant_id=$1 AND state IN ('reserved','reconciling') \
           AND resource_dimension IN ('active_runs','parked_runs') \
         GROUP BY resource_dimension ORDER BY resource_dimension",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(resident_receipts, vec![("active_runs".to_string(), 1)]);
    let second_trigger_state: (String, String) = sqlx::query_as(
        "SELECT trigger.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.trigger_uid=trigger.trigger_uid \
          AND capacity.resource_dimension='scheduled_triggers' \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(second_trigger.trigger.trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        second_trigger_state,
        ("delivered".to_string(), "released".to_string())
    );

    assert_eq!(
        repository
            .admit_schedule_occurrence(
                scope,
                &config,
                ExecutionScheduleRunAdmission {
                    tenant_id,
                    schedule_uid: second_schedule.schedule_uid,
                    schedule_incarnation: second_schedule.schedule_incarnation,
                    occurrence_sequence: 1,
                    trigger_uid: second_trigger.trigger.trigger_uid,
                    trigger_dispatch_uid: second_trigger.dispatch.dispatch_uid,
                    occurrence: due,
                    run: replay_run,
                    next_occurrence: None,
                },
            )
            .await?,
        ExecutionScheduleRunAdmissionOutcome::Replayed {
            run_uid: None,
            activation_dispatch_uid: None,
        }
    );
    let first_run_still_present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM moa.execution_run WHERE run_uid=$1)")
            .bind(admitted_run.run_uid)
            .fetch_one(&pool)
            .await?;
    assert!(first_run_still_present);
    Ok(())
}

#[tokio::test]
async fn schedule_overlap_probes_are_indexed_and_bounded_across_large_terminal_history_db()
-> TestResult {
    // Pins: terminal occurrence history never expands an overlap decision. Skip and QueueOne
    // stop at one indexed row, while Allow reads no more than maximum_concurrent_runs even when
    // more live rows and thousands of terminal rows exist for the same schedule.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let now = moa_test_support::fixtures::pg_now();

    for (ordinal, overlap_policy, maximum_concurrent_runs, extra_queued) in [
        (1_i64, ExecutionScheduleOverlapPolicy::Skip, 1_u64, 0_i64),
        (2, ExecutionScheduleOverlapPolicy::QueueOne, 1, 0),
        (3, ExecutionScheduleOverlapPolicy::Allow, 4, 8),
    ] {
        let schedule_uid = Uuid::now_v7();
        let occurrence = ExecutionScheduleOccurrence {
            at: now - Duration::seconds(20 - ordinal),
            local: (now - Duration::seconds(20 - ordinal)).naive_utc(),
        };
        let next_occurrence = ExecutionScheduleOccurrence {
            at: now - Duration::seconds(1),
            local: (now - Duration::seconds(1)).naive_utc(),
        };
        let mut request = schedule_request(tenant_id, schedule_uid, occurrence.at);
        request.policy.start_at = occurrence.at - Duration::minutes(1);
        request.policy.overlap_policy = overlap_policy;
        request.policy.maximum_concurrent_runs = maximum_concurrent_runs;
        let ExecutionScheduleCreateOutcome::Created {
            schedule,
            trigger: Some(trigger),
        } = repository
            .create_schedule(scope, &config, request, Some(occurrence))
            .await?
        else {
            panic!("overlap fixture must create one due schedule");
        };
        let blueprint = execution_schedule_run_blueprint(&schedule)?;
        insert_schedule_planning_context(&pool, tenant_id, schedule.run_as_identity.id, &blueprint)
            .await?;
        let first_run =
            blueprint.instantiate(&schedule, occurrence, 1, config.maximum_horizon_seconds)?;
        let ExecutionScheduleRunAdmissionOutcome::Admitted {
            run,
            next_trigger: Some(next_trigger),
            ..
        } = repository
            .admit_schedule_occurrence(
                scope,
                &config,
                ExecutionScheduleRunAdmission {
                    tenant_id,
                    schedule_uid,
                    schedule_incarnation: schedule.schedule_incarnation,
                    occurrence_sequence: 1,
                    trigger_uid: trigger.trigger.trigger_uid,
                    trigger_dispatch_uid: trigger.dispatch.dispatch_uid,
                    occurrence,
                    run: first_run,
                    next_occurrence: Some(next_occurrence),
                },
            )
            .await?
        else {
            panic!("first overlap fixture occurrence must be admitted");
        };

        seed_schedule_overlap_history(&pool, run.run_uid, 2_501, extra_queued).await?;
        sqlx::query("ANALYZE moa.execution_run")
            .execute(&pool)
            .await?;
        assert_schedule_overlap_probe(
            &pool,
            tenant_id,
            schedule_uid,
            overlap_policy,
            maximum_concurrent_runs,
        )
        .await?;

        let current = repository
            .load_schedule(scope, tenant_id, schedule_uid)
            .await?
            .expect("advanced overlap fixture schedule remains visible");
        let second_run =
            blueprint.instantiate(&current, next_occurrence, 2, config.maximum_horizon_seconds)?;
        assert!(matches!(
            repository
                .admit_schedule_occurrence(
                    scope,
                    &config,
                    ExecutionScheduleRunAdmission {
                        tenant_id,
                        schedule_uid,
                        schedule_incarnation: current.schedule_incarnation,
                        occurrence_sequence: 2,
                        trigger_uid: next_trigger.trigger.trigger_uid,
                        trigger_dispatch_uid: next_trigger.dispatch.dispatch_uid,
                        occurrence: next_occurrence,
                        run: second_run,
                        next_occurrence: None,
                    },
                )
                .await?,
            ExecutionScheduleRunAdmissionOutcome::Skipped { .. }
        ));
    }
    Ok(())
}

async fn insert_schedule_planning_context(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    owner_id: Uuid,
    blueprint: &ExecutionScheduleRunBlueprint,
) -> TestResult {
    sqlx::query(
        "INSERT INTO moa.execution_planning_context (\
             planning_context_uid, tenant_id, session_id, originating_user_sequence_num, \
             originating_user_event_hash, owner_user_id, planning_context_hash, snapshot\
         ) VALUES ($1, $2, $3, $4, $5, $6, $5, '{}'::JSONB)",
    )
    .bind(blueprint.planning_context_uid)
    .bind(tenant_id.0)
    .bind(blueprint.session_id.0)
    .bind(i64::try_from(blueprint.originating_user_sequence_num)?)
    .bind(blueprint.planning_context_hash.to_string())
    .bind(owner_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_schedule_overlap_history(
    pool: &sqlx::PgPool,
    template_run_uid: Uuid,
    terminal_count: i64,
    extra_queued_count: i64,
) -> TestResult {
    let mut transaction = pool.begin().await?;
    let insert_columns: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(attname::TEXT) FROM pg_catalog.pg_attribute \
         WHERE attrelid='moa.execution_run'::REGCLASS AND attnum>0 \
           AND NOT attisdropped AND attgenerated='' ORDER BY attnum",
    )
    .fetch_all(transaction.as_mut())
    .await?;
    let column_list = insert_columns.join(", ");
    let selected_columns = insert_columns
        .iter()
        .map(|column| format!("populated.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query("ALTER TABLE moa.execution_run DISABLE TRIGGER USER")
        .execute(transaction.as_mut())
        .await?;
    let terminal_sql = format!(
        "INSERT INTO moa.execution_run ({column_list}) \
         SELECT {selected_columns} \
         FROM moa.execution_run AS template_run \
         CROSS JOIN generate_series(1, $2::BIGINT) AS series(ordinal) \
         CROSS JOIN LATERAL jsonb_populate_record(\
             NULL::moa.execution_run, \
             to_jsonb(template_run) || jsonb_build_object(\
                 'run_uid', gen_random_uuid(), \
                 'idempotency_key', NULL, \
                 'schedule_occurrence_sequence', 10000 + series.ordinal, \
                 'status', 'failed', \
                 'activation_state', 'terminal', \
                 'terminal_cause', jsonb_build_object('kind', 'internal_failure'), \
                 'terminal_satisfied_requirement_count', 0, \
                 'terminal_requirement_count', 0, \
                 'terminal_reason', 'internal_failure', \
                 'completed_at', now(), \
                 'processed_wake_epoch', template_run.wake_epoch, \
                 'next_wake_at', NULL\
             )\
         ) AS populated \
         WHERE template_run.run_uid=$1"
    );
    let terminal = sqlx::query(&terminal_sql)
        .bind(template_run_uid)
        .bind(terminal_count)
        .execute(transaction.as_mut())
        .await?;
    assert_eq!(terminal.rows_affected(), u64::try_from(terminal_count)?);
    let queued_sql = format!(
        "INSERT INTO moa.execution_run ({column_list}) \
         SELECT {selected_columns} \
         FROM moa.execution_run AS template_run \
         CROSS JOIN generate_series(1, $2::BIGINT) AS series(ordinal) \
         CROSS JOIN LATERAL jsonb_populate_record(\
             NULL::moa.execution_run, \
             to_jsonb(template_run) || jsonb_build_object(\
                 'run_uid', gen_random_uuid(), \
                 'idempotency_key', NULL, \
                 'schedule_occurrence_sequence', 20000 + series.ordinal\
             )\
         ) AS populated \
         WHERE template_run.run_uid=$1"
    );
    let queued = sqlx::query(&queued_sql)
        .bind(template_run_uid)
        .bind(extra_queued_count)
        .execute(transaction.as_mut())
        .await?;
    assert_eq!(queued.rows_affected(), u64::try_from(extra_queued_count)?);
    sqlx::query("ALTER TABLE moa.execution_run ENABLE TRIGGER USER")
        .execute(transaction.as_mut())
        .await?;
    transaction.commit().await?;
    Ok(())
}

async fn assert_schedule_overlap_probe(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    schedule_uid: Uuid,
    overlap_policy: ExecutionScheduleOverlapPolicy,
    maximum_concurrent_runs: u64,
) -> TestResult {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(transaction.as_mut())
        .await?;
    let (explain, expected_index, expected_rows) = match overlap_policy {
        ExecutionScheduleOverlapPolicy::Skip => (
            sqlx::query_scalar(
                "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) \
                 SELECT EXISTS (SELECT 1 FROM moa.execution_run \
                 WHERE tenant_id=$1 AND schedule_uid=$2 \
                   AND status NOT IN \
                     ('completed','partial','blocked','unsupported','failed','cancelled'))",
            )
            .bind(tenant_id.0)
            .bind(schedule_uid)
            .fetch_one(transaction.as_mut())
            .await?,
            "execution_run_schedule_nonterminal_idx",
            1,
        ),
        ExecutionScheduleOverlapPolicy::QueueOne => (
            sqlx::query_scalar(
                "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) \
                 SELECT EXISTS (SELECT 1 FROM moa.execution_run \
                 WHERE tenant_id=$1 AND schedule_uid=$2 AND status='queued')",
            )
            .bind(tenant_id.0)
            .bind(schedule_uid)
            .fetch_one(transaction.as_mut())
            .await?,
            "execution_run_schedule_queued_idx",
            1,
        ),
        ExecutionScheduleOverlapPolicy::Allow => (
            sqlx::query_scalar(
                "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) \
                 SELECT count(*) FROM (SELECT 1 FROM moa.execution_run \
                 WHERE tenant_id=$1 AND schedule_uid=$2 \
                   AND status NOT IN \
                     ('completed','partial','blocked','unsupported','failed','cancelled') \
                 LIMIT $3) AS bounded_nonterminal_runs",
            )
            .bind(tenant_id.0)
            .bind(schedule_uid)
            .bind(i64::try_from(maximum_concurrent_runs)?)
            .fetch_one(transaction.as_mut())
            .await?,
            "execution_run_schedule_nonterminal_idx",
            maximum_concurrent_runs,
        ),
    };
    let scan = explain_index_scan(&explain, expected_index)
        .expect("overlap probe must use its policy-specific partial index");
    assert_eq!(
        scan.get("Actual Rows").and_then(serde_json::Value::as_u64),
        Some(expected_rows),
        "overlap probe must stop at its semantic bound"
    );
    transaction.rollback().await?;
    Ok(())
}

fn explain_index_scan<'a>(
    value: &'a serde_json::Value,
    expected_index: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("Index Name").and_then(serde_json::Value::as_str) == Some(expected_index)
            {
                return Some(object);
            }
            object
                .values()
                .find_map(|child| explain_index_scan(child, expected_index))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| explain_index_scan(child, expected_index)),
        _ => None,
    }
}

fn schedule_request(
    tenant_id: TenantId,
    schedule_uid: Uuid,
    now: chrono::DateTime<Utc>,
) -> ExecutionScheduleCreateRequest {
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let revision_uid = Uuid::now_v7();
    let approved_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(1_000),
        max_tokens: Some(1_000),
        max_tasks: Some(10),
        max_tool_calls: Some(100),
        max_retrieved_bytes: Some(10_000),
        deadline_at: None,
    };
    let catalog =
        ExecutionCapabilityCatalog::build(Vec::new()).expect("empty capability catalog is valid");
    let blueprint = ExecutionScheduleRunBlueprint {
        session_id: SessionId::new(),
        originating_user_sequence_num: 1,
        planning_context_uid: Uuid::now_v7(),
        planning_context_hash: ExecutionHash::from_bytes([9; 32]),
        goal: ExecutionGoalContract {
            objective: "produce the recurring weekday report".to_string(),
            requirements: Vec::new(),
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: CanonicalExecutionPlan {
            definition: ExecutionPlanDefinition {
                cancel_policy: ExecutionCancelPolicy::RetainEffects,
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
                nodes: vec![ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: Vec::new(),
                    depends_on: Vec::new(),
                    when: None,
                    input: serde_json::json!({}),
                    output_schema: serde_json::json!({"type":"object"}),
                    operation: ExecutionOperation::Output {
                        value: serde_json::json!({"report":"ready"}),
                    },
                    compensation: None,
                    retry: RetryPolicy {
                        max_attempts: 1,
                        initial_backoff_ms: 1,
                        max_backoff_ms: 1,
                    },
                    budget: None,
                }],
            },
            plan_hash: ExecutionHash::from_bytes([10; 32]),
            catalog_hash: catalog.catalog_hash,
            estimate: ExecutionEstimate {
                cost_microusd: 1,
                tokens: 1,
                tasks: 1,
                tool_calls: 1,
                retrieved_bytes: 1,
            },
            report: ExecutionValidationReport::default(),
        },
        catalog,
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        source_provenance: ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: "skill://weekday-report".to_string(),
            skill_template_revision_uid: revision_uid,
        },
        input: serde_json::json!({"report":"weekday"}),
        approved_budget: approved_budget.clone(),
        deadline_offset_seconds: Some(2 * 60 * 60),
    };
    let template_snapshot =
        serde_json::to_value(blueprint).expect("schedule blueprint fixture must serialize");
    ExecutionScheduleCreateRequest {
        tenant_id,
        schedule_uid,
        name: "weekday report".to_string(),
        template: ExecutionScheduleTemplate {
            revision_uid,
            template_hash: execution_schedule_template_hash(&template_snapshot)
                .expect("template fixture must canonicalize"),
            snapshot: template_snapshot,
        },
        run_as_identity: identity.clone(),
        origin: ExecutionScheduleOrigin {
            request_uid: Uuid::now_v7(),
            created_by: identity,
            source: ExecutionScheduleOriginSource::TenantApi,
        },
        policy: ExecutionSchedulePolicy {
            timezone: "UTC".to_string(),
            calendar_expression: "0 0 9 * * 1-5".to_string(),
            start_at: now,
            end_at: Some(now + Duration::days(30)),
            missed_fire_policy: ExecutionScheduleMissedFirePolicy::FireOnce,
            overlap_policy: ExecutionScheduleOverlapPolicy::QueueOne,
            dst_policy: ExecutionScheduleDstPolicy::Earliest,
            maximum_concurrent_runs: 1,
            occurrence_budget: serde_json::to_value(approved_budget)
                .expect("budget fixture must serialize"),
        },
    }
}
