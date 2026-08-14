//! DB-backed execution dispatch reconciliation and exact trigger delivery contracts.

use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use moa_config::ExecutionConfig;
use moa_core::{
    traits::{Identity, IdentityType},
    types::identifiers::TenantId,
};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    outbox::{
        ExecutionDispatchFailureOutcome, ExecutionDispatchRetryPolicy, ExecutionMaintenanceJobKind,
        ExecutionMaintenanceSettlementOutcome,
    },
    trigger::{
        ExecutionTriggerFireOutcome, ExecutionTriggerKind, ExecutionTriggerNoOp,
        NewExecutionTrigger,
    },
};
use serde_json::json;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn reconciliation_checkpoint_rejects_superseded_completion_and_records_failure_db()
-> TestResult {
    // Pins: overlapping infrastructure invocations cannot let an older pass
    // overwrite the durable completion receipt for a newer reconciliation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let scope = ExecutionScope::ControlPlane;
    let kind = ExecutionMaintenanceJobKind::DispatchReconciliation;
    let first = repository.begin_execution_maintenance(scope, kind).await?;
    let second = repository.begin_execution_maintenance(scope, kind).await?;
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(
        repository
            .complete_execution_maintenance(scope, kind, first.generation)
            .await?,
        ExecutionMaintenanceSettlementOutcome::StaleOrMissing
    );
    let failed = repository
        .fail_execution_maintenance(
            scope,
            kind,
            second.generation,
            "injected bounded reconciliation failure",
        )
        .await?;
    let ExecutionMaintenanceSettlementOutcome::Applied(failed) = failed else {
        return Err("current checkpoint generation must accept failure".into());
    };
    assert_eq!(
        failed.last_error.as_deref(),
        Some("injected bounded reconciliation failure")
    );
    assert!(failed.last_failure_at.is_some());
    assert!(failed.last_succeeded_at.is_none());
    Ok(())
}

#[tokio::test]
async fn reconciliation_repairs_only_one_bounded_indexed_window_db() -> TestResult {
    // Pins: the infrastructure reconciliation target never becomes an unbounded
    // poller; each invocation repairs at most its configured SKIP LOCKED window.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::ControlPlane;

    for _ in 1..=3 {
        let schedule_uid = insert_schedule(&pool, tenant_id).await?;
        let write = repository
            .create_trigger(
                scope,
                &ExecutionConfig::default(),
                schedule_trigger(
                    tenant_id,
                    schedule_uid,
                    1,
                    Utc::now() - Duration::minutes(1),
                ),
            )
            .await?;
        sqlx::query("DELETE FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1")
            .bind(write.dispatch.dispatch_uid)
            .execute(&pool)
            .await?;
    }

    assert_eq!(
        repository
            .reconcile_due_trigger_dispatches(scope, 6)
            .await?
            .len(),
        2
    );
    assert_eq!(
        repository
            .reconcile_due_trigger_dispatches(scope, 6)
            .await?
            .len(),
        1
    );
    assert!(
        repository
            .reconcile_due_trigger_dispatches(scope, 6)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn claimed_dispatches_ack_and_retry_correctness_work_under_exact_owner_fences_db()
-> TestResult {
    // Pins: a dispatcher ACKs only after accepted delivery, abandoned owners cannot
    // settle another claim, and correctness-critical trigger delivery keeps retrying.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let schedule_uid = insert_schedule(&pool, tenant_id).await?;
    let scope = ExecutionScope::ControlPlane;
    for occurrence_sequence in 1..=3 {
        repository
            .create_trigger(
                scope,
                &ExecutionConfig::default(),
                schedule_trigger(
                    tenant_id,
                    schedule_uid,
                    occurrence_sequence,
                    Utc::now() - Duration::minutes(1),
                ),
            )
            .await?;
    }

    let claimed = repository
        .claim_due_dispatches(scope, "dispatcher-a", 3, StdDuration::from_secs(30))
        .await?;
    assert_eq!(claimed.len(), 3);
    assert!(
        repository
            .claim_due_dispatches(scope, "dispatcher-b", 3, StdDuration::from_secs(30))
            .await?
            .is_empty()
    );

    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &[claimed[0].dispatch_uid], "dispatcher-b")
            .await?,
        Vec::<Uuid>::new()
    );
    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &[claimed[0].dispatch_uid], "dispatcher-a")
            .await?,
        vec![claimed[0].dispatch_uid]
    );

    let retry = ExecutionDispatchRetryPolicy {
        max_attempts: 2,
        base_delay: StdDuration::from_secs(1),
        maximum_delay: StdDuration::from_secs(2),
    };
    assert!(matches!(
        repository
            .record_dispatch_failure(
                scope,
                claimed[1].dispatch_uid,
                "dispatcher-a",
                "injected acceptance failure",
                retry,
            )
            .await?,
        ExecutionDispatchFailureOutcome::RetryScheduled { .. }
    ));
    let exhausted_retry = ExecutionDispatchRetryPolicy {
        max_attempts: 1,
        base_delay: StdDuration::from_secs(1),
        maximum_delay: StdDuration::from_secs(1),
    };
    assert!(matches!(
        repository
            .record_dispatch_failure(
                scope,
                claimed[2].dispatch_uid,
                "dispatcher-a",
                "injected permanent acceptance failure",
                exhausted_retry,
            )
            .await?,
        ExecutionDispatchFailureOutcome::RetryScheduled { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn trigger_delivery_rechecks_due_time_and_settles_fallback_outbox_db() -> TestResult {
    // Pins: send_after cannot fire a trigger early, while a current due delivery
    // atomically settles both canonical trigger state and its recovery outbox row.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let schedule_uid = insert_schedule(&pool, tenant_id).await?;
    let scope = ExecutionScope::ControlPlane;
    let future = repository
        .create_trigger(
            scope,
            &ExecutionConfig::default(),
            schedule_trigger(tenant_id, schedule_uid, 2, Utc::now() + Duration::hours(1)),
        )
        .await?;
    assert_eq!(
        repository
            .fire_trigger(scope, future.trigger.trigger_uid,)
            .await?,
        ExecutionTriggerFireOutcome::NoOp(ExecutionTriggerNoOp::NotDue)
    );

    let due = repository
        .create_trigger(
            scope,
            &ExecutionConfig::default(),
            schedule_trigger(
                tenant_id,
                schedule_uid,
                1,
                Utc::now() - Duration::seconds(1),
            ),
        )
        .await?;
    assert_eq!(
        repository
            .fire_trigger(scope, due.trigger.trigger_uid)
            .await?,
        ExecutionTriggerFireOutcome::Delivered { activation: None }
    );
    let fallback_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1",
    )
    .bind(due.dispatch.dispatch_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(fallback_state, "delivered");
    Ok(())
}

fn schedule_trigger(
    tenant_id: TenantId,
    schedule_uid: Uuid,
    occurrence_sequence: u64,
    due_at: chrono::DateTime<Utc>,
) -> NewExecutionTrigger {
    NewExecutionTrigger {
        trigger_uid: Uuid::now_v7(),
        tenant_id,
        run_uid: None,
        task_id: None,
        compensation_id: None,
        schedule_uid: Some(schedule_uid),
        schedule_incarnation: Some(1),
        kind: ExecutionTriggerKind::ScheduleOccurrence,
        controller_generation: None,
        attempt_generation: None,
        compensation_generation: None,
        compensation_attempt_generation: None,
        occurrence_sequence: Some(occurrence_sequence),
        due_at,
        payload: json!({ "occurrence_sequence": occurrence_sequence }),
    }
}

async fn insert_schedule(pool: &sqlx::PgPool, tenant_id: TenantId) -> Result<Uuid, sqlx::Error> {
    let schedule_uid = Uuid::now_v7();
    let identity = Identity {
        identity_type: IdentityType::Service,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let identity = serde_json::to_value(identity).expect("fixture identity must serialize");
    sqlx::query(
        r#"
        INSERT INTO moa.execution_schedule (
            schedule_uid, tenant_id, owner_user_id, name, timezone,
            calendar_expression, template_revision_uid, template_snapshot,
            template_hash, run_as_identity, creation_origin, missed_fire_policy,
            overlap_policy, dst_policy, occurrence_budget, start_at
        ) VALUES (
            $1, $2, 'scheduler', $3, 'UTC', '0 * * * *', $4, '{}'::JSONB,
            $5, $6, jsonb_build_object(
                'request_uid', $7::TEXT,
                'created_by', $6::JSONB,
                'source', jsonb_build_object('kind', 'tenant_api')
            ), 'skip', 'skip', 'earliest', '{}'::JSONB, now()
        )
        "#,
    )
    .bind(schedule_uid)
    .bind(tenant_id.0)
    .bind(format!("reconcile-{schedule_uid}"))
    .bind(Uuid::now_v7())
    .bind("0".repeat(64))
    .bind(identity)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(schedule_uid)
}
