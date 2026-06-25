//! HTTP routes exposed by the MOA edge service.

use crate::proxy::OrchestratorProxy;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{any, get, patch, post};
use futures_util::stream;
use moa_core::traits::{AuthProvider, Credential, Identity};
use moa_core::wire::SessionProgress;
use moa_core::{
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    Event, EventRange, EventRecord, SequenceNum, SessionId, TenantId,
};
#[cfg(feature = "auth0")]
use serde::Deserialize;
use serde::Serialize;
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
        .route(
            "/v1/workspaces/{workspace_id}/contacts/verification/start",
            post(handle_public_contact_verification_start),
        )
        .route(
            "/v1/workspaces/{workspace_id}/contacts/verification/complete",
            post(handle_public_contact_verification_complete),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agent-sessions/{session_id}/contacts/verification/start",
            post(handle_public_session_contact_verification_start),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agent-sessions/{session_id}/contacts/verification/complete",
            post(handle_public_session_contact_verification_complete),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agent-sessions",
            post(handle_public_agent_session_init),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agent-sessions/{session_id}/promote",
            post(handle_public_agent_session_promote),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agent-sessions/{session_id}/channel",
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
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/start_verification",
        [("workspace_id", serde_json::json!(workspace_id))],
    )
    .await
}

async fn handle_public_contact_verification_complete(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/complete_verification",
        [("workspace_id", serde_json::json!(workspace_id))],
    )
    .await
}

async fn handle_public_session_contact_verification_start(
    State(state): State<AppState>,
    Path((workspace_id, session_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/start_verification",
        [
            ("workspace_id", serde_json::json!(workspace_id)),
            ("session_id", serde_json::json!(session_id)),
        ],
    )
    .await
}

async fn handle_public_session_contact_verification_complete(
    State(state): State<AppState>,
    Path((workspace_id, session_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/complete_verification",
        [
            ("workspace_id", serde_json::json!(workspace_id)),
            ("session_id", serde_json::json!(session_id)),
        ],
    )
    .await
}

async fn handle_public_agent_session_init(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/init_session",
        [("workspace_id", serde_json::json!(workspace_id))],
    )
    .await
}

async fn handle_public_agent_session_promote(
    State(state): State<AppState>,
    Path((workspace_id, session_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/promote_session",
        [
            ("workspace_id", serde_json::json!(workspace_id)),
            ("session_id", serde_json::json!(session_id)),
        ],
    )
    .await
}

async fn handle_public_agent_session_channel_change(
    State(state): State<AppState>,
    Path((workspace_id, session_id)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    forward_public_contact_route(
        state,
        headers,
        body,
        "/Contacts/change_session_channel",
        [
            ("workspace_id", serde_json::json!(workspace_id)),
            ("session_id", serde_json::json!(session_id)),
        ],
    )
    .await
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
        moa_core::wire::TurnOutcomeKind::Completed => "completed",
        moa_core::wire::TurnOutcomeKind::Cancelled => "cancelled",
        moa_core::wire::TurnOutcomeKind::Failed => "failed",
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
    if *method == Method::GET
        && let Some(rest) = uri
            .path()
            .strip_prefix("/v1/workspaces/")
            .and_then(|rest| rest.strip_suffix("/action-reviews"))
    {
        let workspace_id = rest.trim_matches('/');
        if workspace_id.is_empty() || workspace_id.contains('/') {
            return RouteTranslation::BadRequest("bad workspace id");
        }
        let body = match serde_json::to_vec(&serde_json::json!({ "workspace_id": workspace_id })) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "serialize action review list body failed");
                return RouteTranslation::BadRequest("bad action review list body");
            }
        };
        return RouteTranslation::Forward {
            method: Method::POST,
            path: "/ActionReviews/list_pending".to_string(),
            body,
        };
    }
    if *method == Method::POST
        && let Some(rest) = uri
            .path()
            .strip_prefix("/v1/workspaces/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let mut segments = rest.split('/');
        let Some(workspace_id) = segments.next() else {
            return RouteTranslation::BadRequest("bad workspace id");
        };
        if segments.next() != Some("action-reviews") {
            return RouteTranslation::NoChange;
        }
        let Some(review_id_text) = segments.next() else {
            return RouteTranslation::BadRequest("bad action review id");
        };
        if segments.next().is_some() || workspace_id.is_empty() {
            return RouteTranslation::BadRequest("bad action review path");
        }
        let review_id = match Uuid::parse_str(review_id_text) {
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
                ("workspace_id", serde_json::json!(workspace_id)),
                ("review_id", serde_json::json!(review_id)),
            ],
        );
    }
    if *method == Method::POST
        && let Some(rest) = uri
            .path()
            .strip_prefix("/v1/workspaces/")
            .and_then(|rest| rest.strip_suffix("/contacts/tokens"))
    {
        let workspace_id = rest.trim_matches('/');
        if workspace_id.is_empty() || workspace_id.contains('/') {
            return RouteTranslation::BadRequest("bad workspace id");
        }
        return translate_json_object_with_fields(
            body,
            "/Contacts/issue_token",
            "bad contact token body",
            "contact token body must be object",
            "serialize contact token body failed",
            [("workspace_id", serde_json::json!(workspace_id))],
        );
    }
    if let Some(translation) = translate_workspace_agent_route(method, uri, body) {
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
            "/v1/analytics/experiment-stats" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/experiment_stats".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/analytics/learning-candidates" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/learning_candidates".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/analytics/session-search" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Analytics/session_search".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/generate-plan" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/generate_plan".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/run-plan" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/run".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/status" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/status".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/trials" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/trials".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/trial-status" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/trial_status".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/cancel" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/cancel".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/propose-improvements" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/propose_improvements".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/scores" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/scores".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/compare" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/compare".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/agent-revision-simulations" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/run_agent_revision_simulation".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/experiments/agent-revision-simulations/compare" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Experiments/compare_agent_revision_simulation".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/sessions/create-agent" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/SessionStore/create_agent_session".to_string(),
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
            "/v1/skills/bootstrap-workspace-default" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Skills/bootstrap_workspace_default".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/artifacts/import" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Artifacts/import".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/artifacts/export" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Artifacts/export".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/artifacts/list" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Artifacts/list".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/artifacts/validate" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Artifacts/validate".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/artifacts/publish" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Artifacts/publish".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/learning-candidates/get" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LearningReview/get".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/learning-candidates/accept-skill" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LearningReview/accept_skill".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/learning-candidates/reject" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/LearningReview/reject".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/workflows/run" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Workflows/run".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/workflows/status" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Workflows/status".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/workflows/cancel" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Workflows/cancel".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/workflows/decide-review" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Workflows/decide_review".to_string(),
                    body: body.to_vec(),
                };
            }
            "/v1/workflows/signal" => {
                return RouteTranslation::Forward {
                    method: Method::POST,
                    path: "/Workflows/signal".to_string(),
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

fn translate_workspace_agent_route(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
) -> Option<RouteTranslation> {
    let rest = uri.path().strip_prefix("/v1/workspaces/")?;
    let mut segments = rest.split('/');
    let workspace_id = segments.next()?;
    if workspace_id.is_empty() {
        return Some(RouteTranslation::BadRequest("bad workspace id"));
    }

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
                [("workspace_id", serde_json::json!(workspace_id))],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::GET => {
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_installations",
                "bad agent installations list body",
                [("workspace_id", serde_json::json!(workspace_id))],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::POST => {
            Some(translate_json_object_with_fields(
                body,
                "/AgentDefinitions/install",
                "bad agent install body",
                "agent install body must be object",
                "serialize agent install body failed",
                [("workspace_id", serde_json::json!(workspace_id))],
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
                    ("workspace_id", serde_json::json!(workspace_id)),
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
                    ("workspace_id", serde_json::json!(workspace_id)),
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
                [("workspace_id", serde_json::json!(workspace_id))],
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
                    ("workspace_id", serde_json::json!(workspace_id)),
                    ("run_uid", serde_json::json!(run_uid)),
                ],
            ))
        }
        _ => None,
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
    use chrono::Utc;
    use moa_core::traits::{AuthError, Identity, IdentityType};
    use moa_core::wire::{SessionSnapshot, TurnOutcome, TurnOutcomeKind};
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
        // Pins: contact token issuance stays on the authenticated proxy and injects the path workspace id.
        let uri = "/v1/workspaces/workspace-a/contacts/tokens"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"display_name":"Ada"}"#);

        let translation = translate_public_route(&Method::POST, &uri, &body);

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
                        "workspace_id": "workspace-a"
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
        let list_uri = "/v1/workspaces/workspace-a/action-reviews"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate_public_route(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/list_pending");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("list body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({ "workspace_id": "workspace-a" })
                );
            }
            RouteTranslation::NoChange => {
                panic!("action review list should translate to ActionReviews service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("action review list should not fail translation: {message}")
            }
        }

        let decision_uri =
            "/v1/workspaces/workspace-a/action-reviews/11111111-1111-1111-1111-111111111111/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"decision":"cleared","reason":null}"#);
        let decision_translation =
            translate_public_route(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "workspace_id": "workspace-a",
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
        let list_translation = translate_public_route(&Method::GET, &list_uri, &Bytes::new());
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
        let decision_translation =
            translate_public_route(&Method::POST, &decision_uri, &decision_body);
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
            (
                "/v1/analytics/experiment-stats",
                "/Analytics/experiment_stats",
                r#"{"workspace_id":"workspace-a","from_time":null,"to_time":null,"limit":20}"#,
            ),
            (
                "/v1/analytics/learning-candidates",
                "/Analytics/learning_candidates",
                r#"{"workspace_id":"workspace-a","status":"proposed","limit":20}"#,
            ),
            (
                "/v1/analytics/session-search",
                "/Analytics/session_search",
                r#"{"workspace_id":"workspace-a","query":"refresh token","from_time":null,"to_time":null,"event_types":["user_message"],"limit":10}"#,
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
            let body = Bytes::from_static(br#"{"workspace_id":"workspace-a"}"#);

            let translation = translate_public_route(&Method::POST, &uri, &body);

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
            let body = Bytes::from_static(br#"{"workspace_id":"workspace-a"}"#);

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
                        body.as_ref(),
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
    fn configured_agent_public_routes_translate_to_restate_handlers() {
        // Pins: tenant-configurable agent product routes reach AgentDefinitions and simulation handlers.
        let installation_uid = "11111111-1111-1111-1111-111111111111";
        let revision_uid = "22222222-2222-2222-2222-222222222222";
        let run_uid = "33333333-3333-3333-3333-333333333333";
        let cases = vec![
            (
                Method::GET,
                "/v1/workspaces/workspace-a/agent-definitions".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_definitions",
                serde_json::json!({ "workspace_id": "workspace-a" }),
            ),
            (
                Method::GET,
                "/v1/workspaces/workspace-a/agent-installations".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_installations",
                serde_json::json!({ "workspace_id": "workspace-a" }),
            ),
            (
                Method::POST,
                "/v1/workspaces/workspace-a/agent-installations".to_string(),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","metadata":{{"tier":"gold"}}}}"#
                )),
                "/AgentDefinitions/install",
                serde_json::json!({
                    "workspace_id": "workspace-a",
                    "revision_uid": revision_uid,
                    "metadata": { "tier": "gold" }
                }),
            ),
            (
                Method::GET,
                format!(
                    "/v1/workspaces/workspace-a/agent-installations/{installation_uid}/deployments"
                ),
                Bytes::new(),
                "/AgentDefinitions/list_deployments",
                serde_json::json!({
                    "workspace_id": "workspace-a",
                    "installation_uid": installation_uid
                }),
            ),
            (
                Method::POST,
                format!(
                    "/v1/workspaces/workspace-a/agent-installations/{installation_uid}/deployments"
                ),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","reason":"candidate passed"}}"#
                )),
                "/AgentDefinitions/deploy",
                serde_json::json!({
                    "workspace_id": "workspace-a",
                    "installation_uid": installation_uid,
                    "revision_uid": revision_uid,
                    "reason": "candidate passed"
                }),
            ),
            (
                Method::POST,
                "/v1/workspaces/workspace-a/agent-simulations".to_string(),
                Bytes::from(format!(
                    r#"{{"name":"compare support","plan_revision_uid":"{revision_uid}","base":{{"variant_key":"base","revision_uid":"{revision_uid}"}}}}"#
                )),
                "/Experiments/run_agent_revision_simulation",
                serde_json::json!({
                    "workspace_id": "workspace-a",
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
                format!("/v1/workspaces/workspace-a/agent-simulations/{run_uid}/compare"),
                Bytes::from_static(
                    br#"{"base_variant_key":"base","candidate_variant_keys":["candidate"]}"#,
                ),
                "/Experiments/compare_agent_revision_simulation",
                serde_json::json!({
                    "workspace_id": "workspace-a",
                    "run_uid": run_uid,
                    "base_variant_key": "base",
                    "candidate_variant_keys": ["candidate"]
                }),
            ),
        ];

        for (method, public_path, body, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");

            let translation = translate_public_route(&method, &uri, &body);

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
        let body = Bytes::from_static(br#"{"meta":{"workspace_id":"workspace-a"},"agent":{}}"#);

        let translation = translate_public_route(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/SessionStore/create_agent_session");
                assert_eq!(forwarded_body, body.as_ref());
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

        let translation = translate_public_route(&Method::POST, &uri, &body);

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

        let translation = translate_public_route(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::BadRequest(message) => assert_eq!(message, "bad session id"),
            RouteTranslation::Forward { path, .. } => {
                panic!("bad session id should not translate to {path}")
            }
            RouteTranslation::NoChange => panic!("bad session id should be rejected"),
        }
    }

    #[test]
    fn stale_experiment_alias_routes_do_not_translate() {
        // Pins: removed experiment aliases cannot bypass the product-shaped public API.
        let stale_paths = [
            "/v1/experiments/run",
            "/v1/experiments/generate_plan",
            "/v1/experiments/trial_status",
        ];

        for public_path in stale_paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{"workspace_id":"workspace-a"}"#);

            match translate_public_route(&Method::POST, &uri, &body) {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
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
                "/v1/skills/bootstrap-workspace-default",
                "/Skills/bootstrap_workspace_default",
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
    fn artifact_public_routes_translate_to_restate_handlers() {
        // Pins: hosted artifact edge routes forward to the internal Artifacts service paths.
        let cases = [
            ("/v1/artifacts/import", "/Artifacts/import"),
            ("/v1/artifacts/export", "/Artifacts/export"),
            ("/v1/artifacts/list", "/Artifacts/list"),
            ("/v1/artifacts/validate", "/Artifacts/validate"),
            ("/v1/artifacts/publish", "/Artifacts/publish"),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{"workspace_id":"workspace-a"}"#);

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
                        body.as_ref(),
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
    fn learning_candidate_public_routes_translate_to_restate_handlers() {
        // Pins: hosted learning-review edge routes forward to the internal LearningReview service paths.
        let cases = [
            ("/v1/learning-candidates/get", "/LearningReview/get"),
            (
                "/v1/learning-candidates/accept-skill",
                "/LearningReview/accept_skill",
            ),
            ("/v1/learning-candidates/reject", "/LearningReview/reject"),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(
                br#"{"workspace_id":"workspace-a","candidate_id":"11111111-1111-1111-1111-111111111111"}"#,
            );

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
                        body.as_ref(),
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
            let body = Bytes::from_static(br#"{"workspace_id":"workspace-a"}"#);

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
                        body.as_ref(),
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
