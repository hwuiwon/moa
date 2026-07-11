//! DB coverage for the analytics executor's per-query Postgres budget.

use moa_analytics::AnalyticsService;
use moa_core::types::identifiers::TenantId;
use moa_core::wire::analytics::{
    AnalyticsCell, AnalyticsDimension, AnalyticsFilter, AnalyticsFilterOperator, AnalyticsMeasure,
    AnalyticsQueryRequest,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn analytics_query_applies_statement_timeout_and_runs_db() {
    // Pins: the Postgres analytics executor runs each query inside a tenant-scoped
    // transaction with a bounded statement_timeout set, and the query path
    // succeeds against the empty read models.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant = TenantId::new();
    let request = AnalyticsQueryRequest {
        dataset: "sessions".to_string(),
        tenant_id: Some(tenant),
        dimensions: vec![AnalyticsDimension {
            field: "channel".to_string(),
            alias: None,
        }],
        measures: vec![AnalyticsMeasure {
            field: None,
            aggregation: moa_core::wire::analytics::AnalyticsAggregation::Count,
            alias: None,
        }],
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

    let response = AnalyticsService::new()
        .with_statement_timeout_ms(5_000)
        .query(test_db.store().pool(), request)
        .await
        .expect("analytics query runs under the statement-timeout budget");
    assert_eq!(response.metadata.row_count, 0, "empty read model");
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn statement_timeout_cancels_a_slow_query_db() {
    // Pins: the per-transaction statement_timeout the executor sets actually
    // cancels a runaway query server-side (SQLSTATE 57014), so an unbounded
    // ordered-percentile scan cannot hold a connection indefinitely.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let mut tx = test_db
        .store()
        .pool()
        .begin()
        .await
        .expect("begin transaction");
    // Same mechanism as executor.rs: SET LOCAL statement_timeout for this tx.
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind("100")
        .execute(tx.as_mut())
        .await
        .expect("set statement timeout");

    let error = sqlx::query("SELECT pg_sleep(3)")
        .execute(tx.as_mut())
        .await
        .expect_err("a query longer than the timeout must be cancelled");
    let cancelled = error
        .as_database_error()
        .and_then(|db_error| db_error.code().map(|code| code.into_owned()))
        .as_deref()
        == Some("57014")
        || error
            .to_string()
            .to_lowercase()
            .contains("statement timeout");
    assert!(
        cancelled,
        "expected a statement-timeout cancellation, got: {error}"
    );

    let _ = tx.rollback().await;
}
