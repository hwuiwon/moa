//! DB integration coverage for contact-scoped privacy erasure.

use chrono::Utc;
use moa_core::{ContactId, RlsContext, StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    PostgresGraphStore,
};
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, delete_subject_digests, delete_subject_retrieval_lineage,
    enumerate_erase_candidates, hard_purge_erase_candidates,
};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

fn contact_node(
    tenant_id: TenantId,
    contact_id: ContactId,
    uid: Uuid,
    name: &str,
) -> NodeWriteIntent {
    let subject_user_id = contact_id.to_string();
    NodeWriteIntent {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(subject_user_id.clone()),
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({
            "name": name,
            "source": "erasure_db_memory",
            "user_id": subject_user_id,
        }),
        pii_class: PiiClass::Phi,
        confidence: Some(0.97),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
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
        valid_from: Utc::now(),
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
    // Pins: privacy erasure can delete contact-owned graph memory and append both
    // the node erase row and the contact-scoped summary changelog while running
    // as the app role under contact RLS.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
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
    assert_eq!(candidates[0].name, "contact erasure fact");
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
    assert_eq!(erase_rows, 1);

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
    assert_eq!(summary.2["erased_count"], 1);
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
    assert_eq!(erased, 2);
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
    );
    let other_graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, other_contact_id),
    );

    let active_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            active_uid,
            "target active private fact",
        ))
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
    other_graph
        .create_node(contact_node(
            tenant_id,
            other_contact_id,
            other_uid,
            "other contact private fact",
        ))
        .await
        .expect("seed other-contact node");

    let mut target_uids = vec![active_uid, invalidated_uid, superseded_uid, replacement_uid];
    target_uids.sort_unstable();
    for uid in &target_uids {
        seed_embedding(session_store.pool(), tenant_id, contact_id, *uid).await;
    }
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
    assert_eq!(target_embedding_count, 4);

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
    let expected_node_audit_counts = target_uids
        .iter()
        .copied()
        .map(|uid| (uid, 1_i64))
        .collect::<Vec<_>>();
    assert_eq!(node_audit_counts, expected_node_audit_counts);

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
            "reason": "all-version dsar erasure request",
            "subject_user_id": legacy_subject_user_id,
            "erased_count": 4,
        })]
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
