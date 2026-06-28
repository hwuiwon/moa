//! Direct edge identity diagnostic route.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use super::{AppState, authenticate_direct_request};

/// Returns the authenticated caller identity resolved at the edge.
#[tracing::instrument(skip(state, headers))]
// SAFETY: Informational identity diagnostic; authentication rejects unauthenticated requests first.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authenticate_direct_request(&state, &headers, "/v1/whoami").await {
        Ok(identity) => Json(identity).into_response(),
        Err(response) => response,
    }
}
