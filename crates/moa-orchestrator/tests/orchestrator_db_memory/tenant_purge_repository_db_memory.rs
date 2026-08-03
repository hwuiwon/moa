use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use moa_artifacts::release::PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID;
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

type AuthzOutboxIdentityState = (
    String,
    String,
    String,
    String,
    i32,
    Option<Uuid>,
    String,
    i64,
    i32,
    Option<String>,
    bool,
);
type AuthzOutboxDeliveryState = (Uuid, String, String, i64, i32, Option<String>, bool, bool);

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
    // Transcripts for BOTH tenants: the purged tenant's proves the append-only
    // guard is actually crossed, the neighbour's proves the maintenance escape
    // hatch the purge opens is closed again when its transaction ends.
    let purged_session_id = seed_session_transcript(&test_db, tenant_id).await;
    seed_session_transcript(&test_db, NEIGHBOUR_TENANT).await;
    // Tenant purge inverts only durable actual desired tuples. Seed both
    // identities explicitly so this test cannot pass by synthesizing a guessed
    // users-by-sessions Cartesian product.
    sqlx::query(
        r#"
        INSERT INTO authz_outbox
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
        VALUES
            ('write', $1, 'workspace', $2, 1, $3),
            ('write', $4, 'tenant', $5, 1, $3)
        "#,
    )
    .bind(format!("workspace:{}", moa_core::WORKSPACE_ID))
    .bind(format!("tenant:{tenant_id}"))
    .bind(tenant_id)
    .bind(format!("tenant:{tenant_id}"))
    .bind(format!("session:{purged_session_id}"))
    .execute(pool)
    .await
    .expect("seed actual desired tenant authorization tuples");
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start tenant-wide destruction fence");

    // The purge runs through a BOUNDED pool so the maintenance-hatch assertion
    // below can inspect every connection the purge could have used. That is the
    // real risk: `set_config` opens the append-only hatch, and a connection that
    // carried it back into the pool would silently disable the guard for whatever
    // ran next on it. Asserting through the large shared pool would almost always
    // pick a connection the purge never touched and prove nothing.
    //
    // Two connections also exercise progress hand-off across pooled connections;
    // every purge batch itself is one short autocommit transaction.
    const PURGE_POOL_CONNECTIONS: u32 = 2;
    let purge_pool = PgPoolOptions::new()
        .max_connections(PURGE_POOL_CONNECTIONS)
        .connect(test_db.database_url())
        .await
        .expect("open a bounded pool for the purge");
    let first = purge_relational(&purge_pool, tenant_id, &operation_id)
        .await
        .expect("registered tenant families should purge");
    let replay = purge_relational(&purge_pool, tenant_id, &operation_id)
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
            // Two inverse authorization tuples, not one: the workspace tuple plus
            // the `tenant -> session` tuple for the seeded session. Inverse intent
            // is per-subject, so this count tracks what the fixture actually
            // contains rather than being a constant — a session that produced no
            // tuple would mean the purge left its FGA relationships behind.
            ("public.authz_outbox".to_string(), 2),
        ])
    );

    // The transcript is gone. `events` is append-only behind a per-row BEFORE
    // DELETE trigger, so this assertion is the one that fails — with P0001
    // `events table is append-only` — if the purge stops setting
    // `moa.events_maintenance`. Before the fixture seeded an event, that trigger
    // never fired and a purge that could not delete any real tenant's
    // conversation passed this suite.
    let transcript: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM events WHERE tenant_id = $1),
            (SELECT count(*) FROM sessions WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load purged tenant transcript state");
    assert_eq!(
        transcript,
        (0, 0),
        "a purged tenant's events and sessions must be gone"
    );

    // The escape hatch closed with the transaction. `set_config(..., true)` is
    // transaction-local, so no connection may carry `moa.events_maintenance` back
    // into the pool. The first user of an escape hatch must not be whoever quietly
    // disables it for everyone, and this is what makes that checkable.
    //
    // Every connection is held at once before any is inspected, so the check
    // covers whichever one ran the purge transaction rather than whichever one the
    // pool happens to hand back first.
    let mut held = Vec::new();
    for _ in 0..PURGE_POOL_CONNECTIONS {
        held.push(
            purge_pool
                .acquire()
                .await
                .expect("hold every pooled connection for the hatch check"),
        );
    }
    for (index, connection) in held.iter_mut().enumerate() {
        let hatch: Option<String> =
            sqlx::query_scalar("SELECT current_setting('moa.events_maintenance', true)")
                .fetch_one(&mut **connection)
                .await
                .expect("read the append-only maintenance setting");
        assert_ne!(
            hatch.as_deref(),
            Some("on"),
            "connection {index} carried the append-only maintenance hatch back into the pool, \
             observed: {hatch:?}"
        );

        let refused = sqlx::query("DELETE FROM events WHERE tenant_id = $1")
            .bind(NEIGHBOUR_TENANT)
            .execute(&mut **connection)
            .await
            .expect_err("the append-only guard must still refuse an ordinary delete");
        assert!(
            refused.to_string().contains("append-only"),
            "connection {index} expected the append-only guard to refuse, observed: {refused}"
        );
    }

    // The neighbour tenant's source-ACL state is untouched. This is the failure the
    // purged-tenant residue map structurally cannot catch: a step that dropped
    // its `WHERE tenant_id = $1` would leave that map empty and still have
    // destroyed every other tenant's source-ACL state.
    let neighbour_acl: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM moa.knowledge_source_acl_keys WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_source_acl_snapshots WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_source_acl_entries WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_source_principal_bindings WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_source_principal_group_bindings WHERE tenant_id = $1)
        "#,
    )
    .bind(NEIGHBOUR_TENANT)
    .fetch_one(pool)
    .await
    .expect("load neighbour tenant source-ACL state");
    assert_eq!(
        neighbour_acl,
        (1, 1, 1, 1, 1, 1),
        "another tenant's source-ACL state must survive this tenant's purge"
    );

    let neighbour_connectors: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM moa.connector_connections WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.connector_action_bindings WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.connector_action_invocations WHERE tenant_id = $1)
        "#,
    )
    .bind(NEIGHBOUR_TENANT)
    .fetch_one(pool)
    .await
    .expect("load neighbour tenant connector state");
    assert_eq!(
        neighbour_connectors,
        (2, 1, 1),
        "another tenant's action and knowledge connector parents, binding, and invocation must survive this tenant's purge"
    );

    // The neighbour's pending lineage and archived transcript both survive. These
    // are the assertions an `AND FALSE` or a dropped `WHERE` dies on: the purged
    // tenant's residue map cannot see them, because it only counts rows belonging
    // to the tenant being purged.
    let neighbour_transcript: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $1),
            (SELECT count(*) FROM session_event_archives WHERE tenant_id = $2)
        "#,
    )
    .bind(StoragePartitionId::for_tenant(TenantId::from(NEIGHBOUR_TENANT)).as_str())
    .bind(NEIGHBOUR_TENANT)
    .fetch_one(pool)
    .await
    .expect("load neighbour lineage and archive state");
    assert_eq!(
        neighbour_transcript,
        (1, 1),
        "another tenant's pending lineage and archived transcript must survive this \
         tenant's purge"
    );

    // Behavior Lab score provenance: gone for the purged tenant (the residue map
    // above already proves that), still present for the neighbour. A step that
    // dropped its storage-partition predicate would satisfy the residue map and
    // fail here.
    let neighbour_partition = StoragePartitionId::for_tenant(TenantId::from(NEIGHBOUR_TENANT));
    let neighbour_provenance: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.experiment_score_provenance WHERE storage_partition_id = $1",
    )
    .bind(neighbour_partition.to_string())
    .fetch_one(pool)
    .await
    .expect("load neighbour experiment score provenance");
    assert_eq!(
        neighbour_provenance, 1,
        "another tenant's Behavior Lab score provenance must survive this tenant's purge"
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
    // Every enqueued tuple, by identity — not `fetch_one`, which silently
    // inspected whichever row came back first and would pass while the other was
    // wrong or missing. This is also what makes the residue count of two above a
    // statement rather than a magic number: the rows are named here.
    let inverse_tuples: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT op, status, tuple_relation, tuple_object FROM authz_outbox \
         WHERE tenant_id = $1 ORDER BY tuple_relation",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .expect("load inverse authorization intent");
    assert_eq!(
        inverse_tuples,
        vec![
            (
                "delete".to_string(),
                "pending".to_string(),
                "tenant".to_string(),
                format!("session:{purged_session_id}"),
            ),
            (
                "delete".to_string(),
                "pending".to_string(),
                "workspace".to_string(),
                format!("tenant:{tenant_id}"),
            ),
        ],
        "the purge must enqueue inverse intent for the tenant AND for each of its \
         sessions; a missing session tuple leaves that session's FGA relationships \
         behind after its rows are gone"
    );

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
async fn tenant_purge_locked_delete_rollback_retries_same_stage_db_memory() {
    // Pins: a row already locked by an uncommitted DELETE must make the purge
    // batch fail quickly and atomically. It must not skip the row, advance the
    // durable stage, or count work that did not commit; once the DELETE rolls
    // back, the same stage must retry and remove the restored row.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap locked tenant-purge db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let storage_partition_id = StoragePartitionId::for_tenant(TenantId::from(tenant_id));

    seed_tenant(pool, tenant_id).await;
    sqlx::query("INSERT INTO users (id, tenant_id, email) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(tenant_id)
        .bind(format!("locked-delete-{user_id}@example.test"))
        .execute(pool)
        .await
        .expect("seed the user row that DELETE will lock");
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(pool)
        .await
        .expect("start the bounded tenant purge");

    // All stages before users are empty in this focused fixture. Advance the
    // durable cursor directly so the first production batch reaches the locked
    // row without spending 65 empty transactions on test setup.
    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'public.users' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .execute(pool)
    .await
    .expect("focus purge progress on the users stage");
    let progress_before: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("load purge progress before the conflicting DELETE");
    assert_eq!(
        progress_before,
        (
            "in_progress".to_string(),
            "public.users".to_string(),
            0,
            0,
            0,
        )
    );

    let mut locked_delete = pool.begin().await.expect("begin the locking DELETE");
    let locked_rows = sqlx::query("DELETE FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .execute(&mut *locked_delete)
        .await
        .expect("delete the user without committing");
    assert_eq!(locked_rows.rows_affected(), 1);

    let started = Instant::now();
    let locked_error = tokio::time::timeout(
        Duration::from_secs(3),
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT batch_state, stage, affected \
             FROM moa.run_tenant_purge_batch($1, $2)",
        )
        .bind(tenant_id)
        .bind(&operation_id)
        .fetch_one(pool),
    )
    .await
    .expect("the transaction-local one-second lock timeout must bound the purge batch")
    .expect_err("the purge batch must report the locked row instead of skipping it");
    let elapsed = started.elapsed();
    let sqlstate = locked_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(sqlstate.as_deref(), Some("55P03"));
    assert!(
        elapsed < Duration::from_secs(3),
        "locked purge batch exceeded its bound: {elapsed:?}"
    );

    let progress_after_timeout: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("load purge progress after the lock timeout");
    assert_eq!(
        progress_after_timeout, progress_before,
        "a timed-out batch must leave the stage and every counter unchanged"
    );

    locked_delete
        .rollback()
        .await
        .expect("roll back the conflicting DELETE and restore the user");
    let restored: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("count the restored user");
    assert_eq!(restored, 1);

    let retry: (String, String, i64) = sqlx::query_as(
        "SELECT batch_state, stage, affected \
         FROM moa.run_tenant_purge_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("retry the same users stage after releasing the row lock");
    assert_eq!(
        retry,
        ("in_progress".to_string(), "public.users".to_string(), 1)
    );
    let user_residue: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("count user residue after the successful retry");
    assert_eq!(user_residue, 0);

    let outcome = purge_relational(pool, tenant_id, &operation_id)
        .await
        .expect("finish the tenant purge after the successful retry");
    assert_eq!(outcome, RelationalPurgeOutcome::Committed);
    let final_progress: (String, String, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("load committed purge progress");
    assert_eq!(final_progress.0, "relationally_committed");
    assert_eq!(final_progress.1, "complete");
    assert_eq!(final_progress.2, 2, "user plus tenant must be deleted");
    assert!(
        final_progress.3 > 1,
        "completion must durably advance stages"
    );
    assert_eq!(
        tenant_residue(pool, tenant_id, storage_partition_id.as_str()).await,
        BTreeMap::from([
            ("moa.destruction_operation_fence".to_string(), 1),
            ("moa.tenant_purge_operations".to_string(), 1),
        ]),
        "only committed purge-control rows may remain"
    );
}

#[tokio::test]
async fn tenant_purge_unknown_stage_and_invalid_state_pair_fail_closed_db_memory() {
    // Pins: corrupted durable progress is never interpreted as finalization.
    // SQL owns catalog membership, while Rust rejects impossible state/stage
    // pairs before issuing a relational batch; neither path may advance state.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap invalid tenant-purge progress db");
    let pool = test_db.store().pool();

    let unknown_tenant = Uuid::new_v4();
    let unknown_operation = format!("tenant-purge-{unknown_tenant}");
    seed_tenant(pool, unknown_tenant).await;
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(unknown_tenant)
        .bind(&unknown_operation)
        .execute(pool)
        .await
        .expect("start unknown-stage purge fixture");
    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'moa.unknown_stage' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(unknown_tenant)
    .bind(&unknown_operation)
    .execute(pool)
    .await
    .expect("inject unknown durable stage");
    let unknown_before: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(unknown_tenant)
    .fetch_one(pool)
    .await
    .expect("load unknown-stage progress");

    let sql_error = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT batch_state, stage, affected \
         FROM moa.run_tenant_purge_batch($1, $2)",
    )
    .bind(unknown_tenant)
    .bind(&unknown_operation)
    .fetch_one(pool)
    .await
    .expect_err("SQL must reject an unknown durable purge stage");
    assert_eq!(
        sql_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()))
            .as_deref(),
        Some("55000")
    );
    assert!(
        sql_error.to_string().contains("unknown tenant purge stage"),
        "unexpected unknown-stage SQL error: {sql_error}"
    );
    let repository_error = purge_relational(pool, unknown_tenant, &unknown_operation)
        .await
        .expect_err("repository must propagate the unknown-stage failure");
    assert!(
        repository_error.contains("unknown tenant purge stage"),
        "unexpected repository unknown-stage error: {repository_error}"
    );
    let unknown_after: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(unknown_tenant)
    .fetch_one(pool)
    .await
    .expect("reload unknown-stage progress");
    assert_eq!(unknown_after, unknown_before);

    let invalid_tenant = Uuid::new_v4();
    let invalid_operation = format!("tenant-purge-{invalid_tenant}");
    seed_tenant(pool, invalid_tenant).await;
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(invalid_tenant)
        .bind(&invalid_operation)
        .execute(pool)
        .await
        .expect("start invalid-pair purge fixture");
    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'complete' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(invalid_tenant)
    .bind(&invalid_operation)
    .execute(pool)
    .await
    .expect("inject invalid in-progress/complete pair");
    let invalid_before: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(invalid_tenant)
    .fetch_one(pool)
    .await
    .expect("load invalid-pair progress");
    let invalid_error = purge_relational(pool, invalid_tenant, &invalid_operation)
        .await
        .expect_err("repository must reject an invalid progress pair");
    assert_eq!(
        invalid_error,
        "invalid tenant purge progress state/stage pair in_progress/complete"
    );
    let invalid_after: (String, String, i64, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(invalid_tenant)
    .fetch_one(pool)
    .await
    .expect("reload invalid-pair progress");
    assert_eq!(invalid_after, invalid_before);
}

#[tokio::test]
async fn tenant_purge_catalog_preserves_global_simulator_authority_db_memory() {
    // Pins: nullable global-RLS scope columns do not make the platform mandate
    // and evidence import tenant-owned. Catalog coverage must ignore both, and a
    // tenant purge must leave their global rows unchanged.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap global-authority catalog db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    seed_tenant(pool, tenant_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.simulator_certification_evidence_import (
            mandate_uid, storage_partition_id, user_id, study_uid,
            study_artifact_hash, source_manifest_hash, source_reference,
            imported_by
        )
        VALUES ($1, NULL, NULL, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
    .bind(Uuid::new_v4())
    .bind(vec![0xAA_u8; 32])
    .bind(vec![0_u8; 32])
    .bind("fixture://global-authority")
    .bind("tenant-purge-catalog-test")
    .execute(pool)
    .await
    .expect("seed one global evidence import");
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start global-authority catalog fence");

    assert_eq!(
        purge_relational(pool, tenant_id, &operation_id)
            .await
            .expect("global authority tables are outside tenant purge ownership"),
        RelationalPurgeOutcome::Committed
    );
    let authority_rows: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM moa.simulator_certification_mandate
             WHERE mandate_uid = $1),
            (SELECT count(*) FROM moa.simulator_certification_evidence_import
             WHERE mandate_uid = $1)
        "#,
    )
    .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
    .fetch_one(pool)
    .await
    .expect("count preserved global simulator authority");
    assert_eq!(authority_rows, (1, 1));
}

#[tokio::test]
async fn tenant_purge_catalog_drift_fails_closed_with_resumable_progress_db_memory() {
    // Pins: adding a tenant-owned table without registering its purge behavior
    // prevents final commitment. Earlier bounded batches remain durably recorded,
    // so correcting the catalog can resume instead of rolling back unbounded work.
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

    let error = purge_relational(pool, tenant_id, &operation_id)
        .await
        .expect_err("unregistered tenant table must reject purge");
    assert!(
        error.contains("tenant purge catalog drift")
            && error.contains("moa.unregistered_tenant_payload"),
        "unexpected catalog-drift error: {error}"
    );
    let rows_after_failure: (i64, i64, String, String) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT count(*) FROM moa.unregistered_tenant_payload WHERE tenant_id = $1),
            (SELECT status FROM moa.tenant_purge_operations WHERE tenant_id = $1),
            (SELECT current_stage FROM moa.tenant_purge_operations WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("inspect durable failed-finalization state");
    assert_eq!(
        rows_after_failure,
        (
            0,
            1,
            "in_progress".to_string(),
            "moa.tenant_purge_operations".to_string()
        ),
        "catalog drift must preserve the fence/progress and the unowned residue, but must never commit"
    );
}

#[tokio::test]
async fn tenant_purge_preserves_active_and_receipted_deletes_and_reactivates_dead_letters_db_memory()
 {
    // Pins: actual tenant-attributed tuples are the source of truth. Existing
    // pending/in-flight deletes remain active, succeeded deletes remain receipts,
    // and a dead-letter delete is reactivated without changing tuple identity.
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

    assert_eq!(
        purge_relational(pool, tenant_id, &operation_id)
            .await
            .expect("valid delete states must drain and finalize"),
        RelationalPurgeOutcome::Committed
    );
    let rows_after_purge: (i64, String, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT status FROM moa.tenant_purge_operations WHERE tenant_id = $1),
            (SELECT count(*) FROM authz_outbox WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("inspect exact authz residue state");
    assert_eq!(
        rows_after_purge,
        (0, "relationally_committed".to_string(), 4)
    );
    let states: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT split_part(tuple_object, ':', 3), status, generation \
         FROM authz_outbox WHERE tenant_id = $1 ORDER BY 1",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .expect("load preserved and reactivated authz states");
    assert_eq!(
        states,
        vec![
            ("dead_letter".to_string(), "pending".to_string(), 2),
            ("in_flight".to_string(), "in_flight".to_string(), 1),
            ("pending".to_string(), "pending".to_string(), 1),
            ("succeeded".to_string(), "succeeded".to_string(), 1),
        ]
    );
}

#[tokio::test]
async fn tenant_purge_removes_an_expired_vector_claim_as_bounded_residue_db_memory() {
    // Pins: after real vector remote-I/O releases its shared tenant lock, an
    // expired durable claim is ordinary bounded residue and cannot strand purge.
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
        VALUES ($1, $2, 'upsert', $3, now() - INTERVAL '1 second', now())
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("seed expired pre-fence vector claim");
    start_destruction(
        pool,
        TenantId::from(tenant_id),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start vector-claim destruction fence");

    assert_eq!(
        purge_relational(pool, tenant_id, &operation_id)
            .await
            .expect("expired vector claim should be bounded purge residue"),
        RelationalPurgeOutcome::Committed
    );
    let committed_state: (i64, i64, String) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM tenants WHERE id = $1),
            (SELECT count(*) FROM moa.vector_sync_outbox WHERE storage_partition_id = $2),
            (SELECT status FROM moa.tenant_purge_operations WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .bind(storage_partition_id.as_str())
    .fetch_one(pool)
    .await
    .expect("inspect committed expired-claim purge state");
    assert_eq!(
        committed_state,
        (0, 0, "relationally_committed".to_string())
    );
}

#[tokio::test]
async fn tenant_purge_rejects_and_drains_concurrent_writers_for_every_scope_mode_db_memory() {
    // Pins: every catalog scope resolver takes exactly one shared advisory lock
    // per distinct tenant in a multi-row statement. Purge admission must wait
    // for that pre-fence statement to commit, then the durable fence must reject
    // the same target-plus-neighbour statement with 55000 and roll it back as a
    // unit, including the otherwise-unfenced neighbour rows.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap scope-mode tenant-purge db");
    let observer_pool = test_db.store().pool();

    for mode in TenantWriteScopeMode::ALL {
        let target_tenant = Uuid::new_v4();
        let neighbour_tenant = Uuid::new_v4();
        seed_tenant(observer_pool, target_tenant).await;
        seed_tenant(observer_pool, neighbour_tenant).await;
        let fixture =
            seed_tenant_write_scope_fixture(&test_db, mode, target_tenant, neighbour_tenant).await;
        let operation_id = format!("scope-mode-{}-{target_tenant}", mode.name());
        let replica_pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(test_db.database_url())
            .await
            .unwrap_or_else(|error| panic!("connect {} replica pool: {error}", mode.name()));

        let mut writer = replica_pool
            .begin()
            .await
            .unwrap_or_else(|error| panic!("begin {} pre-fence writer: {error}", mode.name()));
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .unwrap_or_else(|error| panic!("read {} writer pid: {error}", mode.name()));
        let pre_fence_write = sqlx::query(&fixture.update_sql)
            .bind(&fixture.pre_marker)
            .execute(&mut *writer)
            .await
            .unwrap_or_else(|error| panic!("run {} pre-fence statement: {error}", mode.name()));
        assert_eq!(
            pre_fence_write.rows_affected(),
            fixture.expected_rows,
            "{} fixture must touch duplicate target rows plus its neighbour",
            mode.name()
        );

        let writer_locks: Vec<(String, bool)> = sqlx::query_as(
            "SELECT mode, granted FROM pg_locks \
             WHERE pid = $1 AND locktype = 'advisory' \
             ORDER BY mode, granted",
        )
        .bind(writer_pid)
        .fetch_all(observer_pool)
        .await
        .unwrap_or_else(|error| panic!("inspect {} writer locks: {error}", mode.name()));
        assert_eq!(
            writer_locks,
            vec![
                ("ShareLock".to_string(), true),
                ("ShareLock".to_string(), true),
            ],
            "{} transition table must coalesce duplicate rows to the target and neighbour tenants",
            mode.name()
        );

        let mut purge_connection = replica_pool
            .acquire()
            .await
            .unwrap_or_else(|error| panic!("acquire {} purge connection: {error}", mode.name()));
        let purge_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *purge_connection)
            .await
            .unwrap_or_else(|error| panic!("read {} purge pid: {error}", mode.name()));
        let purge_tenant = target_tenant;
        let purge_operation = operation_id.clone();
        let purge_task = tokio::spawn(async move {
            sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
                .bind(purge_tenant)
                .bind(purge_operation)
                .execute(&mut *purge_connection)
                .await
        });

        wait_for_advisory_lock_waiter(observer_pool, purge_pid)
            .await
            .unwrap_or_else(|error| panic!("observe {} purge lock waiter: {error}", mode.name()));
        let fence_while_writer_open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.destruction_operation_fence \
             WHERE tenant_id = $1 AND subject_id IS NULL",
        )
        .bind(target_tenant)
        .fetch_one(observer_pool)
        .await
        .unwrap_or_else(|error| panic!("inspect {} blocked fence: {error}", mode.name()));
        assert_eq!(
            fence_while_writer_open,
            0,
            "{} purge must not publish a fence before the pre-fence writer drains",
            mode.name()
        );

        writer
            .commit()
            .await
            .unwrap_or_else(|error| panic!("commit {} pre-fence writer: {error}", mode.name()));
        tokio::time::timeout(Duration::from_secs(3), purge_task)
            .await
            .unwrap_or_else(|_| {
                panic!("{} purge remained blocked after writer commit", mode.name())
            })
            .unwrap_or_else(|error| panic!("join {} purge task: {error}", mode.name()))
            .unwrap_or_else(|error| panic!("start {} tenant purge: {error}", mode.name()));

        assert_eq!(
            count_tenant_write_fixture_marker(observer_pool, &fixture, &fixture.pre_marker).await,
            fixture.expected_rows as i64,
            "{} pre-fence statement must commit all fixture rows",
            mode.name()
        );
        let post_fence_result = sqlx::query(&fixture.update_sql)
            .bind(&fixture.post_marker)
            .execute(&replica_pool)
            .await;
        let post_fence_error = match post_fence_result {
            Ok(result) => panic!(
                "{} post-fence statement unexpectedly updated {} rows",
                mode.name(),
                result.rows_affected()
            ),
            Err(error) => error,
        };
        let post_fence_sqlstate = post_fence_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        assert_eq!(
            post_fence_sqlstate.as_deref(),
            Some("55000"),
            "{} post-fence statement must fail with object-not-in-prerequisite-state",
            mode.name()
        );
        assert_eq!(
            count_tenant_write_fixture_marker(observer_pool, &fixture, &fixture.post_marker).await,
            0,
            "{} rejected statement must not change target or neighbour rows",
            mode.name()
        );
        assert_eq!(
            count_tenant_write_fixture_marker(observer_pool, &fixture, &fixture.pre_marker).await,
            fixture.expected_rows as i64,
            "{} rejected target-plus-neighbour statement must roll back atomically",
            mode.name()
        );

        replica_pool.close().await;
    }
}

#[tokio::test]
async fn tenant_purge_dead_letter_behind_authz_cursor_resets_and_redrains_db_memory() {
    // Pins: a delete that dead-letters after its UUID has fallen behind the
    // persisted authz cursor cannot strand finalization. The final relational
    // stage rewinds authz to NULL, and the production inversion batch requeues
    // exactly that row without perturbing the already-active neighbour tuple.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap behind-cursor dead-letter db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let behind_id = Uuid::from_u128(1);
    let first_cursor_id = Uuid::from_u128(1000);
    let final_cursor_id = Uuid::from_u128(1001);
    seed_tenant(pool, tenant_id).await;
    sqlx::query(
        r#"
        INSERT INTO authz_outbox
            (id, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
        SELECT
            lpad(to_hex(ordinal), 32, '0')::UUID,
            'write',
            format('user:%s', ordinal),
            'member',
            format('tenant:%s:tuple:%s', $1::TEXT, ordinal),
            1,
            $1
        FROM generate_series(1, 1001) AS ordinal
        "#,
    )
    .bind(tenant_id)
    .execute(pool)
    .await
    .expect("seed 1001 deterministically ordered desired authz tuples");
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(pool)
        .await
        .expect("start behind-cursor tenant purge");

    let first_page: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("invert the initial ordered authz page");
    assert_eq!(first_page, (1000, 1000, false, Some(first_cursor_id)));
    sqlx::query(
        "UPDATE authz_outbox \
         SET status = 'dead_letter', attempts = 7, last_error = 'delivery exhausted', \
             lease_token = $2, lease_expires_at = now() + INTERVAL '1 minute' \
         WHERE id = $1",
    )
    .bind(behind_id)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("dead-letter the delete behind the persisted 1000-row cursor");
    let behind_before_redrain: AuthzOutboxIdentityState = sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id, \
                status, generation, attempts, last_error, lease_token IS NOT NULL \
         FROM authz_outbox WHERE id = $1",
    )
    .bind(behind_id)
    .fetch_one(pool)
    .await
    .expect("load behind-cursor dead letter before the redrain");
    assert_eq!(
        behind_before_redrain,
        (
            "delete".to_string(),
            "user:1".to_string(),
            "member".to_string(),
            format!("tenant:{tenant_id}:tuple:1"),
            1,
            Some(tenant_id),
            "dead_letter".to_string(),
            2,
            7,
            Some("delivery exhausted".to_string()),
            true,
        ),
        "delivery failure must not rewrite tuple identity or tenant attribution"
    );
    let final_page: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("invert the one-row tail beyond the persisted cursor");
    assert_eq!(final_page, (1, 1, false, Some(final_cursor_id)));
    let exhausted_page: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("advance from the exhausted 1001-row authz pass");
    assert_eq!(exhausted_page, (0, 0, true, None));
    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'moa.tenant_purge_operations' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .execute(pool)
    .await
    .expect("focus finalization on the last catalog stage");

    let rewind: (String, String, i64) = sqlx::query_as(
        "SELECT batch_state, stage, affected \
         FROM moa.run_tenant_purge_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("rewind finalization for the behind-cursor dead letter");
    assert_eq!(rewind, ("in_progress".to_string(), "authz".to_string(), 0));
    let rewound_progress: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT current_stage, authz_cursor, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load rewound authz progress");
    assert_eq!(rewound_progress, ("authz".to_string(), None, 4));

    let redrain: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("redrain from the reset authz cursor");
    assert_eq!(redrain, (1000, 1, false, Some(first_cursor_id)));
    let exact_states: Vec<AuthzOutboxDeliveryState> = sqlx::query_as(
        "SELECT id, op, status, generation, attempts, last_error, \
                    lease_token IS NULL, lease_expires_at IS NULL \
             FROM authz_outbox WHERE id IN ($1, $2, $3) ORDER BY id",
    )
    .bind(behind_id)
    .bind(first_cursor_id)
    .bind(final_cursor_id)
    .fetch_all(pool)
    .await
    .expect("load representative exact redrained authz tuple state");
    assert_eq!(
        exact_states,
        vec![
            (
                behind_id,
                "delete".to_string(),
                "pending".to_string(),
                3,
                0,
                None,
                true,
                true,
            ),
            (
                first_cursor_id,
                "delete".to_string(),
                "pending".to_string(),
                2,
                0,
                None,
                true,
                true,
            ),
            (
                final_cursor_id,
                "delete".to_string(),
                "pending".to_string(),
                2,
                0,
                None,
                true,
                true,
            ),
        ],
        "only the behind-cursor dead letter may be reactivated and generation-bumped"
    );
    let aggregate_state: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                count(*) FILTER (WHERE generation = 2), \
                count(*) FILTER (WHERE generation = 3), \
                count(*) FILTER (WHERE status = 'pending' AND attempts = 0 \
                                 AND last_error IS NULL AND lease_token IS NULL \
                                 AND lease_expires_at IS NULL) \
         FROM authz_outbox WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load aggregate exact redrain state");
    assert_eq!(aggregate_state, (1001, 1000, 1, 1001));
    let redrain_tail: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("scan the one-row tail of the reset authz pass");
    assert_eq!(redrain_tail, (1, 0, false, Some(final_cursor_id)));
    let redrain_exhausted: (i32, i32, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .fetch_one(pool)
    .await
    .expect("finish the reset 1001-row authz pass");
    assert_eq!(redrain_exhausted, (0, 0, true, None));
    let final_progress: (String, Option<Uuid>, i64) = sqlx::query_as(
        "SELECT current_stage, authz_cursor, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load progress after the exact redrain");
    assert_eq!(final_progress.0, "public.oauth_tokens");
    assert_eq!(final_progress.1, Some(final_cursor_id));
    assert_eq!(final_progress.2, 7);
}

#[tokio::test]
async fn tenant_purge_release_stage_resumes_after_process_loss_at_1001_boundary_db_memory() {
    // Pins: the release-control stage commits 1,000 then 1 target policy rows,
    // persists its counters between process-local pools, and resumes to final
    // commitment through the production repository on process C. A different
    // tenant's policy must survive every batch.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap release-policy boundary db");
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let neighbour_tenant = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    seed_tenant(pool, tenant_id).await;
    seed_tenant(pool, neighbour_tenant).await;
    seed_release_policy_rows(pool, tenant_id, 1001)
        .await
        .expect("seed 1001 target release policies");
    seed_release_policy_rows(pool, neighbour_tenant, 1)
        .await
        .expect("seed neighbour release policy");
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(pool)
        .await
        .expect("start release-policy boundary purge");
    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'moa.artifact_release_policy' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .execute(pool)
    .await
    .expect("focus progress on the release-policy stage");

    let first =
        run_one_purge_batch_from_fresh_pool(test_db.database_url(), tenant_id, &operation_id).await;
    assert_eq!(
        first,
        (
            "in_progress".to_string(),
            "moa.artifact_release_policy".to_string(),
            1000,
        )
    );
    assert_eq!(
        release_policy_progress(pool, tenant_id).await,
        ("moa.artifact_release_policy".to_string(), 1000, 1000, 1,)
    );
    assert_eq!(
        release_policy_counts(pool, tenant_id, neighbour_tenant).await,
        (1, 1)
    );

    let second =
        run_one_purge_batch_from_fresh_pool(test_db.database_url(), tenant_id, &operation_id).await;
    assert_eq!(
        second,
        (
            "in_progress".to_string(),
            "moa.artifact_release_policy".to_string(),
            1,
        )
    );
    assert_eq!(
        release_policy_progress(pool, tenant_id).await,
        ("moa.artifact_release_policy".to_string(), 1001, 1001, 2,)
    );
    assert_eq!(
        release_policy_counts(pool, tenant_id, neighbour_tenant).await,
        (0, 1)
    );

    let process_c = PgPoolOptions::new()
        .max_connections(2)
        .connect(test_db.database_url())
        .await
        .expect("connect process C purge pool");
    let resumed = purge_relational(&process_c, tenant_id, &operation_id)
        .await
        .expect("process C must resume and complete the persisted purge");
    assert_eq!(resumed, RelationalPurgeOutcome::Committed);
    process_c.close().await;

    let committed_progress: (String, String, i64, i64) = sqlx::query_as(
        "SELECT status, current_stage, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load process-loss resumed purge completion");
    assert_eq!(committed_progress.0, "relationally_committed");
    assert_eq!(committed_progress.1, "complete");
    assert_eq!(
        committed_progress.2, 1002,
        "1001 release policies plus the target tenant must be deleted"
    );
    assert!(
        committed_progress.3 > 3,
        "process C must durably traverse the remaining catalog stages"
    );
    assert_eq!(
        release_policy_counts(pool, tenant_id, neighbour_tenant).await,
        (0, 1)
    );
    let tenant_counts: (i64, i64) = sqlx::query_as(
        "SELECT \
             (SELECT count(*) FROM tenants WHERE id = $1), \
             (SELECT count(*) FROM tenants WHERE id = $2)",
    )
    .bind(tenant_id)
    .bind(neighbour_tenant)
    .fetch_one(pool)
    .await
    .expect("load target and neighbour tenants after process-loss resume");
    assert_eq!(tenant_counts, (0, 1));
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

#[derive(Clone, Copy, Debug)]
enum TenantWriteScopeMode {
    TenantId,
    StoragePartitionId,
    TenantPrimaryKey,
    Auth0CibaApproval,
    ScimGroupMember,
    ApiKeyRevocation,
    SessionEventDedupe,
}

impl TenantWriteScopeMode {
    const ALL: [Self; 7] = [
        Self::TenantId,
        Self::StoragePartitionId,
        Self::TenantPrimaryKey,
        Self::Auth0CibaApproval,
        Self::ScimGroupMember,
        Self::ApiKeyRevocation,
        Self::SessionEventDedupe,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::TenantId => "tenant_id",
            Self::StoragePartitionId => "storage_partition_id",
            Self::TenantPrimaryKey => "tenant_primary_key",
            Self::Auth0CibaApproval => "auth0_ciba_approval",
            Self::ScimGroupMember => "scim_group_member",
            Self::ApiKeyRevocation => "api_key_revocation",
            Self::SessionEventDedupe => "session_event_dedupe",
        }
    }
}

#[derive(Debug)]
struct TenantWriteScopeFixture {
    update_sql: String,
    count_sql: String,
    pre_marker: String,
    post_marker: String,
    expected_rows: u64,
}

async fn seed_tenant_write_scope_fixture(
    test_db: &moa_test_support::postgres::TestDb,
    mode: TenantWriteScopeMode,
    target_tenant: Uuid,
    neighbour_tenant: Uuid,
) -> TenantWriteScopeFixture {
    let pool = test_db.store().pool();
    let pre_marker = format!("pre-{}-{target_tenant}", mode.name());
    let post_marker = format!("post-{}-{target_tenant}", mode.name());

    let (update_sql, count_sql, expected_rows) = match mode {
        TenantWriteScopeMode::TenantId => {
            let mut user_ids = seed_scope_users(pool, target_tenant, 2, mode.name()).await;
            user_ids.extend(seed_scope_users(pool, neighbour_tenant, 1, mode.name()).await);
            let predicate = uuid_in_predicate("id", &user_ids);
            (
                format!("UPDATE users SET display_name = $1 WHERE {predicate}"),
                format!("SELECT count(*) FROM users WHERE {predicate} AND display_name = $1"),
                3,
            )
        }
        TenantWriteScopeMode::StoragePartitionId => {
            let mut sync_ids = Vec::with_capacity(3);
            for tenant_id in [target_tenant, target_tenant, neighbour_tenant] {
                let sync_id: i64 = sqlx::query_scalar(
                    "INSERT INTO moa.vector_sync_outbox \
                     (storage_partition_id, uid, op) VALUES ($1, $2, 'delete') \
                     RETURNING sync_id",
                )
                .bind(tenant_id.to_string())
                .bind(Uuid::new_v4())
                .fetch_one(pool)
                .await
                .expect("seed storage-partition scope row");
                sync_ids.push(sync_id);
            }
            let ids = sync_ids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "UPDATE moa.vector_sync_outbox SET last_error = $1 WHERE sync_id IN ({ids})"
                ),
                format!(
                    "SELECT count(*) FROM moa.vector_sync_outbox \
                     WHERE sync_id IN ({ids}) AND last_error = $1"
                ),
                3,
            )
        }
        TenantWriteScopeMode::TenantPrimaryKey => {
            let tenant_ids = [target_tenant, neighbour_tenant];
            let predicate = uuid_in_predicate("id", &tenant_ids);
            (
                format!("UPDATE tenants SET name = $1 WHERE {predicate}"),
                format!("SELECT count(*) FROM tenants WHERE {predicate} AND name = $1"),
                2,
            )
        }
        TenantWriteScopeMode::Auth0CibaApproval => {
            let mut user_ids = seed_scope_users(pool, target_tenant, 2, mode.name()).await;
            user_ids.extend(seed_scope_users(pool, neighbour_tenant, 1, mode.name()).await);
            let session_ids = [
                seed_scope_session(test_db, target_tenant).await,
                seed_scope_session(test_db, target_tenant).await,
                seed_scope_session(test_db, neighbour_tenant).await,
            ];
            let approval_ids = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
            for ((approval_id, session_id), user_id) in approval_ids
                .iter()
                .zip(session_ids.iter())
                .zip(user_ids.iter())
            {
                sqlx::query(
                    r#"
                    INSERT INTO auth0_ciba_approvals
                        (id, session_id, deciding_user_id, awakeable_id, auth_req_id,
                         poll_interval_ms, next_poll_at, expires_at)
                    VALUES ($1, $2, $3, $4, $5, 1000, now(), now() + INTERVAL '1 hour')
                    "#,
                )
                .bind(approval_id)
                .bind(session_id)
                .bind(user_id)
                .bind(format!("awakeable-{approval_id}"))
                .bind(format!("auth-request-{approval_id}"))
                .execute(pool)
                .await
                .expect("seed CIBA approval scope row");
            }
            let predicate = uuid_in_predicate("id", &approval_ids);
            (
                format!("UPDATE auth0_ciba_approvals SET deny_reason = $1 WHERE {predicate}"),
                format!(
                    "SELECT count(*) FROM auth0_ciba_approvals \
                     WHERE {predicate} AND deny_reason = $1"
                ),
                3,
            )
        }
        TenantWriteScopeMode::ScimGroupMember => {
            let mut user_ids = seed_scope_users(pool, target_tenant, 2, mode.name()).await;
            user_ids.extend(seed_scope_users(pool, neighbour_tenant, 1, mode.name()).await);
            let target_group = seed_scope_group(pool, target_tenant, mode.name()).await;
            let neighbour_group = seed_scope_group(pool, neighbour_tenant, mode.name()).await;
            for (group_id, user_id) in [
                (target_group, user_ids[0]),
                (target_group, user_ids[1]),
                (neighbour_group, user_ids[2]),
            ] {
                sqlx::query("INSERT INTO scim_group_members (group_id, user_id) VALUES ($1, $2)")
                    .bind(group_id)
                    .bind(user_id)
                    .execute(pool)
                    .await
                    .expect("seed SCIM group-member scope row");
            }
            let predicate = uuid_in_predicate("user_id", &user_ids);
            (
                format!(
                    "UPDATE scim_group_members SET added_at = $1::TIMESTAMPTZ WHERE {predicate}"
                ),
                format!(
                    "SELECT count(*) FROM scim_group_members \
                     WHERE {predicate} AND added_at = $1::TIMESTAMPTZ"
                ),
                3,
            )
        }
        TenantWriteScopeMode::ApiKeyRevocation => {
            let mut user_ids = seed_scope_users(pool, target_tenant, 2, mode.name()).await;
            user_ids.extend(seed_scope_users(pool, neighbour_tenant, 1, mode.name()).await);
            let tenant_ids = [target_tenant, target_tenant, neighbour_tenant];
            let mut revocation_ids = Vec::with_capacity(3);
            for (user_id, tenant_id) in user_ids.iter().zip(tenant_ids) {
                let api_key_id = Uuid::new_v4();
                sqlx::query(
                    r#"
                    INSERT INTO api_keys
                        (id, prefix, hash, owner_user_id, tenant_id, name, env)
                    VALUES ($1, $2, 'fixture-hash', $3, $4, 'purge fixture', 'dev')
                    "#,
                )
                .bind(api_key_id)
                .bind(format!("scope-{api_key_id}"))
                .bind(user_id)
                .bind(tenant_id)
                .execute(pool)
                .await
                .expect("seed API key parent for revocation scope");
                let revocation_id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO api_key_revocations (id, api_key_id, reason) \
                     VALUES ($1, $2, 'scope fixture')",
                )
                .bind(revocation_id)
                .bind(api_key_id)
                .execute(pool)
                .await
                .expect("seed API-key revocation scope row");
                revocation_ids.push(revocation_id);
            }
            let predicate = uuid_in_predicate("id", &revocation_ids);
            (
                format!("UPDATE api_key_revocations SET reason = $1 WHERE {predicate}"),
                format!(
                    "SELECT count(*) FROM api_key_revocations WHERE {predicate} AND reason = $1"
                ),
                3,
            )
        }
        TenantWriteScopeMode::SessionEventDedupe => {
            let session_ids = [
                seed_scope_session(test_db, target_tenant).await,
                seed_scope_session(test_db, target_tenant).await,
                seed_scope_session(test_db, neighbour_tenant).await,
            ];
            let dedupe_key = format!("scope-dedupe-{target_tenant}");
            for session_id in &session_ids {
                sqlx::query(
                    "INSERT INTO session_event_dedupe (session_id, dedupe_key, sequence_num) \
                     VALUES ($1, $2, 0)",
                )
                .bind(session_id)
                .bind(&dedupe_key)
                .execute(pool)
                .await
                .expect("seed session-event dedupe scope row");
            }
            let predicate = uuid_in_predicate("session_id", &session_ids);
            (
                format!(
                    "UPDATE session_event_dedupe SET sequence_num = $1::BIGINT \
                     WHERE {predicate} AND dedupe_key = '{dedupe_key}'"
                ),
                format!(
                    "SELECT count(*) FROM session_event_dedupe \
                     WHERE {predicate} AND dedupe_key = '{dedupe_key}' \
                       AND sequence_num = $1::BIGINT"
                ),
                3,
            )
        }
    };

    let (pre_marker, post_marker) = match mode {
        TenantWriteScopeMode::ScimGroupMember => (
            "2000-01-01T00:00:00Z".to_string(),
            "2001-01-01T00:00:00Z".to_string(),
        ),
        TenantWriteScopeMode::SessionEventDedupe => ("41".to_string(), "42".to_string()),
        _ => (pre_marker, post_marker),
    };
    TenantWriteScopeFixture {
        update_sql,
        count_sql,
        pre_marker,
        post_marker,
        expected_rows,
    }
}

async fn seed_scope_users(pool: &PgPool, tenant_id: Uuid, count: usize, label: &str) -> Vec<Uuid> {
    let mut user_ids = Vec::with_capacity(count);
    for ordinal in 0..count {
        let user_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, external_id, display_name) \
             VALUES ($1, $2, $3, $4, 'scope fixture')",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(format!("{label}-{ordinal}-{user_id}@example.test"))
        .bind(format!("{label}-{ordinal}-{user_id}"))
        .execute(pool)
        .await
        .expect("seed scope-mode user parent");
        user_ids.push(user_id);
    }
    user_ids
}

async fn seed_scope_group(pool: &PgPool, tenant_id: Uuid, label: &str) -> Uuid {
    let group_id = Uuid::new_v4();
    sqlx::query("INSERT INTO scim_groups (id, tenant_id, display_name) VALUES ($1, $2, $3)")
        .bind(group_id)
        .bind(tenant_id)
        .bind(format!("{label}-{group_id}"))
        .execute(pool)
        .await
        .expect("seed SCIM group parent");
    group_id
}

async fn seed_scope_session(test_db: &moa_test_support::postgres::TestDb, tenant_id: Uuid) -> Uuid {
    moa_core::traits::SessionStore::create_session(
        test_db.store(),
        moa_core::types::session::SessionMeta {
            tenant_id: TenantId::from(tenant_id),
            created_by: Some(moa_core::types::contact::SessionActorRef::Identity {
                id: Uuid::new_v4(),
            }),
            model: moa_core::types::identifiers::ModelId::new("purge-scope-test"),
            agent_context: Some(moa_core::types::agent::AgentContext::system_default()),
            ..moa_core::types::session::SessionMeta::default()
        },
    )
    .await
    .expect("seed session parent for scope-mode fixture")
    .0
}

fn uuid_in_predicate(column: &str, ids: &[Uuid]) -> String {
    let values = ids
        .iter()
        .map(|id| format!("'{id}'::UUID"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{column} IN ({values})")
}

async fn wait_for_advisory_lock_waiter(pool: &PgPool, pid: i32) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let locks: Vec<(String, bool)> = sqlx::query_as(
                "SELECT mode, granted FROM pg_locks \
                 WHERE pid = $1 AND locktype = 'advisory' \
                 ORDER BY mode, granted",
            )
            .bind(pid)
            .fetch_all(pool)
            .await
            .map_err(|error| format!("query purge advisory locks: {error}"))?;
            if locks == vec![("ExclusiveLock".to_string(), false)] {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "purge never appeared as the single exclusive advisory waiter".to_string())?
}

async fn count_tenant_write_fixture_marker(
    pool: &PgPool,
    fixture: &TenantWriteScopeFixture,
    marker: &str,
) -> i64 {
    sqlx::query_scalar(&fixture.count_sql)
        .bind(marker)
        .fetch_one(pool)
        .await
        .expect("count exact scope-mode fixture marker")
}

async fn seed_release_policy_rows(
    pool: &PgPool,
    tenant_id: Uuid,
    count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH policies AS (
            SELECT
                gen_random_uuid() AS policy_uid,
                $1::TEXT AS storage_partition_id,
                format('purge-boundary-%s-%s', $1::TEXT, ordinal) AS name,
                ordinal AS revision,
                '[{"id":"target_completed","version":"v1"}]'::JSONB
                    AS blocking_assertions,
                '[{"metric":"target_completed"}]'::JSONB AS primary_gate_family,
                3600::BIGINT AS attestation_ttl_secs,
                digest(format('resource-%s-%s', $1::TEXT, ordinal), 'sha256')
                    AS resource_policy_hash
            FROM generate_series(1, $2::INT) AS ordinal
        )
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash, valid_to
        )
        SELECT
            policy_uid, storage_partition_id, NULL, name, revision, 'skill_visibility',
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash,
            moa.artifact_release_policy_content_hash(
                name, revision, 'skill_visibility', blocking_assertions,
                primary_gate_family, attestation_ttl_secs, resource_policy_hash
            ),
            now()
        FROM policies
        "#,
    )
    .bind(tenant_id)
    .bind(count)
    .execute(pool)
    .await?;
    Ok(())
}

async fn run_one_purge_batch_from_fresh_pool(
    database_url: &str,
    tenant_id: Uuid,
    operation_id: &str,
) -> (String, String, i64) {
    let process_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect fresh process-loss purge pool");
    let outcome = sqlx::query_as(
        "SELECT batch_state, stage, affected \
         FROM moa.run_tenant_purge_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(&process_pool)
    .await
    .expect("run one bounded purge batch from a fresh pool");
    process_pool.close().await;
    outcome
}

async fn release_policy_progress(pool: &PgPool, tenant_id: Uuid) -> (String, i64, i64, i64) {
    sqlx::query_as(
        "SELECT current_stage, stage_deleted_count, total_deleted_count, batch_count \
         FROM moa.tenant_purge_operations WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("load release-policy purge progress")
}

async fn release_policy_counts(
    pool: &PgPool,
    tenant_id: Uuid,
    neighbour_tenant: Uuid,
) -> (i64, i64) {
    sqlx::query_as(
        "SELECT \
             (SELECT count(*) FROM moa.artifact_release_policy \
              WHERE storage_partition_id = $1::TEXT), \
             (SELECT count(*) FROM moa.artifact_release_policy \
              WHERE storage_partition_id = $2::TEXT)",
    )
    .bind(tenant_id)
    .bind(neighbour_tenant)
    .fetch_one(pool)
    .await
    .expect("load target and neighbour release-policy counts")
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
    // A session AND at least one event, for BOTH tenants. The event is the
    // load-bearing row: the append-only guard on `events` is a per-row BEFORE
    // DELETE trigger, so a fixture with zero events never fires it, and a purge
    // that cannot delete a real tenant's transcript passes anyway. The
    // neighbour's transcript proves two things at once — that the purge is
    // scoped, and that the maintenance escape hatch the purge opens is closed
    // again afterwards.
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

    seed_source_acl_families(pool, tenant_id, storage_partition_id).await;
    let neighbour_partition = StoragePartitionId::for_tenant(TenantId::from(NEIGHBOUR_TENANT));
    seed_source_acl_families(pool, NEIGHBOUR_TENANT, &neighbour_partition).await;
    seed_connector_families(pool, tenant_id).await;
    seed_connector_families(pool, NEIGHBOUR_TENANT).await;
    // Behavior Lab score provenance for BOTH tenants. The purged tenant's rows
    // prove the explicit delete runs; the neighbour's prove it is scoped. Without
    // the neighbour, a step that lost its `WHERE storage_partition_id = $1` would
    // still leave the purged-tenant residue map empty and pass.
    seed_experiment_score_provenance(pool, storage_partition_id).await;
    seed_experiment_score_provenance(pool, &neighbour_partition).await;

    GlobalFixtureIds {
        oauth_client_id,
        root_generation,
    }
}

/// Seeds the generic connector parent, binding, and invocation with no cascade
/// between invocation and binding, so the purge must honor their catalog order.
async fn seed_connector_families(pool: &PgPool, tenant_id: Uuid) {
    let connection_uid = Uuid::new_v4();
    let binding_uid = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO moa.connector_connections (
            connection_uid, tenant_id, display_name, built_in_key, built_in_version,
            lifecycle_status, health_status
        )
        VALUES ($1, $2, 'purge connector', 'knowledge:nango', 1, 'active', 'ready')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id)
    .execute(pool)
    .await
    .expect("seed connector connection");
    sqlx::query(
        r#"
        INSERT INTO moa.connector_action_bindings (
            binding_uid, tenant_id, connection_uid, action_id, connection_generation,
            compiled_contract, contract_hash, governed_contract_revision, minimum_effect
        )
        VALUES (
            $1, $2, $3, 'read', 1, '{}'::JSONB, repeat('a', 64), 'runtime-v1', 'allow'
        )
        "#,
    )
    .bind(binding_uid)
    .bind(tenant_id)
    .bind(connection_uid)
    .execute(pool)
    .await
    .expect("seed connector action binding");
    sqlx::query(
        r#"
        INSERT INTO moa.connector_action_invocations (
            invocation_uid, tenant_id, connection_uid, binding_uid,
            connection_generation, tool_call_id, request_hash
        )
        VALUES ($1, $2, $3, $4, 1, $5, repeat('b', 64))
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(connection_uid)
    .bind(binding_uid)
    .bind(format!("purge-call-{tenant_id}"))
    .execute(pool)
    .await
    .expect("seed connector action invocation");
}

/// Seeds one session and one event for `tenant_id` through the production path.
///
/// The event exists so the append-only `events` guard is reachable at all: it is
/// a per-row `BEFORE DELETE` trigger, so a tenant with no events never fires it
/// and every assertion about deleting a transcript is vacuous.
///
/// Driven through `SessionStore` rather than raw INSERTs because a session is not
/// one row. It needs creator attribution and a committed `session_agent_context`
/// (a deferred constraint trigger refuses it otherwise), and reproducing that
/// chain by hand is how a fixture drifts from the shape production writes.
async fn seed_session_transcript(
    test_db: &moa_test_support::postgres::TestDb,
    tenant_id: Uuid,
) -> Uuid {
    let session_id = moa_core::traits::SessionStore::create_session(
        test_db.store(),
        moa_core::types::session::SessionMeta {
            tenant_id: TenantId::from(tenant_id),
            created_by: Some(moa_core::types::contact::SessionActorRef::Identity {
                id: Uuid::from_u128(1),
            }),
            model: moa_core::types::identifiers::ModelId::new("test-model"),
            agent_context: Some(moa_core::types::agent::AgentContext::system_default()),
            ..moa_core::types::session::SessionMeta::default()
        },
    )
    .await
    .expect("seed the session whose transcript the purge must delete");
    test_db
        .store()
        .append_events(
            session_id,
            vec![moa_session::EventAppend {
                event: moa_core::events::Event::UserMessage {
                    text: "purge fixture transcript".to_string(),
                    attachments: Vec::new(),
                },
                dedupe_key: None,
            }],
        )
        .await
        .expect("seed the event that makes the append-only guard reachable");

    // Both purge-relevant per-session/per-partition families, for EVERY tenant this
    // helper is called with. Seeding the neighbour too is what makes the deletes
    // falsifiable: with only the purged tenant, an `AND FALSE` on either predicate
    // is indistinguishable from deleting the right rows, because deleting
    // everything and deleting exactly the target look identical when there is only
    // one target.
    let partition = StoragePartitionId::for_tenant(TenantId::from(tenant_id));
    sqlx::query(
        r#"
        INSERT INTO analytics.lineage_journal
            (journal_id, storage_partition_id, event_class, payload, accepted_at, available_at)
        VALUES ($1, $2, 'lineage', '{}'::JSONB, NOW(), NOW())
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(partition.as_str())
    .execute(test_db.store().pool())
    .await
    .expect("seed pending lineage a purge must drain");
    // Archived transcript for the same session. Its FK to `sessions` is
    // ON DELETE RESTRICT, so this row also proves the purge deletes archives
    // BEFORE the sessions they reference rather than relying on a cascade.
    sqlx::query(
        r#"
        INSERT INTO session_event_archives
            (session_id, tenant_id, format_version, event_count, first_sequence_num,
             last_sequence_num, payload, content_digest, archived_at)
        VALUES ($1, $2, 1, 1, 0, 0, '\x00'::BYTEA, repeat('a', 32)::BYTEA, NOW())
        "#,
    )
    .bind(session_id.0)
    .bind(tenant_id)
    .execute(test_db.store().pool())
    .await
    .expect("seed an archived transcript the purge must remove before its session");
    session_id.0
}

/// Tenant whose rows must outlive a different tenant's purge.
///
/// Fixed rather than random so a leaked row is identifiable in a failure dump,
/// and distinct from every generated fixture tenant.
const NEIGHBOUR_TENANT: Uuid = Uuid::from_u128(0x5EED_0000_0000_0000_0000_0000_0000_0001);

/// Seeds every source-ACL table so the exact-residue assertion proves they are
/// purged rather than merely registered.
///
/// Registration alone only satisfies `assert_catalog_coverage`; it says nothing
/// about whether the DELETE actually reached any rows. Snapshots and entries
/// would cascade from the connection and object, but the bindings, the tenant
/// epoch, and the fingerprint key have no cascade at all — leaving them behind
/// would keep a purged tenant's keyed principal material recoverable.
async fn seed_source_acl_families(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &StoragePartitionId,
) {
    let partition = storage_partition_id.to_string();
    let connection_uid = Uuid::new_v4();
    let object_uid = Uuid::new_v4();
    let snapshot_uid = Uuid::new_v4();
    // Two opaque fingerprints in the stored 34-byte shape: two key-version bytes
    // followed by a 32-byte digest.
    let member = [vec![0_u8, 1_u8], vec![0xA1_u8; 32]].concat();
    let group = [vec![0_u8, 1_u8], vec![0xB2_u8; 32]].concat();

    sqlx::query(
        r#"
        INSERT INTO moa.connector_connections
            (connection_uid, tenant_id, display_name, built_in_key, built_in_version,
             non_secret_config, lifecycle_status, health_status)
        VALUES ($1, $2, 'google-drive', 'knowledge:nango', 1,
                jsonb_build_object(
                    'provider_config_key', 'purge-config',
                    'provider_connection_id', 'purge-account',
                    'connector', 'google-drive'
                ),
                'active', 'ready')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id)
    .execute(pool)
    .await
    .expect("seed source-ACL connector parent");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections
            (connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
             provider_connection_id, connector)
        VALUES ($1, $2, $3, 'nango', 'purge-config', 'purge-account', 'google-drive')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id)
    .bind(&partition)
    .execute(pool)
    .await
    .expect("seed source-ACL connection");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects
            (object_uid, tenant_id, storage_partition_id, connection_id, object_type,
             external_object_id, status, acl_state)
        VALUES ($1, $2, $3, $4, 'document', 'purge-doc', 'active', 'incomplete')
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(connection_uid)
    .execute(pool)
    .await
    .expect("seed source-ACL object");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_acl_snapshots
            (snapshot_uid, tenant_id, storage_partition_id, connection_id, object_id,
             provider_revision, snapshot_hash, complete, entry_count, captured_at)
        VALUES ($1, $2, $3, $4, $5, 'rev-1', 'hash-1', TRUE, 1, now())
        "#,
    )
    .bind(snapshot_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(connection_uid)
    .bind(object_uid)
    .execute(pool)
    .await
    .expect("seed source-ACL snapshot");

    sqlx::query(
        r#"
        UPDATE moa.knowledge_objects
        SET acl_state = 'current',
            acl_revision = 'rev-1',
            current_acl_snapshot_id = $2
        WHERE object_uid = $1
        "#,
    )
    .bind(object_uid)
    .bind(snapshot_uid)
    .execute(pool)
    .await
    .expect("make the seeded ACL snapshot current");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_acl_entries
            (entry_uid, tenant_id, storage_partition_id, snapshot_id, entry_kind,
             principal_kind, principal_fingerprint, fingerprint_key_version)
        VALUES ($1, $2, $3, $4, 'allow', 'user', $5, 1)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&partition)
    .bind(snapshot_uid)
    .bind(member.as_slice())
    .execute(pool)
    .await
    .expect("seed source-ACL entry");

    // Deliberately connection-less: these are the rows the connection cascade
    // does NOT reach, so only an explicit purge step removes them.
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_principal_bindings
            (binding_uid, tenant_id, storage_partition_id, contact_id, principal_kind,
             principal_fingerprint, fingerprint_key_version, verified_at)
        VALUES ($1, $2, $3, $4, 'user', $5, 1, now())
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&partition)
    .bind(Uuid::new_v4())
    .bind(member.as_slice())
    .execute(pool)
    .await
    .expect("seed source-ACL principal binding");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_principal_group_bindings
            (binding_uid, tenant_id, storage_partition_id, member_fingerprint, group_kind,
             group_fingerprint, fingerprint_key_version, verified_at)
        VALUES ($1, $2, $3, $4, 'group', $5, 1, now())
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(&partition)
    .bind(member.as_slice())
    .bind(group.as_slice())
    .execute(pool)
    .await
    .expect("seed source-ACL group binding");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_acl_keys
            (tenant_id, key_version, key_handle, wrapped_key)
        VALUES ($1, 1, 'purge-key-handle', $2)
        ON CONFLICT (tenant_id, key_version) DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(vec![4_u8, 5, 6])
    .execute(pool)
    .await
    .expect("seed source-ACL fingerprint key");

    // The epoch row is written by the triggers above, not by hand: if a trigger
    // stopped firing, this assertion catches it here rather than letting the
    // purge test pass on a table that was silently always empty.
    let epoch: i64 = sqlx::query_scalar(
        "SELECT epoch FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("ACL writes must have created a tenant epoch row");
    assert!(epoch > 0, "ACL writes must have bumped the tenant epoch");
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

fn hex64(character: char) -> String {
    std::iter::repeat_n(character, 64).collect()
}

/// Seeds one experiment run, trial, score run, and provenance row for a tenant.
///
/// The provenance row is the point; the rest exist because V000041's composite
/// foreign keys refuse a provenance row whose trial, run, pinned plan revision,
/// and storage partition do not line up exactly.
async fn seed_experiment_score_provenance(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
) {
    let partition = storage_partition_id.to_string();
    let score_run_id = Uuid::new_v4();
    let run_uid = Uuid::new_v4();
    let trial_uid = Uuid::new_v4();
    let plan_revision_uid = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
         VALUES ($1, $2, NULL, 'experiment_trial')",
    )
    .bind(score_run_id)
    .bind(&partition)
    .execute(pool)
    .await
    .expect("seed experiment score run");

    sqlx::query(
        r#"INSERT INTO moa.experiment_run (
             run_uid, storage_partition_id, user_id, name, target_kind, status, target, variant,
             scorecard, score_run_id, artifact_revision_uids, created_by_identity,
             plan_artifact_uid, resource_envelope, simulator_policy
         ) VALUES ($1, $2, NULL, 'purge experiment', 'agent_loop', 'completed', '{}'::jsonb,
                   '{}'::jsonb, '{}'::jsonb, $3, '{}', '{}'::jsonb,
                   '00000000-0000-4000-8000-0000000d74f0',
                   '{"version": 1,
                     "run_limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
                     "trial_limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
                     "deadline_at": "1970-01-01T00:00:00Z"}'::jsonb,
                   '{}'::jsonb)"#,
    )
    .bind(run_uid)
    .bind(&partition)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("seed experiment run");

    sqlx::query(
        r#"INSERT INTO moa.experiment_trial (
             trial_uid, run_uid, storage_partition_id, user_id, trial_key, status, target_kind,
             variant_key, plan_revision_uid, simulator, simulator_model, score_run_id,
             resource_envelope
         ) VALUES ($1, $2, $3, NULL, 'purge/0', 'completed', 'agent_loop', 'baseline', $4,
                   '{}'::jsonb, 'sim-model', $5,
                   '{"version": 1,
                     "limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
                     "deadline": "1970-01-01T00:00:00Z"}'::jsonb)"#,
    )
    .bind(trial_uid)
    .bind(run_uid)
    .bind(&partition)
    .bind(plan_revision_uid)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("seed experiment trial");

    // Seeded so the purge catalog entry for the reservation ledger is exercised
    // rather than merely present: without a row here, deleting that registration
    // would change nothing observable.
    sqlx::query(
        r#"INSERT INTO moa.experiment_resource_reservation (
             reservation_uid, run_uid, trial_uid, storage_partition_id, user_id,
             reservation_key, component, state, reserved
         ) VALUES ($1, $2, $3, $4, NULL, 'purge/reservation/0', 'target', 'open',
                   '{"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0}'::jsonb)"#,
    )
    .bind(Uuid::new_v4())
    .bind(run_uid)
    .bind(trial_uid)
    .bind(&partition)
    .execute(pool)
    .await
    .expect("seed experiment resource reservation");

    let score_id = Uuid::new_v4();
    let score_ts = chrono::Utc::now();
    let target_session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO analytics.scores (
             score_id, ts, storage_partition_id, target_kind, session_id, run_id, name,
             value_type, value_boolean, source, model_or_evaluator
         ) VALUES ($1, $2, $3, 'session', $4, $5, 'target_completed', 'boolean',
                   TRUE, 'product_evaluator', 'target_completed@v1')",
    )
    .bind(score_id)
    .bind(score_ts)
    .bind(&partition)
    .bind(target_session_id)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("seed experiment score");

    sqlx::query(
        "INSERT INTO moa.experiment_score_provenance (
             score_id, score_ts, storage_partition_id, user_id, score_run_id, experiment_run_uid,
             plan_revision_uid, trial_uid, target_session_id, target_execution_run_uid,
             evaluator_id, evaluator_version, score_name, value_type, evidence_ref, evidence_hash
         ) VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, NULL, 'target_completed', 'v1',
                   'target_completed', 'boolean', 'session:purge#seq=1', $9)",
    )
    .bind(score_id)
    .bind(score_ts)
    .bind(&partition)
    .bind(score_run_id)
    .bind(run_uid)
    .bind(plan_revision_uid)
    .bind(trial_uid)
    .bind(target_session_id)
    .bind(vec![1_u8; 32])
    .execute(pool)
    .await
    .expect("seed experiment score provenance");
}
