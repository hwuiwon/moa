//! Session event search response assembly for analytics.

use moa_core::EventRecord;
use moa_core::wire::analytics::{SessionSearchRequest, SessionSearchResponse, SessionSearchResult};

use super::redaction::redacted_event_snippet;

/// Converts event search records into redacted public search results.
#[must_use]
pub fn session_search_response_from_events(
    request: SessionSearchRequest,
    events: Vec<EventRecord>,
) -> SessionSearchResponse {
    SessionSearchResponse {
        tenant_id: request.tenant_id,
        query: request.query,
        results: events
            .iter()
            .map(|event| SessionSearchResult {
                session_id: event.session_id,
                event_id: event.id,
                sequence_num: event.sequence_num,
                event_type: event.event_type,
                timestamp: event.timestamp,
                snippet: redacted_event_snippet(&event.event),
            })
            .collect(),
    }
}
