mod support {
    pub mod openai_wiremock;
    pub mod wiremock_common;
}

use std::time::Duration;

use moa_core::{
    CompletionContent, LLMProvider, MoaError, StopReason, ToolCallContent, ToolInvocation,
};
use moa_providers::OpenAIProvider;
use serde_json::json;
use tokio::time::{advance, timeout};
use wiremock::MockServer;

use support::openai_wiremock::{OPENAI_MODEL, mount_openai_sse};
use support::wiremock_common::{
    minimal_request, mount_always_status, mount_retry_then_sse, request_count, tool_request,
};

const TEXT: &str = include_str!("support/fixtures/sse/openai_text.sse");
const TEXT_SPLIT: &str = include_str!("support/fixtures/sse/openai_text_split.sse");
const TOOL_CALL: &str = include_str!("support/fixtures/sse/openai_tool_call.sse");
const MALFORMED: &str = include_str!("support/fixtures/sse/openai_malformed.sse");
const DISCONNECT: &str = include_str!("support/fixtures/sse/openai_disconnect.sse");

#[tokio::test]
async fn openai_offline_completion_returns_text_for_minimal_request() {
    let server = MockServer::start().await;
    mount_openai_sse(&server, TEXT, "What is 2+2?").await;
    let provider = provider(&server, 0);

    let response = provider
        .complete(minimal_request("What is 2+2?"))
        .await
        .expect("offline request should start")
        .collect()
        .await
        .expect("offline stream should complete");

    assert_eq!(response.text, "Hello");
    assert_eq!(response.usage.input_tokens_cache_read, 2);
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn openai_offline_streaming_yields_text_deltas_then_terminal_event() {
    let server = MockServer::start().await;
    mount_openai_sse(&server, TEXT_SPLIT, "Count from 1 to 5").await;
    let provider = provider(&server, 0);

    let mut stream = provider
        .complete(minimal_request("Count from 1 to 5"))
        .await
        .expect("offline request should start");
    let first = stream.next().await.expect("first delta").expect("first ok");
    let second = stream
        .next()
        .await
        .expect("second delta")
        .expect("second ok");
    let response = stream.collect().await.expect("terminal response");

    assert_eq!(first, CompletionContent::Text("Hel".to_string()));
    assert_eq!(second, CompletionContent::Text("lo".to_string()));
    assert_eq!(response.text, "Hello");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn openai_offline_tool_call_response_parses_into_provider_event() {
    let server = MockServer::start().await;
    mount_openai_sse(&server, TOOL_CALL, "emit_token").await;
    let provider = provider(&server, 0);

    let mut stream = provider
        .complete(tool_request("Call emit_token with OFFLINE."))
        .await
        .expect("offline request should start");
    let event = stream.next().await.expect("tool event").expect("tool ok");
    let response = stream.collect().await.expect("terminal response");

    assert_eq!(
        event,
        CompletionContent::ToolCall(ToolCallContent {
            invocation: ToolInvocation {
                id: Some("fc_1".to_string()),
                name: "emit_token".to_string(),
                input: json!({ "token": "OFFLINE" }),
            },
            provider_metadata: None,
        })
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn openai_offline_429_response_triggers_retry_with_backoff() {
    tokio::time::pause();
    let server = MockServer::start().await;
    mount_retry_then_sse(
        &server,
        429,
        r#"{"error":{"message":"rate limit","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        TEXT,
    )
    .await;
    let provider = provider(&server, 1);

    let task = tokio::spawn(async move {
        provider
            .complete(minimal_request("retry after rate limit"))
            .await
            .expect("request should start")
            .collect()
            .await
    });
    tokio::task::yield_now().await;
    advance(Duration::from_secs(2)).await;
    let response = task
        .await
        .expect("task should join")
        .expect("retry succeeds");

    assert_eq!(response.text, "Hello");
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn openai_offline_500_response_triggers_retry_then_surfaces_typed_error() {
    tokio::time::pause();
    let server = MockServer::start().await;
    mount_always_status(
        &server,
        500,
        r#"{"error":{"message":"upstream unavailable","type":"server_error","code":"server_error"}}"#,
    )
    .await;
    let provider = provider(&server, 1);

    let task = tokio::spawn(async move {
        provider
            .complete(minimal_request("retry server error"))
            .await
            .expect("request should start")
            .collect()
            .await
    });
    tokio::task::yield_now().await;
    advance(Duration::from_secs(2)).await;
    let error = task
        .await
        .expect("task should join")
        .expect_err("500 should fail");

    assert!(
        matches!(error, MoaError::HttpStatus { status: 500, .. }),
        "expected typed HTTP 500, got {error:?}"
    );
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn openai_offline_malformed_json_response_returns_typed_parse_error() {
    let server = MockServer::start().await;
    mount_openai_sse(&server, MALFORMED, "malformed").await;
    let provider = provider(&server, 0);

    let error = provider
        .complete(minimal_request("malformed"))
        .await
        .expect("request should start")
        .collect()
        .await
        .expect_err("malformed SSE should fail");

    assert!(
        matches!(
            error,
            MoaError::SerializationError(_) | MoaError::StreamError(_) | MoaError::ProviderError(_)
        ),
        "expected typed parse/stream error, got {error:?}"
    );
}

#[tokio::test]
async fn openai_offline_streaming_disconnect_mid_response_surfaces_typed_error_with_partial_events()
{
    let server = MockServer::start().await;
    mount_openai_sse(&server, DISCONNECT, "disconnect").await;
    let provider = provider(&server, 0);

    let mut stream = provider
        .complete(minimal_request("disconnect"))
        .await
        .expect("request should start");
    let partial = timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("partial delta should arrive")
        .expect("partial delta present")
        .expect("partial delta ok");
    let error = stream
        .collect()
        .await
        .expect_err("truncated stream should fail");

    assert_eq!(partial, CompletionContent::Text("Hel".to_string()));
    assert!(
        matches!(error, MoaError::ProviderError(_)),
        "expected ProviderError, got {error:?}"
    );
}

fn provider(server: &MockServer, max_retries: usize) -> OpenAIProvider {
    OpenAIProvider::new("test-key", OPENAI_MODEL)
        .expect("provider config")
        .with_api_base(format!("{}/v1", server.uri()))
        .expect("test API base")
        .with_max_retries(max_retries)
}
