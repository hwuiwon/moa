//! Shared wiremock helpers for offline provider transport coverage.

use moa_core::{types::completion::CompletionRequest, types::context::ContextMessage};
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builds a minimal completion request with deterministic output controls.
pub fn minimal_request(prompt: impl Into<String>) -> CompletionRequest {
    let mut request = CompletionRequest::simple(prompt);
    request.max_output_tokens = Some(64);
    request.temperature = Some(0.0);
    request
}

/// Builds a completion request that exposes one deterministic function tool.
pub fn tool_request(prompt: impl Into<String>) -> CompletionRequest {
    CompletionRequest {
        model: None,
        messages: vec![ContextMessage::user(prompt)],
        tools: vec![emit_token_tool()],
        max_output_tokens: Some(128),
        temperature: Some(0.0),
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    }
}

fn emit_token_tool() -> serde_json::Value {
    json!({
        "name": "emit_token",
        "description": "Echoes a validation token.",
        "input_schema": {
            "type": "object",
            "properties": {
                "token": { "type": "string" }
            },
            "required": ["token"],
            "additionalProperties": false
        }
    })
}

/// Mounts one retryable status response followed by a successful SSE response.
pub async fn mount_retry_then_sse(
    server: &MockServer,
    status: u16,
    error_body: &'static str,
    success_sse_body: &'static str,
) {
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "application/json")
                .set_body_string(error_body),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(any())
        .respond_with(sse_response(fixture_body(success_sse_body)))
        .mount(server)
        .await;
}

/// Mounts one retryable status response for every request.
pub async fn mount_always_status(server: &MockServer, status: u16, body: &'static str) {
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

/// Returns the number of HTTP requests received by the mock server.
pub async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests")
        .len()
}

/// Strips provenance comments from fixture files before serving them.
pub fn fixture_body(raw: &'static str) -> String {
    raw.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Builds a text/event-stream response.
pub fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("cache-control", "no-cache")
        .set_body_raw(body, "text/event-stream")
}
