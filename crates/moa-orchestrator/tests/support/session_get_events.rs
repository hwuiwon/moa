//! SessionStore `get_events` request fixture.

use moa_core::wire::session_store::GetEventsRequest;
use moa_core::{EventRange, SessionId};

/// Returns a request payload for `get_events`.
pub fn get_events_request(session_id: SessionId, range: EventRange) -> GetEventsRequest {
    GetEventsRequest { session_id, range }
}
