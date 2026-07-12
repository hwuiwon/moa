//! Shared Postgres bootstrap and session-store contract helpers for tests.

mod contracts;

use moa_core::{error::MoaError, error::Result};
use moa_session::{PostgresSessionStore, testing};

pub use contracts::{
    test_action_policy_rules, test_create_and_get_session, test_emit_and_get_events,
    test_event_search, test_list_sessions_with_filter, test_session_status_update,
    test_tenant_cost_since,
};

/// Default Docker Compose Postgres URL used by local MOA tests.
pub const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Returns the Postgres URL used by integration tests.
///
/// Uses the same `MOA_DATABASE_URL` runtime setting as the service, falling
/// back to the repository Docker Compose default when unset.
#[must_use]
pub fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
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

/// Creates a Postgres-backed session store in a fresh isolated database.
///
/// The database is cloned from a cached migration template (see
/// [`moa_session::testing`]), so bootstrap is a fast block copy rather than a
/// full migration replay. The returned `database_url` points at the per-test
/// database and `schema_name` is the schema holding its session tables; the
/// database is dropped when the [`TestDb`] is dropped.
pub async fn bootstrap_test_db() -> Result<TestDb> {
    let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    Ok(TestDb {
        store: Some(store),
        database_url,
        schema_name,
    })
}
