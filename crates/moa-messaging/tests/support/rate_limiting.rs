//! Rate-limiting test fixtures.

use std::sync::Arc;

use moa_messaging::MessagingSendResponse;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Starts a mock endpoint that returns one 429 response followed by 200 responses.
pub async fn mock_429_then_200(header_name: &str, retry_after: &str) -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(header_name, retry_after)
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    server
}

/// Starts a mock endpoint that always returns 429.
pub async fn mock_always_429(header_name: &str, retry_after: &str) -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(header_name, retry_after)
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;
    server
}

/// Starts a mock endpoint that always returns 200.
pub async fn mock_always_200() -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    server
}

/// Posts one synthetic channel send request to a mock server.
pub async fn post_send(server: Arc<MockServer>) -> moa_core::error::Result<MessagingSendResponse> {
    let response = reqwest::Client::new()
        .post(format!("{}/send", server.uri()))
        .body("{}")
        .send()
        .await
        .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .or_else(|| response.headers().get("X-RateLimit-Reset-After"))
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response
        .text()
        .await
        .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?;
    let mut normalized = MessagingSendResponse::new(status, body);
    if let Some(retry_after) = retry_after {
        normalized = normalized
            .with_header("Retry-After", retry_after.clone())
            .with_header("X-RateLimit-Reset-After", retry_after);
    }
    Ok(normalized)
}
