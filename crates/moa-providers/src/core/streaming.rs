//! Shared server-sent event parsing helpers for provider implementations.

use eventsource_stream::Event as SseEvent;
use moa_core::{error::MoaError, error::Result, types::completion::CompletionResponse};
use reqwest::{RequestBuilder, Response};
use serde::de::DeserializeOwned;

use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::rate_guard::RateGuard;
use crate::core::retry::RetryPolicy;

/// Drives the shared `transport` span phase for a reqwest-backed SSE provider:
/// records the phase, sends the request under the retry policy and the provider's
/// rate guard (retry budget + 429 cooldown), and marks a transport-stage failure
/// when the request ultimately fails.
pub(crate) async fn send_with_transport_phase<F>(
    span_recorder: &LLMSpanRecorder,
    retry_policy: &RetryPolicy,
    guard: &RateGuard,
    build_request: F,
) -> Result<Response>
where
    F: Fn() -> RequestBuilder,
{
    span_recorder.set_phase("transport");
    match retry_policy.send_gated(build_request, guard).await {
        Ok(response) => Ok(response),
        Err(error) => {
            span_recorder.fail_at_stage("transport", &error);
            Err(error)
        }
    }
}

/// Records the terminal span phase for a consumed SSE completion: finishes the
/// span on success, or marks a `stream`-stage failure on error.
pub(crate) fn finalize_streamed_completion(
    span_recorder: &LLMSpanRecorder,
    consumed: Result<CompletionResponse>,
) -> Result<CompletionResponse> {
    match consumed {
        Ok(response) => {
            span_recorder.set_phase("finalize");
            span_recorder.finish(&response);
            Ok(response)
        }
        Err(error) => {
            span_recorder.fail_at_stage("stream", &error);
            Err(error)
        }
    }
}

/// Parses a JSON SSE payload. Decode failures become recoverable
/// [`MoaError::ProviderQuirk`] so the orchestrator pauses the session
/// instead of killing it. Provider modules may still pre-filter via
/// their `is_ignorable_*` helpers before the quirk reaches the supervisor.
///
/// We deliberately log only metadata (event name, payload length, error)
/// — the raw payload may contain user prompts, tool arguments, or
/// other sensitive content and must not land in logs.
pub(crate) fn parse_sse_json<T>(event: &SseEvent) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&event.data).map_err(|error| {
        tracing::warn!(
            %error,
            event = %event.event,
            payload_bytes = event.data.len(),
            "SSE payload failed to deserialize; returning ProviderQuirk"
        );
        MoaError::ProviderQuirk(format!(
            "SSE event '{}' failed to parse: {error}",
            event.event
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Event as SseEvent;

    fn event(data: &str) -> SseEvent {
        SseEvent {
            event: "test".to_string(),
            data: data.to_string(),
            id: String::new(),
            retry: None,
        }
    }

    #[test]
    fn decode_failure_surfaces_provider_quirk() {
        let err = parse_sse_json::<serde_json::Value>(&event("{not json}")).expect_err("must fail");
        assert!(
            matches!(err, MoaError::ProviderQuirk(_)),
            "expected ProviderQuirk, got {err:?}"
        );
        assert!(!err.is_fatal(), "ProviderQuirk must be non-fatal");
    }
}
