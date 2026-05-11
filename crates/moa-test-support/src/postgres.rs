//! Shared Postgres bootstrap and session-store contract helpers for tests.

mod contracts;

use moa_core::{MoaError, Result};
use moa_session::{PostgresSessionStore, testing};
use uuid::Uuid;

pub use contracts::*;

/// Default Docker Compose Postgres URL used by local MOA tests.
pub const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Returns the Postgres URL used by integration tests.
///
/// Lookup order is `MOA_TEST_POSTGRES_URL`, then `TEST_DATABASE_URL`, then
/// `DATABASE_URL`, and finally the repository Docker Compose default.
#[must_use]
pub fn test_database_url() -> String {
    std::env::var("MOA_TEST_POSTGRES_URL")
        .or_else(|_| std::env::var("TEST_DATABASE_URL"))
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string())
}

/// One isolated Postgres schema and session store for a test.
pub struct TestDb {
    store: Option<PostgresSessionStore>,
    database_url: String,
    schema_name: String,
}

impl TestDb {
    /// Returns the isolated session store.
    #[must_use]
    pub fn store(&self) -> &PostgresSessionStore {
        match self.store.as_ref() {
            Some(store) => store,
            None => panic!("TestDb store is only absent while Drop is running"),
        }
    }

    /// Returns the database URL used by this test database.
    #[must_use]
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Returns the schema name isolated for this test database.
    #[must_use]
    pub fn schema_name(&self) -> &str {
        &self.schema_name
    }

    /// Consumes the wrapper and returns the store plus cleanup coordinates.
    ///
    /// This exists for legacy tests that still perform explicit cleanup.
    #[must_use]
    pub fn into_parts(mut self) -> (PostgresSessionStore, String, String) {
        let store = match self.store.take() {
            Some(store) => store,
            None => panic!("TestDb store is only absent while Drop is running"),
        };
        (store, self.database_url.clone(), self.schema_name.clone())
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let Some(store) = self.store.take() else {
            return;
        };
        let database_url = self.database_url.clone();
        let schema_name = self.schema_name.clone();
        let cleanup = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    MoaError::StorageError(format!(
                        "failed to create runtime for TestDb cleanup: {error}"
                    ))
                })?;
            runtime.block_on(async move {
                store.pool().close().await;
                testing::cleanup_test_schema(&database_url, &schema_name).await
            })
        });

        match cleanup.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(?error, "failed to clean up TestDb schema"),
            Err(_) => tracing::warn!("TestDb cleanup thread panicked"),
        }
    }
}

/// Creates a Postgres-backed session store in a fresh isolated schema.
pub async fn bootstrap_test_db() -> Result<TestDb> {
    let database_url = test_database_url();
    let schema_name = format!("test_{}", Uuid::now_v7().simple());
    let store = PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?;
    Ok(TestDb {
        store: Some(store),
        database_url,
        schema_name,
    })
}

/// Creates an isolated store and returns legacy cleanup coordinates.
pub async fn create_isolated_test_store() -> Result<(PostgresSessionStore, String, String)> {
    Ok(bootstrap_test_db().await?.into_parts())
}

/// Drops one isolated Postgres schema created by [`bootstrap_test_db`].
pub async fn cleanup_test_schema(database_url: &str, schema_name: &str) -> Result<()> {
    testing::cleanup_test_schema(database_url, schema_name).await
}
