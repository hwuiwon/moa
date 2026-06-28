use moa_core::{
    MoaConfig, ModelId, SessionActorRef, SessionAttachmentStorageConfig, SessionAttachmentStore,
    SessionMeta, SessionStore, TenantId,
};
use moa_session::{PostgresSessionStore, testing};
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

#[tokio::test]
#[ignore = "requires Postgres and RustFS from docker compose"]
async fn session_attachment_store_round_trips_uploaded_content_across_instances_docker() {
    // Pins: uploaded session attachments are Postgres-backed and readable from another pod/store instance.
    if std::env::var("MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS")
        .ok()
        .as_deref()
        != Some("1")
    {
        panic!("set MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 to run RustFS attachment tests");
    }

    let (writer, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated session attachment store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = writer
        .create_session(test_session_meta(tenant_id))
        .await
        .expect("create session for attachment");

    let attachment = writer
        .put(
            tenant_id,
            session_id,
            None,
            "receipt.png".to_string(),
            "image/png".to_string(),
            b"\x89PNG\r\n\x1a\ncontent".to_vec(),
        )
        .await
        .expect("store attachment");
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
    config.session.attachments = SessionAttachmentStorageConfig::local_rustfs();
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
