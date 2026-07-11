use moa_core::{
    config::MoaConfig, config::SessionAttachmentStorageConfig, traits::SessionAttachmentStore,
    traits::SessionStore, types::identifiers::TenantId,
};
use moa_session::{PostgresSessionStore, testing};
use moa_test_support::fixtures::session_meta_fixture;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires Postgres and RustFS from docker compose"]
async fn session_attachment_store_round_trips_uploaded_content_across_instances_docker() {
    // Pins: uploaded session attachments are Postgres-backed and readable from another pod/store instance.
    // Accept common truthy values (1/true/yes/on, case-insensitive) so a developer's
    // `.env` enables this docker lane without requiring the literal "1".
    let docker_tests_enabled = std::env::var("MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if !docker_tests_enabled {
        panic!("set MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 to run RustFS attachment tests");
    }

    let (writer, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated session attachment store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = writer
        .create_session(session_meta_fixture(tenant_id))
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
