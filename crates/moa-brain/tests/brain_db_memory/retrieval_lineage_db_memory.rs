//! Integration coverage for chunk-provenance columns on retrieval lineage rows.

use moa_core::{ContactId, SessionId, TenantId};
use moa_memory_types::MemoryScope;
use moa_session::testing;
use sqlx::Row;
use uuid::Uuid;

use moa_brain::retrieval::{LineageContext, RetrievalLineageHit, legs::write_retrieval_lineage};
use moa_lineage_core::TurnId;

#[tokio::test]
async fn retrieval_lineage_rows_record_chunk_and_document_provenance_db_memory() {
    // Pins: each ranked retrieval-lineage row stores the denormalized chunk_uid
    // and document_version_uid so a dashboard resolves a turn's answer back to
    // its source document without joining moa.knowledge_chunks; graph-only hits
    // store NULL chunk provenance.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let scope = MemoryScope::Contact {
        tenant_id,
        contact_id,
    };
    let session_id = SessionId::new();
    let turn_id = TurnId::new_v7();
    let chunk_hit_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let document_version_uid = Uuid::now_v7();
    let fact_hit_uid = Uuid::now_v7();

    write_retrieval_lineage(
        session_store.pool().clone(),
        scope,
        LineageContext {
            session_id,
            turn_id: Some(turn_id),
            turn_seq: 3,
        },
        vec![
            RetrievalLineageHit {
                uid: chunk_hit_uid,
                chunk_uid: Some(chunk_uid),
                document_version_uid: Some(document_version_uid),
            },
            RetrievalLineageHit {
                uid: fact_hit_uid,
                chunk_uid: None,
                document_version_uid: None,
            },
        ],
        chrono::Utc::now(),
        true,
    )
    .await
    .expect("write retrieval lineage rows");

    let rows = sqlx::query(
        "SELECT uid, chunk_uid, document_version_uid, turn_id, rank \
         FROM moa.retrieval_lineage WHERE session_id = $1 ORDER BY rank",
    )
    .bind(session_id.0)
    .fetch_all(session_store.pool())
    .await
    .expect("select retrieval lineage rows");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<Uuid, _>("uid"), chunk_hit_uid);
    assert_eq!(rows[0].get::<Option<Uuid>, _>("chunk_uid"), Some(chunk_uid));
    assert_eq!(
        rows[0].get::<Option<Uuid>, _>("document_version_uid"),
        Some(document_version_uid)
    );
    assert_eq!(rows[0].get::<Option<Uuid>, _>("turn_id"), Some(turn_id.0));
    assert_eq!(rows[0].get::<i32, _>("rank"), 1);
    assert_eq!(rows[1].get::<Uuid, _>("uid"), fact_hit_uid);
    assert_eq!(rows[1].get::<Option<Uuid>, _>("chunk_uid"), None);
    assert_eq!(rows[1].get::<Option<Uuid>, _>("document_version_uid"), None);
    assert_eq!(rows[1].get::<i32, _>("rank"), 2);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
