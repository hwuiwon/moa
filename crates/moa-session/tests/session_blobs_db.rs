use moa_core::{BlobStore, ModelId, SessionActorRef, SessionMeta, SessionStore, TenantId};
use moa_session::blob::PostgresBlobStore;
use moa_test_support::postgres::bootstrap_test_db;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn test_session_meta(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema_name.replace('"', "\"\""),
        table_name.replace('"', "\"\"")
    )
}

#[tokio::test]
async fn postgres_blob_store_reads_blob_written_by_previous_instance_db() {
    // Pins: migrated session_blobs storage survives replacing the Postgres blob-store instance.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap test database with session_blobs migration");
    let session_id = test_db
        .store()
        .create_session(test_session_meta(TenantId::from(Uuid::now_v7())))
        .await
        .expect("create session row for blob FK");
    let writer_pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(test_db.database_url())
        .await
        .expect("connect writer blob pool");
    let reader_pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(test_db.database_url())
        .await
        .expect("connect reader blob pool");
    let writer = PostgresBlobStore::new_in_schema(writer_pool.clone(), test_db.schema_name())
        .await
        .expect("create schema-qualified writer blob store");
    let reader = PostgresBlobStore::new_in_schema(reader_pool.clone(), test_db.schema_name())
        .await
        .expect("create schema-qualified reader blob store");

    let blob_id = writer
        .store(&session_id, b"durable postgres blob")
        .await
        .expect("store postgres blob");
    let duplicate_blob_id = writer
        .store(&session_id, b"durable postgres blob")
        .await
        .expect("store duplicate postgres blob");
    assert_eq!(duplicate_blob_id, blob_id);

    let session_blobs = qualified(test_db.schema_name(), "session_blobs");
    let stored_rows = sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM {session_blobs} WHERE session_id = $1"
    ))
    .bind(session_id.0)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count stored blob rows");
    assert_eq!(stored_rows, 1);

    assert!(
        reader
            .exists(&session_id, &blob_id)
            .await
            .expect("check blob existence from second store")
    );
    assert_eq!(
        reader
            .get(&session_id, &blob_id)
            .await
            .expect("read blob from second store"),
        b"durable postgres blob"
    );

    writer_pool.close().await;
    reader_pool.close().await;
}
