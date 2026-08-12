//! HTTP routes exposed by the MOA edge service.
#![allow(clippy::result_large_err)]

use crate::connector_credential_proxy::ConnectorCredentialProxy;
use crate::external_job_callback_proxy::{
    EXTERNAL_JOB_CALLBACK_PUBLIC_ROUTE, ExternalJobCallbackProxy,
    MAX_EXTERNAL_JOB_CALLBACK_BODY_BYTES,
};
use crate::ingress::call_path;
use crate::{headers, proxy::OrchestratorProxy};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{any, get, patch, post, put};
use chrono::{DateTime, Utc};
use moa_auth_providers::oauth_access_token::AuthenticatedPrincipal;
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_config::MoaConfig;
use moa_core::traits::{AuthProvider, Credential, Identity};
use moa_core::{
    error::MoaError, traits::SessionAttachmentStore,
    types::contact::ContactSessionAuthorizationRequest,
    types::contact::ContactSessionAuthorizationResponse,
    types::contact::ContactSessionMessageResponse, types::identifiers::SessionAttachmentId,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_messaging::ProviderDeliverySink;
use moa_session::PostgresSessionStore;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES: usize = 256 * 1024;
const SESSION_MESSAGE_BODY_LIMIT_BYTES: usize = 12 * 1024 * 1024;
const USER_SESSION_COOKIE_NAME: &str = "__Host-user_session";

mod agents;
pub(crate) mod analytics;
mod artifacts;
mod audit;
mod auth;
pub(crate) mod auth_accounts;
mod connectors;
mod contact_messages;
pub(crate) mod dashboard;
mod external_jobs;
mod knowledge;
pub(crate) mod lineage;
mod memory;
mod oauth;
mod sandbox_workspaces;
mod session;
mod session_stream;
mod tenant_accounts;
mod webhook_verification;
mod whoami;

use self::contact_messages::{
    attachment_response, authorization_bearer_token, cleanup_session_attachments,
    persist_session_attachments, session_message_input,
};
use self::session_stream::{
    initial_stream_sequence, last_event_id_sequence, session_message_stream_response,
};
use self::webhook_verification::verify_knowledge_webhook_at_edge;

/// Shared edge application state.
#[derive(Clone)]
pub struct AppState {
    /// Whether public connector management and credential ingress are exposed.
    ///
    /// This rollout switch defaults dark in the binary and is checked before
    /// authentication or request-body translation on every connector route.
    pub connector_management_enabled: bool,
    /// Loaded edge configuration shared by direct read handlers.
    pub config: Arc<MoaConfig>,
    /// Credential resolver used for incoming requests.
    pub auth: Arc<dyn AuthProvider>,
    /// Cached first-party OAuth authorization server.
    pub oauth_server: Arc<moa_auth_providers::OAuthServer>,
    /// One-pass resolver for first-party OAuth access tokens.
    pub oauth_access_tokens: Arc<moa_auth_providers::OAuthAccessTokenProvider>,
    /// OpenFGA client used for direct edge authorization checks.
    pub fga: Option<Arc<FgaClient>>,
    /// Secrets used to verify public tenant-knowledge webhooks at the edge.
    pub knowledge_webhooks: KnowledgeWebhookEdgeConfig,
    /// Postgres pool used by direct edge reads and writes.
    pub pool: Arc<sqlx::PgPool>,
    /// Postgres-backed session store used by direct edge media reads and writes.
    pub session_store: Arc<PostgresSessionStore>,
    /// Deployment-owned account delivery clients.
    pub delivery: Arc<ProviderDeliverySink>,
    /// Internal orchestrator proxy.
    pub proxy: Arc<OrchestratorProxy>,
    /// Exact-path proxy to private orchestrator credential ingress.
    pub connector_credentials: Arc<ConnectorCredentialProxy>,
    /// Exact-path proxy to private asynchronous-provider callback ingress.
    pub external_job_callbacks: Arc<ExternalJobCallbackProxy>,
    /// ClickHouse lineage store when `[clickhouse]` is configured; lineage
    /// reads and offboarding deletes follow the write backend.
    pub clickhouse_lineage: Option<Arc<moa_lineage_sink::ClickHouseStore>>,
    /// ClickHouse analytics query client when `[clickhouse]` is configured;
    /// dashboard queries follow the analytics-export backend.
    pub clickhouse_analytics: Option<Arc<moa_analytics::AnalyticsClickHouseClient>>,
    /// Handle for the instance-owned security audit writer.
    ///
    /// Every authentication outcome is enqueued through this. Holding the
    /// emitter on the state is what makes the writer's ownership visible: the
    /// process that built the state also owns the task and drains it at
    /// shutdown, so an audit event cannot be enqueued onto a writer nobody can
    /// see, join, or stop.
    pub audit: moa_ocsf::AuditEmitter,
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
    crate::mcp::router(
        state,
        crate::mcp::McpHttpConfig::local_default(),
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Build the REST route ladder without the sibling MCP transport router.
pub(crate) fn base_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/auth/login", post(auth_accounts::login))
        .route("/v1/auth/logout", post(auth_accounts::logout))
        .route(
            "/v1/auth/password/reset-request",
            post(auth_accounts::request_password_reset),
        )
        .route(
            "/v1/auth/password/reset",
            post(auth_accounts::reset_password),
        )
        .route("/v1/auth/password", post(auth_accounts::change_password))
        .route(
            "/v1/users/me",
            get(auth_accounts::get_me).patch(auth_accounts::patch_me),
        )
        .route("/v1/tenants/signup", post(tenant_accounts::signup))
        .route(
            "/v1/tenant",
            get(tenant_accounts::get_tenant)
                .patch(tenant_accounts::patch_tenant)
                .delete(tenant_accounts::delete_tenant),
        )
        .route(
            "/v1/tenant/purge/{operation_id}",
            get(tenant_accounts::tenant_purge_status),
        )
        .route(
            "/v1/tenant/users",
            get(tenant_accounts::list_users).post(tenant_accounts::create_user),
        )
        .route("/v1/tenant/invitations", post(tenant_accounts::invite_user))
        .route(
            "/v1/tenant/invitations/accept",
            post(tenant_accounts::accept_invitation),
        )
        .route(
            "/v1/tenant/users/{user_id}/password",
            post(auth_accounts::set_user_password),
        )
        .route("/v1/whoami", get(whoami::handle))
        .route(
            EXTERNAL_JOB_CALLBACK_PUBLIC_ROUTE,
            post(external_jobs::handle_callback)
                .layer(DefaultBodyLimit::max(MAX_EXTERNAL_JOB_CALLBACK_BODY_BYTES)),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth::protected_resource_metadata),
        )
        .route(
            "/oauth/authorize",
            get(oauth::authorize).post(oauth::authorize_decision),
        )
        .route("/oauth/token", post(oauth::token))
        .route("/oauth/introspect", post(oauth::introspect))
        .route("/oauth/revoke", post(oauth::revoke))
        .route("/v1/analytics/catalog", get(analytics::handle_catalog))
        .route("/v1/analytics/query", post(analytics::handle_query))
        .route("/v1/audit/verify", post(audit::handle_verify))
        .route("/v1/lineage/explain", post(lineage::handle_explain))
        .route("/v1/lineage/query", post(lineage::handle_query))
        .route("/v1/lineage/verify", post(lineage::handle_verify))
        .merge(dashboard::router())
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
        .route(
            "/v1/connectors/connections/{connection_id}/credentials/{slot_name}",
            put(connectors::write_credential),
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
    if !state.connector_management_enabled && connectors::matches_management_path(uri.path()) {
        span.record("http.status_code", 404_i64);
        return StatusCode::NOT_FOUND.into_response();
    }
    adopt_client_trace_parent(&span, &headers);
    let principal = match authenticate_edge_request(&state, &headers, &span).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if principal.is_oauth() {
        span.record("http.status_code", 401_i64);
        return (StatusCode::UNAUTHORIZED, "OAuth bearer tokens are MCP-only").into_response();
    }
    let identity = principal.identity;

    let (method, path, body) =
        match translate_public_route(&method, &uri, &body, identity.tenant_id) {
            RouteTranslation::Forward { method, path, body } => (method, call_path(&path), body),
            RouteTranslation::NotFound => {
                span.record("http.status_code", 404_i64);
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
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
) -> Result<AuthenticatedPrincipal, Response> {
    let credential = match credential_for_request(state.auth.as_ref(), headers) {
        Some(credential) => credential,
        None => {
            span.record("moa.edge.auth.result", "missing_credential");
            moa_ocsf::spawn_authn_failure(
                &state.audit,
                Uuid::nil(),
                None,
                "unknown",
                source_ip(headers),
                "missing credential",
            );
            span.record("http.status_code", 401_i64);
            return Err((StatusCode::UNAUTHORIZED, "missing credential").into_response());
        }
    };

    let (principal, provider_name) = match credential {
        Credential::OAuthAccessToken(token) => {
            match state.oauth_access_tokens.authenticate(&token).await {
                Ok(principal) => (principal, state.oauth_access_tokens.name()),
                Err(error) => {
                    span.record("moa.edge.auth.provider", state.oauth_access_tokens.name());
                    span.record("moa.edge.auth.result", "rejected");
                    moa_ocsf::spawn_authn_failure(
                        &state.audit,
                        Uuid::nil(),
                        None,
                        state.oauth_access_tokens.name(),
                        source_ip(headers),
                        &error.to_string(),
                    );
                    span.record("http.status_code", 401_i64);
                    return Err((StatusCode::UNAUTHORIZED, "invalid credential").into_response());
                }
            }
        }
        credential => match state.auth.authenticate(&credential).await {
            Ok(identity) => (
                AuthenticatedPrincipal::from_identity(identity),
                state.auth.name(),
            ),
            Err(error) => {
                span.record(
                    "moa.edge.auth.provider",
                    tracing::field::display(state.auth.name()),
                );
                span.record("moa.edge.auth.result", "rejected");
                moa_ocsf::spawn_authn_failure(
                    &state.audit,
                    Uuid::nil(),
                    None,
                    state.auth.name(),
                    source_ip(headers),
                    &error.to_string(),
                );
                tracing::info!(error = %error, provider = state.auth.name(), "authentication rejected");
                span.record("http.status_code", 401_i64);
                return Err((StatusCode::UNAUTHORIZED, "invalid credential").into_response());
            }
        },
    };
    span.record(
        "moa.edge.auth.provider",
        tracing::field::display(provider_name),
    );
    span.record("moa.edge.auth.result", "accepted");
    moa_ocsf::spawn_authn_success(
        &state.audit,
        principal.identity.tenant_id.0,
        &principal.identity,
        provider_name,
        source_ip(headers),
    );
    Ok(principal)
}

pub(super) async fn authenticate_direct_request(
    state: &AppState,
    headers: &HeaderMap,
    route: &'static str,
) -> Result<Identity, Response> {
    let span = tracing::Span::current();
    span.record("http.route", route);
    if authorization_bearer_token(headers)
        .as_deref()
        .is_some_and(moa_auth_providers::looks_like_oauth_access_token)
    {
        span.record("http.status_code", 401_i64);
        return Err((StatusCode::UNAUTHORIZED, "OAuth bearer tokens are MCP-only").into_response());
    }
    authenticate_edge_request(state, headers, &span)
        .await
        .map(|principal| principal.identity)
}

/// Adopts a client-supplied W3C trace context as the parent of the edge request
/// span, so an externally initiated trace continues through MOA rather than
/// fragmenting at the edge. A missing or malformed `traceparent` is a no-op.
pub(crate) fn adopt_client_trace_parent(span: &tracing::Span, headers: &HeaderMap) {
    moa_observability::adopt_remote_parent(span, |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
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

/// Attach a session cookie to an already-built response.
pub(super) fn attach_set_cookie(mut response: Response, cookie: HeaderValue) -> Response {
    response.headers_mut().append(header::SET_COOKIE, cookie);
    response
}

/// Build the HttpOnly browser session cookie value for an issued session token.
pub(super) fn session_cookie_header(
    token: &str,
    expires_at: DateTime<Utc>,
) -> Result<HeaderValue, String> {
    let max_age_seconds = (expires_at - Utc::now()).num_seconds().max(0);
    let value = format!(
        "{USER_SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_seconds}"
    );
    HeaderValue::from_str(&value).map_err(|error| format!("build session cookie: {error}"))
}

/// Build the cookie-clearing header for logout and invalidation flows.
pub(super) fn clear_session_cookie_header() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-user_session=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0",
    )
}

/// Return the presented first-party user-session token from Authorization or the session cookie.
pub(super) fn user_session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    authorization_bearer_token(headers)
        .filter(|token| moa_auth_providers::looks_like_user_session_token(token))
        .or_else(|| user_session_token_from_cookie(headers))
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
    let path = call_path(&path);

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
    let path = call_path(&path);
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
    adopt_client_trace_parent(&span, &headers);
    let mut input = match session_message_input(session_id, &headers, request, &state).await {
        Ok(input) => input,
        Err(error) => {
            span.record("http.status_code", error.status.as_u16() as i64);
            return (error.status, error.message).into_response();
        }
    };

    let reconnect_after = last_event_id_sequence(&headers);
    let submitted_cursor =
        match initial_stream_sequence(&state, &input.message, reconnect_after).await {
            Ok(next_sequence_num) => next_sequence_num,
            Err(error) => {
                tracing::warn!(error = %error.summary(), "session stream preflight failed");
                span.record("http.status_code", error.status_code().as_u16() as i64);
                return error.into_response();
            }
        };
    // The cursor is the edge's own observation, never the caller's claim. Storing it with
    // the admission is what lets a retry that lost its response resume from the position
    // the original submission started at instead of skipping its events.
    input.message.stream_cursor = Some(submitted_cursor);

    // A reconnect with `Last-Event-ID` still goes through admission: the Session fence is
    // the only thing that can tell a genuine retry from a fresh message, and skipping it
    // here would fabricate an empty response for work that actually started a turn.
    // Attachment slots are deterministic, so re-persisting them replays the originals.
    let mut stored_attachments = Vec::new();
    if !input.uploads.is_empty() {
        match persist_session_attachments(&state, &input.message, input.uploads).await {
            Ok(attachments) => {
                stored_attachments = attachments;
                input.message.attachments.extend(
                    stored_attachments
                        .iter()
                        .map(|stored| stored.attachment.clone()),
                );
            }
            Err(MoaError::SessionAttachmentSlotConflict(message)) => {
                tracing::warn!(%message, "session attachment slot conflict");
                span.record("http.status_code", 409_i64);
                return (
                    StatusCode::CONFLICT,
                    "attachment conflicts with a stored upload",
                )
                    .into_response();
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

    // A caller that resumed explicitly owns its cursor; otherwise the admission's stored
    // cursor wins, so a retry receives the original pre-admission position rather than the
    // stream head this attempt happened to observe.
    let next_sequence_num = match reconnect_after {
        Some(last_event_id) => last_event_id.saturating_add(1),
        None => accepted.stream_cursor.unwrap_or(submitted_cursor),
    };
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
    adopt_client_trace_parent(&span, &headers);
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

#[derive(Debug)]
enum EdgeJsonError {
    Serialize(String),
    Forward(String),
    Upstream {
        status: StatusCode,
        body: String,
        retry_after: Option<String>,
    },
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
            Self::Upstream { status, body, .. } if body.is_empty() => {
                format!("upstream returned {status}")
            }
            Self::Upstream { status, body, .. } => format!("upstream returned {status}: {body}"),
            Self::Read(error) => format!("upstream response read failed: {error}"),
            Self::Decode(error) => format!("upstream response decode failed: {error}"),
        }
    }

    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        let retry_after = match &self {
            Self::Upstream {
                retry_after, body, ..
            } if status == StatusCode::TOO_MANY_REQUESTS => retry_after
                .clone()
                .or_else(|| retry_after_from_terminal_body(body)),
            _ => None,
        };
        let body = match self {
            Self::Upstream { body, .. } if !body.is_empty() => body,
            error => error.summary(),
        };
        let mut response = (status, body).into_response();
        if let Some(retry_after) = retry_after
            && let Ok(value) = retry_after.parse()
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

fn retry_after_from_terminal_body(body: &str) -> Option<String> {
    let marker = "retry_after_ms=";
    let start = body.find(marker)? + marker.len();
    let millis = body[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse::<u64>()
        .ok()?;
    Some(millis.div_ceil(1_000).max(1).to_string())
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
    let path = call_path(&format!("/Contacts/{handler}"));
    let response = state
        .proxy
        .forward_public(Method::POST, &path, body, &HeaderMap::new())
        .await
        .map_err(|error| EdgeJsonError::Forward(error.to_string()))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response
        .bytes()
        .await
        .map_err(|error| EdgeJsonError::Read(error.to_string()))?;
    if !status.is_success() {
        return Err(EdgeJsonError::Upstream {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
            retry_after,
        });
    }
    serde_json::from_slice(&body).map_err(|error| EdgeJsonError::Decode(error.to_string()))
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
        let name_str = name.as_str();
        if name_str.eq_ignore_ascii_case("transfer-encoding")
            || name_str.eq_ignore_ascii_case("connection")
            || name_str.eq_ignore_ascii_case("keep-alive")
        {
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
    if let Some(token) = authorization_bearer_token(headers) {
        return Some(credential_from_bearer_token(token));
    }
    user_session_token_from_cookie(headers).map(Credential::UserSessionToken)
}

fn credential_from_bearer_token(token: String) -> Credential {
    if moa_auth_providers::looks_like_user_session_token(&token) {
        return Credential::UserSessionToken(token);
    }
    // MOA-issued OAuth access tokens share the `moa_` namespace with API keys, so
    // this more specific prefix must be checked first to route them to the opaque
    // OAuth resolver rather than the CRC-validated API-key path.
    if moa_auth_providers::looks_like_oauth_access_token(&token) {
        return Credential::OAuthAccessToken(token);
    }
    if token.starts_with("moa_") {
        return Credential::ApiKey(token);
    }
    Credential::BearerJwt(token)
}

fn user_session_token_from_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            let value = value.trim();
            (name.trim() == USER_SESSION_COOKIE_NAME
                && moa_auth_providers::looks_like_user_session_token(value))
            .then(|| value.to_string())
        })
}

#[derive(Debug, PartialEq, Eq)]
enum RouteTranslation {
    NotFound,
    Forward {
        method: Method,
        path: String,
        body: Vec<u8>,
    },
    BadRequest(&'static str),
}

#[cfg(test)]
impl RouteTranslation {
    #[allow(non_upper_case_globals)]
    const NoChange: Self = Self::NotFound;
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
        connectors::translate,
        sandbox_workspaces::translate,
    ] {
        if let Some(translation) = translate(method, uri, body, tenant_id) {
            return translation;
        }
    }
    RouteTranslation::NotFound
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
    use moa_core::types::identifiers::TenantId;
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
mod tests;
