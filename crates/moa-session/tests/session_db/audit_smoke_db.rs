//! `pgaudit` configuration and local Docker smoke coverage.

use std::{error::Error, process::Command, time::Duration};

use moa_session::testing::{cleanup_test_schema, provision_cloned_database};
use moa_test_support::postgres::test_database_url;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn pgaudit_smoke_requested() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the smoke run regardless of casing/spacing.
    std::env::var("MOA_RUN_PGAUDIT_SMOKE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn central_migrations_configure_pgaudit_labels_and_base_table_grants()
-> Result<(), Box<dyn Error>> {
    let (database_url, schema_name) = provision_cloned_database().await?;
    let pool = match PgPool::connect(&database_url).await {
        Ok(pool) => pool,
        Err(error) => {
            cleanup_test_schema(&database_url, &schema_name).await?;
            return Err(error.into());
        }
    };
    let outcome = async {
        sqlx::query(
            "CREATE TABLE public.pgaudit_label_probe \
             (created_at timestamptz NOT NULL DEFAULT now())",
        )
        .execute(&pool)
        .await?;
        let pgaudit_provider_loaded = sqlx::query(
            "SECURITY LABEL FOR pgaudit ON TABLE public.pgaudit_label_probe IS 'READ, WRITE'",
        )
        .execute(&pool)
        .await
        .is_ok();

        let rows = sqlx::query(
            r#"
            SELECT c.relname, l.label
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_seclabel l
              ON l.objoid = c.oid
             AND l.provider = 'pgaudit'
            WHERE n.nspname = 'moa'
              AND c.relname IN ('node_index', 'edge_index', 'embeddings', 'graph_changelog')
            ORDER BY c.relname
            "#,
        )
        .fetch_all(&pool)
        .await?;

        assert_eq!(rows.len(), 4, "expected all audited base tables to exist");
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
                    "pgaudit label should be absent when the provider is not loaded for {relname}"
                );
            }

            let auditor_can_select = sqlx::query_scalar::<_, bool>(
                "SELECT has_table_privilege('moa_auditor', $1::regclass, 'SELECT')",
            )
            .bind(format!("moa.{relname}"))
            .fetch_one(&pool)
            .await?;
            assert!(
                auditor_can_select,
                "moa_auditor must read the audited base table moa.{relname}"
            );
        }

        let audit_alias_exists: bool =
            sqlx::query_scalar("SELECT to_regclass('moa.audit_logs') IS NOT NULL")
                .fetch_one(&pool)
                .await?;
        assert!(
            !audit_alias_exists,
            "the redundant moa.audit_logs alias must not duplicate graph_changelog"
        );

        Ok::<(), Box<dyn Error>>(())
    }
    .await;

    pool.close().await;
    cleanup_test_schema(&database_url, &schema_name).await?;
    outcome
}

#[tokio::test]
#[ignore = "requires MOA_RUN_PGAUDIT_SMOKE=1 and docker compose postgres with pgaudit logs"]
async fn audit_writes_log_line() -> Result<(), Box<dyn Error>> {
    // Fail loudly rather than passing vacuously: this test is `#[ignore]`d and only
    // runs under `--run-ignored`, so reaching it without the smoke flag means the run
    // was mis-enabled (no pgaudit-configured compose Postgres) and must not be green.
    assert!(
        pgaudit_smoke_requested(),
        "audit_writes_log_line requires MOA_RUN_PGAUDIT_SMOKE=1 (or true/yes/on)"
    );

    let pool = PgPool::connect(&test_database_url()).await?;
    moa_migrations::run(&test_database_url()).await?;
    let uid = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let audit_placeholder = "audit smoke placeholder 123-45-6789";
    sqlx::query(
        "INSERT INTO moa.node_index \
         (uid, label, storage_partition_id, tenant_id, data_subject_id, user_id, name, pii_class, properties_summary) \
         VALUES ($1, 'Fact', $2, $3, $3, NULL, $4, 'none', $5)",
    )
    .bind(uid)
    .bind(tenant_id.to_string())
    .bind(tenant_id)
    .bind(audit_placeholder)
    .bind(serde_json::json!({ "source": audit_placeholder }))
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
        !stdout.contains(audit_placeholder),
        "pgaudit output unexpectedly contained PHI-like plaintext: {stdout}"
    );

    Ok(())
}
