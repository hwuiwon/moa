//! Dashboard session read routes.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::{Engine as _, engine::general_purpose};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{Identity, IdentityType};
use moa_core::{Channel, ContactId, EventType, SessionId, SessionStatus, SessionSummary, TenantId};
use moa_session::store::{
    DashboardEventCursor, DashboardEventPageRequest, DashboardEventTimelineItem,
    DashboardSessionListCursor, DashboardSessionListRequest,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::routes::{AppState, authenticate_direct_request, require_direct_authz, route_error};

#[derive(Debug, Deserialize)]
pub(super) struct SessionListQuery {
    tenant_id: Option<Uuid>,
    limit: Option<usize>,
    cursor: Option<String>,
    status: Option<SessionStatus>,
    channel: Option<Channel>,
    contact_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SessionDetailQuery {
    tenant_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub(super) struct EventListQuery {
    tenant_id: Option<Uuid>,
    limit: Option<usize>,
    cursor: Option<String>,
    #[serde(default)]
    event_type: Vec<EventType>,
}

#[derive(Debug, Serialize)]
struct SessionListResponse {
    sessions: Vec<SessionSummary>,
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct EventListResponse {
    events: Vec<DashboardEventTimelineItem>,
    next_cursor: Option<String>,
}

/// Lists dashboard sessions for a tenant visible to the authenticated operator.
#[tracing::instrument(skip(state, headers))]
pub(super) async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionListQuery>,
    headers: HeaderMap,
) -> Response {
    let tenant_id = match authenticate_dashboard_request(
        &state,
        &headers,
        "/v1/dashboard/sessions",
        query.tenant_id,
    )
    .await
    {
        Ok(tenant_id) => tenant_id,
        Err(response) => return response,
    };
    let cursor = match decode_cursor::<DashboardSessionListCursor>(query.cursor.as_deref()) {
        Ok(cursor) => cursor,
        Err(response) => return response,
    };

    let request = DashboardSessionListRequest {
        limit: query.limit,
        cursor,
        status: query.status,
        channel: query.channel,
        contact_id: query.contact_id.map(ContactId),
    };
    match state
        .session_store
        .list_dashboard_sessions(tenant_id, request)
        .await
    {
        Ok(page) => match encode_cursor(page.next_cursor.as_ref()) {
            Ok(next_cursor) => Json(SessionListResponse {
                sessions: page.sessions,
                next_cursor,
            })
            .into_response(),
            Err(response) => response,
        },
        Err(error) => route_error(error),
    }
}

/// Returns dashboard-safe details for one tenant-owned session.
#[tracing::instrument(skip(state, headers))]
pub(super) async fn get_session(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(query): Query<SessionDetailQuery>,
    headers: HeaderMap,
) -> Response {
    let tenant_id = match authenticate_dashboard_request(
        &state,
        &headers,
        "/v1/dashboard/sessions/{session_id}",
        query.tenant_id,
    )
    .await
    {
        Ok(tenant_id) => tenant_id,
        Err(response) => return response,
    };

    match state
        .session_store
        .get_dashboard_session_detail(tenant_id, SessionId(session_id))
        .await
    {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "session not found").into_response(),
        Err(error) => route_error(error),
    }
}

/// Lists redacted dashboard event timeline entries for one tenant-owned session.
#[tracing::instrument(skip(state, headers))]
pub(super) async fn list_events(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(query): Query<EventListQuery>,
    headers: HeaderMap,
) -> Response {
    let tenant_id = match authenticate_dashboard_request(
        &state,
        &headers,
        "/v1/dashboard/sessions/{session_id}/events",
        query.tenant_id,
    )
    .await
    {
        Ok(tenant_id) => tenant_id,
        Err(response) => return response,
    };
    let cursor = match decode_cursor::<DashboardEventCursor>(query.cursor.as_deref()) {
        Ok(cursor) => cursor,
        Err(response) => return response,
    };

    let event_types = (!query.event_type.is_empty()).then_some(query.event_type);
    let request = DashboardEventPageRequest {
        limit: query.limit,
        cursor,
        event_types,
    };
    match state
        .session_store
        .list_dashboard_session_events(tenant_id, SessionId(session_id), request)
        .await
    {
        Ok(page) => match encode_cursor(page.next_cursor.as_ref()) {
            Ok(next_cursor) => Json(EventListResponse {
                events: page.events,
                next_cursor,
            })
            .into_response(),
            Err(response) => response,
        },
        Err(error) => route_error(error),
    }
}

async fn authenticate_dashboard_request(
    state: &AppState,
    headers: &HeaderMap,
    route: &'static str,
    tenant_id: Option<Uuid>,
) -> Result<TenantId, Response> {
    let identity = authenticate_direct_request(state, headers, route).await?;
    let tenant_id = dashboard_tenant(&identity, tenant_id)?;
    require_dashboard_authz(state, &identity, tenant_id).await?;
    Ok(tenant_id)
}

fn dashboard_tenant(identity: &Identity, tenant_id: Option<Uuid>) -> Result<TenantId, Response> {
    if identity.identity_type == IdentityType::Contact {
        return Err((StatusCode::FORBIDDEN, "forbidden").into_response());
    }
    Ok(tenant_id.map(TenantId::from).unwrap_or(identity.tenant_id))
}

async fn require_dashboard_authz(
    state: &AppState,
    identity: &Identity,
    tenant_id: TenantId,
) -> Result<(), Response> {
    require_direct_authz(
        state,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
}

fn decode_cursor<T>(cursor: Option<&str>) -> Result<Option<T>, Response>
where
    T: DeserializeOwned,
{
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let bytes = general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| malformed_cursor())?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| malformed_cursor())
}

fn encode_cursor<T>(cursor: Option<&T>) -> Result<Option<String>, Response>
where
    T: Serialize,
{
    cursor
        .map(|cursor| {
            serde_json::to_vec(cursor)
                .map(|bytes| general_purpose::URL_SAFE_NO_PAD.encode(bytes))
                .map_err(|error| {
                    tracing::error!(error = %error, "dashboard cursor encode failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "read failed").into_response()
                })
        })
        .transpose()
}

fn malformed_cursor() -> Response {
    (StatusCode::BAD_REQUEST, "malformed cursor").into_response()
}
