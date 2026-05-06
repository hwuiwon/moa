//! pgaudit configuration and local Docker smoke coverage.

use std::{error::Error, process::Command, time::Duration};

use sqlx::PgPool;
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

#[test]
fn pgaudit_migration_marks_phi_tables_and_auditor_view() {
    for expected in [
        "CREATE EXTENSION IF NOT EXISTS pgaudit",
        "SECURITY LABEL FOR pgaudit ON TABLE moa.node_index",
        "SECURITY LABEL FOR pgaudit ON TABLE moa.embeddings",
        "SECURITY LABEL FOR pgaudit ON TABLE moa.graph_changelog",
        "CREATE OR REPLACE VIEW moa.audit_logs",
        "GRANT SELECT ON moa.audit_logs TO moa_auditor",
    ] {
        assert!(
            M22_PGAUDIT_SQL.contains(expected),
            "missing `{expected}` in M22 pgaudit migration"
        );
    }
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
