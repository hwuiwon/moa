//! Cross-backend parity harness: every catalog dataset queried against the
//! Postgres materialized-view backend and the ClickHouse read-model backend on
//! the same seeded data must return identical cells.
//!
//! The corpus is seeded so that *every* catalog dataset has rows (sessions with
//! multi-turn ToolCall/ToolResult/ToolError/BrainResponse/Error sequences, task
//! segments with and without skills, artifact runs + node runs, learning
//! candidates, and experiment runs). The Postgres path refreshes the
//! `analytics.*_fact` matviews (via the store's canonical refresh, the same list
//! `moa-session` uses); the ClickHouse path runs the real exporter
//! (`ensure_clickhouse_schema` + `run_one_pass`) into a per-run isolated CH
//! database. A catalog-driven query battery then runs each dataset's dimensions
//! and measures through both `AnalyticsService::new().query` and
//! `AnalyticsService::clickhouse().query_clickhouse`, and the results are diffed
//! with normalization: rows are canonically sorted by their dimension cells,
//! numbers compared with a relative tolerance, timestamp cells normalized to
//! epoch micros, and `Null == Null`.
//!
//! Run with the compose services (Postgres up, ClickHouse started):
//! `docker compose --profile clickhouse start clickhouse`, then
//! `MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 MOA_DATABASE_URL=... cargo nextest run \
//!  -p moa-orchestrator --run-ignored all -E 'test(analytics_parity)'`.

use chrono::{DateTime, Duration, Utc};
use moa_analytics::{AnalyticsClickHouseClient, AnalyticsService};
use moa_core::config::ClickHouseConfig;
use moa_core::types::identifiers::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDataset, AnalyticsDimension, AnalyticsFieldKind,
    AnalyticsFieldRole, AnalyticsFilter, AnalyticsFilterOperator, AnalyticsMeasure,
    AnalyticsOrderBy, AnalyticsQueryRequest, AnalyticsQueryResponse, AnalyticsSortDirection,
};
use moa_orchestrator::analytics_export::AnalyticsExporter;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Relative tolerance for float measure comparison across the two SQL engines.
const FLOAT_REL_TOLERANCE: f64 = 1e-6;

/// Documented parity exceptions: `(dataset_id, field_id, reason)`.
///
/// A tuple here means the field is knowingly served differently by the
/// ClickHouse backend and the battery skips comparing it. Each is a verified
/// semantic gap, listed in the run report.
const PARITY_EXCEPTIONS: &[(&str, &str, &str)] = &[
    // The Postgres `tool_call_analytics` view exposes `finished_at` as the actual
    // ToolResult/ToolError event timestamp (`COALESCE(result.timestamp,
    // error.timestamp)`). The ClickHouse `tool_call_fact` table persists only
    // `called_at` (`ts`) and `duration_ms`, so the dialect reconstructs
    // `finished_at = called_at + duration_ms`. The two agree only when a tool's
    // self-reported `duration_ms` equals the wall-clock gap between its ToolCall
    // and ToolResult events; they diverge otherwise. CH cannot serve the exact
    // PG value without also exporting the result-event timestamp.
    (
        "tool_calls",
        "finished_at",
        "CH reconstructs finished_at = called_at + reported duration_ms; PG uses the actual \
         ToolResult/ToolError event timestamp",
    ),
];

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn analytics_parity_all_datasets_docker() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    seed_corpus(&pool, tenant).await?;

    // Postgres backend: refresh the `analytics.*_fact` matviews (and their
    // `session_turn_metrics` dependency) using the store's canonical refresh,
    // which is the exact list `moa-session` refreshes in production and orders
    // `session_turn_metrics` before `turn_fact`.
    test_db
        .store()
        .refresh_analytics_materialized_views()
        .await?;

    // ClickHouse backend: bootstrap schema and run one full export pass into an
    // isolated CH database so concurrent runs cannot collide.
    let config = ClickHouseConfig {
        url: std::env::var("MOA_CLICKHOUSE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10061".to_string()),
        database: format!("moa_analytics_parity_{}", Uuid::now_v7().simple()),
        user: Some(std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string())),
        password: Some(
            std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string()),
        ),
        ..ClickHouseConfig::default()
    };
    let exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    exporter.ensure_clickhouse_schema().await?;
    exporter.run_one_pass().await?;

    let ch_client = AnalyticsClickHouseClient::connect(&config);
    let pg_service = AnalyticsService::new();
    let ch_service = AnalyticsService::clickhouse();
    let tenant_id = TenantId::from(tenant);

    let catalog = pg_service.catalog();
    let mut battery_total = 0usize;
    let mut coverage: Vec<String> = Vec::new();

    for dataset in &catalog.datasets {
        let queries = build_battery(dataset, tenant_id);
        let dims = role_fields(dataset, AnalyticsFieldRole::Dimension).len();
        let measures = role_fields(dataset, AnalyticsFieldRole::Measure).len();
        coverage.push(format!(
            "{}: {} queries ({} dims, {} measures)",
            dataset.id,
            queries.len(),
            dims,
            measures
        ));
        for (label, request) in queries {
            battery_total += 1;
            let pg_result = pg_service.query(&pool, request.clone()).await;
            let ch_result = ch_service
                .query_clickhouse(&ch_client, request.clone())
                .await;
            match (pg_result, ch_result) {
                (Ok(pg_response), Ok(ch_response)) => {
                    diff_responses(&dataset.id, &label, &pg_response, &ch_response)?;
                }
                (Err(pg_error), Err(ch_error)) => {
                    // Both backends reject the query the same way (e.g. a shared
                    // validation error) — not a parity violation.
                    eprintln!(
                        "parity: {}/{} rejected by both backends (pg: {pg_error}; ch: {ch_error})",
                        dataset.id, label
                    );
                }
                (pg_result, ch_result) => {
                    return Err(format!(
                        "backend availability mismatch for {}/{}: pg={:?} ch={:?}",
                        dataset.id,
                        label,
                        pg_result.map(|response| response.rows.len()),
                        ch_result.map(|response| response.rows.len()),
                    )
                    .into());
                }
            }
        }
    }

    println!("=== analytics parity battery ===");
    for line in &coverage {
        println!("  {line}");
    }
    println!(
        "  TOTAL: {battery_total} queries across {} datasets, {} documented exceptions",
        catalog.datasets.len(),
        PARITY_EXCEPTIONS.len()
    );

    Ok(())
}

/// Fields of a dataset with the requested role, in catalog order.
fn role_fields(
    dataset: &AnalyticsDataset,
    role: AnalyticsFieldRole,
) -> Vec<&moa_core::wire::analytics::AnalyticsField> {
    dataset
        .fields
        .iter()
        .filter(|field| field.role == role)
        .collect()
}

/// Whether `(dataset, field)` is a documented parity exception to skip.
fn is_excepted(dataset_id: &str, field_id: &str) -> bool {
    PARITY_EXCEPTIONS
        .iter()
        .any(|(dataset, field, _)| *dataset == dataset_id && *field == field_id)
}

/// Builds the representative query battery for one dataset:
/// (a) each dimension alone with a `count`; (b) each measure with `sum` and
/// `avg`; (c) one `p95`; (d) one time-range `between` filter; (e) one
/// `ORDER BY` a measure with `LIMIT`.
fn build_battery(
    dataset: &AnalyticsDataset,
    tenant_id: TenantId,
) -> Vec<(String, AnalyticsQueryRequest)> {
    let mut battery = Vec::new();
    let dims = role_fields(dataset, AnalyticsFieldRole::Dimension);
    let measures = role_fields(dataset, AnalyticsFieldRole::Measure);

    let base = |dataset_id: &str| AnalyticsQueryRequest {
        dataset: dataset_id.to_string(),
        tenant_id: Some(tenant_id),
        dimensions: Vec::new(),
        measures: Vec::new(),
        filters: Vec::new(),
        order_by: Vec::new(),
        limit: Some(1000),
    };
    let count_measure = || AnalyticsMeasure {
        field: None,
        aggregation: AnalyticsAggregation::Count,
        alias: Some("row_count".to_string()),
    };

    // (a) each dimension alone + Count.
    for field in &dims {
        if is_excepted(&dataset.id, &field.id) {
            continue;
        }
        let mut request = base(&dataset.id);
        request.dimensions = vec![AnalyticsDimension {
            field: field.id.clone(),
            alias: None,
        }];
        request.measures = vec![count_measure()];
        battery.push((format!("dim[{}]+count", field.id), request));
    }

    // (b) each numeric measure with Sum and Avg.
    for field in &measures {
        if is_excepted(&dataset.id, &field.id) {
            continue;
        }
        for aggregation in [AnalyticsAggregation::Sum, AnalyticsAggregation::Avg] {
            let mut request = base(&dataset.id);
            request.measures = vec![AnalyticsMeasure {
                field: Some(field.id.clone()),
                aggregation,
                alias: Some("agg".to_string()),
            }];
            battery.push((format!("measure[{}]:{:?}", field.id, aggregation), request));
        }
    }

    // (c) one P95 on the first numeric measure.
    if let Some(field) = measures
        .iter()
        .find(|field| !is_excepted(&dataset.id, &field.id))
    {
        let mut request = base(&dataset.id);
        request.measures = vec![AnalyticsMeasure {
            field: Some(field.id.clone()),
            aggregation: AnalyticsAggregation::P95,
            alias: Some("p95".to_string()),
        }];
        battery.push((format!("measure[{}]:P95", field.id), request));
    }

    // (d) one time-range filter on the dataset's default time field.
    if let Some(time_field) = dataset.default_time_field.as_deref() {
        let mut request = base(&dataset.id);
        request.measures = vec![count_measure()];
        let low = (Utc::now() - Duration::days(2)).to_rfc3339();
        let high = (Utc::now() + Duration::days(1)).to_rfc3339();
        request.filters = vec![AnalyticsFilter {
            field: time_field.to_string(),
            operator: AnalyticsFilterOperator::Between,
            value: Some(AnalyticsCell::Json(json!([low, high]))),
        }];
        battery.push((format!("timefilter[{time_field}]"), request));
    }

    // (e) one ORDER BY a measure + LIMIT. The limit is set above the group count
    // so ORDER BY only reorders (never truncates): the two engines break ties on
    // equal measures differently, and the diff canonically re-sorts anyway, so a
    // truncating limit could drop different tied rows per backend and is avoided
    // deliberately.
    if let (Some(dim), Some(measure)) = (
        dims.iter()
            .find(|field| !is_excepted(&dataset.id, &field.id)),
        measures
            .iter()
            .find(|field| !is_excepted(&dataset.id, &field.id)),
    ) {
        let mut request = base(&dataset.id);
        request.dimensions = vec![AnalyticsDimension {
            field: dim.id.clone(),
            alias: None,
        }];
        request.measures = vec![AnalyticsMeasure {
            field: Some(measure.id.clone()),
            aggregation: AnalyticsAggregation::Sum,
            alias: Some("ordered".to_string()),
        }];
        request.order_by = vec![AnalyticsOrderBy {
            field: "ordered".to_string(),
            direction: AnalyticsSortDirection::Desc,
        }];
        request.limit = Some(1000);
        battery.push((format!("orderby[{}]", measure.id), request));
    }

    battery
}

/// Diffs two analytics responses after normalization, returning `Err` on any
/// structural or cell mismatch with both row sets printed.
fn diff_responses(
    dataset_id: &str,
    label: &str,
    pg: &AnalyticsQueryResponse,
    ch: &AnalyticsQueryResponse,
) -> TestResult<()> {
    if pg.columns.len() != ch.columns.len() {
        return Err(format!(
            "{dataset_id}/{label}: column count differs pg={} ch={}",
            pg.columns.len(),
            ch.columns.len()
        )
        .into());
    }
    for (index, (pg_col, ch_col)) in pg.columns.iter().zip(ch.columns.iter()).enumerate() {
        if pg_col.kind != ch_col.kind {
            return Err(format!(
                "{dataset_id}/{label}: column {index} kind differs pg={:?} ch={:?}",
                pg_col.kind, ch_col.kind
            )
            .into());
        }
    }
    let kinds: Vec<AnalyticsFieldKind> = pg.columns.iter().map(|column| column.kind).collect();
    let dim_indices: Vec<usize> = pg
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.role == AnalyticsFieldRole::Dimension)
        .map(|(index, _)| index)
        .collect();

    let pg_rows = normalized_sorted(&pg.rows, &kinds, &dim_indices);
    let ch_rows = normalized_sorted(&ch.rows, &kinds, &dim_indices);

    if pg_rows.len() != ch_rows.len() {
        return Err(format!(
            "{dataset_id}/{label}: row count differs pg={} ch={}\n  pg={:?}\n  ch={:?}",
            pg_rows.len(),
            ch_rows.len(),
            pg.rows,
            ch.rows
        )
        .into());
    }

    for (row_index, (pg_row, ch_row)) in pg_rows.iter().zip(ch_rows.iter()).enumerate() {
        for (col_index, kind) in kinds.iter().enumerate() {
            let pg_cell = &pg_row[col_index];
            let ch_cell = &ch_row[col_index];
            if !cells_equal(*kind, pg_cell, ch_cell) {
                return Err(format!(
                    "{dataset_id}/{label}: mismatch at row {row_index} col {col_index} ({kind:?}) \
                     pg={pg_cell:?} ch={ch_cell:?}\n  pg_rows={pg_rows:?}\n  ch_rows={ch_rows:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Canonical value used for comparison and sorting, one per cell.
#[derive(Debug, Clone, PartialEq)]
enum NormCell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// Timestamp normalized to epoch microseconds (backend-independent).
    Micros(i64),
}

/// Normalizes each response row to `NormCell`s and sorts rows by their dimension
/// cells, which are exact across backends and so give a backend-independent
/// order.
fn normalized_sorted(
    rows: &[Vec<AnalyticsCell>],
    kinds: &[AnalyticsFieldKind],
    dim_indices: &[usize],
) -> Vec<Vec<NormCell>> {
    let mut normalized: Vec<Vec<NormCell>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(kinds.iter())
                .map(|(cell, kind)| normalize_cell(*kind, cell))
                .collect()
        })
        .collect();
    normalized.sort_by_key(|row| sort_key(row, dim_indices));
    normalized
}

/// Builds a stable, lexicographically comparable key from a row's dimension
/// cells.
fn sort_key(row: &[NormCell], dim_indices: &[usize]) -> String {
    let mut key = String::new();
    for &index in dim_indices {
        match &row[index] {
            NormCell::Null => key.push_str("\u{0}null"),
            NormCell::Bool(value) => key.push_str(if *value { "b1" } else { "b0" }),
            NormCell::Int(value) => key.push_str(&format!("i{value:020}")),
            NormCell::Micros(value) => key.push_str(&format!("t{value:020}")),
            // Floats are never dimensions in the catalog, but normalize defensively.
            NormCell::Float(value) => key.push_str(&format!("f{value:.9}")),
            NormCell::Text(value) => key.push_str(value),
        }
        key.push('\u{1}');
    }
    key
}

/// Normalizes one cell according to its column kind.
fn normalize_cell(kind: AnalyticsFieldKind, cell: &AnalyticsCell) -> NormCell {
    match cell {
        AnalyticsCell::Null => NormCell::Null,
        AnalyticsCell::Bool(value) => NormCell::Bool(*value),
        AnalyticsCell::Number(number) => match kind {
            AnalyticsFieldKind::Integer => number
                .as_i64()
                .map(NormCell::Int)
                .unwrap_or_else(|| NormCell::Float(number.as_f64().unwrap_or(f64::NAN))),
            _ => NormCell::Float(number.as_f64().unwrap_or(f64::NAN)),
        },
        AnalyticsCell::String(value) => match kind {
            // Both backends render timestamps as RFC3339 strings; convert to
            // epoch micros so formatting differences never fail the diff.
            AnalyticsFieldKind::Timestamp => DateTime::parse_from_rfc3339(value)
                .map(|timestamp| NormCell::Micros(timestamp.timestamp_micros()))
                .unwrap_or_else(|_| NormCell::Text(value.clone())),
            _ => NormCell::Text(value.clone()),
        },
        AnalyticsCell::Json(value) => NormCell::Text(value.to_string()),
    }
}

/// Compares two normalized cells: numbers within a relative tolerance, everything
/// else exact, `Null == Null`.
fn cells_equal(_kind: AnalyticsFieldKind, left: &NormCell, right: &NormCell) -> bool {
    match (left, right) {
        (NormCell::Null, NormCell::Null) => true,
        (NormCell::Bool(a), NormCell::Bool(b)) => a == b,
        (NormCell::Int(a), NormCell::Int(b)) => a == b,
        (NormCell::Micros(a), NormCell::Micros(b)) => a == b,
        (NormCell::Text(a), NormCell::Text(b)) => a == b,
        // Allow integer/float cross-representation and float tolerance.
        (NormCell::Float(a), NormCell::Float(b)) => floats_close(*a, *b),
        (NormCell::Int(a), NormCell::Float(b)) | (NormCell::Float(b), NormCell::Int(a)) => {
            floats_close(*a as f64, *b)
        }
        _ => false,
    }
}

fn floats_close(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= FLOAT_REL_TOLERANCE * scale
}

// ---------------------------------------------------------------------------
// Corpus seeding
// ---------------------------------------------------------------------------

/// Seeds a corpus rich enough that every catalog dataset has rows.
async fn seed_corpus(pool: &PgPool, tenant: Uuid) -> TestResult<()> {
    let base = Utc::now() - Duration::days(1);

    // Session 1: chat / completed / claude, two turns exercising ToolCall +
    // ToolResult (success) and ToolCall + ToolError, plus a standalone Error.
    let session1 = Uuid::now_v7();
    seed_session(
        pool,
        tenant,
        session1,
        "chat",
        "completed",
        "claude",
        40,
        8,
        5,
        3,
        22,
        17,
    )
    .await?;
    let tool_a = Uuid::now_v7();
    let tool_b = Uuid::now_v7();
    let events1 = vec![
        (
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_a, "tool_name": "search"}}),
            None,
            0,
        ),
        (
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_a, "success": true, "duration_ms": 42.0}}),
            None,
            50,
        ),
        (
            3,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 110.0, "input_tokens_uncached": 20,
                "input_tokens_cache_write": 4, "input_tokens_cache_read": 6, "output_tokens": 9,
                "cost_cents": 12}}),
            Some(31),
            100,
        ),
        (
            4,
            "ToolCall",
            json!({"data": {"tool_id": tool_b, "tool_name": "fetch"}}),
            None,
            1000,
        ),
        (
            5,
            "ToolError",
            json!({"data": {"tool_id": tool_b, "error": "timeout"}}),
            None,
            1030,
        ),
        (
            6,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 205.0, "input_tokens_uncached": 30,
                "input_tokens_cache_write": 1, "input_tokens_cache_read": 5, "output_tokens": 11,
                "cost_cents": 15}}),
            Some(44),
            1100,
        ),
        (
            7,
            "Error",
            json!({"data": {"message": "recoverable"}}),
            None,
            1200,
        ),
    ];
    seed_events(pool, tenant, session1, &events1, base).await?;

    // Session 2: slack / running / gpt, one tool turn.
    let session2 = Uuid::now_v7();
    seed_session(
        pool, tenant, session2, "slack", "running", "gpt", 12, 3, 2, 1, 9, 4,
    )
    .await?;
    let tool_c = Uuid::now_v7();
    let events2 = vec![
        (
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_c, "tool_name": "summarize"}}),
            None,
            0,
        ),
        (
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_c, "success": false, "duration_ms": 88.0}}),
            None,
            60,
        ),
        (
            3,
            "BrainResponse",
            json!({"data": {"model": "gpt", "duration_ms": 150.0, "input_tokens_uncached": 12,
                "input_tokens_cache_write": 0, "input_tokens_cache_read": 2, "output_tokens": 5,
                "cost_cents": 7}}),
            Some(18),
            120,
        ),
    ];
    seed_events(pool, tenant, session2, &events2, base + Duration::hours(2)).await?;

    // Session 3: chat / failed / claude, one no-tool turn.
    let session3 = Uuid::now_v7();
    seed_session(
        pool, tenant, session3, "chat", "failed", "claude", 3, 1, 1, 0, 4, 2,
    )
    .await?;
    let events3 = vec![(
        1,
        "BrainResponse",
        json!({"data": {"model": "claude", "duration_ms": 90.0, "input_tokens_uncached": 8,
            "input_tokens_cache_write": 0, "input_tokens_cache_read": 0, "output_tokens": 3,
            "cost_cents": 4}}),
        Some(11),
        0,
    )];
    seed_events(pool, tenant, session3, &events3, base + Duration::hours(4)).await?;

    // Task segments: one with skills (feeds the skills dataset via unnest), one
    // without, varying outcome.
    seed_task_segment(
        pool,
        tenant,
        session1,
        0,
        "resolve billing question",
        Some("success"),
        Some(0.91),
        &["search", "fetch"],
        &["search_docs", "summarize"],
        2,
        29,
        base,
        Some(base + Duration::seconds(3)),
    )
    .await?;
    seed_task_segment(
        pool,
        tenant,
        session2,
        0,
        "summarize thread",
        Some("failure"),
        Some(0.40),
        &["summarize"],
        &[],
        1,
        9,
        base + Duration::hours(2),
        Some(base + Duration::hours(2) + Duration::seconds(2)),
    )
    .await?;

    // Artifact run + node run.
    let run_uid = Uuid::now_v7();
    seed_artifact_run(
        pool,
        tenant,
        run_uid,
        session1,
        "skill://billing-flow",
        "completed",
        base,
        Some(base + Duration::seconds(5)),
    )
    .await?;
    seed_artifact_node_run(
        pool,
        tenant,
        run_uid,
        "collect-input",
        "completed",
        base,
        Some(base + Duration::seconds(1)),
    )
    .await?;
    seed_artifact_node_run(
        pool,
        tenant,
        run_uid,
        "draft-reply",
        "failed",
        base + Duration::seconds(1),
        Some(base + Duration::seconds(4)),
    )
    .await?;

    // Learning candidates.
    seed_learning_candidate(pool, tenant, "skill", "proposed", Some(0.72), "low", base).await?;
    seed_learning_candidate(
        pool,
        tenant,
        "memory",
        "evaluating",
        Some(0.55),
        "medium",
        base + Duration::hours(1),
    )
    .await?;

    // Experiment run (needs a score_run parent for the NOT NULL FK).
    let score_run = Uuid::now_v7();
    seed_score_run(pool, tenant, score_run).await?;
    seed_experiment_run(
        pool,
        tenant,
        session1,
        score_run,
        "billing-eval",
        "completed",
        base,
        Some(base + Duration::seconds(30)),
    )
    .await?;

    Ok(())
}

/// Seeds a session with its required agent-context row (deferred constraint) and
/// explicit denormalized rollup columns so the token/cost/count measures have
/// real values. `total_input_tokens` and `cache_hit_rate` are generated columns
/// and are intentionally not set.
#[allow(clippy::too_many_arguments)]
async fn seed_session(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    channel: &str,
    status: &str,
    model: &str,
    event_count: i64,
    turn_count: i64,
    uncached: i64,
    cache_read: i64,
    output: i64,
    cost_cents: i64,
) -> TestResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, storage_partition_id, user_id, channel, model, status, \
             event_count, turn_count, total_input_tokens_uncached, total_input_tokens_cache_write, \
             total_input_tokens_cache_read, total_output_tokens, total_cost_cents) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $10, $11, $12)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind("user-1")
    .bind(channel)
    .bind(model)
    .bind(status)
    .bind(event_count)
    .bind(turn_count)
    .bind(uncached)
    .bind(cache_read)
    .bind(output)
    .bind(cost_cents)
    .execute(&mut *tx)
    .await?;
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;
    // A resolved (non-null) agent_id, as production populates it. When agent_id
    // is NULL the two backends differ (the exporter coalesces it to '' while the
    // Postgres matview passes NULL through) — a documented NULL-handling gap the
    // realistic corpus avoids by resolving the agent.
    let agent_id = Uuid::parse_str("00000000-0000-4000-8000-0000000ada01")?;
    sqlx::query(
        "INSERT INTO session_agent_context \
             (session_id, storage_partition_id, user_id, agent_id, agent_definition_ref, \
              agent_revision_uid, policy_hash, display_name, policy_snapshot) \
         VALUES ($1, $2, 'user-1', $3, 'agent://system-default', $4, 'test-hash', 'Test Agent', '{}'::jsonb)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind(agent_id)
    .bind(default_revision)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

type SeededEvent<'a> = (i64, &'a str, serde_json::Value, Option<i32>, i64);

async fn seed_events(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    events: &[SeededEvent<'_>],
    base: DateTime<Utc>,
) -> TestResult<()> {
    for (sequence_num, event_type, payload, token_count, offset_ms) in events {
        sqlx::query(
            "INSERT INTO events \
                 (id, session_id, storage_partition_id, user_id, tenant_id, sequence_num, \
                  event_type, payload, token_count, timestamp) \
             VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(session)
        .bind(tenant.to_string())
        .bind(tenant)
        .bind(*sequence_num)
        .bind(*event_type)
        .bind(payload)
        .bind(*token_count)
        .bind(base + Duration::milliseconds(*offset_ms))
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_task_segment(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    segment_index: i32,
    task_summary: &str,
    outcome: Option<&str>,
    outcome_confidence: Option<f64>,
    tools_used: &[&str],
    skills_activated: &[&str],
    turn_count: i64,
    token_cost: i64,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    let tools: Vec<String> = tools_used.iter().map(|value| value.to_string()).collect();
    let skills: Vec<String> = skills_activated
        .iter()
        .map(|value| value.to_string())
        .collect();
    sqlx::query(
        "INSERT INTO task_segments \
             (id, session_id, storage_partition_id, user_id, tenant_id, segment_index, \
              task_summary, outcome, outcome_confidence, tools_used, skills_activated, turn_count, \
              token_cost, started_at, ended_at) \
         VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(Uuid::now_v7())
    .bind(session)
    .bind(tenant.to_string())
    .bind(tenant.to_string())
    .bind(segment_index)
    .bind(task_summary)
    .bind(outcome)
    .bind(outcome_confidence)
    .bind(&tools)
    .bind(&skills)
    .bind(turn_count)
    .bind(token_cost)
    .bind(started_at)
    .bind(ended_at)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_artifact_run(
    pool: &PgPool,
    tenant: Uuid,
    run_uid: Uuid,
    session: Uuid,
    procedure_ref: &str,
    status: &str,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;
    sqlx::query(
        "INSERT INTO moa.artifact_run \
             (run_uid, tenant_id, revision_uid, storage_partition_id, user_id, session_id, \
              procedure_ref, status, input, state, started_at, completed_at) \
         VALUES ($1, $2, $3, $4, 'user-1', $5, $6, $7, '{}'::jsonb, '{}'::jsonb, $8, $9)",
    )
    .bind(run_uid)
    .bind(tenant)
    .bind(default_revision)
    .bind(tenant.to_string())
    .bind(session)
    .bind(procedure_ref)
    .bind(status)
    .bind(started_at)
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_artifact_node_run(
    pool: &PgPool,
    tenant: Uuid,
    run_uid: Uuid,
    node_id: &str,
    status: &str,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO moa.artifact_node_run \
             (node_run_uid, run_uid, tenant_id, storage_partition_id, user_id, node_id, status, \
              input, started_at, completed_at) \
         VALUES ($1, $2, $3, $4, 'user-1', $5, $6, '{}'::jsonb, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(run_uid)
    .bind(tenant)
    .bind(tenant.to_string())
    .bind(node_id)
    .bind(status)
    .bind(started_at)
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_learning_candidate(
    pool: &PgPool,
    tenant: Uuid,
    candidate_type: &str,
    status: &str,
    confidence: Option<f64>,
    risk_class: &str,
    updated_at: DateTime<Utc>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO learning_candidates \
             (id, tenant_id, storage_partition_id, candidate_type, status, target_id, \
              target_label, payload, confidence, risk_class, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, '{}'::jsonb, $8, $9, $10, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.to_string())
    .bind(tenant.to_string())
    .bind(candidate_type)
    .bind(status)
    .bind(format!("target-{candidate_type}"))
    .bind(format!("Target {candidate_type}"))
    .bind(confidence)
    .bind(risk_class)
    .bind(updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_score_run(pool: &PgPool, tenant: Uuid, score_run: Uuid) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source) \
         VALUES ($1, $2, 'user-1', 'experiment_run')",
    )
    .bind(score_run)
    .bind(tenant.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_experiment_run(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    score_run: Uuid,
    name: &str,
    status: &str,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO moa.experiment_run \
             (run_uid, tenant_id, storage_partition_id, user_id, name, target_kind, status, \
              target, variant, score_run_id, session_id, created_by_identity, started_at, \
              completed_at) \
         VALUES ($1, $2, $3, 'user-1', $4, 'agent_loop', $5, '{}'::jsonb, '{}'::jsonb, $6, $7, \
              '{}'::jsonb, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(tenant.to_string())
    .bind(name)
    .bind(status)
    .bind(score_run)
    .bind(session)
    .bind(started_at)
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(())
}
