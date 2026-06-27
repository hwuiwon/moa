//! Anthropic wiremock fixtures for offline provider tests.

use wiremock::matchers::{body_string_contains, header_exists, method};
use wiremock::{Mock, MockServer};

use crate::support::wiremock_common::{fixture_body, sse_response};

/// Canonical Anthropic model used by offline provider tests.
pub const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";

/// Mounts a single Anthropic SSE response and basic request-shape matchers.
pub async fn mount_anthropic_sse(
    server: &MockServer,
    sse_body: &'static str,
    expected_body_fragment: &str,
) {
    Mock::given(method("POST"))
        .and(header_exists("x-api-key"))
        .and(header_exists("anthropic-version"))
        .and(body_string_contains(expected_body_fragment))
        .respond_with(sse_response(fixture_body(sse_body)))
        .mount(server)
        .await;
}
