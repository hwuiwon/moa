//! Public asynchronous-provider callback route.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::external_job_callback_proxy::{
    ExternalJobCallbackProxyError, ExternalJobCallbackSelector,
};

use super::AppState;

/// Path selectors outside the provider-controlled callback body.
#[derive(Debug, Deserialize)]
pub(crate) struct ExternalJobCallbackPath {
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: String,
}

/// Forwards one bounded opaque provider callback to the private ingress.
// SAFETY: provider authentication is enforced on the raw callback before its bytes are parsed or persisted.
pub(crate) async fn handle_callback(
    State(state): State<AppState>,
    Path(path): Path<ExternalJobCallbackPath>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let selector = ExternalJobCallbackSelector {
        external_job_uid: path.external_job_uid,
        job_generation: path.job_generation,
        provider_event_id: path.provider_event_id,
    };
    match state
        .external_job_callbacks
        .forward(&selector, &headers, body)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => callback_error_response(error),
    }
}

fn callback_error_response(error: ExternalJobCallbackProxyError) -> Response {
    let status = match error {
        ExternalJobCallbackProxyError::InvalidRequest => StatusCode::BAD_REQUEST,
        ExternalJobCallbackProxyError::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ExternalJobCallbackProxyError::Transport => StatusCode::SERVICE_UNAVAILABLE,
        ExternalJobCallbackProxyError::Rejected { status } => status,
        ExternalJobCallbackProxyError::InvalidResponse => StatusCode::BAD_GATEWAY,
    };
    status.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_errors_never_expose_upstream_or_provider_material() {
        // Pins: public errors are status-only even when the private boundary
        // rejects authentication or violates its response contract.
        assert_eq!(
            callback_error_response(ExternalJobCallbackProxyError::Rejected {
                status: StatusCode::UNAUTHORIZED,
            })
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            callback_error_response(ExternalJobCallbackProxyError::InvalidResponse).status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
