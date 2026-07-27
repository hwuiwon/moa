//! DB integration coverage for contact-scoped privacy erasure.

use std::sync::Arc;

use moa_core::types::security::SensitivityClass;
use moa_core::{
    types::contact::ContactId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::memory::{InformationBarrierId, RlsContext},
};
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, begin_app_scoped_tx, crypto_shred_erased_subject,
    delete_subject_digests, delete_subject_retrieval_lineage, enumerate_erase_candidates,
    hard_purge_erase_candidates,
};
use moa_memory_pii::legal_hold::{
    LegalHoldError, lock_tenant_and_subjects, place_hold, release_hold, start_destruction,
};
use moa_session::testing;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn test_kms() -> Arc<dyn moa_crypto::KeyManagementProvider> {
    Arc::new(moa_crypto::LocalKmsProvider::new())
}

#[tokio::test]
async fn legal_hold_and_subject_destruction_are_linearizable_across_pools_db_memory() {
    // Pins: hold-first blocks destruction, while destruction-first makes a
    // separate pool wait on the tenant lock and then refuse the hold after the
    // durable fence commits.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    let second_pool = PgPool::connect(&database_url)
        .await
        .expect("connect independent pool");

    let hold = place_hold(
        session_store.pool(),
        tenant_id,
        Some(subject_id),
        "preservation order",
        "legal-admin",
    )
    .await
    .expect("place hold first");
    let duplicate = place_hold(
        &second_pool,
        tenant_id,
        Some(subject_id),
        "duplicate preservation order",
        "other-admin",
    )
    .await
    .expect_err("only one active hold may cover one subject");
    assert!(matches!(duplicate, LegalHoldError::Sqlx(_)));
    let blocked = start_destruction(
        &second_pool,
        tenant_id,
        &[subject_id],
        "erase-hold-first",
        "privacy.erase",
    )
    .await
    .expect_err("hold-first must block destruction");
    assert!(matches!(blocked, LegalHoldError::ActiveHold));
    assert!(
        release_hold(session_store.pool(), tenant_id, hold.id, "legal-admin")
            .await
            .expect("release hold")
    );

    let mut fence_tx =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin fence transaction");
    lock_tenant_and_subjects(fence_tx.as_mut(), tenant_id.0, &[subject_id])
        .await
        .expect("take canonical destruction locks");
    let waiting_pool = second_pool.clone();
    let waiter = tokio::spawn(async move {
        place_hold(
            &waiting_pool,
            tenant_id,
            Some(subject_id),
            "too late",
            "legal-admin",
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !waiter.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "hold placement must wait for the destruction lock"
    );
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, $2, 'erase-destruction-first', 'privacy.erase')",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(fence_tx.as_mut())
    .await
    .expect("persist destruction fence while locked");
    fence_tx.commit().await.expect("commit destruction fence");
    let refused = waiter
        .await
        .expect("hold waiter joins")
        .expect_err("destruction-first must refuse later hold");
    assert!(matches!(refused, LegalHoldError::DestructionStarted));

    second_pool.close().await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_subject_destruction_blocks_waiting_tenant_purge_start_across_pools_db_memory() {
    // Pins: a tenant-wide purge start waits behind a subject destruction start
    // on another pool, then refuses admission after the subject fence commits.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    let second_pool = PgPool::connect(&database_url)
        .await
        .expect("connect independent pool");

    let mut subject_tx =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin subject destruction transaction");
    lock_tenant_and_subjects(subject_tx.as_mut(), tenant_id.0, &[subject_id])
        .await
        .expect("take canonical subject destruction locks");

    let waiting_pool = second_pool.clone();
    let tenant_purge = tokio::spawn(async move {
        start_destruction(
            &waiting_pool,
            tenant_id,
            &[],
            "purge-after-subject",
            "tenant.purge",
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !tenant_purge.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "tenant purge start must wait for the tenant destruction lock"
    );

    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, $2, 'erase-subject-first', 'privacy.erase')",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(subject_tx.as_mut())
    .await
    .expect("persist subject destruction fence while locked");
    subject_tx
        .commit()
        .await
        .expect("commit subject destruction fence");

    let conflict = tenant_purge
        .await
        .expect("tenant purge waiter joins")
        .expect_err("subject fence must block tenant-wide destruction admission");
    assert!(matches!(conflict, LegalHoldError::FenceConflict));

    second_pool.close().await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_destruction_fence_blocks_subject_graph_recreation_db_memory() {
    // Pins: the durable subject fence rejects graph writes from every replica
    // throughout resumable erasure gaps.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    start_destruction(
        session_store.pool(),
        tenant_id,
        &[contact_id.0],
        "erase-write-fence",
        "privacy.erase",
    )
    .await
    .expect("start subject destruction");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let error = graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            Uuid::now_v7(),
            "must not be recreated",
        ))
        .await
        .expect_err("fenced subject write must fail");
    assert!(
        error.to_string().contains("destruction is fenced"),
        "unexpected graph write refusal: {error}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_crypto_shred_rechecks_durable_fence_db_memory() {
    // Pins: a resumed graph stage cannot shred a KEK when its durable operation
    // fence no longer matches, even if earlier graph deletion already ran.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    start_destruction(
        session_store.pool(),
        tenant_id,
        &[subject_id],
        "erase-original",
        "privacy.erase",
    )
    .await
    .expect("start subject destruction");
    let mut conn =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin fence mutation");
    sqlx::query(
        "UPDATE moa.destruction_operation_fence SET operation_id = 'erase-other' WHERE tenant_id = $1 AND subject_id = $2",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(conn.as_mut())
    .await
    .expect("simulate mismatched resumed operation");
    conn.commit().await.expect("commit mismatched fence");

    let kms = moa_crypto::LocalKmsProvider::new();
    let error = crypto_shred_erased_subject(
        session_store.pool(),
        &kms,
        tenant_id,
        subject_id,
        "erase-original",
    )
    .await
    .expect_err("mismatched durable fence must block crypto-shred");
    assert!(
        error
            .to_string()
            .contains("durable destruction fence is missing"),
        "unexpected shred refusal: {error}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn contact_node(
    tenant_id: TenantId,
    contact_id: ContactId,
    uid: Uuid,
    name: &str,
) -> NodeWriteIntent {
    let subject_user_id = contact_id.to_string();
    NodeWriteIntent {
        barrier: None,
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(subject_user_id.clone()),
        data_subject_id: contact_id.0,
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({
            "name": name,
            "source": "erasure_db_memory",
            "user_id": subject_user_id,
        }),
        pii_class: SensitivityClass::Phi,
        confidence: Some(0.97),
        valid_from: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
}

#[tokio::test]
async fn privileged_erasure_is_barrier_independent_and_subject_bounded_db_memory() {
    // Pins: empty retrieval clearances hide the candidate from enumeration, but
    // the narrowly granted subject eraser still removes it. Missing or
    // mismatched scope GUCs fail before any row is touched.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let uid = Uuid::now_v7();
    let mut barriered = contact_node(tenant_id, contact_id, uid, "barriered private fact");
    barriered.barrier = Some(InformationBarrierId::parse("legal-matter-alpha").expect("barrier"));
    graph
        .create_node(barriered)
        .await
        .expect("seed barriered contact node");

    let hidden = enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("enumerate without barrier clearance");
    assert!(
        hidden.is_empty(),
        "retrieval enumeration must remain barrier gated"
    );

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id: subject_user_id.clone(),
        reason: "barrier-independent erasure".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-barrier-erasure".to_string(),
    };

    let mut missing_scope = session_store
        .pool()
        .begin()
        .await
        .expect("begin raw transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *missing_scope)
        .await
        .expect("set app role");
    let missing_error = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT moa.erase_memory_data_subject($1, $2, $3)",
    )
    .bind(tenant_id.0)
    .bind(contact_id.0)
    .bind(json!({"approver_id": "admin", "approval_token_jti": "missing-guc"}))
    .fetch_one(&mut *missing_scope)
    .await
    .expect_err("missing GUCs must fail");
    assert_eq!(
        missing_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("42501")
    );
    missing_scope
        .rollback()
        .await
        .expect("rollback raw transaction");

    let mut mismatched = begin_app_scoped_tx(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("begin scoped mismatch transaction");
    let mismatch_error = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT moa.erase_memory_data_subject($1, $2, $3)",
    )
    .bind(other_tenant.0)
    .bind(contact_id.0)
    .bind(json!({"approver_id": "admin", "approval_token_jti": "wrong-tenant"}))
    .fetch_one(mismatched.as_mut())
    .await
    .expect_err("cross-tenant argument must fail");
    assert_eq!(
        mismatch_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("42501")
    );
    mismatched
        .rollback()
        .await
        .expect("rollback mismatch transaction");

    assert_eq!(
        hard_purge_erase_candidates(session_store.pool(), &audit, &hidden)
            .await
            .expect("erase hidden subject rows"),
        1
    );
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count barriered node after erase");
    assert_eq!(remaining, 0);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privileged_erasure_grants_do_not_give_app_bypassrls_db_memory() {
    // Pins: the definer is fixed-path and NOLOGIN/NOBYPASSRLS; only moa_app can
    // execute the function, and moa_app itself remains NOBYPASSRLS.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let row = sqlx::query(
        r#"
        SELECT proc.prosecdef,
               proc.proconfig,
               owner.rolname AS owner_name,
               owner.rolbypassrls AS owner_bypass,
               app.rolbypassrls AS app_bypass,
               has_function_privilege(
                   'moa_app',
                   'moa.erase_memory_data_subject(uuid,uuid,jsonb)',
                   'EXECUTE'
               ) AS app_execute,
               has_function_privilege(
                   'public',
                   'moa.erase_memory_data_subject(uuid,uuid,jsonb)',
                   'EXECUTE'
               ) AS public_execute
          FROM pg_proc AS proc
          JOIN pg_roles AS owner ON owner.oid = proc.proowner
          JOIN pg_roles AS app ON app.rolname = 'moa_app'
         WHERE proc.oid = 'moa.erase_memory_data_subject(uuid,uuid,jsonb)'::regprocedure
        "#,
    )
    .fetch_one(session_store.pool())
    .await
    .expect("inspect privacy eraser function grants");
    assert!(
        row.try_get::<bool, _>("prosecdef")
            .expect("security definer")
    );
    assert_eq!(
        row.try_get::<String, _>("owner_name").expect("owner name"),
        "moa_privacy_eraser"
    );
    assert!(
        !row.try_get::<bool, _>("owner_bypass")
            .expect("owner bypass")
    );
    assert!(!row.try_get::<bool, _>("app_bypass").expect("app bypass"));
    assert!(row.try_get::<bool, _>("app_execute").expect("app execute"));
    assert!(
        !row.try_get::<bool, _>("public_execute")
            .expect("public execute")
    );
    let proconfig = row
        .try_get::<Option<Vec<String>>, _>("proconfig")
        .expect("function config")
        .unwrap_or_default();
    assert!(
        proconfig
            .iter()
            .any(|entry| entry == "search_path=pg_catalog, moa")
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn contact_edge(
    tenant_id: TenantId,
    contact_id: ContactId,
    start_uid: Uuid,
    end_uid: Uuid,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label: EdgeLabel::RelatesTo,
        start_uid,
        end_uid,
        valid_from: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        properties: json!({"source": "erasure_db_memory"}),
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(contact_id.to_string()),
        scope: "contact".to_string(),
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
}

fn embedding_literal() -> String {
    let mut values = vec!["0"; 1024];
    values[0] = "1";
    format!("[{}]", values.join(","))
}

async fn seed_embedding(pool: &PgPool, tenant_id: TenantId, contact_id: ContactId, uid: Uuid) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped embedding seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for embedding seed");
    sqlx::query(
        r#"
        INSERT INTO moa.embeddings
            (uid, storage_partition_id, user_id, label, pii_class, embedding,
             embedding_model, embedding_model_version, valid_to)
        SELECT uid, storage_partition_id, user_id, label, pii_class,
               $2::public.halfvec, 'erasure-test-model', 1, valid_to
        FROM moa.node_index
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .bind(embedding_literal())
    .execute(conn.as_mut())
    .await
    .expect("seed contact embedding row");
    conn.commit().await.expect("commit contact embedding seed");
}

#[tokio::test]
async fn hard_purge_contact_candidates_writes_summary_under_app_role_db_memory() {
    // Pins: privacy erasure deletes contact-owned graph memory through the
    // subject-bounded definer and leaves one redacted contact summary.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            uid,
            "contact erasure fact",
        ))
        .await
        .expect("seed contact graph node");

    let candidates = enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("enumerate contact erase candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].uid, uid);
    assert_eq!(candidates[0].label, "Fact");
    // PHI/restricted content is sealed at rest, so the indexed plaintext `name`
    // that erase enumeration reads is the redaction placeholder, not the secret
    // — the candidate listing (and the erase response sample) never leaks it.
    assert_eq!(candidates[0].name, "[RESTRICTED]");
    assert_eq!(candidates[0].pii_class, "phi");

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id,
        reason: "dsar erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-erasure-db-memory".to_string(),
    };
    let erased = hard_purge_erase_candidates(session_store.pool(), &audit, &candidates)
        .await
        .expect("hard purge contact candidates");
    assert_eq!(erased, 1);
    assert!(
        graph
            .get_node(uid)
            .await
            .expect("read purged graph node")
            .is_none(),
        "purged node should not remain visible"
    );

    let mut conn = ScopedConn::begin_contact(session_store.pool(), tenant_id, contact_id)
        .await
        .expect("begin contact-scoped changelog read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    let erase_rows = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_uid = $1
          AND contact_id = $2
        "#,
    )
    .bind(uid)
    .bind(contact_id.0)
    .fetch_one(conn.as_mut())
    .await
    .expect("count contact node erase rows");
    assert_eq!(erase_rows, 0, "per-node payload history must be removed");

    let summary = sqlx::query_as::<_, (String, Option<Uuid>, serde_json::Value)>(
        r#"
        SELECT scope, contact_id, payload
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'contact'
          AND target_uid = $1
        "#,
    )
    .bind(contact_id.0)
    .fetch_one(conn.as_mut())
    .await
    .expect("read contact erasure summary row");
    assert_eq!(summary.0, "contact");
    assert_eq!(summary.1, Some(contact_id.0));
    assert_eq!(summary.2["redacted"], true);
    assert_eq!(summary.2["nodes_deleted"], 1);
    conn.commit().await.expect("commit changelog read");

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn hard_purge_tolerates_absent_candidate_db_memory() {
    // Pins: an already-absent candidate counts as completed progress rather than a
    // terminal NotFound error, so a resumed erasure that re-enumerates a partially
    // purged subject never strands on the first missing node.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let present_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            present_uid,
            "present fact",
        ))
        .await
        .expect("seed present contact node");

    let mut candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
            .await
            .expect("enumerate contact erase candidates");
    assert_eq!(candidates.len(), 1);
    // Prepend a candidate whose node is already gone (concurrent purge or resume).
    candidates.insert(
        0,
        EraseCandidate {
            uid: Uuid::now_v7(),
            label: "Fact".to_string(),
            name: "already purged".to_string(),
            pii_class: "phi".to_string(),
        },
    );

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id,
        reason: "resumed erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-absent-candidate-db-memory".to_string(),
    };
    let erased = hard_purge_erase_candidates(session_store.pool(), &audit, &candidates)
        .await
        .expect("hard purge tolerates already-absent candidate");
    assert_eq!(erased, 1);
    assert!(
        graph
            .get_node(present_uid)
            .await
            .expect("read purged present node")
            .is_none(),
        "the present candidate must still be purged"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn delete_subject_digest_and_lineage_rows_db_memory() {
    // Pins: erasure closure deletes the subject's standing memory-digest and
    // retrieval-lineage rows, which graph-node purges never touch.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();

    seed_digest_row(
        session_store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
    )
    .await;
    seed_lineage_row(
        session_store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
    )
    .await;

    let digests_deleted = delete_subject_digests(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("delete subject digests");
    assert_eq!(digests_deleted, 1);
    let lineage_deleted =
        delete_subject_retrieval_lineage(session_store.pool(), tenant_id, &subject_user_id)
            .await
            .expect("delete subject retrieval lineage");
    assert_eq!(lineage_deleted, 1);

    let remaining_digests = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.memory_digests WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_one(session_store.pool())
    .await
    .expect("count remaining digest rows");
    assert_eq!(remaining_digests, 0);
    let remaining_lineage = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.retrieval_lineage WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_one(session_store.pool())
    .await
    .expect("count remaining lineage rows");
    assert_eq!(remaining_lineage, 0);

    // A re-run is idempotent: nothing remains to delete.
    let digests_deleted_again =
        delete_subject_digests(session_store.pool(), tenant_id, &subject_user_id)
            .await
            .expect("re-run digest deletion");
    assert_eq!(digests_deleted_again, 0);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn seed_digest_row(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    storage_partition_id: &str,
) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped digest seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for digest seed");
    sqlx::query(
        r#"
        INSERT INTO moa.memory_digests
            (storage_partition_id, user_id, content, version, updated_at)
        VALUES ($1, $2, $3, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.to_string())
    .bind("What I know about this contact:\n- prefers dark mode\n")
    .execute(conn.as_mut())
    .await
    .expect("seed memory digest row");
    conn.commit().await.expect("commit digest seed");
}

async fn seed_lineage_row(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    storage_partition_id: &str,
) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped lineage seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for lineage seed");
    sqlx::query(
        r#"
        INSERT INTO moa.retrieval_lineage
            (storage_partition_id, user_id, session_id, turn_seq, uid, rank, retrieved_at)
        VALUES ($1, $2, $3, 1, $4, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.to_string())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(conn.as_mut())
    .await
    .expect("seed retrieval lineage row");
    conn.commit().await.expect("commit lineage seed");
}

#[tokio::test]
async fn hard_purge_contact_candidates_includes_historical_versions_db_memory() {
    // Pins: a contact hard purge enumerates and erases every attributable node version,
    // including invalidated and superseded history, plus incident graph/vector rows and
    // exact audit records, without touching another contact in the same tenant.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let canonical_subject_user_id = contact_id.to_string();
    let legacy_subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let other_graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, other_contact_id),
        test_kms(),
    );

    let active_uid = Uuid::now_v7();
    let mut active_node = contact_node(
        tenant_id,
        contact_id,
        active_uid,
        "target active private fact",
    );
    active_node.pii_class = SensitivityClass::None;
    graph
        .create_node(active_node)
        .await
        .expect("seed active target node");

    let invalidated_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            invalidated_uid,
            "target invalidated private fact",
        ))
        .await
        .expect("seed target node to invalidate");
    graph
        .create_edge(contact_edge(
            tenant_id,
            contact_id,
            invalidated_uid,
            active_uid,
        ))
        .await
        .expect("seed edge incident to invalidated target node");
    graph
        .invalidate_node(invalidated_uid, "historical erasure regression")
        .await
        .expect("invalidate target node");

    let superseded_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            superseded_uid,
            "target superseded private fact",
        ))
        .await
        .expect("seed target node to supersede");
    let replacement_uid = Uuid::now_v7();
    let written_replacement_uid = graph
        .supersede_node(
            superseded_uid,
            contact_node(
                tenant_id,
                contact_id,
                replacement_uid,
                "target replacement private fact",
            ),
        )
        .await
        .expect("supersede target node");
    assert_eq!(written_replacement_uid, replacement_uid);
    graph
        .create_edge(contact_edge(
            tenant_id,
            contact_id,
            active_uid,
            replacement_uid,
        ))
        .await
        .expect("seed edge incident to active target nodes");

    let other_uid = Uuid::now_v7();
    let mut other_node = contact_node(
        tenant_id,
        other_contact_id,
        other_uid,
        "other contact private fact",
    );
    other_node.pii_class = SensitivityClass::None;
    other_graph
        .create_node(other_node)
        .await
        .expect("seed other-contact node");

    let mut target_uids = vec![active_uid, invalidated_uid, superseded_uid, replacement_uid];
    target_uids.sort_unstable();
    seed_embedding(session_store.pool(), tenant_id, contact_id, active_uid).await;
    seed_embedding(session_store.pool(), tenant_id, other_contact_id, other_uid).await;

    let canonical_candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &canonical_subject_user_id)
            .await
            .expect("enumerate canonical contact erase candidates");
    let canonical_candidate_uids = canonical_candidates
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    assert_eq!(canonical_candidate_uids, target_uids);

    let legacy_candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &legacy_subject_user_id)
            .await
            .expect("enumerate legacy contact erase candidates");
    let legacy_candidate_uids = legacy_candidates
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    assert_eq!(legacy_candidate_uids, target_uids);
    assert!(!legacy_candidate_uids.contains(&other_uid));

    let target_node_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count seeded target nodes");
    assert_eq!(target_node_count, 4);
    let target_incident_edge_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.edge_index WHERE start_uid = ANY($1) OR end_uid = ANY($1)",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count seeded target incident edges");
    assert_eq!(target_incident_edge_count, 3);
    let target_embedding_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count seeded target embeddings");
    assert_eq!(target_embedding_count, 1);

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id: legacy_subject_user_id.clone(),
        reason: "all-version dsar erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-all-version-erasure-db-memory".to_string(),
    };
    let erased = hard_purge_erase_candidates(session_store.pool(), &audit, &legacy_candidates)
        .await
        .expect("hard purge every target contact version");
    assert_eq!(erased, 4);

    let remaining_target_nodes =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count target nodes after hard purge");
    assert_eq!(remaining_target_nodes, 0);
    let remaining_target_edges = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.edge_index WHERE start_uid = ANY($1) OR end_uid = ANY($1)",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count target incident edges after hard purge");
    assert_eq!(remaining_target_edges, 0);
    let remaining_target_embeddings =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count target embeddings after hard purge");
    assert_eq!(remaining_target_embeddings, 0);
    let queued_external_deletes = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.vector_sync_outbox WHERE uid = ANY($1) AND op = 'delete'",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count transactional external-vector deletes");
    assert_eq!(queued_external_deletes, 4);

    let other_node_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = $1")
            .bind(other_uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count preserved other-contact node");
    assert_eq!(other_node_count, 1);
    let other_embedding_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = $1")
            .bind(other_uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count preserved other-contact embedding");
    assert_eq!(other_embedding_count, 1);

    let node_audit_counts = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT target_uid, COUNT(*)
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'node'
          AND target_uid = ANY($1)
        GROUP BY target_uid
        ORDER BY target_uid
        "#,
    )
    .bind(&target_uids)
    .fetch_all(session_store.pool())
    .await
    .expect("read per-node erase audit counts");
    assert!(
        node_audit_counts.is_empty(),
        "subject payload history must be removed"
    );

    let summary_payloads = sqlx::query_scalar::<_, serde_json::Value>(
        r#"
        SELECT payload
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'contact'
          AND target_uid = $1
        ORDER BY change_id
        "#,
    )
    .bind(contact_id.0)
    .fetch_all(session_store.pool())
    .await
    .expect("read contact erase summary audit rows");
    assert_eq!(
        summary_payloads,
        vec![json!({
            "redacted": true,
            "nodes_deleted": 4,
            "edges_deleted": 3,
            "embeddings_deleted": 1,
        })]
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
