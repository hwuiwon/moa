//! HTTP routes exposed by the MOA edge service.
#![allow(clippy::result_large_err)]

use crate::ingress::{IngressScope, call_path};
use crate::{headers, proxy::OrchestratorProxy};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{any, get, patch, post};
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{AuthProvider, Credential, Identity};
use moa_core::{
    ContactSessionAuthorizationRequest, ContactSessionAuthorizationResponse,
    ContactSessionMessageResponse, MoaConfig, MoaError, SessionAttachmentId,
    SessionAttachmentStore, SessionId, TenantId,
};
use moa_session::PostgresSessionStore;
#[cfg(feature = "auth0")]
use serde::Deserialize;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES: usize = 256 * 1024;
const SESSION_MESSAGE_BODY_LIMIT_BYTES: usize = 12 * 1024 * 1024;

mod agents;
mod analytics;
mod artifacts;
mod audit;
mod auth;
mod contact_messages;
mod knowledge;
mod lineage;
mod memory;
mod session;
mod session_stream;
mod tools;
mod webhook_verification;
mod whoami;

use self::contact_messages::{
    attachment_response, authorization_bearer_token, cleanup_session_attachments,
    persist_session_attachments, session_message_input,
};
use self::session_stream::{
    initial_stream_sequence, last_event_id_sequence, session_message_stream_response,
};
#[cfg(feature = "auth0")]
use self::webhook_verification::verify_auth0_signature;
use self::webhook_verification::verify_knowledge_webhook_at_edge;

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
            // Every catch-all route reaching the proxy is a read, status poll, or
            // lifecycle write; turn-starting message posts take dedicated handlers, so
            // these stay unscoped and never consume a tenant's concurrency slot.
            RouteTranslation::Forward { method, path, body } => {
                (method, call_path(&IngressScope::Unscoped, &path), body)
            }
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
    let path = call_path(&IngressScope::Unscoped, &path);

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
    // Contact verification, session init, promote, and channel change set up sessions;
    // the turn only starts when a message is posted, so these stay unscoped.
    let path = call_path(&IngressScope::Unscoped, &path);
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

    let reconnect_after = last_event_id_sequence(&headers);
    let next_sequence_num =
        match initial_stream_sequence(&state, &input.message, reconnect_after).await {
            Ok(next_sequence_num) => next_sequence_num,
            Err(error) => {
                tracing::warn!(error = %error.summary(), "session stream preflight failed");
                span.record("http.status_code", error.status_code().as_u16() as i64);
                return error.into_response();
            }
        };

    if reconnect_after.is_some() {
        // Reconnect (Last-Event-ID present): the message was already admitted on the original
        // request. Re-admitting it here would start a duplicate turn, so resume the stream from
        // the cursor without re-admitting or re-persisting attachments.
        span.record("http.status_code", 200_i64);
        let resumed = ContactSessionMessageResponse {
            session_id: input.message.session_id,
            queued: false,
            started_turn_id: None,
        };
        return session_message_stream_response(state, input.message, resumed, next_sequence_num);
    }

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
        input.message.tenant_id,
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

    session_message_stream_response(state, input.message, accepted, next_sequence_num)
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
        tenant_id,
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

/// Flow-control scope for a `Contacts` ingress call.
///
/// `send_message` queues a message on the `Session` VO and starts a turn, so it
/// consumes the tenant's agent-work concurrency and is enrolled in per-tenant
/// admission control. Verification, authorization, and progress reads are cheap
/// and stay unscoped so they never wait behind a tenant's turn concurrency.
fn contacts_scope(handler: &str, tenant_id: TenantId) -> IngressScope {
    match handler {
        "send_message" => IngressScope::Tenant(tenant_id),
        _ => IngressScope::Unscoped,
    }
}

async fn call_contacts_handler<I, O>(
    state: &AppState,
    handler: &str,
    input: &I,
    tenant_id: TenantId,
) -> Result<O, EdgeJsonError>
where
    I: Serialize + ?Sized,
    O: serde::de::DeserializeOwned,
{
    let body =
        serde_json::to_vec(input).map_err(|error| EdgeJsonError::Serialize(error.to_string()))?;
    let path = call_path(
        &contacts_scope(handler, tenant_id),
        &format!("/Contacts/{handler}"),
    );
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
    use moa_core::TenantId;
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

    #[test]
    fn only_send_message_takes_the_tenant_flow_control_scope() {
        // Pins: posting a message starts a turn and is enrolled in per-tenant admission
        // control, while contact-session reads and lifecycle calls (progress,
        // authorize_session, init_session) stay unscoped so a status poll or session setup
        // never consumes a tenant's concurrency slot.
        let tenant = TenantId::from(
            Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("tenant uuid parses"),
        );

        assert_eq!(
            contacts_scope("send_message", tenant),
            IngressScope::Tenant(tenant)
        );
        for read_handler in ["progress", "authorize_session", "init_session"] {
            assert_eq!(
                contacts_scope(read_handler, tenant),
                IngressScope::Unscoped,
                "{read_handler} must not consume tenant concurrency"
            );
        }
    }
}
