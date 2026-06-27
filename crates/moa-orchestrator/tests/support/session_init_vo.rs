//! SessionStore `init_session_vo` request fixture.

use moa_core::wire::session_store::InitSessionVoRequest;
use moa_core::{SessionId, SessionMeta};

/// Returns a request payload for `init_session_vo`.
pub fn init_session_vo_request(session_id: SessionId, meta: SessionMeta) -> InitSessionVoRequest {
    InitSessionVoRequest { session_id, meta }
}
