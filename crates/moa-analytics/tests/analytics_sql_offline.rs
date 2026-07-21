//! Offline pins for compiled analytics SQL across both backends.
//!
//! These snapshot the exact SQL the compiler emits for every catalog dataset in
//! the Postgres and ClickHouse dialects, and pin the tenant-scope injection that
//! isolates every dataset on both backends. They run fully offline: compilation
//! never touches a database.

use moa_analytics::{AnalyticsBackend, AnalyticsCompiler, analytics_catalog};
use moa_core::types::identifiers::TenantId;
use moa_wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDimension, AnalyticsFilter,
    AnalyticsFilterOperator, AnalyticsMeasure, AnalyticsOrderBy, AnalyticsQueryRequest,
    AnalyticsSortDirection,
};

/// The datasets served by both backends, in catalog order.
const DATASETS: &[&str] = &[
    "sessions",
    "turns",
    "tool_calls",
    "task_segments",
    "skills",
    "execution_runs",
    "execution_tasks",
    "learning_candidates",
    "experiment_runs",
    "events",
];

/// Datasets served by the Postgres backend only (no ClickHouse source).
const PG_ONLY_DATASETS: &[&str] = &["citation_precision", "skill_usage"];

fn dim(field: &str) -> AnalyticsDimension {
    AnalyticsDimension {
        field: field.to_string(),
        alias: None,
    }
}

fn count() -> AnalyticsMeasure {
    AnalyticsMeasure {
        field: None,
        aggregation: AnalyticsAggregation::Count,
        alias: None,
    }
}

fn measure(field: &str, aggregation: AnalyticsAggregation) -> AnalyticsMeasure {
    AnalyticsMeasure {
        field: Some(field.to_string()),
        aggregation,
        alias: None,
    }
}

fn eq(field: &str, value: &str) -> AnalyticsFilter {
    AnalyticsFilter {
        field: field.to_string(),
        operator: AnalyticsFilterOperator::Eq,
        value: Some(AnalyticsCell::String(value.to_string())),
    }
}

fn contains(field: &str, value: &str) -> AnalyticsFilter {
    AnalyticsFilter {
        field: field.to_string(),
        operator: AnalyticsFilterOperator::Contains,
        value: Some(AnalyticsCell::String(value.to_string())),
    }
}

fn is_in(field: &str, values: &[&str]) -> AnalyticsFilter {
    AnalyticsFilter {
        field: field.to_string(),
        operator: AnalyticsFilterOperator::In,
        value: Some(AnalyticsCell::Json(serde_json::json!(values))),
    }
}

fn between_window(field: &str) -> AnalyticsFilter {
    AnalyticsFilter {
        field: field.to_string(),
        operator: AnalyticsFilterOperator::Between,
        value: Some(AnalyticsCell::Json(serde_json::json!([
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z"
        ]))),
    }
}

fn order_count_desc() -> AnalyticsOrderBy {
    AnalyticsOrderBy {
        field: "count".to_string(),
        direction: AnalyticsSortDirection::Desc,
    }
}

/// Builds a representative query per dataset that exercises the dialect-specific
/// translations: UUID/string/boolean dimensions, timestamp `between` windows,
/// `eq`/`contains`/`in` filters, percentile/sum/avg measures, and `count(*)`.
fn request_for(dataset: &str) -> AnalyticsQueryRequest {
    let tenant = Some(TenantId::new());
    let (dimensions, measures, filters) = match dataset {
        "sessions" => (
            vec![dim("agent_id"), dim("channel")],
            vec![
                count(),
                measure("total_cost_cents", AnalyticsAggregation::P95),
                measure("tool_call_count", AnalyticsAggregation::Sum),
                measure("cache_hit_rate", AnalyticsAggregation::Avg),
                measure("duration_seconds", AnalyticsAggregation::P50),
            ],
            vec![eq("status", "active"), between_window("created_at")],
        ),
        "turns" => (
            vec![dim("model"), dim("turn_number")],
            vec![
                count(),
                measure("llm_ms", AnalyticsAggregation::P95),
                measure("pipeline_ms", AnalyticsAggregation::Avg),
                measure("llm_ttft_ms", AnalyticsAggregation::Avg),
                measure("total_input_tokens", AnalyticsAggregation::Sum),
            ],
            vec![eq("channel", "chat"), between_window("finished_at")],
        ),
        "tool_calls" => (
            vec![dim("tool_name"), dim("success")],
            vec![count(), measure("duration_ms", AnalyticsAggregation::P95)],
            vec![contains("tool_name", "search"), between_window("called_at")],
        ),
        "task_segments" => (
            vec![dim("outcome"), dim("agent_id")],
            vec![
                count(),
                measure("outcome_confidence", AnalyticsAggregation::Avg),
                measure("token_cost", AnalyticsAggregation::Sum),
                measure("duration_ms", AnalyticsAggregation::P95),
            ],
            vec![
                is_in("outcome", &["success", "failure"]),
                between_window("started_at"),
            ],
        ),
        "skills" => (
            vec![dim("skill_name"), dim("channel")],
            vec![
                count(),
                measure("token_cost", AnalyticsAggregation::Sum),
                measure("duration_ms", AnalyticsAggregation::P95),
            ],
            vec![between_window("started_at")],
        ),
        "skill_usage" => (
            vec![dim("skill_name"), dim("channel")],
            vec![
                count(),
                measure("token_cost", AnalyticsAggregation::Sum),
                measure("duration_ms", AnalyticsAggregation::P95),
            ],
            vec![between_window("started_at")],
        ),
        "execution_runs" => (
            vec![
                dim("active_plan_hash"),
                dim("skill_template_revision_uid"),
                dim("status"),
                dim("logical_task_count"),
            ],
            vec![
                count(),
                measure("actual_cost_microusd", AnalyticsAggregation::Sum),
                measure("queue_to_start_ms", AnalyticsAggregation::P95),
                measure("duration_ms", AnalyticsAggregation::P95),
            ],
            vec![between_window("started_at")],
        ),
        "execution_tasks" => (
            vec![
                dim("capability_name"),
                dim("capability_version"),
                dim("status"),
                dim("failure_class"),
            ],
            vec![
                count(),
                measure("actual_cost_microusd", AnalyticsAggregation::Sum),
                measure("citation_count", AnalyticsAggregation::Sum),
                measure("queue_latency_ms", AnalyticsAggregation::P95),
                measure("duration_ms", AnalyticsAggregation::P95),
            ],
            vec![between_window("started_at")],
        ),
        "learning_candidates" => (
            vec![dim("candidate_type"), dim("status"), dim("risk_class")],
            vec![count(), measure("confidence", AnalyticsAggregation::Avg)],
            vec![between_window("updated_at")],
        ),
        "experiment_runs" => (
            vec![dim("name"), dim("status"), dim("error_present")],
            vec![count(), measure("duration_ms", AnalyticsAggregation::P95)],
            vec![between_window("updated_at")],
        ),
        "events" => (
            vec![dim("event_type"), dim("session_id")],
            vec![count(), measure("token_count", AnalyticsAggregation::Sum)],
            vec![between_window("occurred_at")],
        ),
        "citation_precision" => (
            vec![dim("retrieved_day")],
            vec![
                count(),
                measure("cited_hit", AnalyticsAggregation::Sum),
                measure("cited_hit", AnalyticsAggregation::Avg),
                measure("cited_verified_hit", AnalyticsAggregation::Avg),
            ],
            vec![between_window("retrieved_at")],
        ),
        other => panic!("no request template for dataset {other}"),
    };

    AnalyticsQueryRequest {
        dataset: dataset.to_string(),
        tenant_id: tenant,
        dimensions,
        measures,
        filters,
        order_by: vec![order_count_desc()],
        limit: Some(50),
    }
}

fn compile(dataset: &str, backend: AnalyticsBackend) -> moa_analytics::CompiledAnalyticsQuery {
    AnalyticsCompiler::with_backend(analytics_catalog(), backend)
        .compile(request_for(dataset))
        .unwrap_or_else(|error| panic!("compile {dataset}/{}: {error}", backend.as_str()))
}

#[test]
fn postgres_sql_is_pinned_for_every_dataset_offline() {
    // Pins: the Postgres dialect emission for every dataset. This is the
    // byte-for-byte guard that the ClickHouse backend did not change today's
    // Postgres SQL.
    for dataset in DATASETS.iter().chain(PG_ONLY_DATASETS) {
        let compiled = compile(dataset, AnalyticsBackend::Postgres);
        insta::assert_snapshot!(format!("postgres_{dataset}"), compiled.sql);
    }
}

#[test]
fn citation_precision_matches_citations_to_retrieval_hit_keys_offline() {
    // Pins: the citation-precision relation joins injected retrieval hits
    // (rank <= 3, turn-linked) to the turn's durable Citation lineage rows
    // (record_kind 4) and matches a citation to a hit by every key mapping
    // `emit_context_lineage` can produce: `source_chunk_id` against the
    // knowledge chunk uid or the graph node uid, and `source_node_uid`
    // against the graph node uid.
    let sql = compile("citation_precision", AnalyticsBackend::Postgres).sql;

    assert!(sql.contains("FROM moa.retrieval_lineage AS rl"), "{sql}");
    assert!(
        sql.contains("rl.turn_id IS NOT NULL AND rl.rank <= 3"),
        "injected hits must be the turn-linked rendered evidence window: {sql}"
    );
    assert!(
        sql.contains("tl.turn_id = rl.turn_id AND tl.record_kind = 4"),
        "citations must come from the same turn's Citation lineage rows: {sql}"
    );
    assert!(
        sql.contains("citation.value ->> 'source_chunk_id' = rl.uid::TEXT")
            && sql.contains("citation.value ->> 'source_chunk_id' = rl.chunk_uid::TEXT")
            && sql.contains("citation.value ->> 'source_node_uid' = rl.uid::TEXT"),
        "a citation must match a hit by chunk uid or graph node uid: {sql}"
    );
    assert!(
        sql.contains("(citation.value -> 'verifier' ->> 'verified')::BOOLEAN"),
        "verified precision must read the cascade verifier verdict: {sql}"
    );
    // The generic surface computes injected_hits via count(), cited_hits via
    // sum(cited_hit), and citation_rate via avg(cited_hit).
    assert!(sql.contains("COUNT(*)::BIGINT"), "{sql}");
    assert!(sql.contains("SUM(d.cited_hit)"), "{sql}");
    assert!(sql.contains("AVG(d.cited_hit)"), "{sql}");
}

#[test]
fn clickhouse_backend_rejects_postgres_only_datasets_offline() {
    // Pins: datasets without a ClickHouse source (citation_precision joins
    // `moa.retrieval_lineage`, which is never exported to ClickHouse) fail
    // compilation with a clean backend-availability error instead of emitting
    // Postgres SQL at a ClickHouse server.
    for dataset in PG_ONLY_DATASETS {
        let error =
            AnalyticsCompiler::with_backend(analytics_catalog(), AnalyticsBackend::ClickHouse)
                .compile(request_for(dataset))
                .expect_err("postgres-only dataset must not compile for clickhouse");
        assert!(
            matches!(error, moa_analytics::Error::BackendFieldUnavailable { .. }),
            "expected BackendFieldUnavailable for {dataset}, got {error}"
        );
    }
}

#[test]
fn clickhouse_sql_is_pinned_for_every_dataset_offline() {
    // Pins: the ClickHouse dialect emission for every dataset, including the
    // dim/fact FROM shapes, FINAL usage, `?` binds, quantileExactInclusive,
    // countIf-derived session counts, and microsecond timestamp projection.
    for dataset in DATASETS {
        let compiled = compile(dataset, AnalyticsBackend::ClickHouse);
        insta::assert_snapshot!(format!("clickhouse_{dataset}"), compiled.sql);
    }
}

#[test]
fn tenant_scope_is_injected_first_on_every_dataset_and_backend_offline() {
    // Pins: both backends bind the tenant id as the first parameter and gate the
    // driving table on it, so no dataset can be queried cross-tenant.
    // Postgres-only datasets are pinned on the Postgres backend alone.
    let cases = DATASETS
        .iter()
        .flat_map(|dataset| {
            [
                (*dataset, AnalyticsBackend::Postgres),
                (*dataset, AnalyticsBackend::ClickHouse),
            ]
        })
        .chain(
            PG_ONLY_DATASETS
                .iter()
                .map(|dataset| (*dataset, AnalyticsBackend::Postgres)),
        );
    for (dataset, backend) in cases {
        let request = request_for(dataset);
        let tenant = request.tenant_id.expect("request carries a tenant");
        let compiled = AnalyticsCompiler::with_backend(analytics_catalog(), backend)
            .compile(request)
            .unwrap_or_else(|error| panic!("compile {dataset}: {error}"));

        // The tenant filter is on the driving table for most datasets, but a
        // ClickHouse source may inject it into its own subquery (events), so
        // match the predicate without the alias prefix.
        let expected_predicate = match backend {
            AnalyticsBackend::Postgres => "tenant_id = $1::UUID",
            AnalyticsBackend::ClickHouse => "tenant_id = toUUID(?)",
        };
        assert!(
            compiled.sql.contains(expected_predicate),
            "{dataset}/{} missing tenant predicate in: {}",
            backend.as_str(),
            compiled.sql
        );
        assert_eq!(
            compiled.bind_values.first(),
            Some(&moa_analytics::AnalyticsBindValue::String(
                tenant.to_string()
            )),
            "{dataset}/{} must bind the tenant id first",
            backend.as_str()
        );
    }
}

#[test]
fn clickhouse_events_raw_reads_are_duplicate_tolerant_offline() {
    // Pins the ReplacingMergeTree dedup rules for the un-FINAL events_raw stream:
    // session counts use uniqExactIf (never countIf), the events dataset counts
    // with uniqExact, and the events source dedups to one row per
    // (session_id, sequence_num) with LIMIT 1 BY behind the tenant filter.
    let sessions = compile("sessions", AnalyticsBackend::ClickHouse).sql;
    assert!(
        sessions.contains("uniqExactIf(event_id, event_type = 'ToolCall')")
            && sessions.contains("uniqExactIf(event_id, event_type = 'Error')"),
        "session counts must be duplicate-tolerant: {sessions}"
    );
    assert!(
        !sessions.contains("countIf("),
        "session counts must not use countIf over events_raw: {sessions}"
    );

    let events = compile("events", AnalyticsBackend::ClickHouse).sql;
    assert!(
        events.contains("LIMIT 1 BY (session_id, sequence_num)"),
        "events source must dedup the raw stream: {events}"
    );
    assert!(
        events.contains("WHERE tenant_id = toUUID(?)"),
        "events dedup must run behind the tenant filter: {events}"
    );
    assert!(
        events.contains("uniqExact(d.event_id)"),
        "events count must be duplicate-tolerant: {events}"
    );
    assert!(
        !events.contains(" count()"),
        "events dataset must not use raw count(): {events}"
    );
}

#[test]
fn execution_catalog_is_normalized_and_has_clickhouse_field_parity_offline() {
    // Pins: Task 11 exposes canonical normalized execution fields on both
    // backends and never restores Task 9 compatibility aliases or raw prose.
    let catalog = analytics_catalog();
    for dataset_id in ["execution_runs", "execution_tasks"] {
        let dataset = catalog
            .datasets
            .iter()
            .find(|dataset| dataset.id == dataset_id)
            .expect("execution dataset");
        let fields: std::collections::BTreeSet<_> = dataset
            .fields
            .iter()
            .map(|field| field.id.as_str())
            .collect();
        for forbidden in [
            "task_uid",
            "source_ref",
            "plan_hash",
            "capability_ref",
            "error",
            "error_present",
        ] {
            assert!(
                !fields.contains(forbidden),
                "{dataset_id} restored forbidden field {forbidden}"
            );
        }
    }

    let run_sql = compile("execution_runs", AnalyticsBackend::ClickHouse).sql;
    assert!(
        run_sql.contains("d.active_plan_hash")
            && run_sql.contains("toString(d.skill_template_revision_uid)")
            && run_sql.contains("d.logical_task_count")
            && run_sql.contains("d.duration_ms"),
        "{run_sql}"
    );
    let task_sql = compile("execution_tasks", AnalyticsBackend::ClickHouse).sql;
    assert!(
        task_sql.contains("d.capability_name")
            && task_sql.contains("d.capability_version")
            && task_sql.contains("d.failure_class")
            && task_sql.contains("d.duration_ms"),
        "{task_sql}"
    );
}
