//! Postgres-backed session storage for MOA.
//!
//! Database migrations live in `moa-migrations`; this crate owns runtime
//! session-store queries.

pub mod analytics;
mod attachment_storage;
pub mod blob;
pub mod neon;
pub mod queries;
pub mod store;
pub mod testing;

use std::sync::Arc;

use moa_core::{MoaConfig, Result};

pub use blob::FileBlobStore;
pub use neon::NeonBranchManager;
pub use store::PostgresSessionStore;

/// Creates the shared Postgres session store from config and verifies connectivity.
pub async fn create_session_store(config: &MoaConfig) -> Result<Arc<PostgresSessionStore>> {
    let store = PostgresSessionStore::from_config(config).await?;
    store.ping().await?;
    Ok(Arc::new(store))
}
