//! SessionStore `append_event` request fixture.

use moa_core::wire::session_store::AppendEventRequest;
use moa_core::{Event, SessionId};

/// Returns a request payload for `append_event`.
pub fn append_event_request(session_id: SessionId, event: Event) -> AppendEventRequest {
    AppendEventRequest { session_id, event }
}
