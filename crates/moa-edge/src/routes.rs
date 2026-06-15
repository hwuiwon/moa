//! HTTP routes exposed by the MOA edge service.

use crate::proxy::OrchestratorProxy;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use moa_core::traits::{AuthProvider, Credential};
#[cfg(feature = "auth0")]
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[cfg(feature = "auth0")]
use hmac::{Hmac, Mac};
#[cfg(feature = "auth0")]
use sha2::Sha256;
#[cfg(feature = "auth0")]
use subtle::ConstantTimeEq;

/// Shared edge application state.
#[derive(Clone)]
pub struct AppState {
    /// Credential resolver used for incoming requests.
    pub auth: Arc<dyn AuthProvider>,
    /// Postgres pool used by unauthenticated webhooks that update auth metadata.
    pub pool: Arc<sqlx::PgPool>,
    /// Internal orchestrator proxy.
    pub proxy: Arc<OrchestratorProxy>,
}

/// Build the edge router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/v1/security/secret-scanning/github",
            post(handle_github_secret_scan),
        )
        .route(
            "/v1/webhooks/auth0/connection-linked",
            post(handle_auth0_connection_webhook),
        )
        .route("/v1/{*rest}", any(handle_proxy))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

#[tracing::instrument(
    skip(state, headers, body),
    fields(
        http.method = %method,
        http.target = %uri,
        http.route = "/v1/{*rest}",
        http.status_code = tracing::field::Empty,
        moa.edge.auth.provider = tracing::field::Empty,
        moa.edge.auth.result = tracing::field::Empty,
    )
)]
async fn handle_proxy(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let span = tracing::Span::current();
    let credential = match credential_for_request(state.auth.as_ref(), &headers) {
        Some(credential) => credential,
        None => {
            span.record("moa.edge.auth.result", "missing_credential");
            if let Err(error) = moa_ocsf::emit_authn_failure(
                &state.pool,
                Uuid::nil(),
                None,
                "unknown",
                source_ip(&headers),
                "missing credential",
            )
            .await
            {
                tracing::error!(error = %error, "security audit write failed for missing credential");
                span.record("http.status_code", 500_i64);
                return (StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response();
            }
            span.record("http.status_code", 401_i64);
            return (StatusCode::UNAUTHORIZED, "missing credential").into_response();
        }
    };

    let identity = match state.auth.authenticate(&credential).await {
        Ok(identity) => identity,
        Err(error) => {
            span.record(
                "moa.edge.auth.provider",
                tracing::field::display(state.auth.name()),
            );
            span.record("moa.edge.auth.result", "rejected");
            if let Err(audit_error) = moa_ocsf::emit_authn_failure(
                &state.pool,
                Uuid::nil(),
                None,
                state.auth.name(),
                source_ip(&headers),
                &error.to_string(),
            )
            .await
            {
                tracing::error!(
                    error = %audit_error,
                    auth_error = %error,
                    "security audit write failed for rejected credential"
                );
                span.record("http.status_code", 500_i64);
                return (StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response();
            }
            tracing::info!(error = %error, provider = state.auth.name(), "authentication rejected");
            span.record("http.status_code", 401_i64);
            return (StatusCode::UNAUTHORIZED, "invalid credential").into_response();
        }
    };
    span.record(
        "moa.edge.auth.provider",
        tracing::field::display(state.auth.name()),
    );
    span.record("moa.edge.auth.result", "accepted");
    if let Err(error) = moa_ocsf::emit_authn_success(
        &state.pool,
        identity.tenant_id,
        &identity,
        state.auth.name(),
        source_ip(&headers),
    )
    .await
    {
        tracing::error!(error = %error, "security audit write failed for authenticated request");
        span.record("http.status_code", 500_i64);
        return (StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response();
    }

    let original_path = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or(uri.path())
        .to_string();
    let (method, path, body) = match translate_public_route(&method, &uri, &body) {
        RouteTranslation::Forward { method, path, body } => (method, path, body),
        RouteTranslation::NoChange => (method, original_path, body.to_vec()),
        RouteTranslation::BadRequest(message) => {
            span.record("http.status_code", 400_i64);
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };
    let response = match state
        .proxy
        .forward(&identity, method, &path, body, &headers)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "proxy forward failed");
            span.record("http.status_code", 502_i64);
            return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
        }
    };

    span.record("http.status_code", response.status().as_u16() as i64);
    response_to_axum(response).await
}

fn credential_for_request(auth: &dyn AuthProvider, headers: &HeaderMap) -> Option<Credential> {
    extract_credential(headers)
        .or_else(|| (!auth.requires_credentials()).then(|| Credential::ApiKey(String::new())))
}

async fn handle_github_secret_scan() -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        [(
            "x-moa-reason",
            "not-yet-implemented-pending-github-partner-registration",
        )],
        "GitHub secret scanning partner endpoint is not implemented until registration is complete",
    )
        .into_response()
}

#[cfg(feature = "auth0")]
async fn handle_auth0_connection_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let secret = match std::env::var("MOA_AUTH_AUTH0_WEBHOOK_SECRET") {
        Ok(secret) if !secret.trim().is_empty() => secret,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook secret not configured",
            )
                .into_response();
        }
    };
    if !verify_auth0_signature(&headers, &body, &secret) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }
    let payload: Auth0ConnectionLinkedWebhook = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad webhook body").into_response(),
    };
    let user_id = match payload.user_id {
        Some(user_id) => user_id,
        None => {
            let Some(auth0_sub) = payload.auth0_sub.as_deref() else {
                return (StatusCode::BAD_REQUEST, "user_id or auth0_sub required").into_response();
            };
            match lookup_auth0_user_id(&state.pool, auth0_sub).await {
                Ok(Some(user_id)) => user_id,
                Ok(None) => {
                    return (StatusCode::NOT_FOUND, "auth0 user mapping not found").into_response();
                }
                Err(error) => {
                    tracing::error!(error = %error, "lookup auth0 user for connection webhook failed");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
                }
            }
        }
    };

    let result = sqlx::query(
        r#"
        INSERT INTO linked_connections
            (user_id, connection_name, scopes_granted, external_sub)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (user_id, connection_name)
        DO UPDATE SET
            scopes_granted = EXCLUDED.scopes_granted,
            external_sub = EXCLUDED.external_sub,
            linked_at = NOW()
        "#,
    )
    .bind(user_id)
    .bind(&payload.connection_name)
    .bind(&payload.scopes_granted)
    .bind(payload.external_sub.as_deref())
    .execute(&*state.pool)
    .await;
    match result {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(error) => {
            tracing::error!(error = %error, "upsert linked connection failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

#[cfg(not(feature = "auth0"))]
async fn handle_auth0_connection_webhook() -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Auth0 connection webhooks require the auth0 feature",
    )
        .into_response()
}

#[cfg(feature = "auth0")]
#[derive(Debug, Deserialize)]
struct Auth0ConnectionLinkedWebhook {
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    auth0_sub: Option<String>,
    connection_name: String,
    #[serde(default)]
    scopes_granted: Vec<String>,
    #[serde(default)]
    external_sub: Option<String>,
}

#[cfg(feature = "auth0")]
async fn lookup_auth0_user_id(
    pool: &sqlx::PgPool,
    auth0_sub: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT user_id FROM auth0_user_map WHERE sub = $1 LIMIT 1")
            .bind(auth0_sub)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(user_id,)| user_id))
}

#[cfg(feature = "auth0")]
fn verify_auth0_signature(headers: &HeaderMap, body: &[u8], secret: &str) -> bool {
    type HmacSha256 = Hmac<Sha256>;
    let signature = headers
        .get("auth0-signature")
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get("Auth0-Signature")
                .and_then(|value| value.to_str().ok())
        });
    let Some(signature) = signature else {
        return false;
    };
    let signature = signature
        .strip_prefix("sha256=")
        .unwrap_or(signature)
        .trim();
    let Ok(provided) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    expected.as_slice().ct_eq(provided.as_slice()).into()
}

fn source_ip(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
        })
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

enum RouteTranslation {
    NoChange,
    Forward {
        method: Method,
        path: String,
        body: Vec<u8>,
    },
    BadRequest(&'static str),
}

fn translate_public_route(method: &Method, uri: &Uri, body: &Bytes) -> RouteTranslation {
    if *method == Method::GET && uri.path() == "/v1/whoami" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Whoami/whoami".to_string(),
            body: Vec::new(),
        };
    }
    if *method == Method::GET && uri.path() == "/v1/approvals" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Approvals/list_mine".to_string(),
            body: Vec::new(),
        };
    }
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/approvals/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let approval_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return RouteTranslation::BadRequest("bad approval id"),
        };
        let mut value: serde_json::Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return RouteTranslation::BadRequest("bad decision body"),
        };
        let Some(object) = value.as_object_mut() else {
            return RouteTranslation::BadRequest("decision body must be object");
        };
        object.insert("id".to_string(), serde_json::json!(approval_id));
        let bytes = match serde_json::to_vec(&value) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "serialize approval decision body failed");
                return RouteTranslation::BadRequest("bad decision body");
            }
        };
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Approvals/decide".to_string(),
            body: bytes,
        };
    }
    if *method == Method::POST {
        match uri.path() {
            "/v1/analytics/session-stats" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/session_stats".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/analytics/workspace-stats" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/workspace_stats".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/analytics/tool-stats" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/tool_stats".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/analytics/cache-stats" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/cache_stats".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/plan" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/plan".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/suites/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/suites_list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/run" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/run".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/run-status" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/run_status".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/datasets/register" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/datasets_register".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/datasets/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/datasets_list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/replay" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/replay".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/scores" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/scores".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/evals/compare" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Eval/compare".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/promote-workspace" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/promote_workspace".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/rollback-promotion" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/rollback_promotion".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/finalize-promotion" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/finalize_promotion".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/checkpoints/create" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/checkpoint_create".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/checkpoints/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/checkpoint_list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/checkpoints/rollback" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/checkpoint_rollback".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/admin-maintenance/checkpoints/cleanup" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/AdminMaintenance/checkpoint_cleanup".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/memory/search" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Memory/search".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/memory/show" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Memory/show".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/memory/ingest-documents" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Memory/ingest_documents".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/memory/retrieve-debug" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Memory/retrieve_debug".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/lineage/explain" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LineageAdmin/explain".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/lineage/query" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LineageAdmin/query".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/lineage/export" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LineageAdmin/export".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/lineage/verify" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LineageAdmin/verify".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/lineage/erase" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LineageAdmin/erase".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/privacy/export" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Privacy/export".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/privacy/erase" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Privacy/erase".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/skills/export" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Skills/export".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/skills/import" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Skills/import".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/skills/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Skills/list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/skills/bootstrap-global" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Skills/bootstrap_global".to_string(),
                    body: body.to_vec(),
                };
            }
            _ => {}
        }
    }
    if *method == Method::POST && uri.path() == "/v1/agents" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Agents/register".to_string(),
            body: body.to_vec(),
        };
    }
    if *method == Method::GET && uri.path() == "/v1/agents" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Agents/list".to_string(),
            body: Vec::new(),
        };
    }
    if let Some(rest) = uri.path().strip_prefix("/v1/agents/") {
        if *method == Method::GET {
            return translate_uuid_path(rest, "/Agents/get");
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/deactivate")
        {
            return translate_uuid_path(id, "/Agents/deactivate");
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/can-act-as")
        {
            return translate_agent_act_as(id, body, "/Agents/grant_can_act_as");
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/revoke-can-act-as")
        {
            return translate_agent_act_as(id, body, "/Agents/revoke_can_act_as");
        }
    }
    if *method == Method::POST && uri.path() == "/v1/authz/tuple-write" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Authz/write_tuple".to_string(),
            body: body.to_vec(),
        };
    }
    RouteTranslation::NoChange
}

fn translate_uuid_path(id: &str, target: &str) -> RouteTranslation {
    let value = match Uuid::parse_str(id) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad id"),
    };
    let body = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "serialize UUID body failed");
            return RouteTranslation::BadRequest("bad id");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body,
    }
}

fn translate_agent_act_as(agent_id: &str, body: &Bytes, target: &str) -> RouteTranslation {
    let agent_id = match Uuid::parse_str(agent_id) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad agent id"),
    };
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad agent act-as body"),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest("agent act-as body must be object");
    };
    object.insert("agent_id".to_string(), serde_json::json!(agent_id));
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "serialize agent act-as body failed");
            return RouteTranslation::BadRequest("bad agent act-as body");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body: bytes,
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use axum::http::header::AUTHORIZATION;
    use moa_core::traits::{AuthError, Identity, IdentityType};

    use super::*;

    struct StrictAuth;

    #[async_trait]
    impl AuthProvider for StrictAuth {
        async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
            Err(AuthError::Rejected)
        }

        fn name(&self) -> &'static str {
            "strict"
        }
    }

    struct DisabledAuth;

    #[async_trait]
    impl AuthProvider for DisabledAuth {
        async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
            Ok(Identity {
                identity_type: IdentityType::Service,
                id: Uuid::nil(),
                tenant_id: Uuid::nil(),
                api_key_id: None,
                acting_on_behalf_of: None,
            })
        }

        fn name(&self) -> &'static str {
            "disabled"
        }

        fn requires_credentials(&self) -> bool {
            false
        }
    }

    #[test]
    fn strict_auth_requires_authorization_header() {
        // Pins: normal auth providers still reject requests before authentication when no credential is present.
        let headers = HeaderMap::new();

        assert!(credential_for_request(&StrictAuth, &headers).is_none());
    }

    #[test]
    fn disabled_auth_allows_missing_authorization_header() {
        // Pins: auth.provider=disabled can pass through edge requests with no Authorization header.
        let headers = HeaderMap::new();

        assert_eq!(
            credential_for_request(&DisabledAuth, &headers),
            Some(Credential::ApiKey(String::new()))
        );
    }

    #[test]
    fn authorization_header_wins_when_disabled_auth_is_configured() {
        // Pins: disabled auth still forwards an explicitly supplied credential when present.
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            "Bearer moa_dev_example"
                .parse()
                .expect("test auth header should parse"),
        );

        assert_eq!(
            credential_for_request(&DisabledAuth, &headers),
            Some(Credential::ApiKey("moa_dev_example".to_string()))
        );
    }

    #[test]
    fn whoami_public_route_translates_to_restate_handler() {
        // Pins: hosted identity inspection stays available through the public edge API.
        let uri = "/v1/whoami"
            .parse::<Uri>()
            .expect("route path should parse");

        let translation = translate_public_route(&Method::GET, &uri, &Bytes::new());

        match translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Whoami/whoami");
                assert!(
                    body.is_empty(),
                    "whoami should not synthesize a request body"
                );
            }
            RouteTranslation::NoChange => panic!("whoami should translate to Whoami service"),
            RouteTranslation::BadRequest(message) => {
                panic!("whoami should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn approval_public_routes_translate_to_restate_handlers() {
        // Pins: builtin approval list and decision actions stay available through public edge APIs.
        let list_uri = "/v1/approvals"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate_public_route(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Approvals/list_mine");
                assert!(
                    body.is_empty(),
                    "approval list should not synthesize a request body"
                );
            }
            RouteTranslation::NoChange => {
                panic!("approval list should translate to Approvals service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("approval list should not fail translation: {message}")
            }
        }

        let decision_uri = "/v1/approvals/11111111-1111-1111-1111-111111111111/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"outcome":"approved","reason":null}"#);
        let decision_translation =
            translate_public_route(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Approvals/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "id": "11111111-1111-1111-1111-111111111111",
                        "outcome": "approved",
                        "reason": null
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("approval decision should translate to Approvals service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("approval decision should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn analytics_public_routes_translate_to_restate_handlers() {
        // Pins: hosted analytics edge routes forward to the internal Analytics service paths.
        let cases = [
            (
                "/v1/analytics/session-stats",
                "/Analytics/session_stats",
                r#"{"session_id":"11111111-1111-1111-1111-111111111111"}"#,
            ),
            (
                "/v1/analytics/workspace-stats",
                "/Analytics/workspace_stats",
                r#"{"workspace_id":"workspace-a","days":14}"#,
            ),
            (
                "/v1/analytics/tool-stats",
                "/Analytics/tool_stats",
                r#"{"workspace_id":"workspace-a"}"#,
            ),
            (
                "/v1/analytics/cache-stats",
                "/Analytics/cache_stats",
                r#"{"workspace_id":"workspace-a","days":7}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn eval_public_routes_translate_to_restate_handlers() {
        // Pins: hosted eval edge routes forward to the internal Eval service paths.
        let cases = [
            (
                "/v1/evals/plan",
                "/Eval/plan",
                r#"{"workspace_id":"workspace-a","suite_document":"[suite]\nname=\"s\"","config_documents":[]}"#,
            ),
            (
                "/v1/evals/suites/list",
                "/Eval/suites_list",
                r#"{"workspace_id":"workspace-a","documents":[{"source":"suite.toml","body":"[suite]\nname=\"s\""}]}"#,
            ),
            (
                "/v1/evals/run",
                "/Eval/run",
                r#"{"workspace_id":"workspace-a","suite_document":"[suite]\nname=\"s\"","config_documents":[]}"#,
            ),
            (
                "/v1/evals/run-status",
                "/Eval/run_status",
                r#"{"workspace_id":"workspace-a","run_id":"11111111-1111-1111-1111-111111111111"}"#,
            ),
            (
                "/v1/evals/datasets/register",
                "/Eval/datasets_register",
                r#"{"workspace_id":"workspace-a","name":"golden","jsonl":"{}"}"#,
            ),
            (
                "/v1/evals/datasets/list",
                "/Eval/datasets_list",
                r#"{"workspace_id":"workspace-a"}"#,
            ),
            (
                "/v1/evals/replay",
                "/Eval/replay",
                r#"{"workspace_id":"workspace-a","dataset_id":"22222222-2222-2222-2222-222222222222"}"#,
            ),
            (
                "/v1/evals/scores",
                "/Eval/scores",
                r#"{"workspace_id":"workspace-a","run_id":"33333333-3333-3333-3333-333333333333"}"#,
            ),
            (
                "/v1/evals/compare",
                "/Eval/compare",
                r#"{"workspace_id":"workspace-a","base_run":"33333333-3333-3333-3333-333333333333","new_run":"44444444-4444-4444-4444-444444444444"}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn admin_maintenance_public_routes_translate_to_restate_handlers() {
        // Pins: hosted admin-maintenance routes forward to the internal AdminMaintenance service paths.
        let cases = [
            (
                "/v1/admin-maintenance/promote-workspace",
                "/AdminMaintenance/promote_workspace",
                r#"{"workspace_id":"workspace-a","target_backend":"turbopuffer","validate_percent":5,"dual_read_hours":24}"#,
            ),
            (
                "/v1/admin-maintenance/rollback-promotion",
                "/AdminMaintenance/rollback_promotion",
                r#"{"workspace_id":"workspace-a","action":"rollback"}"#,
            ),
            (
                "/v1/admin-maintenance/finalize-promotion",
                "/AdminMaintenance/finalize_promotion",
                r#"{"workspace_id":"workspace-a","action":"finalize"}"#,
            ),
            (
                "/v1/admin-maintenance/checkpoints/create",
                "/AdminMaintenance/checkpoint_create",
                r#"{"label":"before-deploy","session_id":null}"#,
            ),
            (
                "/v1/admin-maintenance/checkpoints/list",
                "/AdminMaintenance/checkpoint_list",
                r#"{}"#,
            ),
            (
                "/v1/admin-maintenance/checkpoints/rollback",
                "/AdminMaintenance/checkpoint_rollback",
                r#"{"id":"br-checkpoint"}"#,
            ),
            (
                "/v1/admin-maintenance/checkpoints/cleanup",
                "/AdminMaintenance/checkpoint_cleanup",
                r#"{}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn memory_public_routes_translate_to_restate_handlers() {
        // Pins: hosted memory edge routes forward to the internal Memory service paths.
        let cases = [
            (
                "/v1/memory/search",
                "/Memory/search",
                r#"{"workspace_id":"workspace-a","query":"auth","limit":10}"#,
            ),
            (
                "/v1/memory/show",
                "/Memory/show",
                r#"{"workspace_id":"workspace-a","uid":"22222222-2222-2222-2222-222222222222"}"#,
            ),
            (
                "/v1/memory/ingest-documents",
                "/Memory/ingest_documents",
                r#"{"workspace_id":"workspace-a","documents":[{"source_name":"Auth","content":"Fact: auth uses JWT"}]}"#,
            ),
            (
                "/v1/memory/retrieve-debug",
                "/Memory/retrieve_debug",
                r#"{"workspace_id":"workspace-a","query":"auth","limit":5,"no_flush_wait":true}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn lineage_and_privacy_public_routes_translate_to_restate_handlers() {
        // Pins: hosted lineage/privacy edge routes forward to internal Restate service paths.
        let cases = [
            (
                "/v1/lineage/explain",
                "/LineageAdmin/explain",
                r#"{"workspace_id":"workspace-a","id":"11111111-1111-1111-1111-111111111111"}"#,
            ),
            (
                "/v1/lineage/query",
                "/LineageAdmin/query",
                r#"{"workspace_id":"workspace-a","sql":"SELECT count(*) FROM lineage","since":"24 hours"}"#,
            ),
            (
                "/v1/lineage/export",
                "/LineageAdmin/export",
                r#"{"workspace_id":"workspace-a","subject":"subject-a"}"#,
            ),
            (
                "/v1/lineage/verify",
                "/LineageAdmin/verify",
                r#"{"workspace_id":"workspace-a","window":"hot","since":"24 hours"}"#,
            ),
            (
                "/v1/lineage/erase",
                "/LineageAdmin/erase",
                r#"{"workspace_id":"workspace-a","subject":"00ff"}"#,
            ),
            (
                "/v1/privacy/export",
                "/Privacy/export",
                r#"{"workspace_id":"workspace-a","subject_user_id":"22222222-2222-2222-2222-222222222222","reason":"GDPR","approval_token":"token"}"#,
            ),
            (
                "/v1/privacy/erase",
                "/Privacy/erase",
                r#"{"workspace_id":"workspace-a","subject_user_id":"22222222-2222-2222-2222-222222222222","reason":"GDPR","approval_token":"token"}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn skills_public_routes_translate_to_restate_handlers() {
        // Pins: hosted skills edge routes forward to the internal Skills service paths.
        let cases = [
            (
                "/v1/skills/export",
                "/Skills/export",
                r#"{"workspace_id":"workspace-a"}"#,
            ),
            (
                "/v1/skills/import",
                "/Skills/import",
                r#"{"workspace_id":"workspace-a","scope":{"kind":"workspace","workspace_id":"workspace-a"},"documents":[]}"#,
            ),
            (
                "/v1/skills/list",
                "/Skills/list",
                r#"{"workspace_id":"workspace-a"}"#,
            ),
            (
                "/v1/skills/bootstrap-global",
                "/Skills/bootstrap_global",
                r#"{"documents":[]}"#,
            ),
        ];

        for (public_path, internal_path, json_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(json_body.as_bytes().to_vec());

            let translation = translate_public_route(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    assert_eq!(
                        forwarded_body,
                        json_body.as_bytes(),
                        "{public_path} body should pass through unchanged"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn agent_public_routes_translate_to_restate_handlers() {
        // Pins: public agent lifecycle routes forward to the remaining Agents service.
        let body = Bytes::from_static(br#"{"display_name":"reviewer"}"#);
        let create_uri = "/v1/agents"
            .parse::<Uri>()
            .expect("agent register path should parse");
        match translate_public_route(&Method::POST, &create_uri, &body) {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Agents/register");
                assert_eq!(forwarded_body, body.to_vec());
            }
            RouteTranslation::NoChange => panic!("agent register should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent register should not fail translation: {message}")
            }
        }

        let list_uri = "/v1/agents"
            .parse::<Uri>()
            .expect("agent list path should parse");
        match translate_public_route(&Method::GET, &list_uri, &Bytes::new()) {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Agents/list");
                assert!(body.is_empty(), "agent list should not synthesize a body");
            }
            RouteTranslation::NoChange => panic!("agent list should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent list should not fail translation: {message}")
            }
        }

        let agent_id = "11111111-1111-1111-1111-111111111111";
        let uuid_cases = [
            (Method::GET, format!("/v1/agents/{agent_id}"), "/Agents/get"),
            (
                Method::POST,
                format!("/v1/agents/{agent_id}/deactivate"),
                "/Agents/deactivate",
            ),
        ];
        for (method, public_path, internal_path) in uuid_cases {
            let uri = public_path.parse::<Uri>().expect("agent path should parse");
            match translate_public_route(&method, &uri, &Bytes::new()) {
                RouteTranslation::Forward { method, path, body } => {
                    assert_eq!(method, Method::POST);
                    assert_eq!(path, internal_path);
                    let forwarded: Uuid =
                        serde_json::from_slice(&body).expect("forwarded UUID should parse");
                    assert_eq!(
                        forwarded,
                        Uuid::parse_str(agent_id).expect("agent fixture UUID should parse")
                    );
                }
                RouteTranslation::NoChange => panic!("{public_path} should translate"),
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }

        let user_id = "22222222-2222-2222-2222-222222222222";
        let act_as_body = Bytes::from(format!(r#"{{"user_id":"{user_id}"}}"#));
        let act_as_cases = [
            (
                format!("/v1/agents/{agent_id}/can-act-as"),
                "/Agents/grant_can_act_as",
            ),
            (
                format!("/v1/agents/{agent_id}/revoke-can-act-as"),
                "/Agents/revoke_can_act_as",
            ),
        ];
        for (public_path, internal_path) in act_as_cases {
            let uri = public_path.parse::<Uri>().expect("agent path should parse");
            match translate_public_route(&Method::POST, &uri, &act_as_body) {
                RouteTranslation::Forward { method, path, body } => {
                    assert_eq!(method, Method::POST);
                    assert_eq!(path, internal_path);
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&body).expect("forwarded act-as body should parse");
                    assert_eq!(
                        forwarded,
                        serde_json::json!({
                            "agent_id": agent_id,
                            "user_id": user_id
                        })
                    );
                }
                RouteTranslation::NoChange => panic!("{public_path} should translate"),
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }
}
