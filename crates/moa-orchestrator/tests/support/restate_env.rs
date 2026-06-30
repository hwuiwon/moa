//! Restate connection endpoints and the shared serialization lock for e2e tests.

use tokio::sync::Mutex;

/// Serializes ignored Restate e2e tests that share the same local Restate server.
pub static RESTATE_E2E_LOCK: Mutex<()> = Mutex::const_new(());

/// Return the Restate admin URL used by e2e tests.
pub fn restate_admin_url() -> String {
    std::env::var("MOA_RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:10011".to_string())
}

/// Return the Restate ingress URL used by e2e tests.
pub fn restate_ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:10010".to_string())
}
