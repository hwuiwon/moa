//! OpenAI wiremock fixtures for offline provider tests.

use wiremock::matchers::{body_string_contains, header_exists, method};
use wiremock::{Mock, MockServer};

use crate::support::wiremock_common::{fixture_body, sse_response};

/// Canonical OpenAI model used by offline provider tests.
pub const OPENAI_MODEL: &str = "gpt-5.4";

/// Mounts a single OpenAI Responses SSE response and basic request-shape matchers.
///
/// Mirrors the Anthropic/Gemini helpers: the mock only answers a `POST` that
/// carries a bearer `authorization` header and whose serialized request body
/// contains `expected_body_fragment`, so the offline tests actually assert the
/// request that the provider builds (not just that some request arrived).
pub async fn mount_openai_sse(
    server: &MockServer,
    sse_body: &'static str,
    expected_body_fragment: &str,
) {
    Mock::given(method("POST"))
        .and(header_exists("authorization"))
        .and(body_string_contains(expected_body_fragment))
        .respond_with(sse_response(fixture_body(sse_body)))
        .mount(server)
        .await;
}
