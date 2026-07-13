//! DB-backed shape test for the `analytics.guardrail_hourly` materialized view.
//!
//! Applies the real V000334 migration over a minimal `events` table so the MV's
//! aggregation, columns, and CONCURRENTLY-refresh unique index are pinned to the
//! shipped DDL rather than a copy.

use moa_test_support::fixtures::quote_identifier;
use serde_json::json;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

/// The shipped guardrail MV migration, executed verbatim by the test.
const GUARDRAIL_MV_MIGRATION: &str = include_str!(
    "../../../moa-migrations/migrations/postgres/V000334__analytics_guardrail_hourly.sql"
);

#[tokio::test]
async fn guardrail_hourly_aggregates_guardrail_checks_db() {
    // Pins: the MV counts only GuardrailCheck events, buckets them by
    // tenant/partition/direction/passed/enforced, and refreshes concurrently.
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let partition = tenant.to_string();

    // Two failed input checks in the same hour collapse into one row (checks=2).
    insert_guardrail(&pool, tenant, &partition, "input", false, true).await;
    insert_guardrail(&pool, tenant, &partition, "input", false, true).await;
    // A passed output check is a distinct row.
    insert_guardrail(&pool, tenant, &partition, "output", true, true).await;
    // A non-guardrail event must be excluded from the rollup.
    insert_non_guardrail(&pool, tenant, &partition).await;

    sqlx::raw_sql("REFRESH MATERIALIZED VIEW CONCURRENTLY analytics.guardrail_hourly")
        .execute(&pool)
        .await
        .expect("concurrent refresh should succeed via the unique index");

    let injection_checks: i64 = sqlx::query_scalar(
        "SELECT checks FROM analytics.guardrail_hourly \
         WHERE tenant_id = $1 AND direction = 'input' AND passed = FALSE",
    )
    .bind(tenant)
    .fetch_one(&pool)
    .await
    .expect("failed input checks should aggregate into one row");
    assert_eq!(
        injection_checks, 2,
        "two failed input guardrail checks should count as the prompt-injection signal"
    );

    let total_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.guardrail_hourly WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&pool)
            .await
            .expect("guardrail rows should be readable");
    assert_eq!(
        total_rows, 2,
        "only the two guardrail outcome combinations produce rows; the non-guardrail event is excluded"
    );

    // The MV exposes exactly the documented columns. Materialized views are
    // absent from information_schema, so introspect via pg_attribute.
    let columns: Vec<String> = sqlx::query(
        "SELECT a.attname AS column_name \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'analytics' AND c.relname = 'guardrail_hourly' \
           AND c.relkind = 'm' AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attname",
    )
    .fetch_all(&pool)
    .await
    .expect("MV columns should be introspectable")
    .into_iter()
    .map(|row| row.get::<String, _>("column_name"))
    .collect();
    assert_eq!(
        columns,
        vec![
            "bucket".to_string(),
            "checks".to_string(),
            "direction".to_string(),
            "enforced".to_string(),
            "passed".to_string(),
            "storage_partition_id".to_string(),
            "tenant_id".to_string(),
        ],
    );
}

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("guardrail_hourly_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(&schema_name)
    ))
    .execute(&pool)
    .await
    .expect("test schema should be created");
    // Minimal `events` table carrying only the columns the MV reads.
    sqlx::raw_sql(
        r#"
        CREATE TABLE events (
            id UUID NOT NULL,
            session_id UUID NOT NULL,
            tenant_id UUID NOT NULL,
            storage_partition_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            payload JSONB NOT NULL,
            timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (id)
        );
        "#,
    )
    .execute(&pool)
    .await
    .expect("minimal events table should apply");
    sqlx::raw_sql(GUARDRAIL_MV_MIGRATION)
        .execute(&pool)
        .await
        .expect("guardrail MV migration should apply");
    pool
}

async fn insert_guardrail(
    pool: &PgPool,
    tenant: Uuid,
    partition: &str,
    direction: &str,
    passed: bool,
    enforced: bool,
) {
    let payload = json!({
        "type": "GuardrailCheck",
        "data": {
            "direction": direction,
            "mode": "enforce",
            "passed": passed,
            "enforced": enforced,
            "policy_hash": "test-hash"
        }
    });
    insert_event(pool, tenant, partition, "GuardrailCheck", payload).await;
}

async fn insert_non_guardrail(pool: &PgPool, tenant: Uuid, partition: &str) {
    let payload = json!({ "type": "Warning", "data": { "message": "noise" } });
    insert_event(pool, tenant, partition, "Warning", payload).await;
}

async fn insert_event(
    pool: &PgPool,
    tenant: Uuid,
    partition: &str,
    event_type: &str,
    payload: serde_json::Value,
) {
    sqlx::query(
        "INSERT INTO events (id, session_id, tenant_id, storage_partition_id, event_type, payload, timestamp) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(tenant)
    .bind(partition)
    .bind(event_type)
    .bind(payload)
    .execute(pool)
    .await
    .expect("event should insert");
}
