//! Shared server-sent event parsing helpers for provider implementations.

use std::{pin::Pin, time::Duration};

use eventsource_stream::Event as SseEvent;
use futures_util::{Stream, StreamExt};
use moa_config::ProviderStreamTimeoutConfig;
use moa_core::{error::MoaError, error::Result, types::completion::CompletionResponse};
use reqwest::{RequestBuilder, Response};
use serde::de::DeserializeOwned;

use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::pacer::RatePacer;
use crate::core::rate_guard::RateGuard;
use crate::core::retry::RetryPolicy;

/// Stateful first-event, idle-gap, and total deadline tracker for one provider stream.
pub(crate) struct StreamDeadline {
    config: ProviderStreamTimeoutConfig,
    started_at: tokio::time::Instant,
    waiting_for_first_event: bool,
}

impl StreamDeadline {
    /// Starts a deadline tracker from the current Tokio clock.
    pub(crate) fn new(config: ProviderStreamTimeoutConfig) -> Self {
        Self {
            config,
            started_at: tokio::time::Instant::now(),
            waiting_for_first_event: true,
        }
    }

    /// Returns the next stream item or a typed timeout error for the deadline reached.
    pub(crate) async fn next<S>(&mut self, mut stream: Pin<&mut S>) -> Result<Option<S::Item>>
    where
        S: Stream + ?Sized,
    {
        let total = Duration::from_millis(self.config.total_ms);
        let Some(total_remaining) = total.checked_sub(self.started_at.elapsed()) else {
            return Err(stream_timeout("total", self.config.total_ms));
        };
        let (phase, phase_ms) = if self.waiting_for_first_event {
            ("first-byte", self.config.first_byte_ms)
        } else {
            ("idle", self.config.idle_ms)
        };
        let phase_timeout = Duration::from_millis(phase_ms);
        let wait = phase_timeout.min(total_remaining);

        match tokio::time::timeout(wait, stream.next()).await {
            Ok(item) => {
                if item.is_some() {
                    self.waiting_for_first_event = false;
                }
                Ok(item)
            }
            Err(_) if total_remaining <= phase_timeout => {
                Err(stream_timeout("total", self.config.total_ms))
            }
            Err(_) => Err(stream_timeout(phase, phase_ms)),
        }
    }
}

fn stream_timeout(phase: &str, timeout_ms: u64) -> MoaError {
    MoaError::ProviderTimeout(format!(
        "provider stream {phase} timeout after {timeout_ms}ms"
    ))
}

/// Drives the shared `transport` span phase for a reqwest-backed SSE provider:
/// records the phase, sends the request under the retry policy and the provider's
/// rate guard (retry budget + 429 cooldown), and marks a transport-stage failure
/// when the request ultimately fails.
pub(crate) async fn send_with_transport_phase<F>(
    span_recorder: &LLMSpanRecorder,
    retry_policy: &RetryPolicy,
    guard: &RateGuard,
    pacer: &RatePacer,
    model: &str,
    build_request: F,
) -> Result<Response>
where
    F: Fn() -> RequestBuilder,
{
    span_recorder.set_phase("transport");
    match retry_policy
        .send_gated(build_request, guard, pacer, model)
        .await
    {
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
    use futures_util::{pin_mut, stream};

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

    #[tokio::test(start_paused = true)]
    async fn stream_deadline_distinguishes_first_byte_idle_and_total_timeouts() {
        // Pins: stalled streams release their provider task with the exact
        // deadline class operators need to diagnose permit exhaustion.
        let config = ProviderStreamTimeoutConfig {
            first_byte_ms: 10,
            idle_ms: 20,
            total_ms: 50,
        };

        let first_byte_stream = stream::pending::<usize>();
        pin_mut!(first_byte_stream);
        let first_byte_error = StreamDeadline::new(config)
            .next(first_byte_stream.as_mut())
            .await
            .expect_err("a stream with no first event must time out");
        assert!(
            matches!(first_byte_error, MoaError::ProviderTimeout(message) if message.contains("first-byte timeout after 10ms"))
        );

        let idle_stream = stream::iter([1usize]).chain(stream::pending());
        pin_mut!(idle_stream);
        let mut idle_deadline = StreamDeadline::new(config);
        assert_eq!(
            idle_deadline
                .next(idle_stream.as_mut())
                .await
                .expect("first event should arrive"),
            Some(1)
        );
        let idle_error = idle_deadline
            .next(idle_stream.as_mut())
            .await
            .expect_err("a stream that stalls after one event must time out");
        assert!(
            matches!(idle_error, MoaError::ProviderTimeout(message) if message.contains("idle timeout after 20ms"))
        );

        let total_config = ProviderStreamTimeoutConfig {
            first_byte_ms: 10,
            idle_ms: 10,
            total_ms: 12,
        };
        let total_stream = stream::unfold(0usize, |value| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((value, value + 1))
        });
        pin_mut!(total_stream);
        let mut total_deadline = StreamDeadline::new(total_config);
        assert_eq!(
            total_deadline
                .next(total_stream.as_mut())
                .await
                .expect("first event should arrive before total deadline"),
            Some(0)
        );
        assert_eq!(
            total_deadline
                .next(total_stream.as_mut())
                .await
                .expect("second event should arrive before total deadline"),
            Some(1)
        );
        let total_error = total_deadline
            .next(total_stream.as_mut())
            .await
            .expect_err("frequent events must not extend the total deadline");
        assert!(
            matches!(total_error, MoaError::ProviderTimeout(message) if message.contains("total timeout after 12ms"))
        );
    }
}
