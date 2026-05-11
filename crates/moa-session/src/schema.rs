//! Embedded `PostgreSQL` migrations for the session store.

use moa_core::{MoaError, Result};
use sqlx::{Acquire, PgConnection, PgPool, raw_sql};

struct SessionMigration {
    name: &'static str,
    sql: &'static str,
}

const SESSION_MIGRATIONS: &[SessionMigration] = &[
    SessionMigration {
        name: "000_scope_helpers.sql",
        sql: include_str!("../migrations/postgres/000_scope_helpers.sql"),
    },
    SessionMigration {
        name: "001_initial.sql",
        sql: include_str!("../migrations/postgres/001_initial.sql"),
    },
    SessionMigration {
        name: "002_add_session_cache_columns.sql",
        sql: include_str!("../migrations/postgres/002_add_session_cache_columns.sql"),
    },
    SessionMigration {
        name: "003_add_context_snapshots.sql",
        sql: include_str!("../migrations/postgres/003_add_context_snapshots.sql"),
    },
    SessionMigration {
        name: "004_session_generated_columns.sql",
        sql: include_str!("../migrations/postgres/004_session_generated_columns.sql"),
    },
    SessionMigration {
        name: "005_analytic_views.sql",
        sql: include_str!("../migrations/postgres/005_analytic_views.sql"),
    },
    SessionMigration {
        name: "006_daily_workspace_metrics.sql",
        sql: include_str!("../migrations/postgres/006_daily_workspace_metrics.sql"),
    },
    SessionMigration {
        name: "007_model_tier_analytics.sql",
        sql: include_str!("../migrations/postgres/007_model_tier_analytics.sql"),
    },
    SessionMigration {
        name: "008_task_segments.sql",
        sql: include_str!("../migrations/postgres/008_task_segments.sql"),
    },
    SessionMigration {
        name: "009_resolution_views.sql",
        sql: include_str!("../migrations/postgres/009_resolution_views.sql"),
    },
    SessionMigration {
        name: "010_intents_learning_log.sql",
        sql: include_str!("../migrations/postgres/010_intents_learning_log.sql"),
    },
    SessionMigration {
        name: "011_three_tier_rls.sql",
        sql: include_str!("../migrations/postgres/011_three_tier_rls.sql"),
    },
    SessionMigration {
        name: "012_age_bootstrap.sql",
        sql: include_str!("../migrations/postgres/012_age_bootstrap.sql"),
    },
    SessionMigration {
        name: "013_node_index.sql",
        sql: include_str!("../migrations/postgres/013_node_index.sql"),
    },
    SessionMigration {
        name: "014_embeddings.sql",
        sql: include_str!("../migrations/postgres/014_embeddings.sql"),
    },
    SessionMigration {
        name: "015_graph_changelog.sql",
        sql: include_str!("../migrations/postgres/015_graph_changelog.sql"),
    },
    SessionMigration {
        name: "016_ingest.sql",
        sql: include_str!("../migrations/postgres/016_ingest.sql"),
    },
    SessionMigration {
        name: "017_skills.sql",
        sql: include_str!("../migrations/postgres/017_skills.sql"),
    },
    SessionMigration {
        name: "018_skill_addendum.sql",
        sql: include_str!("../migrations/postgres/018_skill_addendum.sql"),
    },
    SessionMigration {
        name: "019_pgaudit.sql",
        sql: include_str!("../migrations/postgres/019_pgaudit.sql"),
    },
    SessionMigration {
        name: "020_privacy_export.sql",
        sql: include_str!("../migrations/postgres/020_privacy_export.sql"),
    },
    SessionMigration {
        name: "021_privacy_erase.sql",
        sql: include_str!("../migrations/postgres/021_privacy_erase.sql"),
    },
    SessionMigration {
        name: "022_vector_backend_turbopuffer.sql",
        sql: include_str!("../migrations/postgres/022_vector_backend_turbopuffer.sql"),
    },
    SessionMigration {
        name: "023_workspace_vector_promotion.sql",
        sql: include_str!("../migrations/postgres/023_workspace_vector_promotion.sql"),
    },
    SessionMigration {
        name: "024_lineage.sql",
        sql: include_str!("../migrations/postgres/024_lineage.sql"),
    },
    SessionMigration {
        name: "025_lineage_scores.sql",
        sql: include_str!("../migrations/postgres/025_lineage_scores.sql"),
    },
    SessionMigration {
        name: "026_lineage_audit.sql",
        sql: include_str!("../migrations/postgres/026_lineage_audit.sql"),
    },
    SessionMigration {
        name: "027_events_append_only.sql",
        sql: include_str!("../migrations/postgres/027_events_append_only.sql"),
    },
];

pub(crate) const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

/// Runs all embedded `PostgreSQL` migrations idempotently on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => {
            // SQLx uses one `_sqlx_migrations` table per database. MOA keeps
            // migration files next to their owning crate, so this migrator must
            // ignore versions owned by other per-crate migrators such as
            // `moa-authz`.
            let mut migrator = sqlx::migrate!("./migrations/postgres");
            migrator.set_ignore_missing(true);
            migrator.run(pool).await.map_err(|error| {
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

    // Session advisory locks only protect work done by the lock-holding backend.
    let result = migrate_in_schema_locked(&mut lock_conn, schema_name).await;
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

async fn migrate_in_schema_locked(conn: &mut PgConnection, schema_name: &str) -> Result<()> {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    raw_sql(
        "CREATE EXTENSION IF NOT EXISTS age; LOAD 'age'; CREATE EXTENSION IF NOT EXISTS vector;",
    )
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    let mut tx = conn.begin().await.map_err(map_sqlx_error)?;
    let search_path = format!("{}, public", quote_identifier(schema_name));
    for migration in SESSION_MIGRATIONS {
        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        raw_sql(migration.sql)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "postgres schema migration `{}` failed for `{schema_name}`: {error}",
                    migration.name
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
