//! Docker-gated session attachment storage behavior against Postgres and RustFS.

use moa_config::MoaConfig;
use moa_config::SessionAttachmentStorageConfig;
use moa_core::{
    error::MoaError, traits::SessionAttachmentStore, traits::SessionStore,
    types::contact::ClientMessageId, types::contact::SessionAttachmentDisposition,
    types::contact::SessionAttachmentSlot, types::contact::SessionAttachmentUpload,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_session::{PostgresSessionStore, testing};
use moa_test_support::fixtures::session_meta_fixture;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Accepts common truthy values (1/true/yes/on, case-insensitive) so a developer's
/// `.env` enables this docker lane without requiring the literal "1".
fn require_docker_tests_enabled() {
    let enabled = std::env::var("MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if !enabled {
        panic!("set MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 to run RustFS attachment tests");
    }
}

/// Local RustFS attachment storage for this lane, honoring the compose host port.
///
/// `docker-compose` publishes RustFS on `MOA_RUSTFS_PORT` because 9000 is a popular port
/// (minio, k3d node ports), so a developer whose host already uses 9000 maps it elsewhere.
/// The lane resolves the same `MOA_OBJECT_STORE_ENDPOINT` the deployment overlay
/// uses, then that published port, before falling back to the documented default.
fn configure_local_attachment_storage(config: &mut MoaConfig) {
    config.session.attachments = SessionAttachmentStorageConfig::default();
    config.object_store = moa_config::ObjectStoreConfig::local_rustfs();
    if let Some(endpoint) = std::env::var("MOA_OBJECT_STORE_ENDPOINT")
        .ok()
        .filter(|endpoint| !endpoint.trim().is_empty())
    {
        config.object_store.endpoint = Some(endpoint);
    } else if let Some(port) = std::env::var("MOA_RUSTFS_PORT")
        .ok()
        .filter(|port| !port.trim().is_empty())
    {
        config.object_store.endpoint = Some(format!("http://127.0.0.1:{}", port.trim()));
    }
}

/// Provisions an isolated Postgres schema whose store writes to local RustFS.
async fn create_attachment_test_store() -> (PostgresSessionStore, String, String) {
    let (database_url, schema_name) = testing::provision_cloned_database()
        .await
        .expect("provision isolated attachment test database");
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect attachment test pool");
    let mut config = MoaConfig::default();
    config.database.url = database_url.clone();
    config.database.schema = Some(schema_name.clone());
    configure_local_attachment_storage(&mut config);
    let store = PostgresSessionStore::from_existing_pool_with_config(&config, pool)
        .await
        .expect("create attachment test store");
    (store, database_url, schema_name)
}

fn slot(
    tenant_id: TenantId,
    session_id: SessionId,
    message: &str,
    ordinal: u16,
) -> SessionAttachmentSlot {
    SessionAttachmentSlot {
        tenant_id,
        session_id,
        client_message_id: ClientMessageId::new(message).expect("test client message id is valid"),
        ordinal,
    }
}

fn upload(name: &str, content: &[u8]) -> SessionAttachmentUpload {
    SessionAttachmentUpload {
        contact_id: None,
        name: name.to_string(),
        mime_type: "image/png".to_string(),
        content: content.to_vec(),
    }
}

#[tokio::test]
#[ignore = "requires Postgres and RustFS from docker compose"]
async fn session_attachment_store_round_trips_uploaded_content_across_instances_docker() {
    // Pins: uploaded session attachments are Postgres-backed and readable from another pod/store instance.
    require_docker_tests_enabled();

    let (writer, database_url, schema_name) = create_attachment_test_store().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = writer
        .create_session(session_meta_fixture(tenant_id))
        .await
        .expect("create session for attachment");

    let stored = writer
        .put(
            &slot(tenant_id, session_id, "client-message-round-trip", 0),
            upload("receipt.png", b"\x89PNG\r\n\x1a\ncontent"),
        )
        .await
        .expect("store attachment");
    assert_eq!(stored.disposition, SessionAttachmentDisposition::Created);
    let attachment = stored.attachment;
    let attachment_id = attachment.id.expect("stored attachment should have id");

    let reader_pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect second session attachment store pool");
    let mut config = MoaConfig::default();
    config.database.url = database_url.clone();
    config.database.schema = Some(schema_name.clone());
    configure_local_attachment_storage(&mut config);
    let reader = PostgresSessionStore::from_existing_pool_with_config(&config, reader_pool.clone())
        .await
        .expect("create second session attachment store");
    let (read_attachment, content) = reader
        .get(tenant_id, session_id, attachment_id)
        .await
        .expect("read attachment from second store");
    let listed = reader
        .list_for_session(tenant_id, session_id)
        .await
        .expect("list session attachments");

    assert_eq!(content, b"\x89PNG\r\n\x1a\ncontent");
    assert_eq!(read_attachment, attachment);
    assert_eq!(listed, vec![attachment]);

    drop(reader);
    reader_pool.close().await;
    drop(writer);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop session attachment test schema");
}

#[tokio::test]
#[ignore = "requires Postgres and RustFS from docker compose"]
async fn retried_attachment_slot_replays_and_changed_content_conflicts_docker() {
    // Pins: the deterministic attachment slot. A retried upload of the same message must
    // reuse one row and one object and report a replay, a slot reused with different bytes
    // or metadata must fail typed with the original content still intact, and a different
    // ordinal must remain a separate attachment. Without this, a disconnect before the
    // first response duplicates every uploaded photo.
    require_docker_tests_enabled();

    let (store, database_url, schema_name) = create_attachment_test_store().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = store
        .create_session(session_meta_fixture(tenant_id))
        .await
        .expect("create session for attachment");
    let first_slot = slot(tenant_id, session_id, "client-message-replay", 0);
    let original_bytes = b"\x89PNG\r\n\x1a\noriginal".to_vec();

    let created = store
        .put(&first_slot, upload("receipt.png", &original_bytes))
        .await
        .expect("store attachment");
    assert_eq!(created.disposition, SessionAttachmentDisposition::Created);

    let replayed = store
        .put(&first_slot, upload("receipt.png", &original_bytes))
        .await
        .expect("replay attachment");
    assert_eq!(replayed.disposition, SessionAttachmentDisposition::Replayed);
    assert_eq!(replayed.attachment, created.attachment);
    assert_eq!(
        store
            .list_for_session(tenant_id, session_id)
            .await
            .expect("list after replay"),
        vec![created.attachment.clone()],
        "a replayed upload must not create a second attachment row"
    );

    let changed_content = store
        .put(
            &first_slot,
            upload("receipt.png", b"\x89PNG\r\n\x1a\nchanged"),
        )
        .await
        .expect_err("changed content must conflict");
    assert!(
        matches!(changed_content, MoaError::SessionAttachmentSlotConflict(_)),
        "unexpected error for changed content: {changed_content}"
    );
    let changed_metadata = store
        .put(&first_slot, upload("invoice.png", &original_bytes))
        .await
        .expect_err("changed metadata must conflict");
    assert!(
        matches!(changed_metadata, MoaError::SessionAttachmentSlotConflict(_)),
        "unexpected error for changed metadata: {changed_metadata}"
    );

    let attachment_id = created
        .attachment
        .id
        .expect("stored attachment should have id");
    let (_, content_after_conflicts) = store
        .get(tenant_id, session_id, attachment_id)
        .await
        .expect("read attachment after rejected retries");
    assert_eq!(
        content_after_conflicts, original_bytes,
        "a rejected retry must not overwrite the stored object"
    );

    let second_slot = slot(tenant_id, session_id, "client-message-replay", 1);
    let second = store
        .put(&second_slot, upload("receipt.png", &original_bytes))
        .await
        .expect("store second attachment of the same message");
    assert_eq!(second.disposition, SessionAttachmentDisposition::Created);
    assert_ne!(second.attachment.id, created.attachment.id);
    assert_eq!(
        store
            .list_for_session(tenant_id, session_id)
            .await
            .expect("list after second ordinal")
            .len(),
        2
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop session attachment test schema");
}
