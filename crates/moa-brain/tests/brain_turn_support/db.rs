// DB-backed brain-turn integration-test support.

use moa_session::{PostgresSessionStore, testing};

async fn test_session_store() -> std::sync::Arc<PostgresSessionStore> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store()
        .await
        .unwrap();
    std::sync::Arc::new(store)
}
