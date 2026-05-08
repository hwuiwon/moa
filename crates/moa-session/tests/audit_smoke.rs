//! pgaudit configuration and local Docker smoke coverage.

use std::{error::Error, process::Command, time::Duration};

use sqlx::{PgPool, Row};
use uuid::Uuid;

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:25432/moa";
const M22_PGAUDIT_SQL: &str = include_str!("../migrations/postgres/019_pgaudit.sql");

fn pgaudit_smoke_requested() -> bool {
    matches!(
        std::env::var("MOA_RUN_PGAUDIT_SMOKE").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string())
}

#[tokio::test]
async fn pgaudit_migration_configures_labels_when_provider_loaded_and_auditor_view()
-> Result<(), Box<dyn Error>> {
    let pool = PgPool::connect(&test_database_url()).await?;
    let schema_name = format!("moa_pgaudit_test_{}", Uuid::now_v7().simple());
    let quoted_schema = quote_identifier(&schema_name);

    sqlx::query(&format!("CREATE SCHEMA {quoted_schema}"))
        .execute(&pool)
        .await?;
    for table in ["node_index", "embeddings", "graph_changelog", "label_probe"] {
        sqlx::query(&format!(
            "CREATE TABLE {quoted_schema}.{} (created_at timestamptz NOT NULL DEFAULT now())",
            quote_identifier(table)
        ))
        .execute(&pool)
        .await?;
    }

    sqlx::query("CREATE EXTENSION IF NOT EXISTS pgaudit")
        .execute(&pool)
        .await?;
    let pgaudit_provider_loaded = sqlx::query(&format!(
        "SECURITY LABEL FOR pgaudit ON TABLE {quoted_schema}.{} IS 'READ, WRITE'",
        quote_identifier("label_probe")
    ))
    .execute(&pool)
    .await
    .is_ok();

    let migration_sql = M22_PGAUDIT_SQL
        .replace("moa.", &format!("{quoted_schema}."))
        .replace("SCHEMA moa", &format!("SCHEMA {quoted_schema}"));
    sqlx::raw_sql(&migration_sql).execute(&pool).await?;

    let rows = sqlx::query(
        r#"
        SELECT c.relname, l.label
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        LEFT JOIN pg_seclabel l
          ON l.objoid = c.oid
         AND l.provider = 'pgaudit'
        WHERE n.nspname = $1
          AND c.relname IN ('node_index', 'embeddings', 'graph_changelog')
        ORDER BY c.relname
        "#,
    )
    .bind(&schema_name)
    .fetch_all(&pool)
    .await?;

    assert_eq!(rows.len(), 3, "expected all PHI tables to exist");
    for row in &rows {
        let relname = row.try_get::<String, _>("relname")?;
        let label = row.try_get::<Option<String>, _>("label")?;
        if pgaudit_provider_loaded {
            assert_eq!(
                label.as_deref(),
                Some("READ, WRITE"),
                "unexpected pgaudit label for {relname}"
            );
        } else {
            assert!(
                label.is_none(),
                "pgaudit label should not be present when the provider is not loaded for {relname}"
            );
        }
    }

    let audit_view_exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1
              AND c.relname = 'audit_logs'
              AND c.relkind = 'v'
        )
        "#,
    )
    .bind(&schema_name)
    .fetch_one(&pool)
    .await?;
    assert!(audit_view_exists, "expected moa.audit_logs view to exist");

    let audit_view_regclass = format!("{quoted_schema}.audit_logs");
    let auditor_can_select = sqlx::query_scalar::<_, bool>(
        "SELECT has_table_privilege('moa_auditor', $1::regclass, 'SELECT')",
    )
    .bind(audit_view_regclass)
    .fetch_one(&pool)
    .await?;
    assert!(
        auditor_can_select,
        "expected moa_auditor to have SELECT on moa.audit_logs"
    );

    sqlx::query(&format!("DROP SCHEMA {quoted_schema} CASCADE"))
        .execute(&pool)
        .await?;

    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[tokio::test]
#[ignore = "requires MOA_RUN_PGAUDIT_SMOKE=1 and docker compose postgres with pgaudit logs"]
async fn audit_writes_log_line() -> Result<(), Box<dyn Error>> {
    if !pgaudit_smoke_requested() {
        return Ok(());
    }

    let pool = PgPool::connect(&test_database_url()).await?;
    moa_session::schema::migrate(&pool, None).await?;
    let uid = Uuid::now_v7();
    let phi_like_placeholder = "audit smoke placeholder 123-45-6789";
    sqlx::query(
        "INSERT INTO moa.node_index \
         (uid, label, workspace_id, user_id, name, pii_class, properties_summary) \
         VALUES ($1, 'Fact', 'audit-smoke', NULL, $2, 'phi', $3)",
    )
    .bind(uid)
    .bind(phi_like_placeholder)
    .bind(serde_json::json!({ "source": phi_like_placeholder }))
    .execute(&pool)
    .await?;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let output = Command::new("docker")
        .args([
            "compose",
            "exec",
            "-T",
            "postgres",
            "sh",
            "-lc",
            "grep -R \"AUDIT:.*INSERT.*moa.node_index\" /var/log/postgresql || true",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "docker compose grep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(&pool)
        .await?;

    assert!(stdout.contains("AUDIT:"), "{stdout}");
    assert!(stdout.contains("INSERT"), "{stdout}");
    assert!(stdout.contains("moa.node_index"), "{stdout}");
    assert!(
        !stdout.contains(phi_like_placeholder),
        "pgaudit output unexpectedly contained PHI-like plaintext: {stdout}"
    );

    Ok(())
}
