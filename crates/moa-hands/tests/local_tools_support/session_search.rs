// Session-search local-tools test support.

use std::sync::Arc;

use moa_core::{Event, SessionStore, ToolInvocation};
use moa_hands::ToolRouter;
use moa_session::{PostgresSessionStore, testing};
use serde_json::json;
use tempfile::tempdir;

async fn test_session_store() -> Arc<PostgresSessionStore> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await.unwrap();
    Arc::new(store)
}
