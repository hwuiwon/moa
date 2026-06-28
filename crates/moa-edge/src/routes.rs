//! HTTP routes exposed by the MOA edge service.
#![allow(clippy::result_large_err)]

use crate::{headers, proxy::OrchestratorProxy};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{any, get, patch, post};
use base64::{Engine as _, engine::general_purpose};
use futures_util::stream;
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{AuthProvider, Credential, Identity};
use moa_core::wire::turn::SessionProgress;
use moa_core::{
    Attachment, ContactSessionAuthorizationRequest, ContactSessionAuthorizationResponse,
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    Event, EventRange, EventRecord, MAX_CONTACT_SESSION_ATTACHMENT_BYTES,
    MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES, MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES,
    MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE, MoaConfig, MoaError, SequenceNum,
    SessionAttachmentId, SessionAttachmentStore, SessionId, SessionStore, TenantId,
    normalize_contact_session_photo_mime, validate_contact_session_message_text,
};
use moa_session::PostgresSessionStore;
#[cfg(feature = "auth0")]
use serde::Deserialize;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES: usize = 256 * 1024;
const SESSION_MESSAGE_BODY_LIMIT_BYTES: usize = 12 * 1024 * 1024;
const MAX_SESSION_PHOTO_DIMENSION: u32 = 12_000;
const MAX_SESSION_PHOTO_PIXELS: u64 = 25_000_000;

mod agents;
mod analytics;
mod artifacts;
mod audit;
mod auth;
mod knowledge;
mod lineage;
mod memory;
mod session;
mod tools;
mod whoami;
mod workflows;

/// Shared edge application state.
#[derive(Clone)]
pub struct AppState {
    /// Loaded edge configuration shared by direct read handlers.
    pub config: Arc<MoaConfig>,
    /// Credential resolver used for incoming requests.
    pub auth: Arc<dyn AuthProvider>,
    /// OpenFGA client used for direct edge authorization checks.
    pub fga: Option<Arc<FgaClient>>,
    /// Shared secret used to verify Auth0 connection-linked webhooks.
    pub auth0_webhook_secret: Option<String>,
    /// Secrets used to verify public tenant-knowledge webhooks at the edge.
    pub knowledge_webhooks: KnowledgeWebhookEdgeConfig,
    /// Postgres pool used by unauthenticated webhooks that update auth metadata.
    pub pool: Arc<sqlx::PgPool>,
    /// Postgres-backed session store used by direct edge media reads and writes.
    pub session_store: Arc<PostgresSessionStore>,
    /// Internal orchestrator proxy.
    pub proxy: Arc<OrchestratorProxy>,
}

/// Edge-local verification config for public tenant-knowledge webhooks.
#[derive(Clone, Debug, Default)]
pub struct KnowledgeWebhookEdgeConfig {
    /// Nango HMAC signing key.
    pub nango_signing_key: Option<String>,
    /// Merge HMAC signature key.
    pub merge_signature_key: Option<String>,
    /// LlamaParse HMAC or Svix signing key.
    pub llamaparse_signing_key: Option<String>,
    /// Optional LlamaParse custom header gate.
    pub llamaparse_custom_header: Option<(String, String)>,
    /// Reducto HMAC or Svix signing key.
    pub reducto_signing_key: Option<String>,
    /// Optional Reducto custom header gate.
    pub reducto_custom_header: Option<(String, String)>,
}

/// Build the edge router.
pub fn router(state: AppState) -> Router {
    moa_authz::configure_security_audit((*state.pool).clone(), false);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/whoami", get(whoami::handle))
        .route(
            "/v1/analytics/session-stats",
            post(analytics::handle_session_stats),
        )
        .route(
            "/v1/analytics/tenant-stats",
            post(analytics::handle_tenant_stats),
        )
        .route(
            "/v1/analytics/tool-stats",
            post(analytics::handle_tool_stats),
        )
        .route(
            "/v1/analytics/cache-stats",
            post(analytics::handle_cache_stats),
        )
        .route(
            "/v1/analytics/experiment-stats",
            post(analytics::handle_experiment_stats),
        )
        .route(
            "/v1/analytics/learning-candidates",
            post(analytics::handle_learning_candidates),
        )
        .route(
            "/v1/analytics/session-search",
            post(analytics::handle_session_search),
        )
        .route("/v1/audit/verify", post(audit::handle_verify))
        .route("/v1/lineage/explain", post(lineage::handle_explain))
        .route("/v1/lineage/query", post(lineage::handle_query))
        .route("/v1/lineage/verify", post(lineage::handle_verify))
        .route(
            "/v1/security/secret-scanning/github",
            post(handle_github_secret_scan),
        )
        .route(
            "/v1/webhooks/auth0/connection-linked",
            post(handle_auth0_connection_webhook),
        )
        .route(
            "/v1/knowledge/webhooks/llamaparse",
            post(handle_knowledge_llamaparse_webhook)
                .layer(DefaultBodyLimit::max(KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/knowledge/webhooks/reducto",
            post(handle_knowledge_reducto_webhook)
                .layer(DefaultBodyLimit::max(KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/knowledge/webhooks/nango",
            post(handle_knowledge_nango_webhook)
                .layer(DefaultBodyLimit::max(KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/knowledge/webhooks/merge",
            post(handle_knowledge_merge_webhook)
                .layer(DefaultBodyLimit::max(KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/contacts/verification/start",
            post(handle_public_contact_verification_start),
        )
        .route(
            "/v1/contacts/verification/complete",
            post(handle_public_contact_verification_complete),
        )
        .route(
            "/v1/sessions/{session_id}/contacts/verification/start",
            post(handle_public_session_contact_verification_start),
        )
        .route(
            "/v1/sessions/{session_id}/contacts/verification/complete",
            post(handle_public_session_contact_verification_complete),
        )
        .route("/v1/sessions", post(handle_public_agent_session_init))
        .route(
            "/v1/sessions/{session_id}/promote",
            post(handle_public_agent_session_promote),
        )
        .route(
            "/v1/sessions/{session_id}/channel",
            patch(handle_public_agent_session_channel_change),
        )
        .route(
            "/v1/sessions/{session_id}/messages",
            post(handle_session_message_stream)
                .layer(DefaultBodyLimit::max(SESSION_MESSAGE_BODY_LIMIT_BYTES)),
        )
        .route(
            "/v1/sessions/{session_id}/attachments/{attachment_id}",
            get(handle_session_attachment),
        )
        .route("/v1/{*rest}", any(handle_proxy))
        .with_state(state)
}

// SAFETY: Edge health returns static process status and reads no caller-owned data.
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
    let identity = match authenticate_edge_request(&state, &headers, &span).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };

    let original_path = uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or(uri.path())
        .to_string();
    let (method, path, body) =
        match translate_public_route(&method, &uri, &body, identity.tenant_id) {
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

pub(super) async fn authenticate_edge_request(
    state: &AppState,
    headers: &HeaderMap,
    span: &tracing::Span,
) -> Result<Identity, Response> {
    let credential = match credential_for_request(state.auth.as_ref(), headers) {
        Some(credential) => credential,
        None => {
            span.record("moa.edge.auth.result", "missing_credential");
            if let Err(error) = moa_ocsf::emit_authn_failure(
                &state.pool,
                Uuid::nil(),
                None,
                "unknown",
                source_ip(headers),
                "missing credential",
            )
            .await
            {
                tracing::error!(error = %error, "security audit write failed for missing credential");
                span.record("http.status_code", 500_i64);
                return Err(
                    (StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response()
                );
            }
            span.record("http.status_code", 401_i64);
            return Err((StatusCode::UNAUTHORIZED, "missing credential").into_response());
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
                source_ip(headers),
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
                return Err(
                    (StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response()
                );
            }
            tracing::info!(error = %error, provider = state.auth.name(), "authentication rejected");
            span.record("http.status_code", 401_i64);
            return Err((StatusCode::UNAUTHORIZED, "invalid credential").into_response());
        }
    };
    span.record(
        "moa.edge.auth.provider",
        tracing::field::display(state.auth.name()),
    );
    span.record("moa.edge.auth.result", "accepted");
    if let Err(error) = moa_ocsf::emit_authn_success(
        &state.pool,
        identity.tenant_id.0,
        &identity,
        state.auth.name(),
        source_ip(headers),
    )
    .await
    {
        tracing::error!(error = %error, "security audit write failed for authenticated request");
        span.record("http.status_code", 500_i64);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "audit unavailable").into_response());
    }
    Ok(identity)
}

pub(super) async fn authenticate_direct_request(
    state: &AppState,
    headers: &HeaderMap,
    route: &'static str,
) -> Result<Identity, Response> {
    let span = tracing::Span::current();
    span.record("http.route", route);
    authenticate_edge_request(state, headers, &span).await
}

pub(super) fn parse_json_body_with_tenant<T>(
    body: &Bytes,
    tenant_id: TenantId,
) -> Result<T, Response>
where
    T: DeserializeOwned,
{
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad request body").into_response())?;
    let Some(object) = value.as_object_mut() else {
        return Err((StatusCode::BAD_REQUEST, "request body must be object").into_response());
    };
    object.insert("tenant_id".to_string(), serde_json::json!(tenant_id));
    serde_json::from_value(value)
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad request body").into_response())
}

pub(super) fn parse_json_body<T>(body: &Bytes) -> Result<T, Response>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "bad request body").into_response())
}

pub(super) async fn require_direct_authz(
    state: &AppState,
    identity: &Identity,
    object_type: ObjectType,
    object_id: impl std::fmt::Display,
    relation: Relation,
) -> Result<(), Response> {
    let fga = state.fga.as_deref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "authorization engine unavailable",
        )
            .into_response()
    })?;
    require_authz_with_delegation(fga, identity, object_type, object_id, relation)
        .await
        .map_err(|error| authz_error_response(state, error))
}

pub(super) fn route_error(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "direct edge read failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "read failed").into_response()
}

pub(super) fn moa_error_response(error: MoaError) -> Response {
    match error {
        MoaError::SessionNotFound(_) => (StatusCode::NOT_FOUND, error.to_string()).into_response(),
        other => route_error(other),
    }
}

fn authz_error_response(_state: &AppState, error: AuthzCheckError) -> Response {
    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            tracing::info!(
                deny.subject = %subject,
                deny.object = format!("{object_type}:{object_id}"),
                deny.relation = %relation,
                "edge authz denied"
            );
            (StatusCode::FORBIDDEN, "forbidden").into_response()
        }
        AuthzCheckError::Engine(error) => {
            tracing::error!(error = %error, "edge authz engine error; failing closed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "authorization engine unavailable",
            )
                .into_response()
        }
    }
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

async fn handle_public_contact_verification_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(state, headers, body, "/Contacts/start_verification", []).await
}

async fn handle_public_contact_verification_complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(state, headers, body, "/Contacts/complete_verification", []).await
}

async fn handle_public_session_contact_verification_start(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/start_verification",
        [("session_id", serde_json::json!(session_id))],
    )
    .await
}

async fn handle_public_session_contact_verification_complete(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/complete_verification",
        [("session_id", serde_json::json!(session_id))],
    )
    .await
}

async fn handle_public_agent_session_init(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(state, headers, body, "/Contacts/init_session", []).await
}

async fn handle_public_agent_session_promote(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/promote_session",
        [("session_id", serde_json::json!(session_id))],
    )
    .await
}

async fn handle_public_agent_session_channel_change(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/change_session_channel",
        [("session_id", serde_json::json!(session_id))],
    )
    .await
}

async fn handle_knowledge_llamaparse_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_knowledge_provider_webhook(state, headers, body, "llamaparse").await
}

async fn handle_knowledge_reducto_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_knowledge_provider_webhook(state, headers, body, "reducto").await
}

async fn handle_knowledge_nango_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_knowledge_provider_webhook(state, headers, body, "nango").await
}

async fn handle_knowledge_merge_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_knowledge_provider_webhook(state, headers, body, "merge").await
}

async fn forward_knowledge_provider_webhook(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    provider: &'static str,
) -> axum::response::Response {
    if let Err((status, message)) =
        verify_knowledge_webhook_at_edge(provider, &headers, &body, &state.knowledge_webhooks)
    {
        tracing::warn!(
            provider,
            status = status.as_u16(),
            message,
            "knowledge webhook rejected"
        );
        return (status, message).into_response();
    }

    let RouteTranslation::Forward { method, path, body } =
        knowledge::translate_provider_webhook(provider, &headers, &body)
    else {
        return (StatusCode::BAD_REQUEST, "bad knowledge webhook body").into_response();
    };

    match state
        .proxy
        .forward_public(method, &path, body, &headers)
        .await
    {
        Ok(response) => response_to_axum(response).await,
        Err(error) => {
            tracing::error!(error = %error, provider, "knowledge webhook proxy forward failed");
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

fn verify_knowledge_webhook_at_edge(
    provider: &str,
    headers: &HeaderMap,
    body: &[u8],
    config: &KnowledgeWebhookEdgeConfig,
) -> Result<(), (StatusCode, &'static str)> {
    match provider {
        "nango" => {
            let signing_key = config
                .nango_signing_key
                .as_deref()
                .ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
            verify_hmac_header(headers, body, signing_key, "x-nango-hmac-sha256")
        }
        "merge" => {
            let signing_key = config
                .merge_signature_key
                .as_deref()
                .ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
            verify_hmac_header(headers, body, signing_key, "x-merge-webhook-signature")
        }
        "llamaparse" => verify_parser_webhook_at_edge(
            "llamaparse",
            headers,
            body,
            config.llamaparse_signing_key.as_deref(),
            config.llamaparse_custom_header.as_ref(),
        ),
        "reducto" => verify_parser_webhook_at_edge(
            "reducto",
            headers,
            body,
            config.reducto_signing_key.as_deref(),
            config.reducto_custom_header.as_ref(),
        ),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "unknown knowledge webhook provider",
        )),
    }
}

fn verify_parser_webhook_at_edge(
    parser: &str,
    headers: &HeaderMap,
    body: &[u8],
    signing_key: Option<&str>,
    custom_header: Option<&(String, String)>,
) -> Result<(), (StatusCode, &'static str)> {
    if let Some((name, expected)) = custom_header
        && !verify_custom_header(headers, name, expected)
    {
        return Err((StatusCode::UNAUTHORIZED, "invalid webhook header"));
    }
    let signing_key = signing_key.ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
    if webhook_header_value(headers, &["svix-signature", "x-svix-signature"]).is_some() {
        return verify_svix_signature_at_edge(headers, body, signing_key);
    }
    let parser_header = format!("x-{parser}-webhook-signature");
    verify_hmac_header_candidates(
        headers,
        body,
        signing_key,
        &[parser_header.as_str(), "x-moa-knowledge-webhook-signature"],
    )
}

fn verify_hmac_header(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
    header_name: &str,
) -> Result<(), (StatusCode, &'static str)> {
    verify_hmac_header_candidates(headers, body, signing_key, &[header_name])
}

fn verify_hmac_header_candidates(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
    header_names: &[&str],
) -> Result<(), (StatusCode, &'static str)> {
    let signature = webhook_header_value(headers, header_names)
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let signature = decode_webhook_signature(&signature)
        .ok_or((StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    verify_hmac_signature(signing_key.as_bytes(), body, &signature)
}

fn verify_svix_signature_at_edge(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let message_id = webhook_header_value(headers, &["svix-id", "x-svix-id"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let timestamp = webhook_header_value(headers, &["svix-timestamp", "x-svix-timestamp"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > 300 {
        return Err((StatusCode::UNAUTHORIZED, "stale webhook signature"));
    }
    let signature = webhook_header_value(headers, &["svix-signature", "x-svix-signature"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let key = svix_signing_key(signing_key)
        .ok_or((StatusCode::UNAUTHORIZED, "invalid webhook verifier"))?;
    let body = std::str::from_utf8(body)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    let signed_payload = format!("{message_id}.{timestamp}.{body}");
    for candidate in signature.split_whitespace() {
        if let Some(encoded) = candidate.strip_prefix("v1,")
            && let Some(signature) = decode_base64_signature(encoded)
            && verify_hmac_signature(&key, signed_payload.as_bytes(), &signature).is_ok()
        {
            return Ok(());
        }
    }
    Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
}

fn verify_hmac_signature(
    signing_key: &[u8],
    body: &[u8],
    signature: &[u8],
) -> Result<(), (StatusCode, &'static str)> {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(signing_key) else {
        return Err((StatusCode::UNAUTHORIZED, "invalid webhook verifier"));
    };
    mac.update(body);
    mac.verify_slice(signature)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))
}

fn verify_custom_header(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|actual| actual.as_bytes().ct_eq(expected.as_bytes()).into())
}

fn decode_webhook_signature(value: &str) -> Option<Vec<u8>> {
    let value = value.trim().trim_start_matches("sha256=");
    if let Ok(decoded) = hex::decode(value)
        && decoded.len() == 32
    {
        return Some(decoded);
    }
    decode_base64_signature(value)
}

fn decode_base64_signature(value: &str) -> Option<Vec<u8>> {
    general_purpose::STANDARD
        .decode(value.trim())
        .or_else(|_| general_purpose::URL_SAFE.decode(value.trim()))
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(value.trim()))
        .ok()
}

fn svix_signing_key(signing_key: &str) -> Option<Vec<u8>> {
    let Some(encoded) = signing_key.trim().strip_prefix("whsec_") else {
        return Some(signing_key.as_bytes().to_vec());
    };
    decode_base64_signature(encoded)
}

async fn forward_public_contact_route<const N: usize>(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    target: &str,
    fields: [(&str, serde_json::Value); N],
) -> axum::response::Response {
    let RouteTranslation::Forward { method, path, body } = translate_json_object_with_fields(
        &body,
        target,
        "bad contact body",
        "contact body must be object",
        "serialize contact body failed",
        fields,
    ) else {
        return (StatusCode::BAD_REQUEST, "bad contact body").into_response();
    };
    match state
        .proxy
        .forward_public(method, &path, body, &headers)
        .await
    {
        Ok(response) => response_to_axum(response).await,
        Err(error) => {
            tracing::error!(error = %error, "public contact proxy forward failed");
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

#[tracing::instrument(
    skip(state, headers, request),
    fields(
        http.route = "/v1/sessions/{session_id}/messages",
        http.status_code = tracing::field::Empty,
    )
)]
async fn handle_session_message_stream(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    request: Request,
) -> axum::response::Response {
    let span = tracing::Span::current();
    let mut input = match session_message_input(session_id, &headers, request, &state).await {
        Ok(input) => input,
        Err(error) => {
            span.record("http.status_code", error.status.as_u16() as i64);
            return (error.status, error.message).into_response();
        }
    };

    let next_sequence_num = match initial_stream_sequence(&state, &input.message).await {
        Ok(next_sequence_num) => next_sequence_num,
        Err(error) => {
            tracing::warn!(error = %error.summary(), "session stream preflight failed");
            span.record("http.status_code", error.status_code().as_u16() as i64);
            return error.into_response();
        }
    };
    let mut stored_attachments = Vec::new();
    if !input.uploads.is_empty() {
        match persist_session_attachments(&state, &input.message, input.uploads).await {
            Ok(attachments) => {
                stored_attachments = attachments.clone();
                input.message.attachments.extend(attachments);
            }
            Err(error) => {
                tracing::warn!(error = %error, "session media persistence failed");
                span.record("http.status_code", 500_i64);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to store session media",
                )
                    .into_response();
            }
        }
    }
    let accepted = match call_contacts_handler::<_, ContactSessionMessageResponse>(
        &state,
        "send_message",
        &input.message,
    )
    .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(error = %error.summary(), "session message admission failed");
            cleanup_session_attachments(
                &state,
                input.message.tenant_id,
                input.message.session_id,
                &stored_attachments,
            )
            .await;
            span.record("http.status_code", error.status_code().as_u16() as i64);
            return error.into_response();
        }
    };
    span.record("http.status_code", 200_i64);

    let accepted_frame = SessionMessageAccepted {
        session_id: accepted.session_id,
        queued: accepted.queued,
        started_turn_id: accepted.started_turn_id,
        next_sequence_num,
    };
    let terminal_turn_id = accepted_frame.started_turn_id.clone();
    let mut pending_events = VecDeque::new();
    pending_events.push_back(json_sse_event("accepted", &accepted_frame));

    let stream_state = SessionMessageStreamState {
        app: state,
        tenant_id: input.message.tenant_id,
        session_id: accepted.session_id,
        contact_token: input.message.contact_token,
        next_sequence_num,
        terminal_turn_id,
        pending_events,
        closed: false,
    };
    Sse::new(stream::unfold(stream_state, next_session_message_event))
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[tracing::instrument(
    skip(state, headers),
    fields(
        http.route = "/v1/sessions/{session_id}/attachments/{attachment_id}",
        http.status_code = tracing::field::Empty,
    )
)]
async fn handle_session_attachment(
    State(state): State<AppState>,
    Path((session_id, attachment_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let span = tracing::Span::current();
    let Some(tenant_id) = query
        .get("tenant_id")
        .and_then(|value| Uuid::parse_str(value).ok().map(TenantId::from))
    else {
        span.record("http.status_code", 400_i64);
        return (StatusCode::BAD_REQUEST, "tenant_id is required").into_response();
    };
    let Some(contact_token) = authorization_bearer_token(&headers) else {
        span.record("http.status_code", 401_i64);
        return (StatusCode::UNAUTHORIZED, "contact token is required").into_response();
    };
    let session_id = SessionId(session_id);
    let attachment_id = SessionAttachmentId(attachment_id);

    if let Err(error) = call_contacts_handler::<_, ContactSessionAuthorizationResponse>(
        &state,
        "authorize_session",
        &ContactSessionAuthorizationRequest {
            tenant_id,
            session_id,
            contact_token,
        },
    )
    .await
    {
        tracing::warn!(error = %error.summary(), "session attachment authorization failed");
        span.record("http.status_code", error.status_code().as_u16() as i64);
        return error.into_response();
    }

    let (attachment, content) = match state
        .session_store
        .get(tenant_id, session_id, attachment_id)
        .await
    {
        Ok(stored) => stored,
        Err(MoaError::SessionAttachmentNotFound(_))
        | Err(MoaError::SessionAttachmentObjectNotFound(_)) => {
            span.record("http.status_code", 404_i64);
            return (StatusCode::NOT_FOUND, "attachment not found").into_response();
        }
        Err(error) => {
            tracing::warn!(error = %error, "session attachment read failed");
            span.record("http.status_code", 502_i64);
            return (StatusCode::BAD_GATEWAY, "attachment storage is unavailable").into_response();
        }
    };

    span.record("http.status_code", 200_i64);
    attachment_response(&attachment, content)
}

fn authorization_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn attachment_response(attachment: &Attachment, content: Vec<u8>) -> axum::response::Response {
    let content_len = content.len();
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(mime_type) = attachment.mime_type.as_deref()
        && let Ok(value) = HeaderValue::from_str(mime_type)
    {
        builder = builder.header(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&content_len.to_string()) {
        builder = builder.header(header::CONTENT_LENGTH, value);
    }
    match builder.body(Body::from(content)) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "build attachment response failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response()
        }
    }
}

#[derive(Debug)]
enum EdgeJsonError {
    Serialize(String),
    Forward(String),
    Upstream { status: StatusCode, body: String },
    Read(String),
    Decode(String),
}

impl EdgeJsonError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::Upstream { status, .. } => *status,
            Self::Serialize(_) | Self::Decode(_) | Self::Forward(_) | Self::Read(_) => {
                StatusCode::BAD_GATEWAY
            }
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Serialize(error) => format!("serialize upstream request failed: {error}"),
            Self::Forward(error) => format!("upstream request failed: {error}"),
            Self::Upstream { status, body } if body.is_empty() => {
                format!("upstream returned {status}")
            }
            Self::Upstream { status, body } => format!("upstream returned {status}: {body}"),
            Self::Read(error) => format!("upstream response read failed: {error}"),
            Self::Decode(error) => format!("upstream response decode failed: {error}"),
        }
    }

    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        let body = match self {
            Self::Upstream { body, .. } if !body.is_empty() => body,
            error => error.summary(),
        };
        (status, body).into_response()
    }
}

fn contact_session_message_request(
    session_id: Uuid,
    body: &Bytes,
) -> Result<ContactSessionMessageRequest, &'static str> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "bad session message body")?;
    let Some(object) = value.as_object_mut() else {
        return Err("session message body must be object");
    };
    object.insert("session_id".to_string(), serde_json::json!(session_id));
    let message: ContactSessionMessageRequest =
        serde_json::from_value(value).map_err(|_| "bad session message body")?;
    if !message.attachments.is_empty() {
        return Err("session message attachments must be uploaded as multipart");
    }
    message.validate_admitted_payload()?;
    Ok(message)
}

struct SessionMessageInput {
    message: ContactSessionMessageRequest,
    uploads: Vec<SessionAttachmentUpload>,
}

struct SessionAttachmentUpload {
    name: String,
    mime_type: String,
    content: Vec<u8>,
}

#[derive(Debug)]
struct SessionMessageRequestError {
    status: StatusCode,
    message: &'static str,
}

impl SessionMessageRequestError {
    fn bad_request(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

async fn session_message_input(
    session_id: Uuid,
    headers: &HeaderMap,
    request: Request,
    state: &AppState,
) -> Result<SessionMessageInput, SessionMessageRequestError> {
    if is_multipart_content_type(headers) {
        return multipart_session_message_request(session_id, request, state).await;
    }

    let body = Bytes::from_request(request, state)
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad session message body"))?;
    contact_session_message_request(session_id, &body)
        .map(|message| SessionMessageInput {
            message,
            uploads: Vec::new(),
        })
        .map_err(SessionMessageRequestError::bad_request)
}

async fn multipart_session_message_request(
    session_id: Uuid,
    request: Request,
    state: &AppState,
) -> Result<SessionMessageInput, SessionMessageRequestError> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session message"))?;
    let mut tenant_id = None;
    let mut contact_token = None;
    let mut user_message = String::new();
    let mut model = None;
    let mut max_turns = None;
    let mut uploads = Vec::new();
    let mut total_upload_bytes = 0_usize;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session message"))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        let file_name = field.file_name().map(ToOwned::to_owned);
        let declared_mime = field.content_type().map(ToString::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session part"))?;

        if file_name.is_some() || is_upload_field(&field_name) {
            if bytes.is_empty() {
                return Err(SessionMessageRequestError::bad_request(
                    "photo upload was empty",
                ));
            }
            if uploads.len() >= MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE {
                return Err(SessionMessageRequestError::bad_request(
                    "too many photo uploads",
                ));
            }
            if bytes.len() > MAX_CONTACT_SESSION_ATTACHMENT_BYTES {
                return Err(SessionMessageRequestError::bad_request(
                    "photo upload is too large",
                ));
            }
            total_upload_bytes = total_upload_bytes.saturating_add(bytes.len());
            if total_upload_bytes > MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES {
                return Err(SessionMessageRequestError::bad_request(
                    "photo uploads are too large",
                ));
            }
            let mime_type = canonical_photo_mime(declared_mime.as_deref(), &bytes)?;
            let name = validated_upload_name(file_name.as_deref())?;
            uploads.push(SessionAttachmentUpload {
                name,
                mime_type: mime_type.to_string(),
                content: bytes.to_vec(),
            });
            continue;
        }

        let value = String::from_utf8(bytes.to_vec()).map_err(|_| {
            SessionMessageRequestError::bad_request("multipart text field was not utf-8")
        })?;
        match field_name.as_str() {
            "tenant_id" => {
                let parsed = Uuid::parse_str(value.trim())
                    .map_err(|_| SessionMessageRequestError::bad_request("bad tenant_id"))?;
                tenant_id = Some(TenantId::from(parsed));
            }
            "contact_token" => contact_token = Some(value),
            "user_message" | "text" | "message" => {
                validate_contact_session_message_text(&value)
                    .map_err(SessionMessageRequestError::bad_request)?;
                user_message = value;
            }
            "model" if !value.trim().is_empty() => model = Some(value),
            "max_turns" if !value.trim().is_empty() => {
                max_turns = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| SessionMessageRequestError::bad_request("bad max_turns"))?,
                );
            }
            _ => {}
        }
    }

    if user_message.trim().is_empty() && uploads.is_empty() {
        return Err(SessionMessageRequestError::bad_request(
            "session message requires text or a photo",
        ));
    }

    Ok(SessionMessageInput {
        message: ContactSessionMessageRequest {
            tenant_id: tenant_id
                .ok_or_else(|| SessionMessageRequestError::bad_request("tenant_id is required"))?,
            session_id: SessionId(session_id),
            contact_token: contact_token.ok_or_else(|| {
                SessionMessageRequestError::bad_request("contact_token is required")
            })?,
            user_message,
            attachments: Vec::new(),
            model,
            max_turns,
        },
        uploads,
    })
}

fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    header_media_type(headers, header::CONTENT_TYPE)
        .is_some_and(|media_type| media_type.eq_ignore_ascii_case("multipart/form-data"))
}

fn is_upload_field(name: &str) -> bool {
    matches!(
        name,
        "file" | "files" | "attachment" | "attachments" | "photo" | "photos"
    )
}

fn canonical_photo_mime(
    declared_mime: Option<&str>,
    content: &[u8],
) -> Result<&'static str, SessionMessageRequestError> {
    let sniffed = sniff_photo_mime(content).ok_or_else(|| {
        SessionMessageRequestError::bad_request("only jpeg, png, and webp photos are supported")
    })?;
    let dimensions = photo_dimensions(sniffed, content).ok_or_else(|| {
        SessionMessageRequestError::bad_request("photo dimensions could not be verified")
    })?;
    validate_photo_dimensions(dimensions)?;
    if let Some(declared_mime) = declared_mime
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let lower = declared_mime.to_ascii_lowercase();
        if !lower.starts_with("image/") {
            return Err(SessionMessageRequestError::bad_request(
                "only photo uploads are supported",
            ));
        }
        let Some(normalized) = normalize_contact_session_photo_mime(&lower) else {
            return Err(SessionMessageRequestError::bad_request(
                "only jpeg, png, and webp photos are supported",
            ));
        };
        if normalized != sniffed {
            return Err(SessionMessageRequestError::bad_request(
                "photo MIME type does not match content",
            ));
        }
    }
    Ok(sniffed)
}

fn validated_upload_name(file_name: Option<&str>) -> Result<String, SessionMessageRequestError> {
    let candidate = file_name
        .and_then(|name| {
            name.replace('\\', "/")
                .rsplit('/')
                .next()
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "photo".to_string());
    if candidate.len() > MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES
        || candidate.chars().any(char::is_control)
    {
        return Err(SessionMessageRequestError::bad_request(
            "photo file name is invalid",
        ));
    }
    Ok(candidate)
}

fn sniff_photo_mime(content: &[u8]) -> Option<&'static str> {
    if content.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if content.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if content.len() >= 12 && &content[0..4] == b"RIFF" && &content[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn photo_dimensions(mime_type: &str, content: &[u8]) -> Option<(u32, u32)> {
    match mime_type {
        "image/jpeg" => jpeg_dimensions(content),
        "image/png" => png_dimensions(content),
        "image/webp" => webp_dimensions(content),
        _ => None,
    }
}

fn validate_photo_dimensions(dimensions: (u32, u32)) -> Result<(), SessionMessageRequestError> {
    let (width, height) = dimensions;
    if width == 0 || height == 0 {
        return Err(SessionMessageRequestError::bad_request(
            "photo dimensions are invalid",
        ));
    }
    if width > MAX_SESSION_PHOTO_DIMENSION || height > MAX_SESSION_PHOTO_DIMENSION {
        return Err(SessionMessageRequestError::bad_request(
            "photo dimensions are too large",
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_SESSION_PHOTO_PIXELS {
        return Err(SessionMessageRequestError::bad_request(
            "photo pixel count is too large",
        ));
    }
    Ok(())
}

fn png_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if content.len() < 33 || !content.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }

    let mut index = 8;
    let mut dimensions = None;
    let mut saw_idat = false;
    while index + 12 <= content.len() {
        let chunk_len = u32::from_be_bytes(content[index..index + 4].try_into().ok()?) as usize;
        let chunk_type = &content[index + 4..index + 8];
        let data_start = index + 8;
        let crc_start = data_start.checked_add(chunk_len)?;
        let next = crc_start.checked_add(4)?;
        if next > content.len() {
            return None;
        }

        match chunk_type {
            b"IHDR" => {
                if index != 8 || chunk_len != 13 {
                    return None;
                }
                let width =
                    u32::from_be_bytes(content[data_start..data_start + 4].try_into().ok()?);
                let height =
                    u32::from_be_bytes(content[data_start + 4..data_start + 8].try_into().ok()?);
                dimensions = Some((width, height));
            }
            b"IDAT" => {
                dimensions?;
                saw_idat = true;
            }
            b"IEND" => {
                if chunk_len != 0 {
                    return None;
                }
                if !saw_idat {
                    return None;
                }
                return dimensions;
            }
            _ => {}
        }

        index = next;
    }
    None
}

fn jpeg_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if !content.starts_with(&[0xff, 0xd8]) || !content.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut index = 2;
    while index + 3 < content.len() {
        while index < content.len() && content[index] == 0xff {
            index += 1;
        }
        if index >= content.len() {
            return None;
        }
        let marker = content[index];
        index += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if index + 2 > content.len() {
            return None;
        }
        let segment_len = usize::from(u16::from_be_bytes(
            content[index..index + 2].try_into().ok()?,
        ));
        if segment_len < 2 || index + segment_len > content.len() {
            return None;
        }
        if is_jpeg_start_of_frame(marker) {
            if segment_len < 7 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                content[index + 3..index + 5].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                content[index + 5..index + 7].try_into().ok()?,
            ));
            return Some((width, height));
        }
        index += segment_len;
    }
    None
}

fn is_jpeg_start_of_frame(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn webp_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if content.len() < 30 || &content[0..4] != b"RIFF" || &content[8..12] != b"WEBP" {
        return None;
    }
    let riff_len = u32::from_le_bytes(content[4..8].try_into().ok()?) as usize;
    if riff_len.checked_add(8)? != content.len() {
        return None;
    }
    match &content[12..16] {
        b"VP8X" => {
            let width = read_u24_le(&content[24..27])?.checked_add(1)?;
            let height = read_u24_le(&content[27..30])?.checked_add(1)?;
            Some((width, height))
        }
        b"VP8L" => {
            if content[20] != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes(content[21..25].try_into().ok()?);
            let width = (bits & 0x3fff).checked_add(1)?;
            let height = ((bits >> 14) & 0x3fff).checked_add(1)?;
            Some((width, height))
        }
        b"VP8 " => {
            if &content[23..26] != b"\x9d\x01\x2a" {
                return None;
            }
            let width = u32::from(u16::from_le_bytes(content[26..28].try_into().ok()?) & 0x3fff);
            let height = u32::from(u16::from_le_bytes(content[28..30].try_into().ok()?) & 0x3fff);
            Some((width, height))
        }
        _ => None,
    }
}

fn read_u24_le(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 3] = bytes.try_into().ok()?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

async fn persist_session_attachments(
    state: &AppState,
    message: &ContactSessionMessageRequest,
    uploads: Vec<SessionAttachmentUpload>,
) -> Result<Vec<Attachment>, MoaError> {
    let session = state.session_store.get_session(message.session_id).await?;
    if session.tenant_id != message.tenant_id {
        return Err(MoaError::StorageError(format!(
            "session `{}` does not belong to tenant `{}`",
            message.session_id, message.tenant_id
        )));
    }
    let contact_id = session.contact.as_ref().map(|contact| contact.contact_id);
    let mut attachments = Vec::with_capacity(uploads.len());
    for upload in uploads {
        let attachment = match state
            .session_store
            .put(
                message.tenant_id,
                message.session_id,
                contact_id,
                upload.name,
                upload.mime_type,
                upload.content,
            )
            .await
        {
            Ok(attachment) => attachment,
            Err(error) => {
                cleanup_session_attachments(
                    state,
                    message.tenant_id,
                    message.session_id,
                    &attachments,
                )
                .await;
                return Err(error);
            }
        };
        attachments.push(attachment);
    }
    Ok(attachments)
}

async fn cleanup_session_attachments(
    state: &AppState,
    tenant_id: TenantId,
    session_id: SessionId,
    attachments: &[Attachment],
) {
    for attachment in attachments {
        let Some(attachment_id) = attachment.id else {
            continue;
        };
        if let Err(error) = state
            .session_store
            .delete(tenant_id, session_id, attachment_id)
            .await
        {
            tracing::warn!(
                %error,
                %session_id,
                %attachment_id,
                "failed to clean up session attachment after message rejection"
            );
        }
    }
}

async fn call_contacts_handler<I, O>(
    state: &AppState,
    handler: &str,
    input: &I,
) -> Result<O, EdgeJsonError>
where
    I: Serialize + ?Sized,
    O: serde::de::DeserializeOwned,
{
    let body =
        serde_json::to_vec(input).map_err(|error| EdgeJsonError::Serialize(error.to_string()))?;
    let path = format!("/Contacts/{handler}");
    let response = state
        .proxy
        .forward_public(Method::POST, &path, body, &HeaderMap::new())
        .await
        .map_err(|error| EdgeJsonError::Forward(error.to_string()))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| EdgeJsonError::Read(error.to_string()))?;
    if !status.is_success() {
        return Err(EdgeJsonError::Upstream {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        });
    }
    serde_json::from_slice(&body).map_err(|error| EdgeJsonError::Decode(error.to_string()))
}

async fn initial_stream_sequence(
    state: &AppState,
    message: &ContactSessionMessageRequest,
) -> Result<SequenceNum, EdgeJsonError> {
    let progress = call_contacts_handler::<_, SessionProgress>(
        state,
        "progress",
        &ContactSessionProgressRequest {
            tenant_id: message.tenant_id,
            session_id: message.session_id,
            contact_token: message.contact_token.clone(),
            event_range: EventRange::recent(1),
        },
    )
    .await?;
    Ok(next_sequence_after(&progress.events))
}

struct SessionMessageStreamState {
    app: AppState,
    tenant_id: TenantId,
    session_id: SessionId,
    contact_token: String,
    next_sequence_num: SequenceNum,
    terminal_turn_id: Option<String>,
    pending_events: VecDeque<SseEvent>,
    closed: bool,
}

#[derive(Debug, Serialize)]
struct SessionMessageAccepted {
    session_id: SessionId,
    queued: bool,
    started_turn_id: Option<String>,
    next_sequence_num: SequenceNum,
}

#[derive(Debug, Serialize)]
struct SessionMessageDone {
    session_id: SessionId,
    status: &'static str,
    last_turn_id: Option<String>,
}

async fn next_session_message_event(
    mut state: SessionMessageStreamState,
) -> Option<(Result<SseEvent, Infallible>, SessionMessageStreamState)> {
    if state.closed {
        return None;
    }
    if let Some(event) = state.pending_events.pop_front() {
        return Some((Ok(event), state));
    }

    loop {
        match fetch_stream_progress(&state).await {
            Ok(progress) => {
                let done_event = session_message_stream_done(&state, &progress)
                    .then(|| done_sse_event(&state, &progress));
                enqueue_progress_events(&mut state, progress.events);
                if let Some(event) = state.pending_events.pop_front() {
                    return Some((Ok(event), state));
                }
                if let Some(event) = done_event {
                    state.closed = true;
                    return Some((Ok(event), state));
                }
            }
            Err(error) => {
                state.closed = true;
                return Some((Ok(error_sse_event(error.summary())), state));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn fetch_stream_progress(
    state: &SessionMessageStreamState,
) -> Result<SessionProgress, EdgeJsonError> {
    call_contacts_handler(
        &state.app,
        "progress",
        &ContactSessionProgressRequest {
            tenant_id: state.tenant_id,
            session_id: state.session_id,
            contact_token: state.contact_token.clone(),
            event_range: EventRange {
                from_seq: Some(state.next_sequence_num),
                to_seq: None,
                event_types: None,
                limit: Some(100),
            },
        },
    )
    .await
}

fn enqueue_progress_events(state: &mut SessionMessageStreamState, records: Vec<EventRecord>) {
    for record in records {
        state.next_sequence_num = state
            .next_sequence_num
            .max(record.sequence_num.saturating_add(1));
        state.pending_events.push_back(record_sse_event(&record));
    }
}

fn next_sequence_after(records: &[EventRecord]) -> SequenceNum {
    records
        .iter()
        .map(|record| record.sequence_num)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn session_message_stream_done(
    state: &SessionMessageStreamState,
    progress: &SessionProgress,
) -> bool {
    session_message_terminal_done(state.terminal_turn_id.as_deref(), progress)
}

fn session_message_terminal_done(
    terminal_turn_id: Option<&str>,
    progress: &SessionProgress,
) -> bool {
    if let Some(turn_id) = terminal_turn_id {
        return progress
            .snapshot
            .last_outcome
            .as_ref()
            .is_some_and(|outcome| outcome.turn_id == turn_id);
    }
    progress.snapshot.active_turn_id.is_none() && progress.snapshot.pending_message_count == 0
}

fn done_sse_event(state: &SessionMessageStreamState, progress: &SessionProgress) -> SseEvent {
    let last_turn_id = progress
        .snapshot
        .last_outcome
        .as_ref()
        .map(|outcome| outcome.turn_id.clone());
    json_sse_event(
        "done",
        &SessionMessageDone {
            session_id: state.session_id,
            status: done_status(progress),
            last_turn_id,
        },
    )
}

fn done_status(progress: &SessionProgress) -> &'static str {
    let Some(outcome) = progress.snapshot.last_outcome.as_ref() else {
        return "idle";
    };
    match outcome.kind {
        moa_core::wire::turn::TurnOutcomeKind::Completed => "completed",
        moa_core::wire::turn::TurnOutcomeKind::Cancelled => "cancelled",
        moa_core::wire::turn::TurnOutcomeKind::Failed => "failed",
    }
}

fn record_sse_event(record: &EventRecord) -> SseEvent {
    let event_name = match &record.event {
        Event::ProgressUpdate { .. } => "progress",
        Event::BrainResponse { .. } => "response",
        Event::ToolCall { .. } | Event::ToolResult { .. } | Event::ToolError { .. } => "tool",
        _ => "session_event",
    };
    match SseEvent::default()
        .id(record.sequence_num.to_string())
        .event(event_name)
        .json_data(record)
    {
        Ok(event) => event,
        Err(error) => error_sse_event(format!("failed to serialize session event: {error}")),
    }
}

fn json_sse_event<T: Serialize>(event_name: &'static str, data: &T) -> SseEvent {
    match SseEvent::default().event(event_name).json_data(data) {
        Ok(event) => event,
        Err(error) => error_sse_event(format!("failed to serialize SSE event: {error}")),
    }
}

fn error_sse_event(message: String) -> SseEvent {
    let data = serde_json::json!({ "message": message });
    SseEvent::default().event("error").data(data.to_string())
}

#[cfg(feature = "auth0")]
async fn handle_auth0_connection_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let secret = match state.auth0_webhook_secret.as_deref() {
        Some(secret) if !secret.trim().is_empty() => secret,
        None | Some(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook secret not configured",
            )
                .into_response();
        }
    };
    if !verify_auth0_signature(&headers, &body, secret) {
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
    let token = authorization_bearer_token(headers)?;
    if token.starts_with("moa_") {
        return Some(Credential::ApiKey(token));
    }
    Some(Credential::BearerJwt(token))
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

fn translate_public_route(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> RouteTranslation {
    for translate in [
        auth::translate,
        agents::translate,
        session::translate,
        analytics::translate,
        memory::translate,
        knowledge::translate,
        artifacts::translate,
        workflows::translate,
        tools::translate,
    ] {
        if let Some(translation) = translate(method, uri, body, tenant_id) {
            return translation;
        }
    }
    RouteTranslation::NoChange
}

fn tenant_id_field(tenant_id: TenantId) -> (&'static str, serde_json::Value) {
    ("tenant_id", serde_json::json!(tenant_id))
}

fn tenant_scope_value(tenant_id: TenantId) -> serde_json::Value {
    serde_json::json!({ "tenant": { "tenant_id": tenant_id } })
}

fn translate_json_object_with_tenant_id(
    body: &Bytes,
    target: &str,
    tenant_id: TenantId,
) -> RouteTranslation {
    translate_json_object_with_fields(
        body,
        target,
        "bad tenant route body",
        "tenant route body must be object",
        "serialize tenant route body failed",
        [tenant_id_field(tenant_id)],
    )
}

fn translate_json_object_with_tenant_scope(
    body: &Bytes,
    target: &str,
    tenant_id: TenantId,
) -> RouteTranslation {
    translate_json_object_with_fields(
        body,
        target,
        "bad tenant route body",
        "tenant route body must be object",
        "serialize tenant route body failed",
        [("scope", tenant_scope_value(tenant_id))],
    )
}

fn translate_knowledge_json_body<T>(
    body: &Bytes,
    target: &str,
    tenant_id: TenantId,
) -> RouteTranslation
where
    T: DeserializeOwned + Serialize,
{
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad knowledge route body"),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest("knowledge route body must be object");
    };
    object.insert("tenant_id".to_string(), serde_json::json!(tenant_id));

    let request = match serde_json::from_value::<T>(value) {
        Ok(request) => request,
        Err(_) => return RouteTranslation::BadRequest("bad knowledge route body"),
    };
    let bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "serialize knowledge route body failed");
            return RouteTranslation::BadRequest("bad knowledge route body");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body: bytes,
    }
}

fn rejects_raw_webhook_content_type(headers: &HeaderMap) -> bool {
    let Some(media_type) = header_media_type(headers, header::CONTENT_TYPE) else {
        return false;
    };
    let media_type = media_type.to_ascii_lowercase();

    media_type.starts_with("multipart/")
        || media_type.starts_with("image/")
        || media_type.starts_with("audio/")
        || media_type.starts_with("video/")
        || matches!(
            media_type.as_str(),
            "application/octet-stream"
                | "application/pdf"
                | "application/zip"
                | "application/x-zip-compressed"
                | "application/x-tar"
                | "application/gzip"
        )
}

fn header_media_type(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn forwarded_webhook_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let lowercase_name = name.as_str().to_ascii_lowercase();
            if headers::is_moa_header(&lowercase_name)
                || lowercase_name == "authorization"
                || lowercase_name == "cookie"
                || lowercase_name == "set-cookie"
                || is_proxy_hop_by_hop_header(&lowercase_name)
            {
                return None;
            }
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

fn is_proxy_hop_by_hop_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn webhook_event_id(headers: &HeaderMap, payload: &serde_json::Value) -> Option<String> {
    webhook_header_value(
        headers,
        &["svix-id", "x-svix-id", "webhook-id", "x-webhook-id"],
    )
    .or_else(|| {
        json_string_field(
            payload,
            &[
                "event_id",
                "id",
                "webhook_id",
                "hook.id",
                "job_id",
                "parse_id",
                "data.id",
                "data.job_id",
                "data.parse_id",
            ],
        )
    })
}

fn webhook_event_type(headers: &HeaderMap, payload: &serde_json::Value) -> Option<String> {
    webhook_header_value(
        headers,
        &[
            "svix-event-type",
            "x-event-type",
            "x-webhook-event",
            "x-webhook-event-type",
        ],
    )
    .or_else(|| {
        json_string_field(
            payload,
            &[
                "event_type",
                "type",
                "event",
                "status",
                "hook.event",
                "data.event_type",
                "data.type",
                "data.status",
            ],
        )
    })
}

fn webhook_header_value(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string())
    })
}

fn json_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let mut current = value;
        for segment in key.split('.') {
            current = current.get(segment)?;
        }
        match current {
            serde_json::Value::String(value) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    })
}

fn translate_create_agent_session_route(body: &Bytes, tenant_id: TenantId) -> RouteTranslation {
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad agent session body"),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest("agent session body must be object");
    };
    let Some(meta) = object
        .get_mut("meta")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return RouteTranslation::BadRequest("agent session meta must be object");
    };
    meta.insert("tenant_id".to_string(), serde_json::json!(tenant_id));
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "serialize agent session body failed");
            return RouteTranslation::BadRequest("bad agent session body");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: "/SessionStore/create_agent_session".to_string(),
        body: bytes,
    }
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
    translate_json_object_with_fields(
        body,
        target,
        "bad agent act-as body",
        "agent act-as body must be object",
        "serialize agent act-as body failed",
        [("agent_id", serde_json::json!(agent_id))],
    )
}

fn translate_empty_json_body_with_fields<const N: usize>(
    target: &str,
    bad_body_message: &'static str,
    fields: [(&str, serde_json::Value); N],
) -> RouteTranslation {
    let value = fields
        .into_iter()
        .map(|(name, field_value)| (name.to_string(), field_value))
        .collect::<serde_json::Map<_, _>>();
    let bytes = match serde_json::to_vec(&serde_json::Value::Object(value)) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "serialize synthetic route body failed");
            return RouteTranslation::BadRequest(bad_body_message);
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body: bytes,
    }
}

fn translate_json_object_with_fields<const N: usize>(
    body: &Bytes,
    target: &str,
    bad_body_message: &'static str,
    non_object_message: &'static str,
    serialize_log_message: &'static str,
    fields: [(&str, serde_json::Value); N],
) -> RouteTranslation {
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest(bad_body_message),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest(non_object_message);
    };
    for (name, field_value) in fields {
        object.insert(name.to_string(), field_value);
    }
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "{serialize_log_message}");
            return RouteTranslation::BadRequest(bad_body_message);
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body: bytes,
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};
    use moa_core::TenantId;
    use uuid::Uuid;

    use super::{RouteTranslation, translate_public_route};

    pub(super) const TEST_TENANT_ID: &str = "22222222-2222-2222-2222-222222222222";

    pub(super) fn test_tenant_id() -> TenantId {
        TenantId::from(Uuid::parse_str(TEST_TENANT_ID).expect("test tenant id should parse"))
    }

    pub(super) fn test_tenant_json() -> serde_json::Value {
        serde_json::json!(TEST_TENANT_ID)
    }

    pub(super) fn test_tenant_scope_json() -> serde_json::Value {
        serde_json::json!({ "tenant": { "tenant_id": TEST_TENANT_ID } })
    }

    pub(super) fn translate(method: &Method, uri: &Uri, body: &Bytes) -> RouteTranslation {
        translate_public_route(method, uri, body, test_tenant_id())
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use axum::http::header::AUTHORIZATION;
    use chrono::Utc;
    use moa_core::traits::{AuthError, Identity, IdentityType};
    use moa_core::wire::turn::{SessionSnapshot, TurnOutcome, TurnOutcomeKind};
    use moa_core::{EventType, SessionId, TenantId};

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
                tenant_id: TenantId::from(Uuid::nil()),
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

    /// Computes the hex-encoded HMAC-SHA256 of `body` under `key`, mirroring how
    /// upstream webhook providers sign payloads so the verifier's accept path is
    /// exercised with a genuinely valid signature rather than a precomputed constant.
    fn hmac_sha256_hex(key: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_hmac_header_accepts_valid_signature_and_rejects_tampering() {
        // Pins: the webhook HMAC header verifier (the 401 ingress gate) accepts a
        // correctly signed body, rejects the same signature over a tampered body, and
        // rejects a request with no signature header.
        let key = "edge-webhook-secret";
        let body = br#"{"event":"document.processed"}"#;
        let header = "x-moa-knowledge-webhook-signature";

        let mut headers = HeaderMap::new();
        headers.insert(
            header,
            hmac_sha256_hex(key.as_bytes(), body)
                .parse()
                .expect("hex signature is a valid header value"),
        );

        // A correctly signed body is accepted.
        assert!(verify_hmac_header(&headers, body, key, header).is_ok());

        // The same signature over a tampered body is rejected as invalid.
        let tampered = br#"{"event":"document.deleted!"}"#;
        assert_eq!(
            verify_hmac_header(&headers, tampered, key, header),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        // A request with no signature header is rejected as missing.
        assert_eq!(
            verify_hmac_header(&HeaderMap::new(), body, key, header),
            Err((StatusCode::UNAUTHORIZED, "missing webhook signature"))
        );
    }

    #[test]
    fn verify_hmac_signature_distinguishes_matching_from_forged_signatures() {
        // Pins: the raw HMAC-SHA256 comparison accepts the exact signature, rejects a
        // signature computed over different bytes, and rejects an empty signature.
        let key = b"shared-secret";
        let body = b"payload-bytes";

        let valid = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("valid key");
            mac.update(body);
            mac.finalize().into_bytes()
        };
        assert!(verify_hmac_signature(key, body, valid.as_slice()).is_ok());

        // A signature computed over different bytes does not verify.
        let forged = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("valid key");
            mac.update(b"other-bytes");
            mac.finalize().into_bytes()
        };
        assert_eq!(
            verify_hmac_signature(key, body, forged.as_slice()),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        // An empty signature is rejected.
        assert_eq!(
            verify_hmac_signature(key, body, &[]),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );
    }

    #[test]
    fn verify_svix_signature_accepts_valid_and_rejects_tampered_or_missing() {
        // Pins: the Svix-style edge verifier accepts a `v1,` signature over the
        // `id.timestamp.body` payload, rejects a tampered body, and rejects a request
        // whose signature header is absent (id + timestamp present).
        let key = "svix-shared-secret";
        let message_id = "msg_2abc";
        let timestamp = Utc::now().timestamp();
        let body = r#"{"type":"connection.created"}"#;
        let signed_payload = format!("{message_id}.{timestamp}.{body}");

        let signature = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("valid key");
            mac.update(signed_payload.as_bytes());
            general_purpose::STANDARD.encode(mac.finalize().into_bytes())
        };

        let mut headers = HeaderMap::new();
        headers.insert("svix-id", message_id.parse().expect("valid header value"));
        headers.insert(
            "svix-timestamp",
            timestamp.to_string().parse().expect("valid header value"),
        );
        headers.insert(
            "svix-signature",
            format!("v1,{signature}")
                .parse()
                .expect("valid header value"),
        );

        // A correctly signed payload is accepted.
        assert!(verify_svix_signature_at_edge(&headers, body.as_bytes(), key).is_ok());

        // A tampered body no longer matches the signed payload.
        assert_eq!(
            verify_svix_signature_at_edge(&headers, b"{}", key),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        // A request missing only the signature header is rejected as missing.
        let mut without_sig = HeaderMap::new();
        without_sig.insert("svix-id", message_id.parse().expect("valid header value"));
        without_sig.insert(
            "svix-timestamp",
            timestamp.to_string().parse().expect("valid header value"),
        );
        assert_eq!(
            verify_svix_signature_at_edge(&without_sig, body.as_bytes(), key),
            Err((StatusCode::UNAUTHORIZED, "missing webhook signature"))
        );
    }

    #[cfg(feature = "auth0")]
    #[test]
    fn verify_auth0_signature_accepts_valid_and_rejects_tampered_or_missing() {
        // Pins: the Auth0 webhook verifier accepts a `sha256=` hex HMAC of the body,
        // rejects a tampered body, and rejects a request with no Auth0-Signature header.
        let secret = "auth0-webhook-secret";
        let body = br#"{"type":"user.created"}"#;

        let mut headers = HeaderMap::new();
        headers.insert(
            "auth0-signature",
            format!("sha256={}", hmac_sha256_hex(secret.as_bytes(), body))
                .parse()
                .expect("valid header value"),
        );

        // A correctly signed body is accepted.
        assert!(verify_auth0_signature(&headers, body, secret));

        // A tampered body is rejected.
        assert!(!verify_auth0_signature(&headers, b"{}", secret));

        // A request with no Auth0-Signature header is rejected.
        assert!(!verify_auth0_signature(&HeaderMap::new(), body, secret));
    }

    #[test]
    fn session_message_stream_cursor_starts_after_latest_event() {
        // Pins: a fresh browser message stream does not replay old session history.
        let records = vec![event_record(4), event_record(9), event_record(7)];

        assert_eq!(next_sequence_after(&records), 10);
        assert_eq!(next_sequence_after(&[]), 1);
    }

    #[test]
    fn session_message_stream_finishes_when_started_turn_reports_outcome() {
        // Pins: a stream for an immediately-started message closes on that turn, not on later session idle.
        let progress = session_progress(
            Some("next-turn".to_string()),
            1,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "done".to_string(),
            }),
        );

        assert!(session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));
        assert!(!session_message_terminal_done(
            Some("other-turn"),
            &progress
        ));
    }

    #[test]
    fn queued_session_message_stream_finishes_when_session_is_idle() {
        // Pins: queued messages have no accepted turn id, so their stream waits for the queue to drain.
        let running = session_progress(Some("active-turn".to_string()), 1, None);
        let idle = session_progress(None, 0, None);

        assert!(!session_message_terminal_done(None, &running));
        assert!(session_message_terminal_done(None, &idle));
    }

    #[test]
    fn session_message_request_uses_path_session_id() {
        // Pins: browser clients send the session once in the path; conflicting body values cannot retarget the message.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "session_id":"33333333-3333-3333-3333-333333333333",
                "contact_token":"token",
                "user_message":"hello"
            }"#,
        );

        let request = contact_session_message_request(path_session_id, &body)
            .expect("message request should decode");

        assert_eq!(request.session_id, SessionId(path_session_id));
        assert_eq!(
            request.tenant_id,
            TenantId::from(
                Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                    .expect("tenant id should parse")
            )
        );
        assert_eq!(request.contact_token, "token");
        assert_eq!(request.user_message, "hello");
    }

    #[test]
    fn session_message_request_rejects_json_attachment_refs() {
        // Pins: public clients must upload attachments as multipart so the edge can validate bytes.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "contact_token":"token",
                "attachments":[{
                    "name":"receipt.png",
                    "mime_type":"image/png",
                    "url":"/v1/sessions/11111111-1111-1111-1111-111111111111/attachments/33333333-3333-3333-3333-333333333333",
                    "path":null,
                    "size_bytes":128
                }]
            }"#,
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("json attachment refs should be rejected");

        assert_eq!(
            error,
            "session message attachments must be uploaded as multipart"
        );
    }

    #[test]
    fn session_message_request_rejects_empty_body() {
        // Pins: a contact message must contain either text or at least one attachment ref.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "contact_token":"token"
            }"#,
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("empty message should be rejected");

        assert_eq!(
            error,
            "contact session message requires text or an attachment"
        );
    }

    #[test]
    fn session_message_request_rejects_oversized_text() {
        // Pins: public JSON contact messages cannot force huge text into Restate/session history.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from(
            serde_json::json!({
                "tenant_id": "22222222-2222-2222-2222-222222222222",
                "contact_token": "token",
                "user_message": "x".repeat(moa_core::MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES + 1),
            })
            .to_string(),
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("oversized message should be rejected");

        assert_eq!(error, "session message text is too long");
    }

    #[test]
    fn canonical_photo_mime_requires_supported_image_content() {
        // Pins: upload admission trusts content sniffing over caller-declared MIME type.
        let png = png_with_dimensions(640, 480);
        assert_eq!(
            canonical_photo_mime(Some("image/png"), &png).expect("valid png should be accepted"),
            "image/png"
        );
        assert_eq!(
            canonical_photo_mime(Some("image/jpeg"), &png)
                .expect_err("declared MIME mismatch should be rejected")
                .message,
            "photo MIME type does not match content"
        );
        assert_eq!(
            canonical_photo_mime(Some("application/pdf"), b"%PDF")
                .expect_err("non-photo bytes should be rejected")
                .message,
            "only jpeg, png, and webp photos are supported"
        );
        assert_eq!(
            canonical_photo_mime(Some("image/gif"), &png)
                .expect_err("unsupported declared image type should be rejected")
                .message,
            "only jpeg, png, and webp photos are supported"
        );
    }

    #[test]
    fn canonical_photo_mime_rejects_decompression_bomb_dimensions() {
        // Pins: compressed image bytes must declare bounded dimensions before storage.
        let huge_png = png_with_dimensions(40_000, 40_000);

        let error = canonical_photo_mime(Some("image/png"), &huge_png)
            .expect_err("huge image dimensions should be rejected");

        assert_eq!(error.message, "photo dimensions are too large");
    }

    #[test]
    fn canonical_photo_mime_rejects_header_only_png() {
        // Pins: upload admission requires a minimally structured image container, not only magic bytes.
        let mut header_only_png = Vec::from(&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]);
        header_only_png.extend_from_slice(&640_u32.to_be_bytes());
        header_only_png.extend_from_slice(&480_u32.to_be_bytes());
        header_only_png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);

        let error = canonical_photo_mime(Some("image/png"), &header_only_png)
            .expect_err("header-only png should be rejected");

        assert_eq!(error.message, "photo dimensions could not be verified");
    }

    #[test]
    fn validated_upload_name_rejects_control_characters() {
        // Pins: caller-supplied display names cannot carry control bytes into stored attachment metadata.
        let error = validated_upload_name(Some("invoice\n.png"))
            .expect_err("control characters should be rejected");

        assert_eq!(error.message, "photo file name is invalid");
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        append_png_chunk(&mut bytes, b"IHDR", &ihdr);
        append_png_chunk(
            &mut bytes,
            b"IDAT",
            &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        append_png_chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

    fn append_png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
    }

    fn event_record(sequence_num: SequenceNum) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId(Uuid::nil()),
            sequence_num,
            event_type: EventType::Warning,
            event: Event::Warning {
                message: "test".to_string(),
            },
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn session_progress(
        active_turn_id: Option<String>,
        pending_message_count: u64,
        last_outcome: Option<TurnOutcome>,
    ) -> SessionProgress {
        SessionProgress {
            snapshot: SessionSnapshot {
                session_id: Uuid::nil().to_string(),
                active_turn_id,
                pending_message_count,
                last_outcome,
            },
            active_turn_progress: None,
            events: Vec::new(),
        }
    }
}
