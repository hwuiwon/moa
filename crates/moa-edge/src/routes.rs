//! HTTP routes exposed by the MOA edge service.

use crate::proxy::OrchestratorProxy;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use moa_core::traits::{AuthProvider, Credential};
use std::sync::Arc;

/// Shared edge application state.
#[derive(Clone)]
pub struct AppState {
    /// Credential resolver used for incoming requests.
    pub auth: Arc<dyn AuthProvider>,
    /// Internal orchestrator proxy.
    pub proxy: Arc<OrchestratorProxy>,
}

/// Build the edge router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/{*rest}", any(handle_proxy))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn handle_proxy(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let Some(credential) = extract_credential(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing credential").into_response();
    };

    let identity = match state.auth.authenticate(&credential).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::info!(error = %error, provider = state.auth.name(), "authentication rejected");
            return (StatusCode::UNAUTHORIZED, "invalid credential").into_response();
        }
    };

    let path = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or(uri.path())
        .to_string();
    let response = match state
        .proxy
        .forward(&identity, method, &path, body.to_vec(), &headers)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "proxy forward failed");
            return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
        }
    };

    response_to_axum(response).await
}

async fn response_to_axum(response: reqwest::Response) -> axum::response::Response {
    let status = response.status();
    let headers = response.headers().clone();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(error = %error, "read upstream body failed");
            return (StatusCode::BAD_GATEWAY, "upstream read failed").into_response();
        }
    };

    let mut builder = axum::http::Response::builder().status(status);
    for (name, value) in &headers {
        let lowercase_name = name.as_str().to_ascii_lowercase();
        if matches!(
            lowercase_name.as_str(),
            "transfer-encoding" | "connection" | "keep-alive"
        ) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }

    match builder.body(Body::from(body)) {
        Ok(response) => response.into_response(),
        Err(error) => {
            tracing::error!(error = %error, "build downstream response failed");
            (StatusCode::BAD_GATEWAY, "response build failed").into_response()
        }
    }
}

fn extract_credential(headers: &HeaderMap) -> Option<Credential> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        return None;
    }
    if token.starts_with("moa_") {
        return Some(Credential::ApiKey(token.to_string()));
    }
    Some(Credential::BearerJwt(token.to_string()))
}
