//! Shared server-sent event parsing helpers for provider implementations.

use eventsource_stream::Event as SseEvent;
use moa_core::{MoaError, Result};
use serde::de::DeserializeOwned;

const RAW_PREVIEW_MAX: usize = 240;

/// Parses a JSON SSE payload into a strongly typed Rust value.
///
/// A decode failure here is treated as a provider quirk: the raw chunk is
/// logged at WARN and surfaced as [`MoaError::ProviderQuirk`] so the
/// orchestrator can pause the session instead of killing it. Individual
/// providers may still choose to `is_ignorable_*` filter these quirks
/// before they reach the supervisor.
pub(crate) fn parse_sse_json<T>(event: &SseEvent) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&event.data).map_err(|error| {
        let preview = preview(&event.data);
        tracing::warn!(
            %error,
            event = %event.event,
            raw_preview = %preview,
            "SSE payload failed to deserialize; returning ProviderQuirk"
        );
        MoaError::ProviderQuirk(format!(
            "SSE event '{}' failed to parse: {error}",
            event.event
        ))
    })
}

fn preview(raw: &str) -> &str {
    if raw.len() <= RAW_PREVIEW_MAX {
        raw
    } else {
        // Find the largest char boundary <= RAW_PREVIEW_MAX to avoid slicing
        // in the middle of a multi-byte character.
        let mut cut = RAW_PREVIEW_MAX;
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        &raw[..cut]
    }
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

    #[test]
    fn preview_respects_char_boundaries() {
        // 3-byte UTF-8 character ("…") straddling the cut point.
        let long: String = "…".repeat(100);
        let p = preview(&long);
        assert!(p.is_char_boundary(p.len()));
        assert!(p.len() <= RAW_PREVIEW_MAX);
    }
}
