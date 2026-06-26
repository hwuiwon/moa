//! HTTP routes exposed by the MOA edge service.

use crate::{headers, proxy::OrchestratorProxy};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{any, get, patch, post};
use base64::{Engine as _, engine::general_purpose};
use futures_util::stream;
use moa_core::traits::{AuthProvider, Credential, Identity};
use moa_core::wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeCreateLinkTokenRequest, KnowledgeExchangeTokenRequest,
    KnowledgeObjectInspectRequest, KnowledgeObjectListRequest, KnowledgeProviderWebhookRequest,
    KnowledgeQueryTraceRequest, KnowledgeSyncEventsRequest, KnowledgeSyncRequest,
    KnowledgeSyncStatusRequest,
};
use moa_core::wire::turn::SessionProgress;
use moa_core::{
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    Event, EventRange, EventRecord, SequenceNum, SessionId, TenantId,
};
#[cfg(feature = "auth0")]
use serde::Deserialize;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[cfg(feature = "auth0")]
use hmac::{Hmac, Mac};
#[cfg(feature = "auth0")]
use sha2::Sha256;
#[cfg(feature = "auth0")]
use subtle::ConstantTimeEq;

const KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES: usize = 256 * 1024;

/// Shared edge application state.
#[derive(Clone)]
pub struct AppState {
    /// Credential resolver used for incoming requests.
    pub auth: Arc<dyn AuthProvider>,
    /// Shared secret used to verify Auth0 connection-linked webhooks.
    pub auth0_webhook_secret: Option<String>,
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
            "/v1/agent-sessions/{session_id}/contacts/verification/start",
            post(handle_public_session_contact_verification_start),
        )
        .route(
            "/v1/agent-sessions/{session_id}/contacts/verification/complete",
            post(handle_public_session_contact_verification_complete),
        )
        .route("/v1/agent-sessions", post(handle_public_agent_session_init))
        .route(
            "/v1/agent-sessions/{session_id}/promote",
            post(handle_public_agent_session_promote),
        )
        .route(
            "/v1/agent-sessions/{session_id}/channel",
            patch(handle_public_agent_session_channel_change),
        )
        .route(
            "/v1/sessions/{session_id}/messages",
            post(handle_session_message_stream),
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

async fn authenticate_edge_request(
    state: &AppState,
    headers: &HeaderMap,
    span: &tracing::Span,
) -> Result<Identity, axum::response::Response> {
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
    let RouteTranslation::Forward { method, path, body } =
        translate_knowledge_provider_webhook(provider, &headers, &body)
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
    skip(state, body),
    fields(
        http.route = "/v1/sessions/{session_id}/messages",
        http.status_code = tracing::field::Empty,
    )
)]
async fn handle_session_message_stream(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    body: Bytes,
) -> axum::response::Response {
    let span = tracing::Span::current();
    let message = match contact_session_message_request(session_id, &body) {
        Ok(message) => message,
        Err(message) => {
            span.record("http.status_code", 400_i64);
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };

    let next_sequence_num = match initial_stream_sequence(&state, &message).await {
        Ok(next_sequence_num) => next_sequence_num,
        Err(error) => {
            tracing::warn!(error = %error.summary(), "session stream preflight failed");
            span.record("http.status_code", error.status_code().as_u16() as i64);
            return error.into_response();
        }
    };
    let accepted = match call_contacts_handler::<_, ContactSessionMessageResponse>(
        &state,
        "send_message",
        &message,
    )
    .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(error = %error.summary(), "session message admission failed");
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
        tenant_id: message.tenant_id,
        session_id: accepted.session_id,
        contact_token: message.contact_token,
        next_sequence_num,
        terminal_turn_id,
        pending_events,
        closed: false,
    };
    Sse::new(stream::unfold(stream_state, next_session_message_event))
        .keep_alive(KeepAlive::default())
        .into_response()
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
    serde_json::from_value(value).map_err(|_| "bad session message body")
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

fn translate_public_route(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> RouteTranslation {
    if *method == Method::GET && uri.path() == "/v1/whoami" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/Whoami/whoami".to_string(),
            body: Vec::new(),
        };
    }
    if *method == Method::GET && uri.path() == "/v1/authz-challenges" {
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/AuthzChallenges/list_mine".to_string(),
            body: Vec::new(),
        };
    }
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/authz-challenges/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let challenge_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return RouteTranslation::BadRequest("bad authz challenge id"),
        };
        return translate_json_object_with_fields(
            body,
            "/AuthzChallenges/decide",
            "bad decision body",
            "decision body must be object",
            "serialize authz challenge decision body failed",
            [("id", serde_json::json!(challenge_id))],
        );
    }
    if *method == Method::GET && uri.path() == "/v1/action-reviews" {
        return translate_empty_json_body_with_fields(
            "/ActionReviews/list_pending",
            "bad action review list body",
            [tenant_id_field(tenant_id)],
        );
    }
    if *method == Method::POST
        && let Some(rest) = uri
            .path()
            .strip_prefix("/v1/action-reviews/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let review_id = match Uuid::parse_str(rest) {
            Ok(value) => value,
            Err(_) => return RouteTranslation::BadRequest("bad action review id"),
        };
        return translate_json_object_with_fields(
            body,
            "/ActionReviews/decide",
            "bad action review decision body",
            "action review decision body must be object",
            "serialize action review decision body failed",
            [
                tenant_id_field(tenant_id),
                ("review_id", serde_json::json!(review_id)),
            ],
        );
    }
    if *method == Method::POST && uri.path() == "/v1/contacts/tokens" {
        return translate_json_object_with_fields(
            body,
            "/Contacts/issue_token",
            "bad contact token body",
            "contact token body must be object",
            "serialize contact token body failed",
            [tenant_id_field(tenant_id)],
        );
    }
    if let Some(translation) = translate_tenant_agent_route(method, uri, body, tenant_id) {
        return translation;
    }
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/sessions/")
            .and_then(|rest| rest.strip_suffix("/progress"))
    {
        let session_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return RouteTranslation::BadRequest("bad session id"),
        };
        return RouteTranslation::Forward {
            method: Method::POST,
            path: format!("/Session/{session_id}/progress"),
            body: body.to_vec(),
        };
    }
    if *method == Method::POST {
        match uri.path() {
            "/v1/analytics/session-stats" => {
                return translate_json_object_with_fields(
                    body,
                    "/Analytics/session_stats",
                    "bad session stats body",
                    "session stats body must be object",
                    "serialize session stats body failed",
                    [],
                );
            }
            "/v1/analytics/tenant-stats" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/tenant_stats",
                    tenant_id,
                );
            }
            "/v1/analytics/tool-stats" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/tool_stats",
                    tenant_id,
                );
            }
            "/v1/analytics/cache-stats" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/cache_stats",
                    tenant_id,
                );
            }
            "/v1/analytics/experiment-stats" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/experiment_stats",
                    tenant_id,
                );
            }
            "/v1/analytics/learning-candidates" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/learning_candidates",
                    tenant_id,
                );
            }
            "/v1/analytics/session-search" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Analytics/session_search",
                    tenant_id,
                );
            }
            "/v1/experiments/generate-plan" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/generate_plan",
                    tenant_id,
                );
            }
            "/v1/experiments/run-plan" => {
                return translate_json_object_with_tenant_id(body, "/Experiments/run", tenant_id);
            }
            "/v1/experiments/status" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/status",
                    tenant_id,
                );
            }
            "/v1/experiments/list" => {
                return translate_json_object_with_tenant_id(body, "/Experiments/list", tenant_id);
            }
            "/v1/experiments/trials" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/trials",
                    tenant_id,
                );
            }
            "/v1/experiments/trial-status" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/trial_status",
                    tenant_id,
                );
            }
            "/v1/experiments/cancel" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/cancel",
                    tenant_id,
                );
            }
            "/v1/experiments/propose-improvements" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/propose_improvements",
                    tenant_id,
                );
            }
            "/v1/experiments/scores" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/scores",
                    tenant_id,
                );
            }
            "/v1/experiments/compare" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/compare",
                    tenant_id,
                );
            }
            "/v1/experiments/agent-revision-simulations" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/run_agent_revision_simulation",
                    tenant_id,
                );
            }
            "/v1/experiments/agent-revision-simulations/compare" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Experiments/compare_agent_revision_simulation",
                    tenant_id,
                );
            }
            "/v1/sessions/create-agent" => {
                return translate_create_agent_session_route(body, tenant_id);
            }
            "/v1/admin-maintenance/vector/promote" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/AdminMaintenance/promote_tenant_vector",
                    tenant_id,
                );
            }
            "/v1/admin-maintenance/vector/rollback-promotion" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/AdminMaintenance/rollback_promotion",
                    tenant_id,
                );
            }
            "/v1/admin-maintenance/vector/finalize-promotion" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/AdminMaintenance/finalize_promotion",
                    tenant_id,
                );
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
                return translate_json_object_with_tenant_id(body, "/Memory/search", tenant_id);
            }
            "/v1/memory/show" => {
                return translate_json_object_with_tenant_id(body, "/Memory/show", tenant_id);
            }
            "/v1/memory/ingest-documents" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Memory/ingest_documents",
                    tenant_id,
                );
            }
            "/v1/memory/retrieve-debug" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Memory/retrieve_debug",
                    tenant_id,
                );
            }
            "/v1/knowledge/link-token" => {
                return translate_knowledge_json_body::<KnowledgeCreateLinkTokenRequest>(
                    body,
                    "/Knowledge/create_link_token",
                    tenant_id,
                );
            }
            "/v1/knowledge/exchange-token" => {
                return translate_knowledge_json_body::<KnowledgeExchangeTokenRequest>(
                    body,
                    "/Knowledge/exchange_public_token",
                    tenant_id,
                );
            }
            "/v1/knowledge/sync" => {
                return translate_knowledge_json_body::<KnowledgeSyncRequest>(
                    body,
                    "/Knowledge/sync_connection",
                    tenant_id,
                );
            }
            "/v1/knowledge/sync-status" => {
                return translate_knowledge_json_body::<KnowledgeSyncStatusRequest>(
                    body,
                    "/Knowledge/sync_status",
                    tenant_id,
                );
            }
            "/v1/knowledge/sync-events" => {
                return translate_knowledge_json_body::<KnowledgeSyncEventsRequest>(
                    body,
                    "/Knowledge/sync_events",
                    tenant_id,
                );
            }
            "/v1/knowledge/connections" => {
                return translate_knowledge_json_body::<KnowledgeConnectionListRequest>(
                    body,
                    "/Knowledge/list_connections",
                    tenant_id,
                );
            }
            "/v1/knowledge/objects" => {
                return translate_knowledge_json_body::<KnowledgeObjectListRequest>(
                    body,
                    "/Knowledge/list_objects",
                    tenant_id,
                );
            }
            "/v1/knowledge/object" => {
                return translate_knowledge_json_body::<KnowledgeObjectInspectRequest>(
                    body,
                    "/Knowledge/inspect_object",
                    tenant_id,
                );
            }
            "/v1/knowledge/query-trace" => {
                return translate_knowledge_json_body::<KnowledgeQueryTraceRequest>(
                    body,
                    "/Knowledge/query_trace",
                    tenant_id,
                );
            }
            "/v1/lineage/explain" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LineageAdmin/explain",
                    tenant_id,
                );
            }
            "/v1/lineage/query" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LineageAdmin/query",
                    tenant_id,
                );
            }
            "/v1/lineage/export" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LineageAdmin/export",
                    tenant_id,
                );
            }
            "/v1/lineage/verify" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LineageAdmin/verify",
                    tenant_id,
                );
            }
            "/v1/lineage/erase" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LineageAdmin/erase",
                    tenant_id,
                );
            }
            "/v1/privacy/export" => {
                return translate_json_object_with_tenant_id(body, "/Privacy/export", tenant_id);
            }
            "/v1/privacy/erase" => {
                return translate_json_object_with_tenant_id(body, "/Privacy/erase", tenant_id);
            }
            "/v1/skills/export" => {
                return translate_json_object_with_tenant_id(body, "/Skills/export", tenant_id);
            }
            "/v1/skills/import" => {
                return translate_json_object_with_tenant_scope(body, "/Skills/import", tenant_id);
            }
            "/v1/skills/list" => {
                return translate_json_object_with_tenant_id(body, "/Skills/list", tenant_id);
            }
            "/v1/artifacts/import" => {
                return translate_json_object_with_tenant_scope(
                    body,
                    "/Artifacts/import",
                    tenant_id,
                );
            }
            "/v1/artifacts/export" => {
                return translate_json_object_with_tenant_id(body, "/Artifacts/export", tenant_id);
            }
            "/v1/artifacts/list" => {
                return translate_json_object_with_tenant_id(body, "/Artifacts/list", tenant_id);
            }
            "/v1/artifacts/validate" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Artifacts/validate",
                    tenant_id,
                );
            }
            "/v1/artifacts/publish" => {
                return translate_json_object_with_tenant_scope(
                    body,
                    "/Artifacts/publish",
                    tenant_id,
                );
            }
            "/v1/learning-candidates/get" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/LearningReview/get",
                    tenant_id,
                );
            }
            "/v1/learning-candidates/accept-skill" => {
                return translate_json_object_with_fields(
                    body,
                    "/LearningReview/accept_skill",
                    "bad tenant route body",
                    "tenant route body must be object",
                    "serialize tenant route body failed",
                    [
                        tenant_id_field(tenant_id),
                        ("action", serde_json::json!("accept")),
                        ("reviewer_subject", serde_json::json!("edge")),
                    ],
                );
            }
            "/v1/learning-candidates/reject" => {
                return translate_json_object_with_fields(
                    body,
                    "/LearningReview/reject",
                    "bad tenant route body",
                    "tenant route body must be object",
                    "serialize tenant route body failed",
                    [
                        tenant_id_field(tenant_id),
                        ("action", serde_json::json!("reject")),
                        ("reviewer_subject", serde_json::json!("edge")),
                    ],
                );
            }
            "/v1/workflows/run" => {
                return translate_json_object_with_tenant_id(body, "/Workflows/run", tenant_id);
            }
            "/v1/workflows/status" => {
                return translate_json_object_with_tenant_id(body, "/Workflows/status", tenant_id);
            }
            "/v1/workflows/cancel" => {
                return translate_json_object_with_tenant_id(body, "/Workflows/cancel", tenant_id);
            }
            "/v1/workflows/decide-review" => {
                return translate_json_object_with_tenant_id(
                    body,
                    "/Workflows/decide_review",
                    tenant_id,
                );
            }
            "/v1/workflows/signal" => {
                return translate_json_object_with_tenant_id(body, "/Workflows/signal", tenant_id);
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

fn translate_tenant_agent_route(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    let rest = uri.path().strip_prefix("/v1/")?;
    let mut segments = rest.split('/');

    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some("agent-definitions"), None, None, None) if *method == Method::GET => {
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_definitions",
                "bad agent definitions list body",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::GET => {
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_installations",
                "bad agent installations list body",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::POST => {
            Some(translate_json_object_with_fields(
                body,
                "/AgentDefinitions/install",
                "bad agent install body",
                "agent install body must be object",
                "serialize agent install body failed",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), Some(installation_uid), Some("deployments"), None)
            if *method == Method::GET =>
        {
            let installation_uid = match Uuid::parse_str(installation_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent installation id")),
            };
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_deployments",
                "bad agent deployments list body",
                [
                    tenant_id_field(tenant_id),
                    ("installation_uid", serde_json::json!(installation_uid)),
                ],
            ))
        }
        (Some("agent-installations"), Some(installation_uid), Some("deployments"), None)
            if *method == Method::POST =>
        {
            let installation_uid = match Uuid::parse_str(installation_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent installation id")),
            };
            Some(translate_json_object_with_fields(
                body,
                "/AgentDefinitions/deploy",
                "bad agent deploy body",
                "agent deploy body must be object",
                "serialize agent deploy body failed",
                [
                    tenant_id_field(tenant_id),
                    ("installation_uid", serde_json::json!(installation_uid)),
                ],
            ))
        }
        (Some("agent-simulations"), None, None, None) if *method == Method::POST => {
            Some(translate_json_object_with_fields(
                body,
                "/Experiments/run_agent_revision_simulation",
                "bad agent simulation body",
                "agent simulation body must be object",
                "serialize agent simulation body failed",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-simulations"), Some(run_uid), Some("compare"), None)
            if *method == Method::POST =>
        {
            let run_uid = match Uuid::parse_str(run_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent simulation run id")),
            };
            Some(translate_json_object_with_fields(
                body,
                "/Experiments/compare_agent_revision_simulation",
                "bad agent simulation compare body",
                "agent simulation compare body must be object",
                "serialize agent simulation compare body failed",
                [
                    tenant_id_field(tenant_id),
                    ("run_uid", serde_json::json!(run_uid)),
                ],
            ))
        }
        _ => None,
    }
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

fn translate_knowledge_provider_webhook(
    provider: &'static str,
    headers: &HeaderMap,
    body: &Bytes,
) -> RouteTranslation {
    if body.len() > KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES {
        return RouteTranslation::BadRequest("knowledge webhook body too large");
    }
    if rejects_raw_webhook_content_type(headers) {
        return RouteTranslation::BadRequest(
            "knowledge webhook route does not accept raw document uploads",
        );
    }

    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad knowledge webhook body"),
    };
    if !payload.is_object() {
        return RouteTranslation::BadRequest("knowledge webhook body must be object");
    }
    let Some(event_id) = webhook_event_id(headers, &payload) else {
        return RouteTranslation::BadRequest("knowledge webhook missing event id");
    };
    let Some(event_type) = webhook_event_type(headers, &payload) else {
        return RouteTranslation::BadRequest("knowledge webhook missing event type");
    };

    let request = KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers: forwarded_webhook_headers(headers),
        body_base64: Some(general_purpose::STANDARD.encode(body)),
    };
    let bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, provider, "serialize knowledge webhook body failed");
            return RouteTranslation::BadRequest("bad knowledge webhook body");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: "/Knowledge/provider_webhook".to_string(),
        body: bytes,
    }
}

fn rejects_raw_webhook_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

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
mod tests {
    use async_trait::async_trait;
    use axum::http::header::AUTHORIZATION;
    use base64::Engine as _;
    use chrono::Utc;
    use moa_core::traits::{AuthError, Identity, IdentityType};
    use moa_core::wire::turn::{SessionSnapshot, TurnOutcome, TurnOutcomeKind};
    use moa_core::{EventType, SessionId, TenantId};

    use super::*;

    const TEST_TENANT_ID: &str = "22222222-2222-2222-2222-222222222222";

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

    fn test_tenant_id() -> TenantId {
        TenantId::from(Uuid::parse_str(TEST_TENANT_ID).expect("test tenant id should parse"))
    }

    fn test_tenant_json() -> serde_json::Value {
        serde_json::json!(TEST_TENANT_ID)
    }

    fn test_tenant_scope_json() -> serde_json::Value {
        serde_json::json!({ "tenant": { "tenant_id": TEST_TENANT_ID } })
    }

    fn translate(method: &Method, uri: &Uri, body: &Bytes) -> RouteTranslation {
        translate_public_route(method, uri, body, test_tenant_id())
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
    fn whoami_public_route_translates_to_restate_handler() {
        // Pins: hosted identity inspection stays available through the public edge API.
        let uri = "/v1/whoami"
            .parse::<Uri>()
            .expect("route path should parse");

        let translation = translate(&Method::GET, &uri, &Bytes::new());

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

    #[test]
    fn contact_token_route_translates_to_contacts_service() {
        // Pins: contact token issuance derives the tenant from authenticated identity, not a workspace path.
        let uri = "/v1/contacts/tokens"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"display_name":"Ada"}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Contacts/issue_token");
                let value: serde_json::Value =
                    serde_json::from_slice(&body).expect("translated body should be json");
                assert_eq!(
                    value,
                    serde_json::json!({
                        "display_name": "Ada",
                        "tenant_id": test_tenant_json()
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("contact token route should translate to Contacts service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("contact token route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn action_review_public_routes_translate_to_restate_handlers() {
        // Pins: tenant-admin action-review routes forward to the internal ActionReviews service.
        let list_uri = "/v1/action-reviews"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/list_pending");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("list body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({ "tenant_id": test_tenant_json() })
                );
            }
            RouteTranslation::NoChange => {
                panic!("action review list should translate to ActionReviews service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("action review list should not fail translation: {message}")
            }
        }

        let decision_uri = "/v1/action-reviews/11111111-1111-1111-1111-111111111111/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"decision":"cleared","reason":null}"#);
        let decision_translation = translate(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "tenant_id": test_tenant_json(),
                        "review_id": "11111111-1111-1111-1111-111111111111",
                        "decision": "cleared",
                        "reason": null
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("action review decision should translate to ActionReviews service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("action review decision should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn authz_challenge_public_routes_translate_to_restate_handlers() {
        // Pins: builtin async-authz challenge routes stay separate from action reviews.
        let list_uri = "/v1/authz-challenges"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/AuthzChallenges/list_mine");
                assert!(
                    body.is_empty(),
                    "authz challenge list should not synthesize a request body"
                );
            }
            RouteTranslation::NoChange => {
                panic!("authz challenge list should translate to AuthzChallenges service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("authz challenge list should not fail translation: {message}")
            }
        }

        let decision_uri = "/v1/authz-challenges/22222222-2222-2222-2222-222222222222/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"outcome":"approved","reason":null}"#);
        let decision_translation = translate(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/AuthzChallenges/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "id": "22222222-2222-2222-2222-222222222222",
                        "outcome": "approved",
                        "reason": null
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("authz challenge decision should translate to AuthzChallenges service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("authz challenge decision should not fail translation: {message}")
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
                serde_json::json!({
                    "session_id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/analytics/tenant-stats",
                "/Analytics/tenant_stats",
                serde_json::json!({ "tenant_id": test_tenant_json(), "days": 14 }),
            ),
            (
                "/v1/analytics/tool-stats",
                "/Analytics/tool_stats",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/analytics/cache-stats",
                "/Analytics/cache_stats",
                serde_json::json!({ "tenant_id": test_tenant_json(), "days": 14 }),
            ),
            (
                "/v1/analytics/experiment-stats",
                "/Analytics/experiment_stats",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "from_time": null,
                    "to_time": null,
                    "limit": 20
                }),
            ),
            (
                "/v1/analytics/learning-candidates",
                "/Analytics/learning_candidates",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "status": "proposed",
                    "limit": 20
                }),
            ),
            (
                "/v1/analytics/session-search",
                "/Analytics/session_search",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "query": "refresh token",
                    "from_time": null,
                    "to_time": null,
                    "event_types": ["user_message"],
                    "limit": 10
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if let Some(object) = input_body.as_object_mut() {
                object.remove("tenant_id");
            }
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn eval_public_routes_do_not_translate_to_product_handlers() {
        // Pins: hosted eval is not part of the default public product edge surface.
        let paths = [
            "/v1/evals/plan",
            "/v1/evals/suites/list",
            "/v1/evals/run",
            "/v1/evals/run-status",
            "/v1/evals/datasets/register",
            "/v1/evals/datasets/list",
            "/v1/evals/replay",
            "/v1/evals/scores",
            "/v1/evals/compare",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward {
                    method,
                    path,
                    body: _,
                } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
                }
            }
        }
    }

    #[test]
    fn experiments_public_routes_translate_to_restate_handlers() {
        // Pins: hosted experiment edge routes forward to the internal Experiments service paths.
        let cases = [
            (
                "/v1/experiments/generate-plan",
                "/Experiments/generate_plan",
            ),
            ("/v1/experiments/run-plan", "/Experiments/run"),
            ("/v1/experiments/status", "/Experiments/status"),
            ("/v1/experiments/list", "/Experiments/list"),
            ("/v1/experiments/trials", "/Experiments/trials"),
            ("/v1/experiments/trial-status", "/Experiments/trial_status"),
            ("/v1/experiments/cancel", "/Experiments/cancel"),
            (
                "/v1/experiments/propose-improvements",
                "/Experiments/propose_improvements",
            ),
            ("/v1/experiments/scores", "/Experiments/scores"),
            ("/v1/experiments/compare", "/Experiments/compare"),
            (
                "/v1/experiments/agent-revision-simulations",
                "/Experiments/run_agent_revision_simulation",
            ),
            (
                "/v1/experiments/agent-revision-simulations/compare",
                "/Experiments/compare_agent_revision_simulation",
            ),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded.get("tenant_id"), Some(&test_tenant_json()));
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
    fn configured_agent_public_routes_translate_to_restate_handlers() {
        // Pins: tenant-configurable agent product routes reach AgentDefinitions and simulation handlers.
        let installation_uid = "11111111-1111-1111-1111-111111111111";
        let revision_uid = "22222222-2222-2222-2222-222222222222";
        let run_uid = "33333333-3333-3333-3333-333333333333";
        let cases = vec![
            (
                Method::GET,
                "/v1/agent-definitions".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_definitions",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                Method::GET,
                "/v1/agent-installations".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_installations",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                Method::POST,
                "/v1/agent-installations".to_string(),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","metadata":{{"tier":"gold"}}}}"#
                )),
                "/AgentDefinitions/install",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "revision_uid": revision_uid,
                    "metadata": { "tier": "gold" }
                }),
            ),
            (
                Method::GET,
                format!("/v1/agent-installations/{installation_uid}/deployments"),
                Bytes::new(),
                "/AgentDefinitions/list_deployments",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "installation_uid": installation_uid
                }),
            ),
            (
                Method::POST,
                format!("/v1/agent-installations/{installation_uid}/deployments"),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","reason":"candidate passed"}}"#
                )),
                "/AgentDefinitions/deploy",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "installation_uid": installation_uid,
                    "revision_uid": revision_uid,
                    "reason": "candidate passed"
                }),
            ),
            (
                Method::POST,
                "/v1/agent-simulations".to_string(),
                Bytes::from(format!(
                    r#"{{"name":"compare support","plan_revision_uid":"{revision_uid}","base":{{"variant_key":"base","revision_uid":"{revision_uid}"}}}}"#
                )),
                "/Experiments/run_agent_revision_simulation",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "name": "compare support",
                    "plan_revision_uid": revision_uid,
                    "base": {
                        "variant_key": "base",
                        "revision_uid": revision_uid
                    }
                }),
            ),
            (
                Method::POST,
                format!("/v1/agent-simulations/{run_uid}/compare"),
                Bytes::from_static(
                    br#"{"base_variant_key":"base","candidate_variant_keys":["candidate"]}"#,
                ),
                "/Experiments/compare_agent_revision_simulation",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "run_uid": run_uid,
                    "base_variant_key": "base",
                    "candidate_variant_keys": ["candidate"]
                }),
            ),
        ];

        for (method, public_path, body, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");

            let translation = translate(&method, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method: forwarded_method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(
                        forwarded_method,
                        Method::POST,
                        "{public_path} must use POST"
                    );
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn authenticated_agent_session_route_translates_to_session_store() {
        // Pins: authenticated configured-agent sessions use SessionStore, not the contact session route.
        let uri = "/v1/sessions/create-agent"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"meta":{},"agent":{}}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/SessionStore/create_agent_session");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&forwarded_body).expect("forwarded body should be JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "meta": {
                            "tenant_id": test_tenant_json()
                        },
                        "agent": {}
                    })
                );
            }
            RouteTranslation::NoChange => panic!("agent session route should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent session route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn session_progress_public_route_translates_to_session_vo() {
        // Pins: clients can fetch snapshot, active turn progress, and event history through one Session endpoint.
        let session_id = "11111111-1111-1111-1111-111111111111";
        let uri = format!("/v1/sessions/{session_id}/progress")
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"event_range":{"from_seq":4,"limit":20}}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(
                    path,
                    "/Session/11111111-1111-1111-1111-111111111111/progress"
                );
                assert_eq!(forwarded_body, body.as_ref());
            }
            RouteTranslation::NoChange => panic!("session progress route should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("session progress route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn session_progress_public_route_rejects_bad_session_id() {
        // Pins: malformed public Session/progress paths do not reach the Restate object namespace.
        let uri = "/v1/sessions/not-a-uuid/progress"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::BadRequest(message) => assert_eq!(message, "bad session id"),
            RouteTranslation::Forward { path, .. } => {
                panic!("bad session id should not translate to {path}")
            }
            RouteTranslation::NoChange => panic!("bad session id should be rejected"),
        }
    }

    #[test]
    fn admin_maintenance_public_routes_translate_to_restate_handlers() {
        // Pins: hosted admin-maintenance routes forward to the internal AdminMaintenance service paths.
        let cases = [
            (
                "/v1/admin-maintenance/vector/promote",
                "/AdminMaintenance/promote_tenant_vector",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "target_backend": "turbopuffer",
                    "validate_percent": 5,
                    "dual_read_hours": 24
                }),
            ),
            (
                "/v1/admin-maintenance/vector/rollback-promotion",
                "/AdminMaintenance/rollback_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "rollback"
                }),
            ),
            (
                "/v1/admin-maintenance/vector/finalize-promotion",
                "/AdminMaintenance/finalize_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "finalize"
                }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/create",
                "/AdminMaintenance/checkpoint_create",
                serde_json::json!({ "label": "before-deploy", "session_id": null }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/list",
                "/AdminMaintenance/checkpoint_list",
                serde_json::json!({}),
            ),
            (
                "/v1/admin-maintenance/checkpoints/rollback",
                "/AdminMaintenance/checkpoint_rollback",
                serde_json::json!({ "id": "br-checkpoint" }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/cleanup",
                "/AdminMaintenance/checkpoint_cleanup",
                serde_json::json!({}),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if public_path.contains("/vector/")
                && let Some(object) = input_body.as_object_mut()
            {
                object.remove("tenant_id");
            }
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    if public_path.contains("/vector/") {
                        assert_eq!(forwarded, expected_body, "{public_path} body changed");
                    } else {
                        assert_eq!(
                            forwarded, input_body,
                            "{public_path} body should pass through"
                        );
                    }
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
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "query": "auth",
                    "limit": 10
                }),
            ),
            (
                "/v1/memory/show",
                "/Memory/show",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "uid": "22222222-2222-2222-2222-222222222222"
                }),
            ),
            (
                "/v1/memory/ingest-documents",
                "/Memory/ingest_documents",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "documents": [{"source_name": "Auth", "content": "Fact: auth uses JWT"}]
                }),
            ),
            (
                "/v1/memory/retrieve-debug",
                "/Memory/retrieve_debug",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "query": "auth",
                    "limit": 5,
                    "no_flush_wait": true
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn knowledge_public_routes_translate_to_restate_handlers() {
        // Pins: hosted knowledge routes forward typed DTOs with tenant id derived from auth.
        let connection_uid = "11111111-1111-1111-1111-111111111111";
        let sync_run_uid = "22222222-2222-2222-2222-222222222222";
        let object_uid = "33333333-3333-3333-3333-333333333333";
        let trace_uid = "44444444-4444-4444-4444-444444444444";
        let caller_tenant = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let cases = [
            (
                "/v1/knowledge/link-token",
                "/Knowledge/create_link_token",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "nango",
                    "connector": "google-drive",
                    "external_account_id": "account-1"
                }),
            ),
            (
                "/v1/knowledge/exchange-token",
                "/Knowledge/exchange_public_token",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "merge",
                    "exchange_token": "public-token"
                }),
            ),
            (
                "/v1/knowledge/sync",
                "/Knowledge/sync_connection",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "connection_uid": connection_uid,
                    "parser": "native",
                    "max_records": 25
                }),
            ),
            (
                "/v1/knowledge/sync-status",
                "/Knowledge/sync_status",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sync_run_uid": sync_run_uid
                }),
            ),
            (
                "/v1/knowledge/sync-events",
                "/Knowledge/sync_events",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sync_run_uid": sync_run_uid,
                    "object_uid": object_uid,
                    "cursor": "page-2",
                    "limit": 50
                }),
            ),
            (
                "/v1/knowledge/connections",
                "/Knowledge/list_connections",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "nango"
                }),
            ),
            (
                "/v1/knowledge/objects",
                "/Knowledge/list_objects",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "connection_uid": connection_uid,
                    "object_type": "document",
                    "cursor": "page-2",
                    "limit": 25
                }),
            ),
            (
                "/v1/knowledge/object",
                "/Knowledge/inspect_object",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "object_uid": object_uid
                }),
            ),
            (
                "/v1/knowledge/query-trace",
                "/Knowledge/query_trace",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "trace_uid": trace_uid
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.insert("tenant_id".to_string(), serde_json::json!(caller_tenant));
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn knowledge_public_routes_reject_bad_typed_payloads() {
        // Pins: knowledge route translation validates against the typed wire DTO before forwarding.
        let uri = "/v1/knowledge/sync"
            .parse::<Uri>()
            .expect("route path should parse");
        let missing_connection = Bytes::from_static(br#"{"parser":"native"}"#);
        let non_object = Bytes::from_static(br#"[]"#);

        match translate(&Method::POST, &uri, &missing_connection) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "bad knowledge route body");
            }
            RouteTranslation::Forward { .. } => panic!("missing connection_uid must not forward"),
            RouteTranslation::NoChange => panic!("knowledge sync route should not fall through"),
        }

        match translate(&Method::POST, &uri, &non_object) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge route body must be object");
            }
            RouteTranslation::Forward { .. } => panic!("non-object payload must not forward"),
            RouteTranslation::NoChange => panic!("knowledge sync route should not fall through"),
        }
    }

    #[test]
    fn knowledge_provider_webhooks_translate_without_tenant_injection() {
        // Pins: provider webhooks bypass end-user auth and forward raw signed event material only.
        let body = Bytes::from_static(br#"{"event_id":"evt-1","event_type":"sync.completed"}"#);
        let providers = ["llamaparse", "reducto", "nango", "merge"];

        for provider in providers {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "application/json"
                    .parse()
                    .expect("content-type should parse"),
            );
            headers.insert(
                AUTHORIZATION,
                "Bearer should-not-forward"
                    .parse()
                    .expect("authorization header should parse"),
            );
            headers.insert(
                "x-moa-tenant-id",
                TEST_TENANT_ID.parse().expect("tenant header should parse"),
            );
            headers.insert(
                "x-test-signature",
                "valid".parse().expect("signature header should parse"),
            );

            let translation = translate_knowledge_provider_webhook(provider, &headers, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{provider} must remain POST");
                    assert_eq!(path, "/Knowledge/provider_webhook");
                    let forwarded: KnowledgeProviderWebhookRequest =
                        serde_json::from_slice(&forwarded_body)
                            .expect("forwarded webhook body should decode");
                    assert_eq!(forwarded.provider, provider);
                    assert_eq!(forwarded.event_id, "evt-1");
                    assert_eq!(forwarded.event_type, "sync.completed");
                    assert_eq!(forwarded.payload.get("tenant_id"), None);
                    assert!(
                        forwarded.body_base64.is_some(),
                        "raw body should be base64 encoded"
                    );
                    let raw_body = general_purpose::STANDARD
                        .decode(forwarded.body_base64.as_deref().expect("body_base64"))
                        .expect("body_base64 should decode");
                    assert_eq!(raw_body, body.as_ref());
                    assert!(
                        !forwarded
                            .headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
                        "authorization must not be forwarded for webhook verification"
                    );
                    assert!(
                        !forwarded
                            .headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("x-moa-tenant-id")),
                        "caller-supplied MOA headers must not be forwarded"
                    );
                    assert!(
                        forwarded
                            .headers
                            .iter()
                            .any(|(name, value)| name == "x-test-signature" && value == "valid"),
                        "provider signature header should be forwarded"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{provider} webhook should translate to Knowledge/provider_webhook")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{provider} webhook should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn knowledge_provider_webhooks_reject_documents_and_malformed_events() {
        // Pins: webhook routes accept provider event JSON, not direct document uploads.
        let mut raw_document_headers = HeaderMap::new();
        raw_document_headers.insert(
            header::CONTENT_TYPE,
            "application/pdf"
                .parse()
                .expect("content-type should parse"),
        );
        let document_body = Bytes::from_static(b"%PDF-raw-document");
        match translate_knowledge_provider_webhook(
            "llamaparse",
            &raw_document_headers,
            &document_body,
        ) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(
                    message,
                    "knowledge webhook route does not accept raw document uploads"
                );
            }
            RouteTranslation::Forward { .. } => panic!("raw document webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }

        let mut json_headers = HeaderMap::new();
        json_headers.insert(
            header::CONTENT_TYPE,
            "application/json"
                .parse()
                .expect("content-type should parse"),
        );
        let missing_event_type = Bytes::from_static(br#"{"event_id":"evt-1"}"#);
        match translate_knowledge_provider_webhook("nango", &json_headers, &missing_event_type) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge webhook missing event type");
            }
            RouteTranslation::Forward { .. } => panic!("malformed webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }

        let large_body = Bytes::from(vec![b' '; KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES + 1]);
        match translate_knowledge_provider_webhook("merge", &json_headers, &large_body) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge webhook body too large");
            }
            RouteTranslation::Forward { .. } => panic!("oversized webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }
    }

    #[test]
    fn lineage_and_privacy_public_routes_translate_to_restate_handlers() {
        // Pins: hosted lineage/privacy edge routes forward to internal Restate service paths.
        let cases = [
            (
                "/v1/lineage/explain",
                "/LineageAdmin/explain",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/lineage/query",
                "/LineageAdmin/query",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sql": "SELECT count(*) FROM lineage",
                    "since": "24 hours"
                }),
            ),
            (
                "/v1/lineage/export",
                "/LineageAdmin/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject": "subject-a"
                }),
            ),
            (
                "/v1/lineage/verify",
                "/LineageAdmin/verify",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "window": "hot",
                    "since": "24 hours"
                }),
            ),
            (
                "/v1/lineage/erase",
                "/LineageAdmin/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject": "00ff"
                }),
            ),
            (
                "/v1/privacy/export",
                "/Privacy/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
            (
                "/v1/privacy/erase",
                "/Privacy/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/skills/import",
                "/Skills/import",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "packages": []
                }),
            ),
            (
                "/v1/skills/list",
                "/Skills/list",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            if public_path == "/v1/skills/import" {
                object.remove("scope");
            }
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn artifact_public_routes_translate_to_restate_handlers() {
        // Pins: hosted artifact edge routes forward to the internal Artifacts service paths.
        let cases = [
            (
                "/v1/artifacts/import",
                "/Artifacts/import",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "source_format": "json",
                    "source_text": "{}"
                }),
            ),
            (
                "/v1/artifacts/export",
                "/Artifacts/export",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/artifacts/list",
                "/Artifacts/list",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/artifacts/validate",
                "/Artifacts/validate",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "source_format": "json",
                    "source_text": "{}"
                }),
            ),
            (
                "/v1/artifacts/publish",
                "/Artifacts/publish",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "revision_uid": "11111111-1111-1111-1111-111111111111"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            object.remove("scope");
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn learning_candidate_public_routes_translate_to_restate_handlers() {
        // Pins: hosted learning-review edge routes forward to the internal LearningReview service paths.
        let cases = [
            (
                "/v1/learning-candidates/get",
                "/LearningReview/get",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/learning-candidates/accept-skill",
                "/LearningReview/accept_skill",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "accept",
                    "reviewer_subject": "edge"
                }),
            ),
            (
                "/v1/learning-candidates/reject",
                "/LearningReview/reject",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "reject",
                    "reviewer_subject": "edge"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body =
                Bytes::from_static(br#"{"candidate_id":"11111111-1111-1111-1111-111111111111"}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
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
    fn workflow_public_routes_translate_to_restate_handlers() {
        // Pins: hosted workflow edge routes forward to the internal Workflows service paths.
        let cases = [
            ("/v1/workflows/run", "/Workflows/run"),
            ("/v1/workflows/status", "/Workflows/status"),
            ("/v1/workflows/cancel", "/Workflows/cancel"),
            ("/v1/workflows/decide-review", "/Workflows/decide_review"),
            ("/v1/workflows/signal", "/Workflows/signal"),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded.get("tenant_id"), Some(&test_tenant_json()));
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
        match translate(&Method::POST, &create_uri, &body) {
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
        match translate(&Method::GET, &list_uri, &Bytes::new()) {
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
            match translate(&method, &uri, &Bytes::new()) {
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
            match translate(&Method::POST, &uri, &act_as_body) {
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
