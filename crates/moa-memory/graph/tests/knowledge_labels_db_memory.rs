//! DB integration coverage for tenant-knowledge graph labels.

use chrono::{DateTime, Utc};
use moa_core::TenantId;
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_types::ScopeContext;
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(storage_partition_id: impl AsRef<str>) -> ScopeContext {
    let storage_partition_id = storage_partition_id.as_ref();
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    ScopeContext::tenant(tenant_id)
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.as_bytes().iter().copied().enumerate() {
        let slot = index % 16;
        bytes[slot] = bytes[slot]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(index as u8);
        let mirror = (index * 7 + 3) % 16;
        bytes[mirror] ^= byte.rotate_left((index % 8) as u32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}

fn node_intent(
    storage_partition_id: &str,
    uid: Uuid,
    label: NodeLabel,
    name: &str,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid,
        label,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "knowledge_labels_db_memory" }),
        pii_class: PiiClass::None,
        confidence: Some(0.98),
        valid_from: utc("2026-06-26T00:00:00Z"),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

fn edge_intent(
    storage_partition_id: &str,
    label: EdgeLabel,
    start_uid: Uuid,
    end_uid: Uuid,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        start_uid,
        end_uid,
        properties: json!({ "source": "knowledge_labels_db_memory" }),
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn rls_policy_names(pool: &PgPool, label: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT policyname
        FROM pg_policies
        WHERE schemaname = 'moa_graph'
          AND tablename = $1
        ORDER BY policyname
        "#,
    )
    .bind(label)
    .fetch_all(pool)
    .await
    .expect("read AGE label RLS policies")
}

#[tokio::test]
async fn knowledge_graph_labels_create_read_and_delete() {
    // Pins: tenant-knowledge labels route through real AGE tables, RLS, read expansion, and purge.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = AgeGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );

    let source_uid = Uuid::now_v7();
    let document_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let fact_uid = Uuid::now_v7();
    let entity_uid = Uuid::now_v7();
    let contact_group_uid = Uuid::now_v7();

    for (uid, label, name) in [
        (source_uid, NodeLabel::Source, "tenant source"),
        (document_uid, NodeLabel::Document, "tenant document"),
        (chunk_uid, NodeLabel::Chunk, "tenant chunk"),
        (fact_uid, NodeLabel::Fact, "chunk fact"),
        (entity_uid, NodeLabel::Entity, "contact entity"),
        (
            contact_group_uid,
            NodeLabel::ContactGroup,
            "tenant contact group",
        ),
    ] {
        let created = graph
            .create_node(node_intent(&storage_partition_id, uid, label, name))
            .await
            .unwrap_or_else(|error| panic!("create {} node: {error}", label.as_str()));
        assert_eq!(created, uid);
        let row = graph
            .get_node(uid)
            .await
            .expect("read created node")
            .expect("created node is visible");
        assert_eq!(row.label, label);
    }

    for (label, start_uid, end_uid) in [
        (EdgeLabel::Contains, source_uid, document_uid),
        (EdgeLabel::Contains, document_uid, chunk_uid),
        (EdgeLabel::DerivedFrom, fact_uid, chunk_uid),
        (EdgeLabel::MentionedIn, entity_uid, chunk_uid),
        (EdgeLabel::MemberOf, entity_uid, contact_group_uid),
        (EdgeLabel::DerivedFrom, contact_group_uid, source_uid),
    ] {
        graph
            .create_edge(edge_intent(
                &storage_partition_id,
                label,
                start_uid,
                end_uid,
            ))
            .await
            .unwrap_or_else(|error| panic!("create {} edge: {error}", label.as_str()));
    }

    let source_hits = graph
        .expand_seeds(&[source_uid], 2, None)
        .await
        .expect("expand source document chain");
    let document = source_hits
        .iter()
        .find(|hit| hit.uid == document_uid)
        .expect("document should be one hop from source");
    assert_eq!(document.label, NodeLabel::Document);
    assert_eq!(document.edges, vec![EdgeLabel::Contains]);
    let chunk = source_hits
        .iter()
        .find(|hit| hit.uid == chunk_uid)
        .expect("chunk should be two hops from source");
    assert_eq!(chunk.label, NodeLabel::Chunk);
    assert_eq!(chunk.edges, vec![EdgeLabel::Contains, EdgeLabel::Contains]);

    let entity_hits = graph
        .expand_seeds(&[entity_uid], 1, None)
        .await
        .expect("expand entity memberships and mentions");
    assert!(entity_hits.iter().any(|hit| {
        hit.uid == chunk_uid
            && hit.label == NodeLabel::Chunk
            && hit.edges == vec![EdgeLabel::MentionedIn]
    }));
    assert!(entity_hits.iter().any(|hit| {
        hit.uid == contact_group_uid
            && hit.label == NodeLabel::ContactGroup
            && hit.edges == vec![EdgeLabel::MemberOf]
    }));

    for label in ["Document", "Chunk", "ContactGroup", "CONTAINS", "MEMBER_OF"] {
        let policy_names = rls_policy_names(session_store.pool(), label).await;
        for expected in [
            "owner_dev_access",
            "rd_global",
            "rd_tenant",
            "rd_user",
            "wr_global_promoter",
            "wr_tenant",
            "wr_user",
        ] {
            assert!(
                policy_names.iter().any(|name| name == expected),
                "{label} should include {expected} AGE RLS policy: {policy_names:?}"
            );
        }
    }

    for uid in [
        source_uid,
        document_uid,
        chunk_uid,
        fact_uid,
        entity_uid,
        contact_group_uid,
    ] {
        graph
            .hard_purge(uid, "redacted:knowledge-labels")
            .await
            .unwrap_or_else(|error| panic!("hard purge {uid}: {error}"));
        assert!(
            graph
                .get_node(uid)
                .await
                .expect("read purged node")
                .is_none(),
            "{uid} should be deleted"
        );
    }

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
