//! Real-process coverage for the sealed-content backfill CLI.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_crypto::{Ciphertext, EncryptionContext, encrypt};
use moa_kms::{PostgresKmsProvider, RootKeyRing};
use moa_session::testing;
use serde_json::json;
use sqlx::Row;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

const NODE_CONSTRAINTS: &[&str] = &[
    "node_index_data_subject_required",
    "node_index_data_subject_scope",
    "node_index_sealed_content_state",
];

async fn seed_historical_restricted_node(pool: &sqlx::PgPool, tenant_id: Uuid, uid: Uuid) {
    let mut tx = pool.begin().await.expect("begin CLI historical seed");
    for constraint in NODE_CONSTRAINTS {
        sqlx::query(&format!(
            "ALTER TABLE moa.node_index DROP CONSTRAINT {constraint}"
        ))
        .execute(tx.as_mut())
        .await
        .expect("drop deferred node constraint for CLI fixture");
    }
    sqlx::query("ALTER TABLE moa.embeddings DROP CONSTRAINT embeddings_unsealed_content_only")
        .execute(tx.as_mut())
        .await
        .expect("drop deferred embedding constraint for CLI fixture");
    sqlx::query("ALTER TABLE moa.embeddings DISABLE TRIGGER embeddings_reject_sealed_node")
        .execute(tx.as_mut())
        .await
        .expect("disable sealed embedding trigger for CLI fixture");

    sqlx::query(
        r#"
        INSERT INTO moa.node_index (
            uid, label, storage_partition_id, name, pii_class, confidence,
            properties_summary, content_sealed, data_subject_id
        ) VALUES (
            $1, 'Fact', $2, 'CLI historical secret', 'restricted', 0.9,
            $3, NULL, NULL
        )
        "#,
    )
    .bind(uid)
    .bind(tenant_id.to_string())
    .bind(json!({
        "summary": "CLI historical secret",
        "base_confidence": 0.88,
        "secret": "cli-only-secret",
    }))
    .execute(tx.as_mut())
    .await
    .expect("insert CLI historical node");

    let vector = format!("[{}]", vec!["0"; 1024].join(","));
    sqlx::query(
        r#"
        INSERT INTO moa.embeddings (
            uid, storage_partition_id, label, pii_class, embedding,
            embedding_model, embedding_model_version
        ) VALUES ($1, $2, 'Fact', 'restricted', $3::public.halfvec, 'historical', 1)
        "#,
    )
    .bind(uid)
    .bind(tenant_id.to_string())
    .bind(vector)
    .execute(tx.as_mut())
    .await
    .expect("insert CLI historical embedding");

    for constraint in NODE_CONSTRAINTS {
        let definition = match *constraint {
            "node_index_data_subject_required" => "CHECK (data_subject_id IS NOT NULL)",
            "node_index_data_subject_scope" => {
                "CHECK (data_subject_id = CASE WHEN contact_id IS NOT NULL THEN contact_id ELSE tenant_id END)"
            }
            "node_index_sealed_content_state" => {
                "CHECK (((pii_class IN ('phi', 'restricted') AND data_subject_id IS NOT NULL AND name = '[RESTRICTED]' AND properties_summary = '{\"redacted\": true}'::jsonb AND content_sealed IS NOT NULL AND octet_length(content_sealed) > 0) OR (pii_class NOT IN ('phi', 'restricted') AND content_sealed IS NULL)))"
            }
            _ => unreachable!("known CLI node constraint"),
        };
        sqlx::query(&format!(
            "ALTER TABLE moa.node_index ADD CONSTRAINT {constraint} {definition} NOT VALID"
        ))
        .execute(tx.as_mut())
        .await
        .expect("restore deferred node constraint for CLI fixture");
    }
    sqlx::query(
        "ALTER TABLE moa.embeddings ADD CONSTRAINT embeddings_unsealed_content_only CHECK (pii_class NOT IN ('phi', 'restricted')) NOT VALID",
    )
    .execute(tx.as_mut())
    .await
    .expect("restore deferred embedding constraint for CLI fixture");
    sqlx::query("ALTER TABLE moa.embeddings ENABLE TRIGGER embeddings_reject_sealed_node")
        .execute(tx.as_mut())
        .await
        .expect("restore sealed embedding trigger for CLI fixture");
    tx.commit().await.expect("commit CLI historical seed");
}

#[tokio::test]
async fn backfill_cli_runs_narrow_database_and_kms_path_without_full_runtime_db() {
    // Pins: the real binary's backfill subcommand connects only to Postgres and
    // the mounted durable KMS keyring; unreachable Restate endpoints and a
    // deliberately missing scripted LLM provider fixture are never consulted.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated CLI backfill database");
    let tenant_id = Uuid::now_v7();
    let uid = Uuid::from_u128(50_001);
    seed_historical_restricted_node(store.pool(), tenant_id, uid).await;

    let key_dir = tempfile::tempdir().expect("create mounted root-key fixture");
    let encoded_root_key = BASE64.encode([0x5a_u8; 32]);
    std::fs::write(key_dir.path().join("primary"), &encoded_root_key)
        .expect("write primary root-key generation");

    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
        .arg("backfill-memory-sealed-content")
        .arg("--batch-size")
        .arg("1")
        .env("MOA_DATABASE_URL", &database_url)
        .env("MOA_DATABASE_ADMIN_URL", &database_url)
        .env("MOA_DATABASE_SCHEMA", &schema_name)
        .env("MOA_DATABASE_MAX_CONNECTIONS", "2")
        .env("MOA_KMS_PROVIDER", "postgres")
        .env("MOA_KMS_ROOT_KEY_DIR", key_dir.path())
        .env("MOA_KMS_REQUIRED_GENERATION", "primary")
        .env("MOA_RESTATE_ADMIN_URL", "http://127.0.0.1:9")
        .env("MOA_RESTATE_INGRESS_URL", "http://127.0.0.1:9")
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            "scripted:/definitely/not-read-by-backfill-cli.json",
        )
        .env("MOA_ENVIRONMENT", "dev")
        .env_remove("MOA_KMS_ALLOW_EPHEMERAL");

    let output = timeout(Duration::from_secs(15), command.output())
        .await
        .expect("backfill CLI must not wait for Restate, OpenFGA, or providers")
        .expect("execute real backfill CLI process");
    assert!(
        output.status.success(),
        "backfill CLI failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let row = sqlx::query(
        "SELECT data_subject_id, name, properties_summary, content_sealed, base_confidence FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(store.pool())
    .await
    .expect("read CLI-backfilled node");
    assert_eq!(
        row.try_get::<Uuid, _>("data_subject_id")
            .expect("decode CLI data subject"),
        tenant_id
    );
    assert_eq!(
        row.try_get::<String, _>("name")
            .expect("decode CLI placeholder"),
        "[RESTRICTED]"
    );
    assert_eq!(
        row.try_get::<serde_json::Value, _>("properties_summary")
            .expect("decode CLI redacted properties"),
        json!({ "redacted": true })
    );
    let sealed: Vec<u8> = row
        .try_get("content_sealed")
        .expect("decode CLI sealed content");
    assert!(!sealed.is_empty());
    assert_eq!(
        row.try_get::<Option<f64>, _>("base_confidence")
            .expect("decode CLI confidence sidecar"),
        Some(0.88)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE uid = $1")
            .bind(uid)
            .fetch_one(store.pool())
            .await
            .expect("count CLI-deleted pgvector row"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.vector_sync_outbox WHERE uid = $1 AND op = 'delete'",
        )
        .bind(uid)
        .fetch_one(store.pool())
        .await
        .expect("count CLI external-vector delete"),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.kek WHERE tenant_id = $1 AND subject_id = $2 AND destroyed_at IS NULL",
        )
        .bind(tenant_id)
        .bind(tenant_id)
        .fetch_one(store.pool())
        .await
        .expect("count durable CLI KEK"),
        1
    );
    let restarted_kms = PostgresKmsProvider::new(
        store.pool().clone(),
        RootKeyRing::from_directory_entries(
            key_dir.path().to_path_buf(),
            "primary",
            [("primary", encoded_root_key)],
        )
        .expect("rebuild mounted root-key ring after CLI exit"),
    );
    restarted_kms
        .check_compatibility()
        .await
        .expect("fresh KMS process view is compatible");
    let plaintext = moa_crypto::decrypt(
        &restarted_kms,
        &Ciphertext::from_bytes(&sealed).expect("parse CLI ciphertext"),
        &EncryptionContext::new(tenant_id, tenant_id, uid.to_string(), "restricted"),
    )
    .await
    .expect("fresh durable provider decrypts CLI ciphertext");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&plaintext).expect("parse CLI sealed document"),
        json!({
            "version": 1,
            "name": "CLI historical secret",
            "properties": {
                "summary": "CLI historical secret",
                "secret": "cli-only-secret",
            }
        })
    );
    let validated: Vec<bool> = sqlx::query_scalar(
        r#"
        SELECT constraint_row.convalidated
          FROM pg_catalog.pg_constraint AS constraint_row
          JOIN pg_catalog.pg_class AS table_row ON table_row.oid = constraint_row.conrelid
          JOIN pg_catalog.pg_namespace AS schema_row ON schema_row.oid = table_row.relnamespace
         WHERE schema_row.nspname = 'moa'
           AND table_row.relname IN ('node_index', 'embeddings')
           AND constraint_row.conname = ANY($1)
         ORDER BY constraint_row.conname
        "#,
    )
    .bind([
        "embeddings_unsealed_content_only",
        "node_index_data_subject_required",
        "node_index_data_subject_scope",
        "node_index_sealed_content_state",
    ])
    .fetch_all(store.pool())
    .await
    .expect("read CLI validated constraints");
    assert_eq!(validated, vec![true, true, true, true]);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated CLI backfill database");
}

#[tokio::test]
async fn kms_rewrap_cli_activates_drains_and_then_retires_generation_db() {
    // Pins: the real maintenance command tolerates an old active generation,
    // atomically activates MOA_KMS_REQUIRED_GENERATION, drains bounded rewrap
    // batches, and only then performs an explicitly requested retirement.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated CLI KMS database");
    let key_dir = tempfile::tempdir().expect("create mounted root-key fixture");
    let old_key = BASE64.encode([0x31_u8; 32]);
    let new_key = BASE64.encode([0x72_u8; 32]);
    std::fs::write(key_dir.path().join("old"), &old_key).expect("write old root key");
    std::fs::write(key_dir.path().join("new"), &new_key).expect("write new root key");

    let old_provider = PostgresKmsProvider::new(
        store.pool().clone(),
        RootKeyRing::from_directory_entries(
            key_dir.path().to_path_buf(),
            "old",
            [("old", &old_key), ("new", &new_key)],
        )
        .expect("build old-active root-key ring"),
    );
    old_provider
        .check_compatibility()
        .await
        .expect("initialize old active generation");
    let tenant_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    let context =
        EncryptionContext::new(tenant_id, subject_id, "kms-rewrap-cli-record", "restricted");
    let ciphertext = encrypt(&old_provider, b"rewrap survives restart", &context)
        .await
        .expect("seed old-generation KEK");

    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
        .arg("kms-rewrap")
        .arg("--batch-size")
        .arg("1")
        .arg("--retire-generation")
        .arg("old")
        .env("MOA_DATABASE_URL", &database_url)
        .env("MOA_DATABASE_ADMIN_URL", &database_url)
        .env("MOA_DATABASE_SCHEMA", &schema_name)
        .env("MOA_DATABASE_MAX_CONNECTIONS", "2")
        .env("MOA_KMS_PROVIDER", "postgres")
        .env("MOA_KMS_ROOT_KEY_DIR", key_dir.path())
        .env("MOA_KMS_REQUIRED_GENERATION", "new")
        .env("MOA_ENVIRONMENT", "dev")
        .env_remove("MOA_KMS_ALLOW_EPHEMERAL");

    let output = timeout(Duration::from_secs(15), command.output())
        .await
        .expect("KMS rewrap CLI must finish without starting runtime services")
        .expect("execute real KMS rewrap CLI process");
    assert!(
        output.status.success(),
        "KMS rewrap CLI failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT active_generation FROM moa.kms_root_key_state WHERE singleton = TRUE",
        )
        .fetch_one(store.pool())
        .await
        .expect("read active KMS generation"),
        "new"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.kek WHERE destroyed_at IS NULL AND root_key_generation = 'new'",
        )
        .fetch_one(store.pool())
        .await
        .expect("count rewrapped KEKs"),
        1
    );
    assert!(
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT retired_at FROM moa.kms_root_key_generations WHERE generation = 'old'",
        )
        .fetch_one(store.pool())
        .await
        .expect("read old generation retirement")
        .is_some()
    );

    let restarted = PostgresKmsProvider::new(
        store.pool().clone(),
        RootKeyRing::from_directory_entries(
            key_dir.path().to_path_buf(),
            "new",
            [("old", old_key), ("new", new_key)],
        )
        .expect("build new-active root-key ring"),
    );
    restarted
        .check_compatibility()
        .await
        .expect("new serving process is compatible");
    assert_eq!(
        moa_crypto::decrypt(&restarted, &ciphertext, &context)
            .await
            .expect("decrypt after rewrap and restart"),
        b"rewrap survives restart"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated CLI KMS database");
}
