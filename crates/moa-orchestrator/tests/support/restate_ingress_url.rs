//! Restate ingress URL helper for e2e tests.

/// Return the Restate ingress URL used by e2e tests.
pub fn restate_ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:10010".to_string())
}
