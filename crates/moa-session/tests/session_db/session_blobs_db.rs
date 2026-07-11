//! PostgreSQL-backed session blob persistence coverage.

use moa_core::{
    error::MoaError, traits::BlobStore, traits::SessionStore, types::identifiers::TenantId,
};
use moa_session::blob::PostgresBlobStore;
use moa_test_support::fixtures::session_meta_fixture;
use moa_test_support::postgres::bootstrap_test_db;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

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
        .create_session(session_meta_fixture(TenantId::from(Uuid::now_v7())))
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

#[tokio::test]
async fn postgres_blob_store_get_many_batches_present_ids_and_omits_missing_db() {
    // Pins: get_many returns exactly the stored blobs for the requested ids in
    // one query, omits ids with no row instead of erroring, and matches per-id
    // get() byte-for-byte — the parity the event-replay fetch relies on.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap test database with session_blobs migration");
    let session_id = test_db
        .store()
        .create_session(session_meta_fixture(TenantId::from(Uuid::now_v7())))
        .await
        .expect("create session row for blob FK");
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(2)
        .connect(test_db.database_url())
        .await
        .expect("connect blob pool");
    let store = PostgresBlobStore::new_in_schema(pool.clone(), test_db.schema_name())
        .await
        .expect("create schema-qualified blob store");

    let id_a = store
        .store(&session_id, b"alpha payload")
        .await
        .expect("store alpha");
    let id_b = store
        .store(&session_id, b"beta payload")
        .await
        .expect("store beta");
    // A well-formed SHA-256-shaped id that was never stored.
    let missing = "0".repeat(64);

    let batch = store
        .get_many(&session_id, &[id_a.clone(), id_b.clone(), missing.clone()])
        .await
        .expect("get_many present and missing ids");
    assert_eq!(batch.len(), 2, "missing id must be omitted, not error");
    assert_eq!(
        batch.get(&id_a).map(Vec::as_slice),
        Some(b"alpha payload".as_slice())
    );
    assert_eq!(
        batch.get(&id_b).map(Vec::as_slice),
        Some(b"beta payload".as_slice())
    );
    assert!(
        !batch.contains_key(&missing),
        "absent blob id must not appear in the batch result"
    );

    // Batch bytes match per-id get() for every present id.
    for id in [&id_a, &id_b] {
        assert_eq!(
            &batch[id],
            &store.get(&session_id, id).await.expect("per-id get parity"),
            "get_many bytes must equal get() bytes"
        );
    }

    // The divergence get_many hides: a per-id get() on a missing id still errors.
    assert!(
        matches!(
            store.get(&session_id, &missing).await,
            Err(MoaError::BlobNotFound(_))
        ),
        "get() on a missing id must surface BlobNotFound"
    );

    // Empty input is a no-op returning an empty map.
    assert!(
        store
            .get_many(&session_id, &[])
            .await
            .expect("get_many empty input")
            .is_empty()
    );

    pool.close().await;
}
