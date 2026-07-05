//! Direct edge routes for the operator dashboard.

use axum::Router;
use axum::routing::get;

use super::AppState;

mod sessions;

/// Builds the operator dashboard route subtree.
pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/dashboard/sessions", get(sessions::list_sessions))
        .route(
            "/v1/dashboard/sessions/{session_id}",
            get(sessions::get_session),
        )
        .route(
            "/v1/dashboard/sessions/{session_id}/events",
            get(sessions::list_events),
        )
}
