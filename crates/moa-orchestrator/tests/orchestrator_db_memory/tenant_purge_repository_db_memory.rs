use std::collections::BTreeMap;

use moa_authz::{FgaClient, FgaConfig};
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_memory_pii::legal_hold::{
    LegalHoldError, begin_destruction_stage_guard, complete_destruction, place_hold, release_hold,
    start_destruction,
};
use moa_orchestrator::workflows::tenant_purge::repository::{
    RelationalPurgeOutcome, purge_relational,
};
use moa_test_support::postgres::bootstrap_test_db;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

#[tokio::test]
async fn tenant_purge_removes_registered_families_in_dependency_order_and_leaves_exact_residue_db_memory()
 {
    // Pins: the production repository removes the new credential, OAuth,
    // privacy, security-event, and vector families in dependency order while
    // retaining only the explicitly redacted compliance tombstones and inverse
    // authorization intent.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap tenant-purge db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let subject_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let storage_partition_id = StoragePartitionId::for_tenant(TenantId::from(tenant_id));

    seed_tenant(pool, tenant_id).await;
    let hold = place_hold(
        pool,
        TenantId::from(tenant_id),
        Some(subject_id),
        "litigation reason that must not survive purge",
        "requesting-admin",
    )
    .await
    .expect("place released-hold fixture");
    assert!(
        release_hold(pool, TenantId::from(tenant_id), hold.id, "releasing-admin",)
            .await
            .expect("release legal-hold fixture")
    );
    let global_ids = seed_purge_families(pool, tenant_id, subject_id, &storage_partition_id).await;
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start tenant-wide destruction fence");

    let first = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect("registered tenant families should purge");
    let replay = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect("same purge operation should replay idempotently");
    assert_eq!(first, RelationalPurgeOutcome::Committed);
    assert_eq!(replay, RelationalPurgeOutcome::AlreadyCommitted);

    let residue = tenant_residue(pool, tenant_id, storage_partition_id.as_str()).await;
    assert_eq!(
        residue,
        BTreeMap::from([
            ("moa.destruction_operation_fence".to_string(), 1),
            ("moa.kek".to_string(), 1),
            ("moa.legal_hold".to_string(), 1),
            ("moa.tenant_purge_operations".to_string(), 1),
            ("public.authz_outbox".to_string(), 1),
        ])
    );

    let hold_tombstone: (Option<Uuid>, String, String, Option<String>, bool) = sqlx::query_as(
        r#"
        SELECT subject_id, reason, placed_by, released_by, released_at IS NOT NULL
        FROM moa.legal_hold
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load released-hold tombstone");
    assert_eq!(
        hold_tombstone,
        (
            None,
            "[REDACTED]".to_string(),
            "[REDACTED]".to_string(),
            Some("[REDACTED]".to_string()),
            true,
        ),
        "released hold residue must not retain the subject or administrative text"
    );
    let kek_tombstone: (bool, bool) = sqlx::query_as(
        "SELECT wrapped_kek IS NULL, destroyed_at IS NOT NULL FROM moa.kek WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load KEK tombstone");
    assert_eq!(kek_tombstone, (true, true));
    let inverse_tuple: (String, String) =
        sqlx::query_as("SELECT op, status FROM authz_outbox WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .expect("load inverse authorization intent");
    assert_eq!(inverse_tuple, ("delete".to_string(), "pending".to_string()));

    let global_rows: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM oauth_clients WHERE client_id = $1),
            (SELECT count(*) FROM moa.kms_root_key_generations WHERE generation = $2),
            (SELECT count(*) FROM moa.kms_root_key_state WHERE active_generation = $2)
        "#,
    )
    .bind(&global_ids.oauth_client_id)
    .bind(&global_ids.root_generation)
    .fetch_one(pool)
    .await
    .expect("count global control-plane rows");
    assert_eq!(global_rows, (1, 1, 1));
}

#[tokio::test]
async fn tenant_purge_rejects_an_unregistered_tenant_table_and_rolls_back_db_memory() {
    // Pins: adding a tenant-owned table without registering its purge behavior
    // fails closed before the relational transaction can leave a partial purge.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap catalog-check db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    seed_tenant(pool, tenant_id).await;
    sqlx::query(
        "CREATE TABLE moa.unregistered_tenant_payload (id UUID PRIMARY KEY, tenant_id UUID NOT NULL)",
    )
    .execute(pool)
    .await
    .expect("create unregistered tenant table fixture");
    sqlx::query("INSERT INTO moa.unregistered_tenant_payload (id, tenant_id) VALUES ($1, $2)")
        .bind(Uuid::new_v4())
        .bind(tenant_id)
        .execute(pool)
        .await
        .expect("seed unregistered tenant row");
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start catalog-check destruction fence");

    let error = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect_err("unregistered tenant table must reject purge");
    assert_eq!(
        error,
        "tenant purge catalog has unregistered tenant-owned tables: moa.unregistered_tenant_payload"
    );
    let rows_after_failure: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT count(*) FROM moa.unregistered_tenant_payload WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.tenant_purge_operations WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("inspect rollback state");
    assert_eq!(rows_after_failure, (1, 1, 0));
}

#[tokio::test]
async fn tenant_purge_rejects_nonpending_or_unrelated_inverse_tuple_residue_and_rolls_back_db_memory()
 {
    // Pins: only the exact pending inverse tuple identities created by this
    // purge may survive; unrelated pending, in-flight, succeeded, and
    // dead-letter delete intent all reject the transaction without partial data
    // removal.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap exact-residue db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    seed_tenant(pool, tenant_id).await;
    for status in ["pending", "in_flight", "succeeded", "dead_letter"] {
        sqlx::query(
            r#"
            INSERT INTO authz_outbox
                (op, tuple_user, tuple_relation, tuple_object, model_version,
                 tenant_id, status)
            VALUES ('delete', $1, 'unrelated', $2, 1, $3, $4)
            "#,
        )
        .bind(format!("operator:{}", Uuid::new_v4()))
        .bind(format!("tenant:{tenant_id}:{status}"))
        .bind(tenant_id)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed disallowed inverse tuple residue");
    }
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start exact-residue destruction fence");

    let error = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect_err("disallowed authz residue must reject purge");
    assert_eq!(
        error,
        "tenant purge left invalid intentional residue: kek=0, legal_hold=0, authz_outbox_invalid=4, authz_outbox_missing=0"
    );
    let rows_after_failure: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT count(*) FROM moa.tenant_purge_operations WHERE tenant_id = $1),
            (SELECT count(*) FROM authz_outbox WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("inspect exact-residue rollback state");
    assert_eq!(rows_after_failure, (1, 0, 4));
}

#[tokio::test]
async fn tenant_purge_relational_cleanup_waits_for_pre_fence_vector_claim_db_memory() {
    // Pins: the relational stage cannot remove its outbox/source rows while a
    // pre-fence vector worker still owns a live lease; once that lease expires,
    // the same durable purge operation resumes and commits.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap vector-claim residue db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let storage_partition_id = StoragePartitionId::for_tenant(TenantId::from(tenant_id));
    seed_tenant(pool, tenant_id).await;
    sqlx::query("INSERT INTO moa.storage_partition_state (storage_partition_id) VALUES ($1)")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .expect("seed vector partition state");
    sqlx::query(
        r#"
        INSERT INTO moa.vector_sync_outbox
            (storage_partition_id, uid, op, claim_token, claim_expires_at,
             processing_started_at)
        VALUES ($1, $2, 'upsert', $3, now() + INTERVAL '5 minutes', now())
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("seed live pre-fence vector claim");
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start vector-claim destruction fence");

    let error = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect_err("live pre-fence vector claim must delay relational cleanup");
    assert_eq!(
        error,
        "relational purge is waiting for active vector-sync claims to settle or expire"
    );
    let rollback_state: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT count(*) FROM moa.vector_sync_outbox WHERE storage_partition_id = $2),
            (SELECT count(*) FROM moa.tenant_purge_operations WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .bind(storage_partition_id.as_str())
    .fetch_one(pool)
    .await
    .expect("inspect live-claim rollback state");
    assert_eq!(rollback_state, (1, 1, 0));

    sqlx::query(
        "UPDATE moa.vector_sync_outbox SET claim_expires_at = now() - INTERVAL '1 second' WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id.as_str())
    .execute(pool)
    .await
    .expect("expire pre-fence vector claim");
    assert_eq!(
        purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
            .await
            .expect("expired vector claim should let relational cleanup resume"),
        RelationalPurgeOutcome::Committed
    );
}

#[tokio::test]
async fn legal_hold_and_destruction_interleavings_are_linearizable_across_pools_db_memory() {
    // Pins: independent Kubernetes replicas observe the same database-owned
    // order: a committed hold blocks destruction, a committed fence blocks a
    // later hold, and every resumed stage sees the durable fence.
    let test_db = bootstrap_test_db().await.expect("bootstrap fence-race db");
    let first_pool = test_db.store().pool();
    let second_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(test_db.database_url())
        .await
        .expect("connect independent replica pool");

    let hold_first_tenant = TenantId::new();
    let hold_first_subject = Uuid::new_v4();
    seed_tenant(first_pool, hold_first_tenant.0).await;
    place_hold(
        first_pool,
        hold_first_tenant,
        Some(hold_first_subject),
        "hold wins",
        "admin-a",
    )
    .await
    .expect("place hold from first pool");
    let hold_first_error = start_destruction(
        &second_pool,
        hold_first_tenant,
        &[hold_first_subject],
        "erase-hold-first",
        "privacy.erase",
    )
    .await
    .expect_err("committed hold must block destruction on another pool");
    assert!(matches!(hold_first_error, LegalHoldError::ActiveHold));

    let destruction_first_tenant = TenantId::new();
    let destruction_first_subject = Uuid::new_v4();
    seed_tenant(first_pool, destruction_first_tenant.0).await;
    start_destruction(
        first_pool,
        destruction_first_tenant,
        &[destruction_first_subject],
        "erase-destruction-first",
        "privacy.erase",
    )
    .await
    .expect("start destruction from first pool");
    let destruction_first_error = place_hold(
        &second_pool,
        destruction_first_tenant,
        Some(destruction_first_subject),
        "too late",
        "admin-b",
    )
    .await
    .expect_err("durable fence must reject a later hold on another pool");
    assert!(matches!(
        destruction_first_error,
        LegalHoldError::DestructionStarted
    ));

    let first_resume = begin_destruction_stage_guard(
        &second_pool,
        destruction_first_tenant,
        &[destruction_first_subject],
        "erase-destruction-first",
    )
    .await
    .expect("resume destructive stage from another pool");
    first_resume
        .finish()
        .await
        .expect("finish resumed stage guard");
    complete_destruction(
        first_pool,
        destruction_first_tenant,
        &[destruction_first_subject],
        "erase-destruction-first",
    )
    .await
    .expect("commit durable destruction fence");
    let committed_resume = begin_destruction_stage_guard(
        &second_pool,
        destruction_first_tenant,
        &[destruction_first_subject],
        "erase-destruction-first",
    )
    .await
    .expect("committed fence remains resumable and attributable");
    committed_resume
        .finish()
        .await
        .expect("finish committed-stage guard");

    let outcomes: (i64, i64, i64, i64, String) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM moa.legal_hold WHERE tenant_id = $1 AND released_at IS NULL),
            (SELECT count(*) FROM moa.destruction_operation_fence WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.legal_hold WHERE tenant_id = $2),
            (SELECT count(*) FROM moa.destruction_operation_fence WHERE tenant_id = $2),
            (SELECT status FROM moa.destruction_operation_fence WHERE tenant_id = $2)
        "#,
    )
    .bind(hold_first_tenant.0)
    .bind(destruction_first_tenant.0)
    .fetch_one(first_pool)
    .await
    .expect("load linearized outcomes");
    assert_eq!(outcomes, (1, 0, 0, 1, "committed".to_string()));

    second_pool.close().await;
}

#[derive(Debug)]
struct GlobalFixtureIds {
    oauth_client_id: String,
    root_generation: String,
}

async fn seed_tenant(pool: &PgPool, tenant_id: Uuid) {
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'purge test')")
        .bind(tenant_id)
        .bind(format!("purge-{tenant_id}"))
        .execute(pool)
        .await
        .expect("seed tenant");
}

async fn seed_purge_families(
    pool: &PgPool,
    tenant_id: Uuid,
    subject_id: Uuid,
    storage_partition_id: &StoragePartitionId,
) -> GlobalFixtureIds {
    let oauth_client_id = format!("purge-client-{tenant_id}");
    let root_generation = format!("purge-root-{tenant_id}");
    let authorization_id = Uuid::new_v4();
    let jti = format!("purge-jti-{tenant_id}");
    let config_hash = hex64('a');
    let csrf_hash = hex64('b');
    let code_hash = hex64('c');
    let access_hash = hex64('d');
    let refresh_hash = hex64('e');

    sqlx::query(
        r#"
        INSERT INTO oauth_clients
            (client_id, client_type, redirect_uris, scopes, config_hash)
        VALUES ($1, 'public', ARRAY['https://client.test/callback'], ARRAY['mcp:read'], $2)
        "#,
    )
    .bind(&oauth_client_id)
    .bind(&config_hash)
    .execute(pool)
    .await
    .expect("seed global OAuth client");
    sqlx::query(
        r#"
        INSERT INTO oauth_authorization_transactions
            (id, tenant_id, client_id, subject_id, subject_type, redirect_uri,
             scopes, resource, code_challenge, code_challenge_method, csrf_hash, expires_at)
        VALUES
            ($1, $2, $3, $4, 'operator', 'https://client.test/callback',
             ARRAY['mcp:read'], 'https://moa.test/mcp', 'challenge', 'S256', $5,
             NOW() + INTERVAL '5 minutes')
        "#,
    )
    .bind(authorization_id)
    .bind(tenant_id)
    .bind(&oauth_client_id)
    .bind(subject_id)
    .bind(&csrf_hash)
    .execute(pool)
    .await
    .expect("seed OAuth authorization transaction");
    sqlx::query(
        r#"
        INSERT INTO oauth_authorization_codes
            (code_hash, authorization_request_id, tenant_id, client_id, subject_id,
             subject_type, redirect_uri, scopes, resource, code_challenge,
             code_challenge_method, expires_at)
        VALUES
            ($1, $2, $3, $4, $5, 'operator', 'https://client.test/callback',
             ARRAY['mcp:read'], 'https://moa.test/mcp', 'challenge', 'S256',
             NOW() + INTERVAL '5 minutes')
        "#,
    )
    .bind(&code_hash)
    .bind(authorization_id)
    .bind(tenant_id)
    .bind(&oauth_client_id)
    .bind(subject_id)
    .execute(pool)
    .await
    .expect("seed OAuth authorization code");
    sqlx::query(
        r#"
        INSERT INTO oauth_tokens
            (tenant_id, client_id, subject_id, subject_type, scopes, resource,
             access_token_hash, access_token_expires_at, refresh_token_hash,
             refresh_token_expires_at)
        VALUES
            ($1, $2, $3, 'operator', ARRAY['mcp:read'], 'https://moa.test/mcp',
             $4, NOW() + INTERVAL '5 minutes', $5, NOW() + INTERVAL '1 hour')
        "#,
    )
    .bind(tenant_id)
    .bind(&oauth_client_id)
    .bind(subject_id)
    .bind(&access_hash)
    .bind(&refresh_hash)
    .execute(pool)
    .await
    .expect("seed OAuth token");
    sqlx::query(
        r#"
        INSERT INTO token_vault_connections
            (tenant_id, user_id, connection_name, provider, access_token_sealed)
        VALUES ($1, $2, 'calendar', 'example', $3)
        "#,
    )
    .bind(tenant_id)
    .bind(subject_id)
    .bind(vec![1_u8, 2, 3])
    .execute(pool)
    .await
    .expect("seed token-vault connection");
    sqlx::query(
        r#"
        INSERT INTO moa.dual_control_request
            (tenant_id, operation_type, operation_ref, requested_by)
        VALUES ($1, 'privacy.erase', $2, 'requester')
        "#,
    )
    .bind(tenant_id)
    .bind(format!("v1:blake3:{}", hex64('f')))
    .execute(pool)
    .await
    .expect("seed dual-control request");
    sqlx::query(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, op, subject_user_id, approver_id, approval_claims, tenant_id)
        VALUES ($1, 'erase', $2, 'approver', $3, $4)
        "#,
    )
    .bind(&jti)
    .bind(subject_id.to_string())
    .bind(serde_json::json!({ "tenant_id": tenant_id }))
    .bind(tenant_id)
    .execute(pool)
    .await
    .expect("seed approval JTI");
    sqlx::query(
        r#"
        INSERT INTO moa.erasure_jobs
            (jti, tenant_id, subject_user_id, request_fingerprint, approver_id,
             approval_claims)
        VALUES ($1, $2, $3, 'request-digest', 'approver', $4)
        "#,
    )
    .bind(&jti)
    .bind(tenant_id)
    .bind(subject_id.to_string())
    .bind(serde_json::json!({ "tenant_id": tenant_id }))
    .execute(pool)
    .await
    .expect("seed erasure job");
    sqlx::query(
        r#"
        INSERT INTO security_events
            (id, tenant_id, class_uid, activity_id, category_uid, severity_id,
             type_uid, event_jcs, signature_hex, signing_key_id, occurred_at,
             retrieval_operation_id)
        VALUES ($1, $2, 1, 1, 1, 1, 1, $3, 'signature', $4, NOW(), $5)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(vec![b'{', b'}'])
    .bind(Uuid::new_v4())
    .bind(format!("retrieval-{tenant_id}"))
    .execute(pool)
    .await
    .expect("seed security event");
    sqlx::query("INSERT INTO moa.storage_partition_state (storage_partition_id) VALUES ($1)")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .expect("seed storage partition state");
    sqlx::query(
        "INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op) VALUES ($1, $2, 'delete')",
    )
    .bind(storage_partition_id.as_str())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("seed vector-sync residue");
    sqlx::query(
        "INSERT INTO moa.kms_root_key_generations (generation, activated_at) VALUES ($1, NOW())",
    )
    .bind(&root_generation)
    .execute(pool)
    .await
    .expect("seed root-key generation");
    sqlx::query("INSERT INTO moa.kms_root_key_state (active_generation) VALUES ($1)")
        .bind(&root_generation)
        .execute(pool)
        .await
        .expect("seed global root-key state");
    sqlx::query(
        r#"
        INSERT INTO moa.kek
            (tenant_id, subject_id, wrapped_kek, root_key_generation)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(tenant_id)
    .bind(subject_id)
    .bind(vec![9_u8, 8, 7])
    .bind(&root_generation)
    .execute(pool)
    .await
    .expect("seed tenant KEK");

    GlobalFixtureIds {
        oauth_client_id,
        root_generation,
    }
}

async fn tenant_residue(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
) -> BTreeMap<String, i64> {
    let tables = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT namespace.nspname, table_row.relname
        FROM pg_catalog.pg_class AS table_row
        JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = table_row.relnamespace
        JOIN pg_catalog.pg_attribute AS column_row ON column_row.attrelid = table_row.oid
        WHERE table_row.relkind IN ('r', 'p')
          AND NOT table_row.relispartition
          AND namespace.nspname IN ('public', 'moa', 'analytics', 'pii_vault')
          AND column_row.attnum > 0
          AND NOT column_row.attisdropped
          AND column_row.attname IN ('tenant_id', 'storage_partition_id')
        GROUP BY namespace.nspname, table_row.relname
        ORDER BY namespace.nspname, table_row.relname
        "#,
    )
    .fetch_all(pool)
    .await
    .expect("load tenant-owned table catalog");
    let mut residue = BTreeMap::new();
    for (schema, table) in tables {
        let qualified = format!("{}.{}", quote_identifier(&schema), quote_identifier(&table));
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {qualified} AS tenant_row WHERE to_jsonb(tenant_row)->>'tenant_id' = $1 OR to_jsonb(tenant_row)->>'storage_partition_id' = $2"
        ))
        .bind(tenant_id.to_string())
        .bind(storage_partition_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("count tenant residue in {schema}.{table}: {error}"));
        if count != 0 {
            residue.insert(format!("{schema}.{table}"), count);
        }
    }
    residue
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn offline_fga() -> FgaClient {
    FgaClient::new(FgaConfig {
        url: "http://127.0.0.1:1".to_string(),
        preshared_key: "tenant-purge-test".to_string(),
        store_id: "tenant-purge-test".to_string(),
        model_id: "tenant-purge-test".to_string(),
        timeout_ms: 100,
    })
    .expect("offline FGA fixture config")
}

fn hex64(character: char) -> String {
    std::iter::repeat_n(character, 64).collect()
}
