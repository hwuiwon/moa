//! Offline coverage for the ClickHouse analytics executor against a mock server.
//!
//! The clickhouse `test-util` mock cannot serve a custom `JSONEachRow` body (the
//! raw-bytes handler is private and `provide` is RowBinary-only), so decoding of
//! populated rows is unit-tested in `clickhouse_exec`. This test pins the
//! executor's request/response plumbing: it builds the client, compiles and
//! binds ClickHouse SQL, fetches, and assembles a response — here against an
//! empty result — proving the wiring and the empty-result path end to end. It
//! also pins the exact exported-table set removed by tenant offboarding.

use clickhouse::Client;
use clickhouse::test::{Mock, handlers};
use moa_analytics::{AnalyticsClickHouseClient, AnalyticsService};
use moa_core::types::identifiers::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDimension, AnalyticsFilter,
    AnalyticsFilterOperator, AnalyticsMeasure, AnalyticsQueryRequest,
};
use uuid::Uuid;

#[tokio::test]
async fn clickhouse_executor_returns_empty_result_and_metadata_offline() {
    // Pins: a ClickHouse-backed service compiles + executes a query and reports
    // dataset, tenant, and a zero row count when the store returns no rows.
    let mock = Mock::new();
    mock.add(handlers::provide(std::iter::empty::<u8>()));

    let client = AnalyticsClickHouseClient::from_client(Client::default().with_url(mock.url()));
    let service = AnalyticsService::clickhouse();

    let tenant = TenantId::new();
    let request = AnalyticsQueryRequest {
        dataset: "sessions".to_string(),
        tenant_id: Some(tenant),
        dimensions: vec![AnalyticsDimension {
            field: "channel".to_string(),
            alias: None,
        }],
        measures: vec![AnalyticsMeasure {
            field: Some("total_cost_cents".to_string()),
            aggregation: AnalyticsAggregation::P95,
            alias: None,
        }],
        // Time-series datasets require a bounded window; a recent lower bound
        // satisfies the validator against the wall clock.
        filters: vec![AnalyticsFilter {
            field: "created_at".to_string(),
            operator: AnalyticsFilterOperator::Gte,
            value: Some(AnalyticsCell::String(
                (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
            )),
        }],
        order_by: Vec::new(),
        limit: Some(10),
    };

    let response = service
        .query_clickhouse(&client, request)
        .await
        .expect("clickhouse query should succeed against the mock");

    assert_eq!(response.rows.len(), 0, "mock returned no rows");
    assert_eq!(response.metadata.row_count, 0);
    assert_eq!(response.metadata.dataset, "sessions");
    assert_eq!(response.metadata.effective_tenant_id, Some(tenant));
    assert_eq!(response.columns.len(), 2, "channel dimension + p95 measure");
}

#[tokio::test]
async fn clickhouse_tenant_purge_targets_execution_dimensions_offline() {
    // Pins: tenant offboarding deletes every exported table, including the execution run/task
    // dimensions, through the real ClickHouse request path in its canonical order.
    let mock = Mock::new();
    let requests = (0..10)
        .map(|_| mock.add(handlers::record_ddl()))
        .collect::<Vec<_>>();
    let client = AnalyticsClickHouseClient::from_client(Client::default().with_url(mock.url()));
    let tenant_id = Uuid::now_v7();

    client
        .purge_tenant(tenant_id)
        .await
        .expect("tenant purge should delete every exported table");

    let expected_tables = [
        "events_raw",
        "dim_sessions",
        "dim_session_agent_context",
        "dim_task_segments",
        "dim_execution_runs",
        "dim_execution_tasks",
        "dim_learning_candidates",
        "dim_experiment_run",
        "turn_fact",
        "tool_call_fact",
    ];
    for (request, expected_table) in requests.into_iter().zip(expected_tables) {
        let query = request.query().await;
        assert_eq!(
            query,
            format!("DELETE FROM `{expected_table}` WHERE tenant_id = '{tenant_id}'"),
            "purge request should target {expected_table} for tenant {tenant_id}"
        );
    }
}
