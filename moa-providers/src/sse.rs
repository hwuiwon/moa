//! Shared server-sent event parsing helpers for provider implementations.

use eventsource_stream::Event as SseEvent;
use moa_core::{MoaError, Result};
use serde::de::DeserializeOwned;

/// Parses a JSON SSE payload into a strongly typed Rust value.
pub(crate) fn parse_sse_json<T>(event: &SseEvent) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&event.data).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to parse SSE payload for event '{}': {error}",
            event.event
        ))
    })
}
