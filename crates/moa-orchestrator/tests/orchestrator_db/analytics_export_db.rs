//! DB-backed coverage for the ClickHouse analytics exporter.
//!
//! These tests drive the real exporter export methods against an isolated,
//! fully-migrated Postgres database (via `bootstrap_test_db`) and a mock
//! ClickHouse server (the `clickhouse` crate `test-util` feature). They pin the
//! contracts that have no other integration coverage:
//! - `turn_fact` rows equal the `session_turn_metrics` matview field-for-field
//!   (parity by construction, since the SQL is shared);
//! - `events_raw` reads the authoritative append-time `turn_number`;
//! - a dimension upsert supersedes a mutated row with a higher `export_version`;
//! - the events cursor resumes from the persisted position after a restart.

use chrono::{DateTime, Duration, Utc};
use clickhouse::Client;
use clickhouse::test::{Mock, handlers};
use moa_analytics_export::{
    AnalyticsExporter, DimExecutionRunRow, DimExecutionTaskRow, DimSessionRow, EventRawRow,
    ToolCallFactRow, TurnFactRow,
};
use moa_artifacts::execution_plan::{ExecutionCancelPolicy, ExecutionPlanDefinition};
use moa_execution::{
    capability::{ExecutionEstimate, ExecutionHash},
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Builds an exporter over the isolated pool pointed at the mock ClickHouse.
/// A one-second poll gives a two-second overlap window for the cursor test.
fn exporter(pool: PgPool, mock: &Mock) -> AnalyticsExporter {
    exporter_with_batch(pool, mock, 5000)
}

/// Exporter with an explicit batch size, to force multi-batch pulls.
fn exporter_with_batch(pool: PgPool, mock: &Mock, batch_rows: usize) -> AnalyticsExporter {
    let client = Client::default().with_url(mock.url());
    AnalyticsExporter::with_client(pool, client, "moa".to_string(), 1, batch_rows)
}

async fn seed_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    // The `session_requires_agent_context` constraint trigger is deferred to
    // commit, so the session and its agent-context row must land in one
    // transaction.
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, storage_partition_id, user_id, channel, model, status) \
         VALUES ($1, $2, $3, 'chat', 'claude', 'running')",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind("user-1")
    .execute(&mut *tx)
    .await?;
    // The system-default artifact revision seeded by V000009; satisfies the
    // agent_revision_uid FK to moa.artifact_revision.
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;
    sqlx::query(
        "INSERT INTO session_agent_context \
             (session_id, storage_partition_id, user_id, agent_definition_ref, agent_revision_uid, \
              policy_hash, display_name, policy_snapshot) \
         VALUES ($1, $2, 'user-1', 'agent://system-default', $3, 'test-hash', 'Test Agent', '{}'::jsonb)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind(default_revision)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

struct EventFixture<'a> {
    pool: &'a PgPool,
    tenant: Uuid,
    session: Uuid,
}

impl EventFixture<'_> {
    async fn insert(
        &self,
        sequence_num: i64,
        turn_number: i64,
        event_type: &str,
        payload: serde_json::Value,
        timestamp: DateTime<Utc>,
    ) -> TestResult<()> {
        sqlx::query(
            "INSERT INTO events \
                 (id, session_id, storage_partition_id, user_id, tenant_id, sequence_num, turn_number, event_type, \
                  payload, timestamp) \
             VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(self.session)
        .bind(self.tenant.to_string())
        .bind(self.tenant)
        .bind(sequence_num)
        .bind(turn_number)
        .bind(event_type)
        .bind(payload)
        .bind(timestamp)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

/// Seeds a two-turn session: for each turn a ToolCall, its ToolResult, then a
/// BrainResponse carrying token/cost/model data.
async fn seed_two_turn_session(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
) -> TestResult<[Uuid; 2]> {
    seed_session(pool, tenant, session).await?;
    let base = moa_test_support::fixtures::pg_now() - Duration::days(1);
    let tool_a = Uuid::now_v7();
    let tool_b = Uuid::now_v7();
    let events = EventFixture {
        pool,
        tenant,
        session,
    };

    events
        .insert(
            1,
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_a, "tool_name": "search"}}),
            base,
        )
        .await?;
    events
        .insert(
            2,
            1,
            "ToolResult",
            json!({"data": {"tool_id": tool_a, "success": true, "duration_ms": 42.0}}),
            base + Duration::milliseconds(50),
        )
        .await?;
    events
        .insert(
            3,
            1,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 100.0, "input_tokens_uncached": 10,
            "input_tokens_cache_write": 2, "input_tokens_cache_read": 3, "output_tokens": 7,
            "cost_cents": 5}}),
            base + Duration::milliseconds(100),
        )
        .await?;
    events
        .insert(
            4,
            2,
            "ToolCall",
            json!({"data": {"tool_id": tool_b, "tool_name": "fetch"}}),
            base + Duration::seconds(1),
        )
        .await?;
    events
        .insert(
            5,
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_b, "success": false, "duration_ms": 10.0}}),
            base + Duration::seconds(1) + Duration::milliseconds(20),
        )
        .await?;
    events
        .insert(
            6,
            2,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 200.0, "input_tokens_uncached": 20,
            "input_tokens_cache_write": 0, "input_tokens_cache_read": 1, "output_tokens": 9,
            "cost_cents": 8}}),
            base + Duration::seconds(1) + Duration::milliseconds(100),
        )
        .await?;
    Ok([tool_a, tool_b])
}

/// Matview projection used to diff `turn_fact` row-for-row.
#[derive(Debug, sqlx::FromRow)]
struct MatviewTurn {
    turn_number: i64,
    model: Option<String>,
    llm_ms: f64,
    tool_ms: f64,
    tool_call_count: i64,
    input_tokens_uncached: i64,
    input_tokens_cache_write: i64,
    input_tokens_cache_read: i64,
    total_input_tokens: i64,
    output_tokens: i64,
    cost_cents: i64,
}

#[derive(Debug, Clone, Copy)]
struct ExecutionAnalyticsFixture {
    run_uid: Uuid,
    task_id: Uuid,
    skill_template_revision_uid: Uuid,
}

async fn seed_execution_analytics_fixture(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
) -> TestResult<ExecutionAnalyticsFixture> {
    let fixture = ExecutionAnalyticsFixture {
        run_uid: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        skill_template_revision_uid: Uuid::now_v7(),
    };
    let planning_context_uid = Uuid::now_v7();
    let planning_hash = "1".repeat(64);
    let plan_hash = ExecutionHash::from_bytes([0x22; 32]);
    let plan_hash_text = plan_hash.to_string();
    let plan = serde_json::to_value(CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::At {
                    at: chrono::Utc::now() + chrono::TimeDelta::hours(1),
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailTask,
            },
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes: Vec::new(),
        },
        plan_hash,
        catalog_hash: plan_hash,
        estimate: ExecutionEstimate {
            cost_microusd: 0,
            tokens: 0,
            tasks: 1,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        report: ExecutionValidationReport::default(),
    })?;

    sqlx::query(
        "INSERT INTO moa.execution_planning_context \
             (planning_context_uid, tenant_id, session_id, originating_user_sequence_num, \
              originating_user_event_hash, owner_user_id, planning_context_hash, snapshot) \
         VALUES ($1, $2, $3, 1, $4, 'user-1', $4, '{}'::JSONB)",
    )
    .bind(planning_context_uid)
    .bind(tenant)
    .bind(session)
    .bind(&planning_hash)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO moa.execution_run \
             (run_uid, tenant_id, session_id, originating_user_sequence_num, planning_context_uid, \
              planning_context_hash, owner_user_id, goal_contract, initial_plan, active_plan, \
              initial_plan_hash, active_plan_hash, capability_catalog, authorization_envelope, \
              source_provenance, source_kind, skill_template_ref, \
              skill_template_revision_uid, input, status, progress_total_tasks) \
         VALUES ($1, $2, $3, 1, $4, $5, 'user-1', \
                 '{\"requirements\":[{\"id\":\"r1\"},{\"id\":\"r2\"}], \
                   \"completion_checks\":[{\"id\":\"c1\"},{\"id\":\"c2\"}]}'::JSONB, \
                 $8, $8, $6, $6, '{}'::JSONB, '{}'::JSONB, \
                 jsonb_build_object( \
                    'kind', 'skill_template', \
                    'skill_template_ref', 'skill://billing-flow', \
                    'skill_template_revision_uid', lower($7::TEXT)), \
                 'skill_template', 'skill://billing-flow', $7, '{}'::JSONB, 'queued', 1)",
    )
    .bind(fixture.run_uid)
    .bind(tenant)
    .bind(session)
    .bind(planning_context_uid)
    .bind(&planning_hash)
    .bind(&plan_hash_text)
    .bind(fixture.skill_template_revision_uid)
    .bind(plan)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'running', started_at = queued_at + INTERVAL '250 milliseconds', \
             updated_at = queued_at + INTERVAL '250 milliseconds' \
         WHERE run_uid = $1",
    )
    .bind(fixture.run_uid)
    .execute(pool)
    .await?;

    let task_created_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT started_at + INTERVAL '100 milliseconds' \
         FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(fixture.run_uid)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.execution_task \
             (task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, attempt, \
              generation, input, task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, \
              estimate_tasks, estimate_tool_calls, estimate_retrieved_bytes, \
              reserved_cost_microusd, reserved_tokens, reserved_tasks, reserved_tool_calls, \
              reserved_retrieved_bytes, actual_cost_microusd, actual_tokens, actual_tasks, \
              actual_tool_calls, actual_retrieved_bytes, current_outcome, error, citations, \
              created_at, started_at, completed_at, updated_at) \
         VALUES ($1, $2, $3, 'lookup', 'invoice-42', 1, 'failed', 2, 3, '{}'::JSONB, \
                 '{\"kind\":\"capability\", \
                   \"reference\":{\"name\":\"docs.search\",\"version\":\"v2\"}}'::JSONB, \
                 '{\"max_attempts\":2,\"initial_backoff_ms\":5,\"max_backoff_ms\":10}'::JSONB, \
                 80, 120, 1, 2, 1024, 0, 0, 0, 0, 0, 75, 110, 1, 2, 900, \
                 '{\"class\":\"invalid_output\"}'::JSONB, \
                 '{\"class\":\"invalid_output\",\"message\":\"raw prose must not export\"}'::JSONB, \
                 '[{\"source\":\"doc-1\"},{\"source\":\"doc-2\"}]'::JSONB, \
                 $4, $4 + INTERVAL '150 milliseconds', \
                 $4 + INTERVAL '950 milliseconds', $4 + INTERVAL '950 milliseconds')",
    )
    .bind(fixture.task_id)
    .bind(fixture.run_uid)
    .bind(tenant)
    .bind(task_created_at)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'completed', output = '{}'::JSONB, \
             completion_check_results = \
                 '[{\"check_id\":\"c1\"},{\"check_id\":\"c2\"}]'::JSONB, \
             terminal_cause = '{\"kind\":\"completion\",\"limit_stop\":null}'::JSONB, \
             terminal_reason = 'completed', terminal_satisfied_requirement_count = 2, \
             terminal_requirement_count = 2, consumed_cost_microusd = 125, \
             consumed_tokens = 456, consumed_tasks = 1, consumed_tool_calls = 3, \
             consumed_retrieved_bytes = 4096, progress_completed_tasks = 1, \
             completed_at = started_at + INTERVAL '2 seconds', \
             updated_at = started_at + INTERVAL '2 seconds' \
         WHERE run_uid = $1",
    )
    .bind(fixture.run_uid)
    .execute(pool)
    .await?;

    Ok(fixture)
}

async fn seed_completed_execution_upgrade_state(
    pool: &PgPool,
    export_version_floor: DateTime<Utc>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO analytics.clickhouse_schema_upgrade_state ( \
             upgrade_key, database_uuid, run_table_uuid, task_table_uuid, \
             stage, upgrade_version, export_version_floor, \
             run_high_water_seq, run_high_water_id, task_high_water_seq, task_high_water_id, \
             run_page_seq, run_page_id, task_page_seq, task_page_id, completed_at \
         ) VALUES ( \
             'execution_dimensions', $3, $4, $5, 'complete', $1, $1, \
             0, $2, 0, $2, 0, $2, 0, $2, NOW() \
         )",
    )
    .bind(export_version_floor)
    .bind(Uuid::nil())
    .bind(Uuid::from_u128(1))
    .bind(Uuid::from_u128(2))
    .bind(Uuid::from_u128(3))
    .execute(pool)
    .await?;
    for table in ["dim_execution_runs", "dim_execution_tasks"] {
        sqlx::query(
            "INSERT INTO analytics.clickhouse_export_state ( \
                 table_name, cursor_ts, cursor_id, exported_at, cursor_seq \
             ) VALUES ($1, $2, $3, $2, 0)",
        )
        .bind(table)
        .bind(DateTime::<Utc>::UNIX_EPOCH)
        .bind(Uuid::nil())
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct ExecutionRunFact {
    run_uid: Uuid,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    session_id: Uuid,
    initial_plan_hash: String,
    active_plan_hash: String,
    plan_revision: i64,
    source_kind: String,
    skill_template_ref: Option<String>,
    skill_template_revision_uid: Option<Uuid>,
    status: String,
    terminal_reason: Option<String>,
    requirement_count: i64,
    satisfied_requirement_count: i64,
    completion_check_count: i64,
    logical_task_count: i64,
    queued_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    queue_to_start_ms: Option<f64>,
    completed_at: Option<DateTime<Utc>>,
    duration_ms: Option<f64>,
    reserved_cost_microusd: i64,
    actual_cost_microusd: i64,
    reserved_tokens: i64,
    actual_tokens: i64,
    reserved_tasks: i64,
    actual_tasks: i64,
    reserved_tool_calls: i64,
    actual_tool_calls: i64,
    reserved_retrieved_bytes: i64,
    actual_retrieved_bytes: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct ExecutionTaskFact {
    task_id: Uuid,
    run_uid: Uuid,
    tenant_id: Uuid,
    node_id: String,
    item_key: String,
    task_kind: String,
    capability_name: Option<String>,
    capability_version: Option<String>,
    plan_revision: i64,
    status: String,
    failure_class: Option<String>,
    attempt: i32,
    generation: i64,
    citation_count: i64,
    queue_latency_ms: Option<f64>,
    duration_ms: Option<f64>,
    reserved_cost_microusd: i64,
    actual_cost_microusd: i64,
    reserved_tokens: i64,
    actual_tokens: i64,
    reserved_tasks: i64,
    actual_tasks: i64,
    reserved_tool_calls: i64,
    actual_tool_calls: i64,
    reserved_retrieved_bytes: i64,
    actual_retrieved_bytes: i64,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn exact_u64(value: i64) -> u64 {
    u64::try_from(value).expect("analytics facts are nonnegative")
}

fn assert_optional_f64_eq(actual: Option<f64>, expected: Option<f64>, field: &str) {
    match (actual, expected) {
        (Some(actual), Some(expected)) => assert!(
            (actual - expected).abs() < 1e-9,
            "{field} differs: actual={actual} expected={expected}"
        ),
        (actual, expected) => assert_eq!(actual, expected, "{field} nullability differs"),
    }
}

fn assert_execution_run_parity(actual: &DimExecutionRunRow, expected: &ExecutionRunFact) {
    assert_eq!(actual.run_uid, expected.run_uid);
    assert_eq!(actual.tenant_id, expected.tenant_id);
    assert_eq!(actual.contact_id, expected.contact_id);
    assert_eq!(actual.session_id, expected.session_id);
    assert_eq!(actual.initial_plan_hash, expected.initial_plan_hash);
    assert_eq!(actual.active_plan_hash, expected.active_plan_hash);
    assert_eq!(actual.plan_revision, exact_u64(expected.plan_revision));
    assert_eq!(actual.source_kind, expected.source_kind);
    assert_eq!(actual.skill_template_ref, expected.skill_template_ref);
    assert_eq!(
        actual.skill_template_revision_uid,
        expected.skill_template_revision_uid
    );
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.terminal_reason, expected.terminal_reason);
    assert_eq!(
        actual.requirement_count,
        exact_u64(expected.requirement_count)
    );
    assert_eq!(
        actual.satisfied_requirement_count,
        exact_u64(expected.satisfied_requirement_count)
    );
    assert_eq!(
        actual.completion_check_count,
        exact_u64(expected.completion_check_count)
    );
    assert_eq!(
        actual.logical_task_count,
        exact_u64(expected.logical_task_count)
    );
    assert_eq!(actual.queued_at, expected.queued_at);
    assert_eq!(actual.started_at, expected.started_at);
    assert_optional_f64_eq(
        actual.queue_to_start_ms,
        expected.queue_to_start_ms,
        "queue_to_start_ms",
    );
    assert_eq!(actual.completed_at, expected.completed_at);
    assert_optional_f64_eq(actual.duration_ms, expected.duration_ms, "run duration_ms");
    assert_eq!(
        actual.reserved_cost_microusd,
        exact_u64(expected.reserved_cost_microusd)
    );
    assert_eq!(
        actual.actual_cost_microusd,
        exact_u64(expected.actual_cost_microusd)
    );
    assert_eq!(actual.reserved_tokens, exact_u64(expected.reserved_tokens));
    assert_eq!(actual.actual_tokens, exact_u64(expected.actual_tokens));
    assert_eq!(actual.reserved_tasks, exact_u64(expected.reserved_tasks));
    assert_eq!(actual.actual_tasks, exact_u64(expected.actual_tasks));
    assert_eq!(
        actual.reserved_tool_calls,
        exact_u64(expected.reserved_tool_calls)
    );
    assert_eq!(
        actual.actual_tool_calls,
        exact_u64(expected.actual_tool_calls)
    );
    assert_eq!(
        actual.reserved_retrieved_bytes,
        exact_u64(expected.reserved_retrieved_bytes)
    );
    assert_eq!(
        actual.actual_retrieved_bytes,
        exact_u64(expected.actual_retrieved_bytes)
    );
    assert_eq!(actual.created_at, expected.created_at);
    assert_eq!(actual.updated_at, expected.updated_at);
}

fn assert_execution_task_parity(actual: &DimExecutionTaskRow, expected: &ExecutionTaskFact) {
    assert_eq!(actual.task_id, expected.task_id);
    assert_eq!(actual.run_uid, expected.run_uid);
    assert_eq!(actual.tenant_id, expected.tenant_id);
    assert_eq!(actual.node_id, expected.node_id);
    assert_eq!(actual.item_key, expected.item_key);
    assert_eq!(actual.task_kind, expected.task_kind);
    assert_eq!(actual.capability_name, expected.capability_name);
    assert_eq!(actual.capability_version, expected.capability_version);
    assert_eq!(actual.plan_revision, exact_u64(expected.plan_revision));
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.failure_class, expected.failure_class);
    assert_eq!(
        actual.attempt,
        u32::try_from(expected.attempt).expect("attempt is nonnegative")
    );
    assert_eq!(actual.generation, exact_u64(expected.generation));
    assert_eq!(actual.citation_count, exact_u64(expected.citation_count));
    assert_optional_f64_eq(
        actual.queue_latency_ms,
        expected.queue_latency_ms,
        "queue_latency_ms",
    );
    assert_optional_f64_eq(actual.duration_ms, expected.duration_ms, "task duration_ms");
    assert_eq!(
        actual.reserved_cost_microusd,
        exact_u64(expected.reserved_cost_microusd)
    );
    assert_eq!(
        actual.actual_cost_microusd,
        exact_u64(expected.actual_cost_microusd)
    );
    assert_eq!(actual.reserved_tokens, exact_u64(expected.reserved_tokens));
    assert_eq!(actual.actual_tokens, exact_u64(expected.actual_tokens));
    assert_eq!(actual.reserved_tasks, exact_u64(expected.reserved_tasks));
    assert_eq!(actual.actual_tasks, exact_u64(expected.actual_tasks));
    assert_eq!(
        actual.reserved_tool_calls,
        exact_u64(expected.reserved_tool_calls)
    );
    assert_eq!(
        actual.actual_tool_calls,
        exact_u64(expected.actual_tool_calls)
    );
    assert_eq!(
        actual.reserved_retrieved_bytes,
        exact_u64(expected.reserved_retrieved_bytes)
    );
    assert_eq!(
        actual.actual_retrieved_bytes,
        exact_u64(expected.actual_retrieved_bytes)
    );
    assert_eq!(actual.started_at, expected.started_at);
    assert_eq!(actual.completed_at, expected.completed_at);
    assert_eq!(actual.created_at, expected.created_at);
    assert_eq!(actual.updated_at, expected.updated_at);
}

#[tokio::test]
async fn analytics_export_turn_fact_matches_matview_db() -> TestResult<()> {
    // Pins: exported turn_fact rows equal the session_turn_metrics matview
    // field-for-field on real event data (shared SQL, validates export plumbing).
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    let [tool_a, _tool_b] = seed_two_turn_session(&pool, tenant, session).await?;
    let base = moa_test_support::fixtures::pg_now() - Duration::hours(12);
    let events = EventFixture {
        pool: &pool,
        tenant,
        session,
    };
    events
        .insert(
            7,
            3,
            "ToolError",
            json!({"data": {"tool_id": tool_a}}),
            base,
        )
        .await?;
    events
        .insert(
            8,
            3,
            "ToolResult",
            json!({"data": {"tool_id": tool_a, "success": false, "duration_ms": 999.0}}),
            base + Duration::milliseconds(1),
        )
        .await?;

    let noisy_session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, noisy_session).await?;

    let mock = Mock::new();
    let turn_handler = mock.add(handlers::record::<TurnFactRow>());
    let tool_handler = mock.add(handlers::record::<ToolCallFactRow>());
    let exporter = exporter(pool.clone(), &mock);

    exporter.export_facts(&[session]).await?;

    let mut turn_rows: Vec<TurnFactRow> = turn_handler.collect().await;
    let mut tool_rows: Vec<ToolCallFactRow> = tool_handler.collect().await;
    turn_rows.sort_by_key(|row| row.turn_number);
    tool_rows.sort_by_key(|row| row.call_sequence_num);

    sqlx::query("REFRESH MATERIALIZED VIEW session_turn_metrics")
        .execute(&pool)
        .await?;
    let matview: Vec<MatviewTurn> = sqlx::query_as(
        "SELECT turn_number, model, llm_ms, tool_ms, tool_call_count, input_tokens_uncached, \
             input_tokens_cache_write, input_tokens_cache_read, total_input_tokens, output_tokens, \
             cost_cents \
         FROM session_turn_metrics WHERE session_id = $1 ORDER BY turn_number",
    )
    .bind(session)
    .fetch_all(&pool)
    .await?;

    assert_eq!(
        turn_rows.len(),
        matview.len(),
        "turn_fact row count must match matview"
    );
    assert_eq!(turn_rows.len(), 2, "two BrainResponse turns expected");
    assert!(
        turn_rows.iter().all(|row| row.session_id == session),
        "the explicit input-session relation must exclude unrelated noisy sessions"
    );
    assert_eq!(
        tool_rows.len(),
        2,
        "only the target session's tool calls export"
    );
    assert!(tool_rows.iter().all(|row| row.session_id == session));
    for (exported, expected) in turn_rows.iter().zip(matview.iter()) {
        assert_eq!(
            exported.tenant_id, tenant,
            "tenant_id stamped from the joined session"
        );
        assert_eq!(exported.session_id, session);
        assert_eq!(exported.turn_number, expected.turn_number);
        assert_eq!(exported.model, expected.model);
        assert!(
            (exported.llm_ms - expected.llm_ms).abs() < 1e-9,
            "llm_ms parity"
        );
        assert!(
            (exported.tool_ms - expected.tool_ms).abs() < 1e-9,
            "tool_ms parity"
        );
        assert_eq!(exported.tool_call_count, expected.tool_call_count);
        assert_eq!(
            exported.input_tokens_uncached,
            expected.input_tokens_uncached
        );
        assert_eq!(
            exported.input_tokens_cache_write,
            expected.input_tokens_cache_write
        );
        assert_eq!(
            exported.input_tokens_cache_read,
            expected.input_tokens_cache_read
        );
        assert_eq!(exported.total_input_tokens, expected.total_input_tokens);
        assert_eq!(exported.output_tokens, expected.output_tokens);
        assert_eq!(exported.cost_cents, expected.cost_cents);
    }
    // Turn 1's ToolResult reports 42ms; turn 2's reports 10ms — proves the
    // per-turn tool window and duration fallback carried through.
    assert!((turn_rows[0].tool_ms - 42.0).abs() < 1e-9);
    assert!((turn_rows[1].tool_ms - 10.0).abs() < 1e-9);
    assert_eq!(tool_rows[0].success, Some(true));
    assert_eq!(tool_rows[0].duration_ms, Some(42.0));

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_events_stamp_turn_number_db() -> TestResult<()> {
    // Pins: events_raw reads the authoritative append-time ordinal; a
    // BrainResponse counts itself.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    let events_handler = mock.add(handlers::record::<EventRawRow>());
    let exporter = exporter(pool.clone(), &mock);

    exporter.export_events().await?;

    let mut rows: Vec<EventRawRow> = events_handler.collect().await;
    rows.sort_by_key(|row| row.sequence_num);
    let turn_numbers: Vec<i64> = rows.iter().map(|row| row.turn_number).collect();
    assert_eq!(
        turn_numbers,
        vec![1, 1, 1, 2, 2, 2],
        "events before/at the first BrainResponse are turn 1; after it, turn 2"
    );
    assert!(rows.iter().all(|row| row.tenant_id == tenant));

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_turn_number_spans_batch_boundary_db() -> TestResult<()> {
    // Pins: the stored turn ordinal survives exporter batch boundaries. With
    // batch_rows=2 the six events are pulled in three batches; the second turn's
    // events land in a later batch than the BrainResponse that opened turn 1.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    // Three batches of two rows each -> three inserts.
    let batch_handlers = [
        mock.add(handlers::record::<EventRawRow>()),
        mock.add(handlers::record::<EventRawRow>()),
        mock.add(handlers::record::<EventRawRow>()),
    ];
    let exporter = exporter_with_batch(pool.clone(), &mock, 2);

    exporter.export_events().await?;

    let mut rows: Vec<EventRawRow> = Vec::new();
    for handler in batch_handlers {
        rows.extend(handler.collect::<Vec<EventRawRow>>().await);
    }
    assert_eq!(
        rows.len(),
        6,
        "all six events exported across three batches"
    );
    rows.sort_by_key(|row| row.sequence_num);
    let turn_numbers: Vec<i64> = rows.iter().map(|row| row.turn_number).collect();
    assert_eq!(
        turn_numbers,
        vec![1, 1, 1, 2, 2, 2],
        "later-batch events keep the append-time turn ordinal"
    );

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_dim_sessions_supersedes_on_update_db() -> TestResult<()> {
    // Pins: re-exporting a mutated session emits a row with a strictly higher
    // export_version (its new updated_at), so ReplacingMergeTree keeps the latest.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    let exporter = exporter(pool.clone(), &mock);

    let first_handler = mock.add(handlers::record::<DimSessionRow>());
    exporter.export_dim_sessions().await?;
    let first: Vec<DimSessionRow> = first_handler.collect().await;
    assert_eq!(first.len(), 1, "the seeded session is exported once");
    let first_version = first[0].export_version;

    // Mutate the session the way the store does (explicit updated_at bump).
    sqlx::query("UPDATE sessions SET status = 'completed', updated_at = NOW() WHERE id = $1")
        .bind(session)
        .execute(&pool)
        .await?;

    let second_handler = mock.add(handlers::record::<DimSessionRow>());
    exporter.export_dim_sessions().await?;
    let second: Vec<DimSessionRow> = second_handler.collect().await;
    assert_eq!(
        second.len(),
        1,
        "the overlap window re-reads the mutated row"
    );
    assert_eq!(second[0].session_id, session);
    assert_eq!(second[0].status, "completed");
    assert!(
        second[0].export_version > first_version,
        "the re-export must carry a higher export_version to supersede the prior copy"
    );

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn execution_analytics_export_matches_postgres_facts_field_for_field_db() -> TestResult<()> {
    // Pins: sequence-backed execution export emits every normalized execution field
    // exactly as the Postgres facts do, with page versions monotonic even when
    // the durable floor is ahead of the database clock.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;
    let fixture = seed_execution_analytics_fixture(&pool, tenant, session).await?;

    sqlx::query("REFRESH MATERIALIZED VIEW analytics.execution_run_fact")
        .execute(&pool)
        .await?;
    sqlx::query("REFRESH MATERIALIZED VIEW analytics.execution_task_fact")
        .execute(&pool)
        .await?;
    let run_fact: ExecutionRunFact = sqlx::query_as(
        "SELECT run_uid, tenant_id, contact_id, session_id, initial_plan_hash, active_plan_hash, \
                plan_revision, source_kind, skill_template_ref, \
                skill_template_revision_uid, status, terminal_reason, requirement_count, \
                satisfied_requirement_count, completion_check_count, logical_task_count, \
                queued_at, started_at, queue_to_start_ms, completed_at, duration_ms, \
                reserved_cost_microusd, actual_cost_microusd, reserved_tokens, actual_tokens, \
                reserved_tasks, actual_tasks, reserved_tool_calls, actual_tool_calls, \
                reserved_retrieved_bytes, actual_retrieved_bytes, created_at, updated_at \
         FROM analytics.execution_run_fact WHERE run_uid = $1",
    )
    .bind(fixture.run_uid)
    .fetch_one(&pool)
    .await?;
    let task_fact: ExecutionTaskFact = sqlx::query_as(
        "SELECT task_id, run_uid, tenant_id, node_id, item_key, task_kind, capability_name, \
                capability_version, plan_revision, status, failure_class, attempt, generation, \
                citation_count, queue_latency_ms, duration_ms, reserved_cost_microusd, \
                actual_cost_microusd, reserved_tokens, actual_tokens, reserved_tasks, \
                actual_tasks, reserved_tool_calls, actual_tool_calls, reserved_retrieved_bytes, \
                actual_retrieved_bytes, started_at, completed_at, created_at, updated_at \
         FROM analytics.execution_task_fact WHERE task_id = $1",
    )
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await?;

    let future_floor = moa_test_support::fixtures::pg_now() + Duration::days(1);
    seed_completed_execution_upgrade_state(&pool, future_floor).await?;
    let mock = Mock::new();
    let run_handler = mock.add(handlers::record::<DimExecutionRunRow>());
    let task_handler = mock.add(handlers::record::<DimExecutionTaskRow>());
    let exporter = exporter(pool.clone(), &mock);

    exporter.export_execution_dimensions().await?;

    let run_rows: Vec<DimExecutionRunRow> = run_handler.collect().await;
    let task_rows: Vec<DimExecutionTaskRow> = task_handler.collect().await;
    assert_eq!(run_rows.len(), 1);
    assert_eq!(task_rows.len(), 1);
    assert_execution_run_parity(&run_rows[0], &run_fact);
    assert_execution_task_parity(&task_rows[0], &task_fact);
    assert!(
        run_rows[0].export_version > future_floor,
        "run page must supersede a future-skewed existing version"
    );
    assert!(
        task_rows[0].export_version > run_rows[0].export_version,
        "each page claims a strictly monotonic export version"
    );

    for (table, expected_id) in [
        ("dim_execution_runs", fixture.run_uid),
        ("dim_execution_tasks", fixture.task_id),
    ] {
        let (cursor_seq, cursor_id, cursor_ts, exported_at, high_water_seq): (
            i64,
            Uuid,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT cursor_seq, cursor_id, cursor_ts, exported_at, pass_high_water_seq \
             FROM analytics.clickhouse_export_state WHERE table_name = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await?;
        assert!(cursor_seq > 0);
        assert_eq!(cursor_id, expected_id);
        assert_eq!(cursor_ts, exported_at);
        assert!(cursor_ts > DateTime::<Utc>::UNIX_EPOCH);
        assert_eq!(high_water_seq, None);
    }

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn execution_export_completes_when_the_captured_high_water_row_moves_db() -> TestResult<()> {
    // Pins: a row updated after a bounded high-water capture moves to a larger
    // sequence without making the old tuple unreconstructable. The interrupted
    // pass completes at its exact durable bound; the update lands next pass.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;
    let fixture = seed_execution_analytics_fixture(&pool, tenant, session).await?;
    seed_completed_execution_upgrade_state(&pool, moa_test_support::fixtures::pg_now()).await?;

    let old_run_seq: i64 =
        sqlx::query_scalar("SELECT analytics_change_seq FROM moa.execution_run WHERE run_uid = $1")
            .bind(fixture.run_uid)
            .fetch_one(&pool)
            .await?;
    let task_seq: i64 = sqlx::query_scalar(
        "SELECT analytics_change_seq FROM moa.execution_task WHERE task_id = $1",
    )
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE analytics.clickhouse_export_state \
         SET pass_high_water_seq = $1, pass_high_water_id = $2, pass_started_at = NOW() \
         WHERE table_name = 'dim_execution_runs'",
    )
    .bind(old_run_seq)
    .bind(fixture.run_uid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE analytics.clickhouse_export_state \
         SET cursor_seq = $1, cursor_id = $2, cursor_ts = NOW(), exported_at = NOW() \
         WHERE table_name = 'dim_execution_tasks'",
    )
    .bind(task_seq)
    .bind(fixture.task_id)
    .execute(&pool)
    .await?;

    sqlx::query(
        "UPDATE moa.execution_run SET updated_at = updated_at + INTERVAL '1 second' \
         WHERE run_uid = $1",
    )
    .bind(fixture.run_uid)
    .execute(&pool)
    .await?;
    let moved_run_seq: i64 =
        sqlx::query_scalar("SELECT analytics_change_seq FROM moa.execution_run WHERE run_uid = $1")
            .bind(fixture.run_uid)
            .fetch_one(&pool)
            .await?;
    assert!(moved_run_seq > old_run_seq);

    let mock = Mock::new();
    let exporter = exporter(pool.clone(), &mock);
    exporter.export_execution_dimensions().await?;
    let (first_cursor, first_id, first_high_water): (i64, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT cursor_seq, cursor_id, pass_high_water_seq \
         FROM analytics.clickhouse_export_state WHERE table_name = 'dim_execution_runs'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((first_cursor, first_id), (old_run_seq, fixture.run_uid));
    assert_eq!(first_high_water, None);

    let run_handler = mock.add(handlers::record::<DimExecutionRunRow>());
    exporter.export_execution_dimensions().await?;
    let rows: Vec<DimExecutionRunRow> = run_handler.collect().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_uid, fixture.run_uid);
    let (second_cursor, second_id): (i64, Uuid) = sqlx::query_as(
        "SELECT cursor_seq, cursor_id FROM analytics.clickhouse_export_state \
         WHERE table_name = 'dim_execution_runs'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((second_cursor, second_id), (moved_run_seq, fixture.run_uid));

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_events_cursor_resumes_after_restart_db() -> TestResult<()> {
    // Pins: a second export pass resumes from the persisted cursor rather than
    // replaying history — only rows past the overlap-rewound cursor are re-read.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;

    let base = moa_test_support::fixtures::pg_now() - Duration::days(1);
    let empty = json!({"data": {}});
    let events = EventFixture {
        pool: &pool,
        tenant,
        session,
    };
    events
        .insert(1, 1, "BrainResponse", empty.clone(), base)
        .await?;
    events
        .insert(
            2,
            2,
            "BrainResponse",
            empty.clone(),
            base + Duration::seconds(10),
        )
        .await?;
    events
        .insert(
            3,
            3,
            "BrainResponse",
            empty.clone(),
            base + Duration::seconds(20),
        )
        .await?;

    let mock = Mock::new();
    let exporter = exporter(pool.clone(), &mock);

    let first_handler = mock.add(handlers::record::<EventRawRow>());
    exporter.export_events().await?;
    let first: Vec<EventRawRow> = first_handler.collect().await;
    assert_eq!(first.len(), 3, "the first pass backfills all three events");

    // A new event well past the two-second overlap window.
    events
        .insert(4, 4, "BrainResponse", empty, base + Duration::seconds(120))
        .await?;

    let second_handler = mock.add(handlers::record::<EventRawRow>());
    exporter.export_events().await?;
    let second: Vec<EventRawRow> = second_handler.collect().await;

    let second_seqs: Vec<i64> = second.iter().map(|row| row.sequence_num).collect();
    assert!(
        second_seqs.contains(&4),
        "the new event must be exported on resume"
    );
    assert!(
        !second_seqs.contains(&1) && !second_seqs.contains(&2),
        "events older than the overlap window must not be re-read: {second_seqs:?}"
    );

    pool.close().await;
    Ok(())
}
