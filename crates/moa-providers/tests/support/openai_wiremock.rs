//! OpenAI wiremock fixtures for offline provider tests.

use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

use crate::support::wiremock_common::{fixture_body, sse_response};

/// Canonical OpenAI model used by offline provider tests.
pub const OPENAI_MODEL: &str = "gpt-5.4";

/// Mounts a single OpenAI Responses SSE response.
pub async fn mount_openai_sse(
    server: &MockServer,
    sse_body: &'static str,
    _expected_body_fragment: &str,
) {
    Mock::given(any())
        .respond_with(sse_response(fixture_body(sse_body)))
        .mount(server)
        .await;
}
