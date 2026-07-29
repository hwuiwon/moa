use std::collections::BTreeMap;

use moa_authz::{FgaClient, FgaConfig};
use moa_core::types::credentials::CredentialRef;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_hands::core::{
    PostgresTenantMcpConnectionBindings, TenantMcpBindingStatus, TenantMcpConnectionBinding,
    TenantMcpConnectionBindingStore,
};
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
    // Transcripts for BOTH tenants: the purged tenant's proves the append-only
    // guard is actually crossed, the neighbour's proves the maintenance escape
    // hatch the purge opens is closed again when its transaction ends.
    let purged_session_id = seed_session_transcript(&test_db, tenant_id).await;
    seed_session_transcript(&test_db, NEIGHBOUR_TENANT).await;
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
    // Two, not one: the purge holds its destruction stage guard on one connection
    // while its transaction runs on another, so a single-connection pool deadlocks
    // waiting for itself.
    const PURGE_POOL_CONNECTIONS: u32 = 2;
    let purge_pool = PgPoolOptions::new()
        .max_connections(PURGE_POOL_CONNECTIONS)
        .connect(test_db.database_url())
        .await
        .expect("open a bounded pool for the purge");
    let first = purge_relational(&purge_pool, &offline_fga(), tenant_id, &operation_id)
        .await
        .expect("registered tenant families should purge");
    let replay = purge_relational(&purge_pool, &offline_fga(), tenant_id, &operation_id)
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

    // The neighbour tenant's rebuild state is untouched. This is the failure the
    // purged-tenant residue map structurally cannot catch: a step that dropped
    // its `WHERE tenant_id = $1` would leave that map empty and still have
    // destroyed every other tenant's rebuild.
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

    let neighbour_rebuild: (i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM moa.knowledge_rebuild_operation WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_rebuild_generation WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_active_generation WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_rebuild_candidate_vector WHERE tenant_id = $1),
            (SELECT count(*) FROM moa.knowledge_rechunk_staging WHERE tenant_id = $1)
        "#,
    )
    .bind(NEIGHBOUR_TENANT)
    .fetch_one(pool)
    .await
    .expect("load neighbour tenant rebuild state");
    assert_eq!(
        neighbour_rebuild,
        (1, 1, 1, 1, 1),
        "another tenant's index-rebuild state must survive this tenant's purge"
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
async fn tenant_purge_admits_mcp_bindings_and_drains_them_in_the_credential_stage_db_memory() {
    // Pins the two halves of the MCP connection binding's purge contract. The
    // binding table is tenant-owned, so the catalog guard would abort every
    // purge if it were unregistered; it is registered, so the relational stage
    // completes. It is also forced-RLS, so that stage's role cannot see the row
    // and deliberately leaves it behind — the bounded `moa_app`-scoped sweep the
    // credential stage runs is what actually removes it, and only for the
    // purged tenant.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap mcp-binding purge db");
    let pool = test_db.store().pool();
    let purged = Uuid::new_v4();
    let retained = Uuid::new_v4();
    let operation_id = format!("tenant-purge-{purged}");
    seed_tenant(pool, purged).await;
    seed_tenant(pool, retained).await;

    let bindings = PostgresTenantMcpConnectionBindings::new(pool.clone());
    for tenant_id in [purged, retained] {
        bindings
            .upsert_binding(&TenantMcpConnectionBinding {
                tenant_id: TenantId::from(tenant_id),
                connection_uid: Uuid::new_v4(),
                server_name: "tenant-search".to_string(),
                credential_ref: CredentialRef::from_uuid(Uuid::new_v4()),
                status: TenantMcpBindingStatus::Active,
                allowed_operations: vec!["search_documents".to_string()],
            })
            .await
            .expect("seed tenant MCP binding");
    }
    start_destruction(
        pool,
        TenantId::from(purged),
        &[],
        &operation_id,
        "tenant.purge",
    )
    .await
    .expect("start mcp-binding destruction fence");

    purge_relational(pool, &offline_fga(), purged, &operation_id)
        .await
        .expect("the registered binding table must not abort the relational stage");

    assert!(
        bindings
            .binding_for_server(TenantId::from(purged), "tenant-search")
            .await
            .expect("read the binding after the relational stage")
            .is_some(),
        "the relational transaction's role cannot see the forced-RLS row, so it must survive that stage"
    );

    let mut removed_total = 0_u64;
    loop {
        let removed = bindings
            .purge_tenant_bindings(TenantId::from(purged), 2)
            .await
            .expect("bounded credential-stage sweep");
        if removed == 0 {
            break;
        }
        removed_total += removed;
    }

    assert_eq!(removed_total, 1);
    assert!(
        bindings
            .binding_for_server(TenantId::from(purged), "tenant-search")
            .await
            .expect("read the purged tenant's binding")
            .is_none()
    );
    assert!(
        bindings
            .binding_for_server(TenantId::from(retained), "tenant-search")
            .await
            .expect("read the retained tenant's binding")
            .is_some(),
        "another tenant's binding must survive the purge"
    );
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
    seed_index_rebuild_families(pool, tenant_id, storage_partition_id).await;
    // The same two families for a tenant the purge must not touch. Both are
    // seeded for the neighbour because both now depend on explicit deletes
    // rather than cascades, which is exactly the shape where a lost
    // `WHERE tenant_id = $1` destroys every tenant at once.
    let neighbour_partition = StoragePartitionId::for_tenant(TenantId::from(NEIGHBOUR_TENANT));
    seed_source_acl_families(pool, NEIGHBOUR_TENANT, &neighbour_partition).await;
    // A neighbour tenant's rebuild state, seeded so the purge has something it
    // must NOT touch. Every step is `WHERE tenant_id = $1`; a step that lost
    // that predicate would destroy every tenant's rebuild state at once, and
    // the purged-tenant residue check cannot see that because it only counts
    // rows belonging to the tenant being purged.
    seed_index_rebuild_families(pool, NEIGHBOUR_TENANT, &neighbour_partition).await;
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

/// Seeds every V000351 index-rebuild table so the exact-residue assertion proves
/// they are purged rather than merely registered.
///
/// Registration alone only satisfies `assert_catalog_coverage`. A `DELETE`
/// against a forced-RLS table the purge role has no policy for removes zero
/// rows and raises no error, and the residue `SELECT count(*)` that follows it
/// is filtered by the same policy — so both read zero and the step looks
/// covered while the rows survive. Seeding is the only thing that distinguishes
/// "deleted" from "invisible".
///
/// Ordered innermost-first, matching the purge steps: staging and candidate
/// vectors cascade from their generation, the active-generation pointer
/// references generations, and the operation's `candidate_generation_uid` is
/// `ON DELETE SET NULL`.
async fn seed_index_rebuild_families(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &StoragePartitionId,
) {
    let partition = storage_partition_id.to_string();
    let operation_uid = Uuid::new_v4();
    let generation_uid = Uuid::new_v4();
    let node_uid = Uuid::new_v4();

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rebuild_operation
            (operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token,
             vectors_total, vectors_rebuilt, estimated_input_tokens, estimated_cost_micros)
        VALUES ($1, $2, $3, 'reembed', 'activated', $4, 1, 1, 128, 12)
        "#,
    )
    .bind(operation_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("seed index-rebuild operation");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rebuild_generation
            (generation_uid, tenant_id, storage_partition_id, generation_seq, operation_uid,
             embedding_model, embedding_model_version, embedding_dimension,
             turbopuffer_namespace, state, complete, vector_count)
        VALUES ($1, $2, $3, 1, $4, 'embed-v4.0', 1, 1024, $5, 'active', TRUE, 1)
        "#,
    )
    .bind(generation_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(operation_uid)
    .bind(format!("moa-purge-{partition}__g1"))
    .execute(pool)
    .await
    .expect("seed index-rebuild generation");

    sqlx::query(
        r#"
        UPDATE moa.knowledge_rebuild_operation
           SET candidate_generation_uid = $2
         WHERE operation_uid = $1
        "#,
    )
    .bind(operation_uid)
    .bind(generation_uid)
    .execute(pool)
    .await
    .expect("point the seeded operation at its generation");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_active_generation
            (storage_partition_id, tenant_id, generation_uid)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(&partition)
    .bind(tenant_id)
    .bind(generation_uid)
    .execute(pool)
    .await
    .expect("seed active-generation pointer");

    // A candidate vector is real tenant embedding material. Leaving one behind
    // would keep a purged tenant's content recoverable from a table nothing
    // else references.
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rebuild_candidate_vector
            (generation_uid, uid, tenant_id, storage_partition_id, label, pii_class,
             embedding, input_digest, input_token_estimate)
        VALUES ($1, $2, $3, $4, 'Fact', 'none', $5::public.halfvec(1024), $6, 32)
        "#,
    )
    .bind(generation_uid)
    .bind(node_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(format!("[{}]", vec!["0.01"; 1024].join(",")))
    .bind(vec![0x5A_u8; 32])
    .execute(pool)
    .await
    .expect("seed index-rebuild candidate vector");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rechunk_staging
            (staging_uid, generation_uid, tenant_id, storage_partition_id,
             document_version_uid, member, payload)
        VALUES ($1, $2, $3, $4, $5, 'chunk', '{"chunks": []}'::JSONB)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(generation_uid)
    .bind(tenant_id)
    .bind(&partition)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .expect("seed rechunk staging row");
}

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
        INSERT INTO moa.knowledge_connections
            (connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
             provider_connection_id, connector, credential_ref, status, acl_mode)
        VALUES ($1, $2, $3, 'nango', 'purge-config', 'purge-account', 'google-drive',
                'vault://purge', 'active', 'provider_managed')
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
             provider_revision, snapshot_hash, provenance, complete, entry_count, captured_at)
        VALUES ($1, $2, $3, $4, $5, 'rev-1', 'hash-1', 'provider_listing', TRUE, 1, now())
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

/// Seeds one experiment run, trial, score run, and provenance row for a tenant.
///
/// The provenance row is the point; the rest exist because V000361's composite
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
        "INSERT INTO moa.experiment_run (
             run_uid, storage_partition_id, user_id, name, target_kind, status, target, variant,
             scorecard, score_run_id, artifact_revision_uids, created_by_identity
         ) VALUES ($1, $2, NULL, 'purge experiment', 'agent_loop', 'completed', '{}'::jsonb,
                   '{}'::jsonb, '{}'::jsonb, $3, '{}', '{}'::jsonb)",
    )
    .bind(run_uid)
    .bind(&partition)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("seed experiment run");

    sqlx::query(
        "INSERT INTO moa.experiment_trial (
             trial_uid, run_uid, storage_partition_id, user_id, trial_key, status, target_kind,
             variant_key, plan_revision_uid, simulator, simulator_model, score_run_id
         ) VALUES ($1, $2, $3, NULL, 'purge/0', 'completed', 'agent_loop', 'baseline', $4,
                   '{}'::jsonb, 'sim-model', $5)",
    )
    .bind(trial_uid)
    .bind(run_uid)
    .bind(&partition)
    .bind(plan_revision_uid)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("seed experiment trial");

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
