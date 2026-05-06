//! Embedded `PostgreSQL` migrations for the session store.

use moa_core::{MoaError, Result};
use sqlx::{PgPool, raw_sql};

const SESSION_MIGRATIONS: &[&str] = &[
    include_str!("../migrations/postgres/000_scope_helpers.sql"),
    include_str!("../migrations/postgres/001_initial.sql"),
    include_str!("../migrations/postgres/002_add_session_cache_columns.sql"),
    include_str!("../migrations/postgres/003_add_context_snapshots.sql"),
    include_str!("../migrations/postgres/004_session_generated_columns.sql"),
    include_str!("../migrations/postgres/005_analytic_views.sql"),
    include_str!("../migrations/postgres/006_daily_workspace_metrics.sql"),
    include_str!("../migrations/postgres/007_model_tier_analytics.sql"),
    include_str!("../migrations/postgres/008_task_segments.sql"),
    include_str!("../migrations/postgres/009_resolution_views.sql"),
    include_str!("../migrations/postgres/010_intents_learning_log.sql"),
    include_str!("../migrations/postgres/011_three_tier_rls.sql"),
    include_str!("../migrations/postgres/012_age_bootstrap.sql"),
    include_str!("../migrations/postgres/013_node_index.sql"),
    include_str!("../migrations/postgres/014_embeddings.sql"),
    include_str!("../migrations/postgres/015_graph_changelog.sql"),
    include_str!("../migrations/postgres/016_ingest.sql"),
    include_str!("../migrations/postgres/017_skills.sql"),
    include_str!("../migrations/postgres/018_skill_addendum.sql"),
    include_str!("../migrations/postgres/019_pgaudit.sql"),
    include_str!("../migrations/postgres/020_privacy_export.sql"),
    include_str!("../migrations/postgres/021_privacy_erase.sql"),
    include_str!("../migrations/postgres/022_vector_backend_turbopuffer.sql"),
    include_str!("../migrations/postgres/023_workspace_vector_promotion.sql"),
    include_str!("../migrations/postgres/024_lineage.sql"),
    include_str!("../migrations/postgres/025_lineage_scores.sql"),
];

pub(crate) const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

/// Runs all embedded `PostgreSQL` migrations idempotently on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => {
            sqlx::migrate!("./migrations/postgres")
                .run(pool)
                .await
                .map_err(|error| {
                    MoaError::StorageError(format!("postgres migration failed: {error}"))
                })?;
            Ok(())
        }
    }
}

async fn migrate_in_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    let mut lock_conn = pool.acquire().await.map_err(map_sqlx_error)?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *lock_conn)
        .await
        .map_err(map_sqlx_error)?;

    let result = migrate_in_schema_locked(pool, schema_name).await;
    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *lock_conn)
        .await
        .map_err(map_sqlx_error);

    match (result, unlock_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn migrate_in_schema_locked(pool: &PgPool, schema_name: &str) -> Result<()> {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(pool)
    .await
    .map_err(map_sqlx_error)?;

    raw_sql(
        "CREATE EXTENSION IF NOT EXISTS age; LOAD 'age'; CREATE EXTENSION IF NOT EXISTS vector;",
    )
    .execute(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    let search_path = format!("{}, public", quote_identifier(schema_name));
    for migration in SESSION_MIGRATIONS {
        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        raw_sql(migration)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "postgres schema migration failed for `{schema_name}`: {error}"
                ))
            })?;
    }

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
