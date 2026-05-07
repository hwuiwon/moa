//! Compatibility re-exports and shared helpers for session integration tests.

#![allow(dead_code)]

#[allow(unused_imports)]
pub use moa_test_support::postgres::{
    test_approval_rules, test_create_and_get_session, test_emit_and_get_events, test_event_search,
    test_list_sessions_with_filter, test_pending_signals, test_session_status_update,
    test_workspace_cost_since,
};

use moa_test_support::postgres::TestDb;
use sqlx::{PgPool, postgres::PgQueryResult};
use uuid::Uuid;

/// Returns whether Prompt 04 Postgres tests should connect to the configured database.
pub fn postgres_url_is_configured() -> bool {
    std::env::var_os("MOA_TEST_POSTGRES_URL").is_some()
}

/// Returns a double-quoted PostgreSQL identifier.
pub fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Returns a schema-qualified table name.
pub fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(table_name)
    )
}

/// Asserts that app-role `UPDATE` and `DELETE` attempts on `events` are blocked.
pub async fn assert_events_append_only_for_app_role(
    test_db: &TestDb,
    event_id: Uuid,
    workspace_id: &str,
    user_id: &str,
) {
    let events = qualified(test_db.schema_name(), "events");
    let update_error = execute_app_role_event_mutation(
        test_db,
        workspace_id,
        user_id,
        &format!("UPDATE {events} SET payload = jsonb_set(payload, '{{blocked}}', 'true'::jsonb) WHERE id = $1"),
        event_id,
    )
    .await
    .expect_err("moa_app UPDATE on events must be blocked");
    assert_events_append_only_error(&update_error);

    let delete_error = execute_app_role_event_mutation(
        test_db,
        workspace_id,
        user_id,
        &format!("DELETE FROM {events} WHERE id = $1"),
        event_id,
    )
    .await
    .expect_err("moa_app DELETE on events must be blocked");
    assert_events_append_only_error(&delete_error);
}

/// Executes a single event-row mutation in a transaction after assuming `moa_app`.
pub async fn execute_app_role_event_mutation(
    test_db: &TestDb,
    workspace_id: &str,
    user_id: &str,
    sql: &str,
    event_id: Uuid,
) -> Result<PgQueryResult, sqlx::Error> {
    let pool = PgPool::connect(test_db.database_url())
        .await
        .expect("connect owner pool for app-role mutation");
    grant_app_role_schema_usage(&pool, test_db.schema_name()).await;
    let mut tx = pool
        .begin()
        .await
        .expect("begin app-role mutation transaction");
    assume_app_role(&mut tx, test_db.schema_name(), workspace_id, user_id).await;

    let result = sqlx::query(sql).bind(event_id).execute(&mut *tx).await;
    let _ = tx.rollback().await;
    pool.close().await;
    result
}

/// Executes a no-bind statement in a transaction after assuming `moa_app`.
pub async fn execute_app_role_statement(
    test_db: &TestDb,
    workspace_id: &str,
    user_id: &str,
    sql: &str,
) -> Result<PgQueryResult, sqlx::Error> {
    let pool = PgPool::connect(test_db.database_url())
        .await
        .expect("connect owner pool for app-role statement");
    grant_app_role_schema_usage(&pool, test_db.schema_name()).await;
    let mut tx = pool
        .begin()
        .await
        .expect("begin app-role statement transaction");
    assume_app_role(&mut tx, test_db.schema_name(), workspace_id, user_id).await;

    let result = sqlx::query(sql).execute(&mut *tx).await;
    let _ = tx.rollback().await;
    pool.close().await;
    result
}

/// Asserts an app-role event mutation failed with the append-only SQLSTATE.
pub fn assert_events_append_only_error(error: &sqlx::Error) {
    let Some(database_error) = error.as_database_error() else {
        panic!("expected database error for append-only guard, got {error}");
    };
    let code = database_error.code().unwrap_or_default();
    assert!(
        matches!(code.as_ref(), "42501" | "P0001"),
        "expected append-only SQLSTATE 42501 or P0001, got {code}: {error}"
    );
}

async fn grant_app_role_schema_usage(pool: &PgPool, schema_name: &str) {
    sqlx::query(&format!(
        "GRANT USAGE ON SCHEMA {} TO moa_app",
        quote_identifier(schema_name)
    ))
    .execute(pool)
    .await
    .expect("grant app role usage on isolated test schema");
}

async fn assume_app_role(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schema_name: &str,
    workspace_id: &str,
    user_id: &str,
) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut **tx)
        .await
        .expect("set app role");
    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
        .bind(format!("{}, public", quote_identifier(schema_name)))
        .execute(&mut **tx)
        .await
        .expect("set search path");
    sqlx::query("SELECT pg_catalog.set_config('moa.workspace_id', $1, true)")
        .bind(workspace_id)
        .execute(&mut **tx)
        .await
        .expect("set workspace GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.user_id', $1, true)")
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .expect("set user GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.scope_tier', 'user', true)")
        .execute(&mut **tx)
        .await
        .expect("set scope tier GUC");
}
