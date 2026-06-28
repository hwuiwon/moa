//! DB integration coverage for contact-scoped privacy erasure.

use chrono::Utc;
use moa_core::{ContactId, RlsContext, StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_pii::erasure::{
    GraphErasureAudit, enumerate_erase_candidates, hard_purge_erase_candidates,
};
use moa_session::testing;
use serde_json::json;
use uuid::Uuid;

fn contact_node(tenant_id: TenantId, contact_id: ContactId, uid: Uuid) -> NodeWriteIntent {
    let subject_user_id = contact_id.to_string();
    NodeWriteIntent {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(subject_user_id.clone()),
        scope: "contact".to_string(),
        name: "contact erasure fact".to_string(),
        properties: json!({
            "name": "contact erasure fact",
            "source": "erasure_db_memory",
            "user_id": subject_user_id,
        }),
        pii_class: PiiClass::Phi,
        confidence: Some(0.97),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
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
    let graph = AgeGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
    );
    let uid = Uuid::now_v7();
    graph
        .create_node(contact_node(tenant_id, contact_id, uid))
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
