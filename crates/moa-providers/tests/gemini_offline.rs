mod support {
    pub mod gemini_wiremock;
    pub mod wiremock_common;
}

use std::time::Duration;

use moa_core::{
    CompletionContent, LLMProvider, MoaError, StopReason, ToolCallContent, ToolInvocation,
};
use moa_providers::GeminiProvider;
use serde_json::json;
use tokio::time::{advance, timeout};
use wiremock::MockServer;

use support::gemini_wiremock::{GEMINI_MODEL, mount_gemini_sse};
use support::wiremock_common::{
    minimal_request, mount_always_status, mount_retry_then_sse, request_count, tool_request,
};

const TEXT: &str = include_str!("support/fixtures/sse/gemini_text.sse");
const TEXT_SPLIT: &str = include_str!("support/fixtures/sse/gemini_text_split.sse");
const TOOL_CALL: &str = include_str!("support/fixtures/sse/gemini_tool_call.sse");
const MALFORMED: &str = include_str!("support/fixtures/sse/gemini_malformed.sse");
const DISCONNECT: &str = include_str!("support/fixtures/sse/gemini_disconnect.sse");
const MAX_TOKENS: &str = include_str!("support/fixtures/sse/gemini_max_tokens.sse");
const UNKNOWN_STOP: &str = include_str!("support/fixtures/sse/gemini_unknown_stop.sse");
const EMPTY: &str = include_str!("support/fixtures/sse/gemini_empty.sse");

#[tokio::test]
async fn gemini_offline_completion_returns_text_for_minimal_request() {
    let server = MockServer::start().await;
    mount_gemini_sse(&server, TEXT, "What is 2+2?").await;
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
async fn gemini_offline_streaming_yields_text_deltas_then_terminal_event() {
    let server = MockServer::start().await;
    mount_gemini_sse(&server, TEXT_SPLIT, "Count from 1 to 5").await;
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
async fn gemini_offline_tool_call_response_parses_into_provider_event() {
    let server = MockServer::start().await;
    mount_gemini_sse(&server, TOOL_CALL, "emit_token").await;
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
                id: Some("fc_stream_1".to_string()),
                name: "emit_token".to_string(),
                input: json!({ "token": "OFFLINE" }),
            },
            provider_metadata: Some(moa_core::ProviderToolCallMetadata::Gemini {
                thought_signature: "sig_stream_1".to_string(),
            }),
        })
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn gemini_offline_429_response_triggers_retry_with_backoff() {
    tokio::time::pause();
    let server = MockServer::start().await;
    mount_retry_then_sse(&server, 429, r#"{"error":{"message":"rate limit"}}"#, TEXT).await;
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
async fn gemini_offline_500_response_triggers_retry_then_surfaces_typed_error() {
    tokio::time::pause();
    let server = MockServer::start().await;
    mount_always_status(
        &server,
        500,
        r#"{"error":{"message":"upstream unavailable"}}"#,
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
async fn gemini_offline_malformed_json_response_returns_typed_parse_error() {
    let server = MockServer::start().await;
    mount_gemini_sse(&server, MALFORMED, "malformed").await;
    let provider = provider(&server, 0);

    let error = provider
        .complete(minimal_request("malformed"))
        .await
        .expect("request should start")
        .collect()
        .await
        .expect_err("malformed SSE should fail");

    assert!(
        matches!(error, MoaError::ProviderQuirk(_)),
        "expected ProviderQuirk parse error, got {error:?}"
    );
}

#[tokio::test]
async fn gemini_offline_streaming_disconnect_mid_response_surfaces_typed_error_with_partial_events()
{
    let server = MockServer::start().await;
    mount_gemini_sse(&server, DISCONNECT, "disconnect").await;
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
        matches!(error, MoaError::StreamError(_)),
        "expected StreamError, got {error:?}"
    );
}

#[tokio::test]
async fn gemini_offline_max_tokens_finish_reason_maps_to_max_tokens() {
    // Pins: a MAX_TOKENS finishReason surfaces as StopReason::MaxTokens with usage intact.
    let server = MockServer::start().await;
    mount_gemini_sse(&server, MAX_TOKENS, "truncate me").await;
    let provider = provider(&server, 0);

    let response = provider
        .complete(minimal_request("truncate me"))
        .await
        .expect("request should start")
        .collect()
        .await
        .expect("stream should complete");

    assert_eq!(response.text, "Partial");
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.usage.output_tokens, 64);
    assert_eq!(response.usage.input_tokens_cache_read, 4);
}

#[tokio::test]
async fn gemini_offline_unknown_finish_reason_maps_to_other() {
    // Pins: an unmodeled finishReason is preserved verbatim as StopReason::Other.
    let server = MockServer::start().await;
    mount_gemini_sse(&server, UNKNOWN_STOP, "stop oddly").await;
    let provider = provider(&server, 0);

    let response = provider
        .complete(minimal_request("stop oddly"))
        .await
        .expect("request should start")
        .collect()
        .await
        .expect("stream should complete");

    assert_eq!(
        response.stop_reason,
        StopReason::Other("SAFETY".to_string())
    );
}

#[tokio::test]
async fn gemini_offline_empty_candidate_yields_empty_text_with_usage() {
    // Pins: a terminal candidate with no parts returns empty text but real usage/stop.
    let server = MockServer::start().await;
    mount_gemini_sse(&server, EMPTY, "say nothing").await;
    let provider = provider(&server, 0);

    let response = provider
        .complete(minimal_request("say nothing"))
        .await
        .expect("request should start")
        .collect()
        .await
        .expect("stream should complete");

    assert_eq!(response.text, "");
    assert!(response.content.is_empty());
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens_uncached, 4);
    assert_eq!(response.usage.input_tokens_cache_read, 3);
    assert_eq!(response.usage.output_tokens, 0);
}

#[tokio::test]
async fn gemini_offline_exhausted_429_surfaces_rate_limited_error() {
    // Pins: a 429 that survives every retry surfaces typed RateLimited, not HttpStatus{429}.
    tokio::time::pause();
    let server = MockServer::start().await;
    mount_always_status(&server, 429, r#"{"error":{"message":"rate limit"}}"#).await;
    let provider = provider(&server, 1);

    let task = tokio::spawn(async move {
        provider
            .complete(minimal_request("retry until exhausted"))
            .await
            .expect("request should start")
            .collect()
            .await
    });
    tokio::task::yield_now().await;
    advance(Duration::from_secs(5)).await;
    let error = task
        .await
        .expect("task should join")
        .expect_err("exhausted 429 should fail");

    assert!(
        matches!(error, MoaError::RateLimited { .. }),
        "expected typed RateLimited, got {error:?}"
    );
    assert_eq!(request_count(&server).await, 2);
}

fn provider(server: &MockServer, max_retries: usize) -> GeminiProvider {
    GeminiProvider::new("test-key", GEMINI_MODEL)
        .expect("provider config")
        .with_api_base(format!("{}/v1beta", server.uri()))
        .with_max_retries(max_retries)
}
