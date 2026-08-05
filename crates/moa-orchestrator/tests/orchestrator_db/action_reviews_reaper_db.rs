//! DB-backed tenant action-review timeout reaper tests.

use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::{
    action_policy::{
        ActionClass, ActionEnvelope, ActionReviewOwner, ExecutionCompensationOrigin,
        ExecutionTaskOrigin, RiskLevel,
    },
    contact::SessionActorRef,
    identifiers::{TenantId, ToolCallId},
    tools::ToolCallRequest,
};
use moa_execution::wire::ExecutionActionReviewResolution;
use moa_orchestrator::services::{
    action_reviews::{
        ExecutionActionReviewSettlement, SettleExecutionActionReviewRequest,
        settle_execution_action_review,
    },
    action_reviews_reaper::ActionReviewReaper,
};
use moa_test_support::postgres::TestDb;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

const TASK_TRACEPARENT: &str = "00-70f5db931a2b4b63a5998b0ca94a6d20-7f3a20f9e6a03e01-01";
const TASK_TRACESTATE: &str = "vendor=task";

type ReviewSettlementRow = (
    String,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<Uuid>,
    Option<chrono::DateTime<chrono::Utc>>,
);

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
async fn timeout_release_survives_crash_between_terminalization_and_delivery_db() {
    // Pins: once timeout is durable, losing the reaper before its Session callback
    // cannot strand the owner's lifecycle hold. A later sweep records the typed
    // timeout event before delivering the release, without creating a continuation.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let review_id = insert_review(&pool, "command_execution", "high", ReviewClock::Expired).await;

    assert_eq!(
        ActionReviewReaper::new(pool.clone())
            .sweep()
            .await
            .expect("timeout should commit without ingress"),
        1
    );
    let delivered_before: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT owner_release_delivered_at FROM tenant_action_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("release state should load");
    assert!(delivered_before.is_none());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock Restate listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should resolve");
    type DeliveryOrder = Arc<Mutex<Vec<&'static str>>>;
    let delivery_order = DeliveryOrder::default();
    let server_state = delivery_order.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/restate/call/SessionStore/append_event",
                    post(
                        |State(order): State<DeliveryOrder>,
                         Json(payload): Json<serde_json::Value>| async move {
                            assert_eq!(
                                payload["event"]["type"],
                                serde_json::Value::String("ActionReviewTimedOut".to_string())
                            );
                            order.lock().await.push("timeout_event");
                            StatusCode::OK
                        },
                    ),
                )
                .route(
                    "/restate/call/Session/{session_id}/release_action_review",
                    post(|State(order): State<DeliveryOrder>| async move {
                        order.lock().await.push("owner_release");
                        StatusCode::OK
                    }),
                )
                .route(
                    "/restate/call/ActionReviewDispatcher/dispatch",
                    post(|| async { Json(serde_json::json!({ "claimed": 0 })) }),
                )
                .with_state(server_state),
        )
        .await
    });

    assert_eq!(
        ActionReviewReaper::with_restate_ingress(pool.clone(), format!("http://{address}"))
            .sweep()
            .await
            .expect("later sweep should deliver durable release"),
        0,
        "the second sweep delivers an existing timeout rather than timing it out again"
    );
    let delivered_after: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT owner_release_delivered_at FROM tenant_action_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("release delivery state should load");
    assert!(delivered_after.is_some());
    assert_eq!(
        delivery_order.lock().await.as_slice(),
        ["timeout_event", "owner_release"],
        "the terminal event must be durable before lifecycle release"
    );
    server.abort();
}

#[tokio::test]
async fn decision_and_timeout_wait_for_owner_registration_db() {
    // Pins: the durable review row is not externally actionable until the Session
    // or Worker registration acknowledgement is persisted.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let review_id = insert_review(&pool, "command_execution", "high", ReviewClock::Expired).await;
    sqlx::query("UPDATE tenant_action_reviews SET owner_registered_at = NULL WHERE id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .expect("fixture should model the pre-registration crash window");

    assert_eq!(
        ActionReviewReaper::new(pool.clone())
            .sweep()
            .await
            .expect("sweep should skip an unregistered owner"),
        0
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(&pool)
            .await
            .expect("review should remain readable");
    assert_eq!(status, "pending");
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
async fn timed_out_execution_review_inserts_resolution_and_wakes_restate_dispatcher_db() {
    // Pins: timeout persistence and its execution-task resolution outbox insert
    // commit atomically, while the process reaper only wakes the Restate-owned
    // dispatcher and never calls the private ExecutionTask workflow directly.
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
    type DispatcherCalls = Arc<Mutex<Vec<serde_json::Value>>>;
    let calls = DispatcherCalls::default();
    let server_calls = calls.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/restate/call/ActionReviewDispatcher/dispatch",
                    post(
                        |State(calls): State<DispatcherCalls>,
                         Json(body): Json<serde_json::Value>| async move {
                            calls.lock().await.push(body);
                            Json(serde_json::json!({ "claimed": 1 }))
                        },
                    ),
                )
                .with_state(server_calls),
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
        attempt_count, 0,
        "only the Restate dispatcher may claim rows"
    );
    assert!(
        delivered_at.is_none(),
        "the process reaper must not acknowledge private workflow delivery"
    );
    assert_eq!(
        calls.lock().await.as_slice(),
        &[serde_json::json!({})],
        "one dispatcher wake must replace direct ExecutionTask ingress"
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
async fn timed_out_compensation_review_creates_one_exact_delivery_db() {
    // Pins: an expired compensation review terminalizes once and its outbox row
    // preserves the exact compensation id instead of routing to the forward task.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let task_origin = insert_execution_task(&pool, tenant_id).await;
    let compensation_origin = insert_execution_compensation(&pool, task_origin).await;
    let mut envelope = action_envelope(tenant_id, None);
    envelope.owner = ActionReviewOwner::ExecutionCompensation {
        session_id: envelope.owner.session_id(),
        origin: compensation_origin,
    };
    let review_id = insert_review_with_envelope(
        &pool,
        "command_execution",
        "high",
        ReviewClock::Expired,
        envelope,
        Some((TASK_TRACEPARENT, TASK_TRACESTATE)),
    )
    .await;

    let reaper = ActionReviewReaper::new(pool.clone());
    assert_eq!(reaper.sweep().await.expect("timeout should commit"), 1);
    assert_eq!(
        reaper.sweep().await.expect("timeout replay should be idle"),
        0
    );
    let (owner_kind, operation_id, generation, count): (String, Uuid, i64, i64) = sqlx::query_as(
        "SELECT owner_kind, operation_id, generation, COUNT(*) OVER() \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("compensation outbox row should exist");
    assert_eq!(owner_kind, "compensation");
    assert_eq!(operation_id, compensation_origin.compensation_id);
    assert_eq!(
        generation,
        i64::try_from(compensation_origin.generation)
            .expect("fixture generation should fit PostgreSQL BIGINT")
    );
    assert_eq!(count, 1, "one review may create only one delivery row");
    delete_execution_run(&pool, task_origin.run_uid).await;
}

#[tokio::test]
async fn owner_settlement_revokes_unclaimed_task_review_before_admin_claim_db() {
    // Pins: when task termination wins the action-review race, the same locked
    // transaction terminalizes the pending review before any admin clear can
    // claim or dispatch its gated effect.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let origin = insert_execution_task(&pool, tenant_id).await;
    let envelope = action_envelope(tenant_id, Some(origin));
    let owner = envelope.owner.clone();
    let review_id = insert_review_with_envelope(
        &pool,
        "command_execution",
        "high",
        ReviewClock::Fresh,
        envelope,
        Some((TASK_TRACEPARENT, TASK_TRACESTATE)),
    )
    .await;

    let settlement = settle_execution_action_review(
        pool.clone(),
        SettleExecutionActionReviewRequest {
            tenant_id,
            review_id,
            owner,
        },
    )
    .await
    .expect("unclaimed task review should settle transactionally");
    assert_eq!(settlement, ExecutionActionReviewSettlement::Revoked);

    let attempted_tool_call_id = Uuid::new_v4();
    let later_claim = sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET decided_by = 'admin-after-task-terminal',
            decided_at = NOW(),
            execution_tool_call_id = $3,
            execution_requested_at = NOW()
        WHERE storage_partition_id = $1
          AND id = $2
          AND status = 'pending'
          AND execution_requested_at IS NULL
        "#,
    )
    .bind(moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(review_id)
    .bind(attempted_tool_call_id)
    .execute(&pool)
    .await
    .expect("later admin claim attempt should be rejected by durable row state");
    assert_eq!(
        later_claim.rows_affected(),
        0,
        "a revoked review must never become dispatchable"
    );

    let (
        status,
        decided_by,
        deny_reason,
        decided_at,
        execution_tool_call_id,
        execution_requested_at,
    ): ReviewSettlementRow = sqlx::query_as(
        r#"
        SELECT status, decided_by, deny_reason, decided_at,
               execution_tool_call_id, execution_requested_at
        FROM tenant_action_reviews
        WHERE id = $1
        "#,
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("revoked review should remain readable");
    assert_eq!(status, "revoked");
    assert_eq!(
        decided_by.as_deref(),
        Some("system:execution-owner-terminal")
    );
    assert_eq!(
        deny_reason.as_deref(),
        Some("execution owner terminated before the reviewed effect was claimed")
    );
    assert!(decided_at.is_some(), "revocation must record when it won");
    assert_eq!(execution_tool_call_id, None);
    assert_eq!(execution_requested_at, None);
    let outbox_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("outbox count should load");
    assert_eq!(
        outbox_count, 0,
        "revocation must not invent an execution resolution delivery"
    );

    delete_execution_run(&pool, origin.run_uid).await;
}

#[tokio::test]
async fn admin_claim_requires_compensation_owner_to_join_definitive_resolution_db() {
    // Pins: when an admin clear claims a compensation review first, owner
    // termination cannot revoke it or erase its exact compensation fence; the
    // compensation workflow must join the already-owned definitive resolution.
    let test_db = test_pool().await;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::new_v4());
    let task_origin = insert_execution_task(&pool, tenant_id).await;
    let compensation_origin = insert_execution_compensation(&pool, task_origin).await;
    let mut envelope = action_envelope(tenant_id, None);
    let compensation_owner = ActionReviewOwner::ExecutionCompensation {
        session_id: envelope.owner.session_id(),
        origin: compensation_origin,
    };
    envelope.owner = compensation_owner.clone();
    let review_id = insert_review_with_envelope(
        &pool,
        "command_execution",
        "high",
        ReviewClock::Fresh,
        envelope,
        Some((TASK_TRACEPARENT, TASK_TRACESTATE)),
    )
    .await;
    let claimed_tool_call_id = Uuid::new_v4();
    let claim = sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET decided_by = 'admin-won-clear-race',
            decided_at = NOW(),
            execution_tool_call_id = $3,
            execution_requested_at = NOW()
        WHERE storage_partition_id = $1
          AND id = $2
          AND status = 'pending'
          AND execution_requested_at IS NULL
        "#,
    )
    .bind(moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(review_id)
    .bind(claimed_tool_call_id)
    .execute(&pool)
    .await
    .expect("admin clear should claim the pending compensation review");
    assert_eq!(claim.rows_affected(), 1);

    let wrong_owner = ActionReviewOwner::ExecutionCompensation {
        session_id: compensation_owner.session_id(),
        origin: ExecutionCompensationOrigin {
            compensation_id: Uuid::new_v4(),
            ..compensation_origin
        },
    };
    settle_execution_action_review(
        pool.clone(),
        SettleExecutionActionReviewRequest {
            tenant_id,
            review_id,
            owner: wrong_owner,
        },
    )
    .await
    .expect_err("a different compensation id must not settle the claimed review");

    let settlement = settle_execution_action_review(
        pool.clone(),
        SettleExecutionActionReviewRequest {
            tenant_id,
            review_id,
            owner: compensation_owner.clone(),
        },
    )
    .await
    .expect("the exact compensation owner should inspect the claimed review");
    assert_eq!(
        settlement,
        ExecutionActionReviewSettlement::JoinRequired,
        "a claimed effect must reach its definitive resolution before compensation terminates"
    );

    let (status, stored_envelope, decided_by, execution_tool_call_id, execution_requested_at): (
        String,
        serde_json::Value,
        Option<String>,
        Option<Uuid>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        r#"
        SELECT status, envelope, decided_by, execution_tool_call_id, execution_requested_at
        FROM tenant_action_reviews
        WHERE id = $1
        "#,
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("claimed compensation review should remain readable");
    let stored_envelope: ActionEnvelope =
        serde_json::from_value(stored_envelope).expect("stored envelope should decode");
    assert_eq!(status, "pending", "join-required work must not be revoked");
    assert_eq!(stored_envelope.owner, compensation_owner);
    assert_eq!(decided_by.as_deref(), Some("admin-won-clear-race"));
    assert_eq!(execution_tool_call_id, Some(claimed_tool_call_id));
    assert!(
        execution_requested_at.is_some(),
        "the winning admin claim must remain durably owned"
    );
    let outbox_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("outbox count should load");
    assert_eq!(
        outbox_count, 0,
        "the outbox must wait for the claimed tool's definitive resolution"
    );

    delete_execution_run(&pool, task_origin.run_uid).await;
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
    let tool_request = ToolCallRequest {
        tool_call_id: envelope.tool_call_id,
        caller_identity: Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::new_v4(),
            tenant_id: envelope.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        provider_tool_use_id: None,
        tool_name: "bash".to_string(),
        expected_tool_contract_revision: "fixture-v1".to_string(),
        input: serde_json::json!({"cmd": "printf ok"}),
        active_canary: None,
        session_id: envelope.owner.session_id(),
        trusted_sandbox_manifest: None,
        worker_id: None,
        resource_budget: Default::default(),
    };
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
             execution_task_traceparent, execution_task_tracestate, owner_registered_at)
        VALUES ($1, $2, $3, $4, 'bash', $5, $6, 'test action', 'printf ok', $7,
                '{{"fields":[],"file_diffs":[]}}'::JSONB, $10,
                'anonymous', 'pending', NOW() - INTERVAL '2 minutes', {expires_at_sql},
                $8, $9, NOW())
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
    .bind(serde_json::to_value(tool_request).expect("tool request should serialize"))
    .execute(pool)
    .await
    .expect("pending review should insert");
    review_id
}

fn action_envelope(
    tenant_id: TenantId,
    execution_origin: Option<ExecutionTaskOrigin>,
) -> ActionEnvelope {
    let session_id = moa_core::types::identifiers::SessionId::new();
    ActionEnvelope {
        review_id: Uuid::new_v4(),
        tenant_id,
        requested_by: SessionActorRef::Anonymous,
        owner: match execution_origin {
            Some(origin) => ActionReviewOwner::ExecutionTask { session_id, origin },
            None => ActionReviewOwner::Coordinator {
                session_id,
                turn_id: "turn-reaper-fixture".to_string(),
                generation: 1,
            },
        },
        tool_call_id: ToolCallId::new(),
        tool_name: "bash".to_string(),
        normalized_input: "printf ok".to_string(),
        input_summary: "test action".to_string(),
        risk_level: RiskLevel::High,
        action_class: ActionClass::CommandExecution,
        origin_kind: None,
        origin_id: None,
        origin_step_id: None,
        idempotency_key: None,
        created_at: moa_test_support::fixtures::pg_now(),
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
            source_provenance, source_kind,
            input, status, queued_at
        ) VALUES ($1, $2, $3, 1, $4, $5, 'test-owner', '{}'::JSONB, '{}'::JSONB,
                  '{}'::JSONB, $5, $5, '{}'::JSONB, '{}'::JSONB, '[]'::JSONB,
                  '{"kind":"generated_plan"}'::JSONB,
                  'generated_plan', '{}'::JSONB, 'queued', NOW())
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

async fn insert_execution_compensation(
    pool: &PgPool,
    task_origin: ExecutionTaskOrigin,
) -> ExecutionCompensationOrigin {
    let compensation_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO moa.execution_compensation (
            compensation_id, run_uid, forward_task_id, tenant_id,
            registered_sequence, forward_generation, compensator, mapped_input
        )
        SELECT $1, task.run_uid, task.task_id, task.tenant_id,
               1, $4, '{}'::JSONB, '{}'::JSONB
        FROM moa.execution_task AS task
        WHERE task.run_uid = $2 AND task.task_id = $3
        "#,
    )
    .bind(compensation_id)
    .bind(task_origin.run_uid)
    .bind(task_origin.task_uid)
    .bind(
        i64::try_from(task_origin.generation)
            .expect("fixture generation should fit PostgreSQL BIGINT"),
    )
    .execute(pool)
    .await
    .expect("execution compensation should insert");
    ExecutionCompensationOrigin {
        run_uid: task_origin.run_uid,
        compensation_id,
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
