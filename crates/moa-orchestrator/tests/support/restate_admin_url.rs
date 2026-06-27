//! Restate admin URL helper for e2e tests.

/// Return the Restate admin URL used by e2e tests.
pub fn restate_admin_url() -> String {
    std::env::var("MOA_RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:10011".to_string())
}
