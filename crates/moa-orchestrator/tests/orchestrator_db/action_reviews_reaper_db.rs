//! DB-backed tenant action-review timeout reaper tests.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use moa_core::types::{
    action_policy::{ActionClass, ActionEnvelope, ExecutionTaskOrigin, RiskLevel},
    contact::SessionActorRef,
    identifiers::{TenantId, ToolCallId},
};
use moa_execution::wire::{ExecutionActionReviewAcknowledgement, ExecutionActionReviewResolution};
use moa_observability::propagation::{TRACE_LINK_TRACEPARENT_HEADER, TRACE_LINK_TRACESTATE_HEADER};
use moa_orchestrator::services::action_reviews_reaper::ActionReviewReaper;
use moa_test_support::postgres::TestDb;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

const RESOLUTION_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const RESOLUTION_TRACESTATE: &str = "vendor=resolution";
const TASK_TRACEPARENT: &str = "00-70f5db931a2b4b63a5998b0ca94a6d20-7f3a20f9e6a03e01-01";
const TASK_TRACESTATE: &str = "vendor=task";

#[tokio::test]
async fn expired_pending_review_is_failed_closed_db() {
    // Pins: a pending tenant action review past its expiry is transitioned to a
    // terminal `timeout` by the reaper. The terminal status is what makes the
    // gated tool fail closed: decide_review rejects any later clear once the row
    // leaves `pending`.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let review_id = insert_review(&pool, "command_execution", "high", ReviewClock::Expired).await;
    let reaper = ActionReviewReaper::new(pool.clone());

    let timed_out = reaper.sweep().await.expect("sweep should complete");

    assert_eq!(timed_out, 1, "the expired review should be failed closed");
    let (status, decided_at, deny_reason): (
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, decided_at, deny_reason FROM tenant_action_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("review row should remain readable");
    assert_eq!(status, "timeout", "review should be terminal timeout");
    assert!(decided_at.is_some(), "timeout must record a decision time");
    assert!(
        deny_reason.is_some(),
        "timeout must record a deny reason so the audit trail explains the closure"
    );
}

#[tokio::test]
async fn unexpired_pending_review_survives_sweep_db() {
    // Pins: a pending review that has not reached its expiry is left pending and
    // still counts toward the pending-queue depth the reaper publishes.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let review_id = insert_review(&pool, "local_write", "low", ReviewClock::Fresh).await;
    let reaper = ActionReviewReaper::new(pool.clone());

    let timed_out = reaper.sweep().await.expect("sweep should complete");
    // Sampling the gauges must decode against the real schema (pins the
    // EXTRACT(EPOCH ...) NUMERIC -> f64 cast).
    reaper
        .sample_gauges()
        .await
        .expect("gauge sampling should decode against the real schema");

    assert_eq!(timed_out, 0, "an unexpired review must not be timed out");
    let status: String =
        sqlx::query_scalar("SELECT status FROM tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(&pool)
            .await
            .expect("review row should remain readable");
    assert_eq!(status, "pending", "unexpired review stays pending");
}

#[tokio::test]
async fn expired_review_claimed_by_durable_execution_is_not_timed_out_db() {
    // Pins: once the keyed action-review service has durably claimed a clear,
    // the timeout reaper cannot steal the still-pending row while its governed
    // tool call is journaled or recovering.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let review_id = insert_review(&pool, "command_execution", "high", ReviewClock::Expired).await;
    sqlx::query("UPDATE tenant_action_reviews SET execution_requested_at = NOW() WHERE id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .expect("test should persist the durable execution claim");
    let reaper = ActionReviewReaper::new(pool.clone());

    let timed_out = reaper.sweep().await.expect("sweep should complete");

    assert_eq!(timed_out, 0, "a claimed execution must not time out");
    let status: String =
        sqlx::query_scalar("SELECT status FROM tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(&pool)
            .await
            .expect("claimed review should remain readable");
    assert_eq!(status, "pending");
    let outbox_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("outbox count should load");
    assert_eq!(outbox_count, 0, "no competing timeout may be delivered");
}

#[tokio::test]
async fn timed_out_execution_review_inserts_and_dispatches_resolution_db() {
    // Pins: timeout persistence and its execution-task resolution outbox insert
    // commit atomically, and one bounded dispatch marks the exact claimed row delivered.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let review_id = insert_execution_review(&pool, tenant_id, origin, ReviewClock::Expired).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock Restate listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should resolve");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/restate/call/ExecutionTask/{task_id}/resolve_action_review",
                post(|| async { Json(ExecutionActionReviewAcknowledgement::Applied) }),
            ),
        )
        .await
    });
    let reaper =
        ActionReviewReaper::with_restate_ingress(pool.clone(), format!("http://{address}"));

    let timed_out = reaper.sweep().await.expect("sweep should deliver timeout");

    assert_eq!(timed_out, 1);
    let (resolution, attempt_count, delivered_at): (
        serde_json::Value,
        i32,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT resolution, attempt_count, delivered_at \
         FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("timeout outbox row should exist");
    assert_eq!(
        serde_json::from_value::<ExecutionActionReviewResolution>(resolution)
            .expect("resolution should decode"),
        ExecutionActionReviewResolution::TimedOut {
            reason: "review expired without a decision".to_string(),
        }
    );
    assert_eq!(
        attempt_count, 1,
        "one bounded dispatch attempt is persisted"
    );
    assert!(
        delivered_at.is_some(),
        "Applied acknowledgement marks delivery"
    );
    server.abort();
    delete_execution_run(&pool, origin.run_uid).await;
}

#[tokio::test]
async fn timed_out_execution_review_copies_original_task_context_atomically_db() {
    // Pins: timeout terminalization copies the immutable creation-time execution-task
    // context into the outbox without substituting the resolver or reaper context.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let review_id = insert_review_with_envelope(
        &pool,
        "command_execution",
        "high",
        ReviewClock::Expired,
        action_envelope(tenant_id, Some(origin)),
        Some((TASK_TRACEPARENT, TASK_TRACESTATE)),
    )
    .await;

    assert_eq!(
        ActionReviewReaper::new(pool.clone())
            .sweep()
            .await
            .expect("timeout should atomically create its outbox row"),
        1
    );

    let stored: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT traceparent, tracestate, task_traceparent, task_tracestate \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("timeout outbox row should exist");
    assert_eq!(
        stored,
        (
            None,
            None,
            Some(TASK_TRACEPARENT.to_string()),
            Some(TASK_TRACESTATE.to_string()),
        )
    );
    delete_execution_run(&pool, origin.run_uid).await;
}

#[tokio::test]
async fn stale_outbox_claim_acknowledgement_cannot_mark_newer_attempt_delivered_db() {
    // Pins: a delayed acknowledgement is fenced by the claimed attempt count
    // and cannot mark a row delivered after crash recovery creates a newer claim.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let review_id = insert_execution_review(&pool, tenant_id, origin, ReviewClock::Expired).await;
    ActionReviewReaper::new(pool.clone())
        .sweep()
        .await
        .expect("timeout should insert outbox row");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock Restate listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should resolve");
    let handler_pool = pool.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/restate/call/ExecutionTask/{task_id}/resolve_action_review",
                post(move || {
                    let pool = handler_pool.clone();
                    async move {
                        sqlx::query(
                            "UPDATE moa.execution_action_review_outbox \
                             SET attempt_count = attempt_count + 1 WHERE review_uid = $1",
                        )
                        .bind(review_id)
                        .execute(&pool)
                        .await
                        .expect("test should advance the claim fence");
                        Json(ExecutionActionReviewAcknowledgement::Applied)
                    }
                }),
            ),
        )
        .await
    });
    let reaper =
        ActionReviewReaper::with_restate_ingress(pool.clone(), format!("http://{address}"));

    assert_eq!(
        reaper
            .dispatch_execution_review_resolutions()
            .await
            .expect("dispatch should complete"),
        1
    );
    let delivered_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT delivered_at FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("outbox row should remain");
    assert!(
        delivered_at.is_none(),
        "the stale attempt must not acknowledge the newer claim"
    );
    server.abort();
    delete_execution_run(&pool, origin.run_uid).await;
}

#[tokio::test]
async fn execution_review_outbox_retries_after_arbitrarily_many_failures_db() {
    // Pins: persisted attempt_count controls claim fencing and backoff only;
    // it never becomes a terminal retry cap before the task acknowledges
    // Applied, Replayed, or AuditedStale.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let review_id = insert_execution_review(&pool, tenant_id, origin, ReviewClock::Expired).await;
    ActionReviewReaper::new(pool.clone())
        .sweep()
        .await
        .expect("timeout should insert outbox row");
    sqlx::query(
        "UPDATE moa.execution_action_review_outbox \
         SET attempt_count = 32, claimed_at = NULL, next_attempt_at = NOW() - INTERVAL '1 second' \
         WHERE review_uid = $1",
    )
    .bind(review_id)
    .execute(&pool)
    .await
    .expect("fixture should represent many prior failed deliveries");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock Restate listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should resolve");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/restate/call/ExecutionTask/{task_id}/resolve_action_review",
                post(|| async { Json(ExecutionActionReviewAcknowledgement::AuditedStale) }),
            ),
        )
        .await
    });
    let reaper =
        ActionReviewReaper::with_restate_ingress(pool.clone(), format!("http://{address}"));

    assert_eq!(
        reaper
            .dispatch_execution_review_resolutions()
            .await
            .expect("late retry dispatch should complete"),
        1,
        "attempt 33 must still be claimed and delivered"
    );
    let (attempt_count, delivered_at): (i32, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as(
            "SELECT attempt_count, delivered_at \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
        )
        .bind(review_id)
        .fetch_one(&pool)
        .await
        .expect("outbox row should remain auditable");
    assert_eq!(attempt_count, 33);
    assert!(
        delivered_at.is_some(),
        "AuditedStale acknowledgement is a terminal delivery acknowledgement"
    );
    server.abort();
    delete_execution_run(&pool, origin.run_uid).await;
}

#[tokio::test]
async fn execution_review_retry_reinjects_resolution_parent_and_preserves_both_pairs_db() {
    // Pins: a failed raw-Restate delivery retries with the first persisted resolution
    // parent while the separately stored execution-task link target remains byte-exact.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let review_id = insert_execution_review(&pool, tenant_id, origin, ReviewClock::Fresh).await;
    let resolution = serde_json::to_value(ExecutionActionReviewResolution::TimedOut {
        reason: "review expired without a decision".to_string(),
    })
    .expect("resolution should serialize");
    sqlx::query(
        r#"
        INSERT INTO moa.execution_action_review_outbox (
            review_uid, tenant_id, contact_id, run_uid, task_id, generation, resolution,
            traceparent, tracestate, task_traceparent, task_tracestate
        )
        SELECT $1, tenant_id, contact_id, run_uid, task_id, 1, $4, $5, $6, $7, $8
        FROM moa.execution_task
        WHERE run_uid = $2 AND task_id = $3
        "#,
    )
    .bind(review_id)
    .bind(origin.run_uid)
    .bind(origin.task_uid)
    .bind(resolution)
    .bind(RESOLUTION_TRACEPARENT)
    .bind(RESOLUTION_TRACESTATE)
    .bind(TASK_TRACEPARENT)
    .bind(TASK_TRACESTATE)
    .execute(&pool)
    .await
    .expect("outbox fixture should insert");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock Restate listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should resolve");
    type CapturedHeaders = Arc<
        Mutex<
            Vec<(
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )>,
        >,
    >;
    let captured = CapturedHeaders::default();
    let server_state = captured.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/restate/call/ExecutionTask/{task_id}/resolve_action_review",
                    post(
                        |State(captured): State<CapturedHeaders>, headers: HeaderMap| async move {
                            let traceparent = headers
                                .get("traceparent")
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned);
                            let tracestate = headers
                                .get("tracestate")
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned);
                            let task_traceparent = headers
                                .get(TRACE_LINK_TRACEPARENT_HEADER)
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned);
                            let task_tracestate = headers
                                .get(TRACE_LINK_TRACESTATE_HEADER)
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned);
                            let mut captured = captured.lock().await;
                            captured.push((
                                traceparent,
                                tracestate,
                                task_traceparent,
                                task_tracestate,
                            ));
                            if captured.len() == 1 {
                                (StatusCode::SERVICE_UNAVAILABLE, "retry").into_response()
                            } else {
                                Json(ExecutionActionReviewAcknowledgement::Applied).into_response()
                            }
                        },
                    ),
                )
                .with_state(server_state),
        )
        .await
    });
    let reaper =
        ActionReviewReaper::with_restate_ingress(pool.clone(), format!("http://{address}"));

    assert_eq!(
        reaper
            .dispatch_execution_review_resolutions()
            .await
            .expect("first dispatch should persist its failure"),
        1
    );
    sqlx::query(
        "UPDATE moa.execution_action_review_outbox \
         SET next_attempt_at = NOW() - INTERVAL '1 second' WHERE review_uid = $1",
    )
    .bind(review_id)
    .execute(&pool)
    .await
    .expect("retry fixture should make the row immediately claimable");
    assert_eq!(
        reaper
            .dispatch_execution_review_resolutions()
            .await
            .expect("second dispatch should acknowledge delivery"),
        1
    );

    let captured = captured.lock().await.clone();
    assert_eq!(
        captured,
        vec![
            (
                Some(RESOLUTION_TRACEPARENT.to_string()),
                Some(RESOLUTION_TRACESTATE.to_string()),
                Some(TASK_TRACEPARENT.to_string()),
                Some(TASK_TRACESTATE.to_string()),
            ),
            (
                Some(RESOLUTION_TRACEPARENT.to_string()),
                Some(RESOLUTION_TRACESTATE.to_string()),
                Some(TASK_TRACEPARENT.to_string()),
                Some(TASK_TRACESTATE.to_string()),
            ),
        ],
        "every retry must reinject the first resolution parent and task link"
    );
    let stored: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT traceparent, tracestate, task_traceparent, task_tracestate \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("stored trace pairs should remain auditable");
    assert_eq!(
        stored,
        (
            Some(RESOLUTION_TRACEPARENT.to_string()),
            Some(RESOLUTION_TRACESTATE.to_string()),
            Some(TASK_TRACEPARENT.to_string()),
            Some(TASK_TRACESTATE.to_string()),
        )
    );
    server.abort();
    delete_execution_run(&pool, origin.run_uid).await;
}

/// Whether the inserted review is already past its expiry.
enum ReviewClock {
    Expired,
    Fresh,
}

async fn test_pool() -> TestDb {
    moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("isolated test Postgres should bootstrap")
}

async fn insert_review(
    pool: &PgPool,
    action_class: &str,
    risk_level: &str,
    clock: ReviewClock,
) -> Uuid {
    let tenant_id = TenantId::from(Uuid::new_v4());
    insert_review_with_envelope(
        pool,
        action_class,
        risk_level,
        clock,
        action_envelope(tenant_id, None),
        None,
    )
    .await
}

async fn insert_execution_review(
    pool: &PgPool,
    tenant_id: TenantId,
    origin: ExecutionTaskOrigin,
    clock: ReviewClock,
) -> Uuid {
    insert_review_with_envelope(
        pool,
        "command_execution",
        "high",
        clock,
        action_envelope(tenant_id, Some(origin)),
        None,
    )
    .await
}

async fn insert_review_with_envelope(
    pool: &PgPool,
    action_class: &str,
    risk_level: &str,
    clock: ReviewClock,
    mut envelope: ActionEnvelope,
    execution_task_trace_context: Option<(&str, &str)>,
) -> Uuid {
    let review_id = Uuid::new_v4();
    envelope.review_id = review_id;
    let expires_at_sql = match clock {
        ReviewClock::Expired => "NOW() - INTERVAL '1 minute'",
        ReviewClock::Fresh => "NOW() + INTERVAL '1 day'",
    };
    sqlx::query(&format!(
        r#"
        INSERT INTO tenant_action_reviews
            (id, tenant_id, storage_partition_id, tool_call_id, tool_name,
             action_class, risk_level, input_summary, normalized_input, envelope,
             preview, tool_request, requested_by, status, created_at, expires_at,
             execution_task_traceparent, execution_task_tracestate)
        VALUES ($1, $2, $3, $4, 'bash', $5, $6, 'test action', 'printf ok', $7,
                '{{"fields":[],"file_diffs":[]}}'::JSONB, '{{}}'::JSONB,
                'anonymous', 'pending', NOW() - INTERVAL '2 minutes', {expires_at_sql},
                $8, $9)
        "#
    ))
    .bind(review_id)
    .bind(envelope.tenant_id.0)
    .bind(
        moa_core::types::identifiers::StoragePartitionId::for_tenant(envelope.tenant_id)
            .to_string(),
    )
    .bind(envelope.tool_call_id.0)
    .bind(action_class)
    .bind(risk_level)
    .bind(serde_json::to_value(envelope).expect("envelope should serialize"))
    .bind(execution_task_trace_context.map(|context| context.0))
    .bind(execution_task_trace_context.map(|context| context.1))
    .execute(pool)
    .await
    .expect("pending review should insert");
    review_id
}

fn action_envelope(
    tenant_id: TenantId,
    execution_origin: Option<ExecutionTaskOrigin>,
) -> ActionEnvelope {
    ActionEnvelope {
        review_id: Uuid::new_v4(),
        tenant_id,
        requested_by: SessionActorRef::Anonymous,
        session_id: None,
        worker_id: None,
        tool_call_id: ToolCallId::new(),
        tool_name: "bash".to_string(),
        normalized_input: "printf ok".to_string(),
        input_summary: "test action".to_string(),
        risk_level: RiskLevel::High,
        action_class: ActionClass::CommandExecution,
        origin_kind: None,
        origin_id: None,
        origin_step_id: None,
        execution_origin,
        idempotency_key: None,
        created_at: chrono::Utc::now(),
    }
}

async fn insert_execution_task(pool: &PgPool, tenant_id: TenantId) -> ExecutionTaskOrigin {
    let run_uid = Uuid::new_v4();
    let task_uid = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let planning_context_uid = Uuid::new_v4();
    let hash = "0".repeat(64);
    sqlx::query(
        r#"
        INSERT INTO moa.execution_planning_context (
            planning_context_uid, tenant_id, session_id,
            originating_user_sequence_num, originating_user_event_hash,
            owner_user_id, planning_context_hash, snapshot
        ) VALUES ($1, $2, $3, 1, $4, 'test-owner', $4, '{}'::JSONB)
        "#,
    )
    .bind(planning_context_uid)
    .bind(tenant_id.0)
    .bind(session_id)
    .bind(&hash)
    .execute(pool)
    .await
    .expect("execution planning context should insert");
    sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, session_id, originating_user_sequence_num,
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract,
            initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, source_kind, route_reason,
            input, status, queued_at
        ) VALUES ($1, $2, $3, 1, $4, $5, 'test-owner', '{}'::JSONB, '{}'::JSONB,
                  '{}'::JSONB, $5, $5, '{}'::JSONB, '{}'::JSONB, '[]'::JSONB,
                  '{"kind":"generated_plan","route_reason":"explicit_durable_execution"}'::JSONB,
                  'generated_plan', 'explicit_durable_execution', '{}'::JSONB, 'queued', NOW())
        "#,
    )
    .bind(run_uid)
    .bind(tenant_id.0)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind(hash)
    .execute(pool)
    .await
    .expect("execution run should insert");
    sqlx::query("UPDATE moa.execution_run SET status = 'running' WHERE run_uid = $1")
        .bind(run_uid)
        .execute(pool)
        .await
        .expect("execution run should start");
    sqlx::query(
        r#"
        INSERT INTO moa.execution_task (
            task_id, run_uid, tenant_id, node_id, item_key, plan_revision,
            status, input, task_kind, retry_policy,
            estimate_cost_microusd, estimate_tokens, estimate_tasks,
            estimate_tool_calls, estimate_retrieved_bytes
        ) VALUES ($1, $2, $3, 'node', 'item', 1, 'running', '{}'::JSONB,
                  '{}'::JSONB, '{}'::JSONB, 0, 0, 1, 0, 0)
        "#,
    )
    .bind(task_uid)
    .bind(run_uid)
    .bind(tenant_id.0)
    .execute(pool)
    .await
    .expect("execution task should insert");
    ExecutionTaskOrigin {
        run_uid,
        task_uid,
        generation: 1,
    }
}

async fn delete_execution_run(pool: &PgPool, run_uid: Uuid) {
    sqlx::query("DELETE FROM moa.execution_run WHERE run_uid = $1")
        .bind(run_uid)
        .execute(pool)
        .await
        .expect("execution fixture should clean up");
}
