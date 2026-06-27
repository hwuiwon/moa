//! Gemini wiremock fixtures for offline provider tests.

use wiremock::matchers::{body_string_contains, header_exists, method};
use wiremock::{Mock, MockServer};

use crate::support::wiremock_common::{fixture_body, sse_response};

/// Canonical Gemini model used by offline provider tests.
pub const GEMINI_MODEL: &str = "gemini-3-flash-preview";

/// Mounts a single Gemini SSE response and basic request-shape matchers.
pub async fn mount_gemini_sse(
    server: &MockServer,
    sse_body: &'static str,
    expected_body_fragment: &str,
) {
    Mock::given(method("POST"))
        .and(header_exists("x-goog-api-key"))
        .and(body_string_contains(expected_body_fragment))
        .respond_with(sse_response(fixture_body(sse_body)))
        .mount(server)
        .await;
}
