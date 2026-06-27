//! Shared lock for ignored Restate e2e tests.

use tokio::sync::Mutex;

/// Serializes ignored Restate e2e tests that share the same local Restate server.
pub static RESTATE_E2E_LOCK: Mutex<()> = Mutex::const_new(());
