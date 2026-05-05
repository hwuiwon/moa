//! Shared server-sent event parsing helpers for provider implementations.

use eventsource_stream::Event as SseEvent;
use moa_core::{MoaError, Result};
use serde::de::DeserializeOwned;

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
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Probe {
        #[allow(dead_code)]
        ok: bool,
    }

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
        let err = parse_sse_json::<Probe>(&event("{not json}")).expect_err("must fail");
        assert!(
            matches!(err, MoaError::ProviderQuirk(_)),
            "expected ProviderQuirk, got {err:?}"
        );
        assert!(!err.is_fatal(), "ProviderQuirk must be non-fatal");
    }
}
