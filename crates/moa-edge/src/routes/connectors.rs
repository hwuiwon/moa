//! Public connector connection route adapters.
//!
//! Secret-free management commands translate to normal Restate handlers.
//! Credential plaintext takes the dedicated authenticated exact-path proxy and
//! never enters a Restate request body.

use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_wire::connectors::{
    ConnectorConnectionCreateRequest, ConnectorConnectionMutationRequest,
    ConnectorConnectionUseRequest,
};
use serde::Serialize;
use uuid::Uuid;

use crate::connector_credential_proxy::{
    ConnectorCredentialProxyError, MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES,
};

use super::{AppState, RouteTranslation, authenticate_direct_request};

const CREDENTIAL_PUBLIC_ROUTE: &str =
    "/v1/connectors/connections/{connection_id}/credentials/{slot_name}";

#[derive(Serialize)]
struct ConnectionSelector {
    connection_id: ConnectorConnectionId,
}

#[derive(Serialize)]
struct ConnectionMutationCommand {
    connection_id: ConnectorConnectionId,
    expected_generation: u64,
}

#[derive(Serialize)]
struct ConnectionUseCommand {
    connection_id: ConnectorConnectionId,
    #[serde(flatten)]
    request: ConnectorConnectionUseRequest,
}

/// Returns whether a path belongs to the exact connector-management subtree.
pub(super) fn matches_management_path(path: &str) -> bool {
    path == "/v1/connectors/connections" || path.starts_with("/v1/connectors/connections/")
}

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    _tenant_id: TenantId,
) -> Option<RouteTranslation> {
    let path = uri.path();
    if !matches_management_path(path) {
        return None;
    }
    if uri.query().is_some() {
        return Some(RouteTranslation::BadRequest(
            "connector routes do not accept query parameters",
        ));
    }

    if path == "/v1/connectors/connections" {
        return Some(match *method {
            Method::POST => translate_create(body),
            Method::GET if body.is_empty() => forward_empty("/ConnectorConnections/list"),
            Method::GET => RouteTranslation::BadRequest("connector list body must be empty"),
            _ => RouteTranslation::NotFound,
        });
    }

    let tail = path
        .strip_prefix("/v1/connectors/connections/")
        .expect("connector prefix was checked above");
    let mut segments = tail.split('/');
    let connection_id = match segments.next().and_then(parse_connection_id) {
        Some(connection_id) => connection_id,
        None => {
            return Some(RouteTranslation::BadRequest(
                "invalid connector connection id",
            ));
        }
    };
    let children: Vec<_> = segments.collect();

    let translation = match children.as_slice() {
        [] if *method == Method::GET && body.is_empty() => serialize_command(
            "/ConnectorConnections/get",
            &ConnectionSelector { connection_id },
        ),
        [] if *method == Method::GET => {
            RouteTranslation::BadRequest("connector get body must be empty")
        }
        [] if *method == Method::DELETE => {
            translate_mutation(body, connection_id, "/ConnectorConnections/disconnect")
        }
        [operation @ ("verify" | "activate" | "suspend" | "resume" | "disconnect" | "delete")]
            if *method == Method::POST =>
        {
            let target = match *operation {
                "verify" => "/ConnectorConnections/verify",
                "activate" => "/ConnectorConnections/activate",
                "suspend" => "/ConnectorConnections/suspend",
                "resume" => "/ConnectorConnections/resume",
                "disconnect" => "/ConnectorConnections/disconnect",
                "delete" => "/ConnectorConnections/delete",
                _ => unreachable!("closed connector lifecycle operation"),
            };
            translate_mutation(body, connection_id, target)
        }
        ["use", operation @ ("grant" | "revoke")] if *method == Method::POST => {
            let target = if *operation == "grant" {
                "/ConnectorConnections/grant_use"
            } else {
                "/ConnectorConnections/revoke_use"
            };
            translate_use(body, connection_id, target)
        }
        _ => RouteTranslation::NotFound,
    };
    Some(translation)
}

fn translate_create(body: &Bytes) -> RouteTranslation {
    let request = match serde_json::from_slice::<ConnectorConnectionCreateRequest>(body) {
        Ok(request) => request,
        Err(_) => return RouteTranslation::BadRequest("invalid connector create request"),
    };
    serialize_command("/ConnectorConnections/create", &request)
}

fn translate_mutation(
    body: &Bytes,
    connection_id: ConnectorConnectionId,
    target: &'static str,
) -> RouteTranslation {
    let request = match serde_json::from_slice::<ConnectorConnectionMutationRequest>(body) {
        Ok(request) => request,
        Err(_) => return RouteTranslation::BadRequest("invalid connector mutation request"),
    };
    serialize_command(
        target,
        &ConnectionMutationCommand {
            connection_id,
            expected_generation: request.expected_generation,
        },
    )
}

fn translate_use(
    body: &Bytes,
    connection_id: ConnectorConnectionId,
    target: &'static str,
) -> RouteTranslation {
    let request = match serde_json::from_slice::<ConnectorConnectionUseRequest>(body) {
        Ok(request) => request,
        Err(_) => return RouteTranslation::BadRequest("invalid connector use request"),
    };
    serialize_command(
        target,
        &ConnectionUseCommand {
            connection_id,
            request,
        },
    )
}

fn forward_empty(target: &'static str) -> RouteTranslation {
    RouteTranslation::Forward {
        method: Method::POST,
        path: target.to_string(),
        body: b"{}".to_vec(),
    }
}

fn serialize_command(target: &'static str, command: &impl Serialize) -> RouteTranslation {
    match serde_json::to_vec(command) {
        Ok(body) => RouteTranslation::Forward {
            method: Method::POST,
            path: target.to_string(),
            body,
        },
        Err(error) => {
            tracing::error!(%error, target, "serialize connector command failed");
            RouteTranslation::BadRequest("invalid connector request")
        }
    }
}

fn parse_connection_id(value: &str) -> Option<ConnectorConnectionId> {
    Uuid::parse_str(value).ok().map(ConnectorConnectionId)
}

/// Proxies one bounded opaque credential body to private orchestrator ingress.
#[tracing::instrument(
    skip(state, request),
    fields(
        http.route = CREDENTIAL_PUBLIC_ROUTE,
        http.status_code = tracing::field::Empty,
        connector.connection_id = %connection_id,
        connector.credential_slot = %slot_name,
    )
)]
pub(super) async fn write_credential(
    State(state): State<AppState>,
    Path((connection_id, slot_name)): Path<(ConnectorConnectionId, CredentialSlotName)>,
    request: Request,
) -> Response {
    if !state.connector_management_enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let span = tracing::Span::current();
    // Do not adopt caller trace state on the plaintext-bearing route. W3C
    // tracestate is intentionally extensible caller text, so carrying it to the
    // private listener would create a second body-data smuggling channel.
    let identity =
        match authenticate_direct_request(&state, request.headers(), CREDENTIAL_PUBLIC_ROUTE).await
        {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    if request.uri().query().is_some() {
        span.record("http.status_code", 400_i64);
        return (StatusCode::BAD_REQUEST, "query parameters are not accepted").into_response();
    }
    if let Err(response) = validate_credential_request_headers(request.headers()) {
        span.record("http.status_code", response.status().as_u16() as i64);
        return response;
    }

    let body = match to_bytes(request.into_body(), MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES).await {
        Ok(body) if !body.is_empty() => body,
        Ok(_) => {
            span.record("http.status_code", 400_i64);
            return (
                StatusCode::BAD_REQUEST,
                "credential request body is required",
            )
                .into_response();
        }
        Err(_) => {
            span.record("http.status_code", 413_i64);
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "credential request is too large",
            )
                .into_response();
        }
    };

    let response = match state
        .connector_credentials
        .forward(&identity, connection_id, &slot_name, body)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ConnectorCredentialProxyError::Rejected { status }) => {
            connector_rejection_response(status)
        }
        Err(ConnectorCredentialProxyError::InvalidRequest) => (
            StatusCode::BAD_REQUEST,
            "credential request body is required",
        )
            .into_response(),
        Err(ConnectorCredentialProxyError::RequestTooLarge) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "credential request is too large",
        )
            .into_response(),
        Err(
            error @ (ConnectorCredentialProxyError::Transport
            | ConnectorCredentialProxyError::InvalidResponse),
        ) => {
            tracing::error!(%error, "private connector credential ingress failed");
            (StatusCode::BAD_GATEWAY, "credential ingress unavailable").into_response()
        }
    };
    span.record("http.status_code", response.status().as_u16() as i64);
    response
}

fn validate_credential_request_headers(headers: &HeaderMap) -> Result<(), Response> {
    let mut content_types = headers.get_all(header::CONTENT_TYPE).iter();
    let content_type = content_types
        .next()
        .filter(|_| content_types.next().is_none())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "credential requests require application/json",
        )
            .into_response());
    }

    let mut content_lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    if let Some(length) = content_lengths.next() {
        if content_lengths.next().is_some() {
            return Err((StatusCode::BAD_REQUEST, "invalid content-length").into_response());
        }
        let length = length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "invalid content-length").into_response())?;
        if length > MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES as u64 {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "credential request is too large",
            )
                .into_response());
        }
    }
    Ok(())
}

fn connector_rejection_response(status: StatusCode) -> Response {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            (StatusCode::BAD_REQUEST, "invalid credential request").into_response()
        }
        StatusCode::FORBIDDEN => (StatusCode::FORBIDDEN, "forbidden").into_response(),
        StatusCode::NOT_FOUND => (StatusCode::NOT_FOUND, "not found").into_response(),
        StatusCode::CONFLICT => (StatusCode::CONFLICT, "credential write conflict").into_response(),
        StatusCode::TOO_MANY_REQUESTS => (
            StatusCode::TOO_MANY_REQUESTS,
            "credential ingress rate limited",
        )
            .into_response(),
        _ => (StatusCode::BAD_GATEWAY, "credential ingress unavailable").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use moa_wire::connectors::{ConnectorDefinitionReference, ConnectorUseSubject};
    use serde_json::{Value, json};
    use std::num::NonZeroU64;

    fn uri(path: &str) -> Uri {
        path.parse().expect("fixture URI should parse")
    }

    fn connection_id() -> ConnectorConnectionId {
        ConnectorConnectionId(Uuid::from_u128(77))
    }

    #[test]
    fn management_path_matcher_covers_only_the_complete_public_subtree() {
        // Pins: the dark-by-default edge gate recognizes every connector
        // management route, including the private credential proxy path,
        // without darkening adjacent public namespaces.
        for path in [
            "/v1/connectors/connections",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/verify",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/activate",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/suspend",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/resume",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/disconnect",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/delete",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/use/grant",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/use/revoke",
            "/v1/connectors/connections/00000000-0000-0000-0000-000000000001/credentials/primary",
        ] {
            assert!(matches_management_path(path), "management path: {path}");
        }

        for path in [
            "/v1/connectors",
            "/v1/connectors/connections-adjacent",
            "/v1/knowledge/connections",
        ] {
            assert!(!matches_management_path(path), "adjacent path: {path}");
        }
    }

    #[test]
    fn connector_management_translation_is_tenantless_and_typed() {
        // Pins: the authenticated identity header is the tenant authority; JSON
        // contains only the typed public request plus path-derived connection id.
        let connection_id = connection_id();
        let create = ConnectorConnectionCreateRequest {
            connection_id,
            display_name: "Billing".to_string(),
            definition_ref: ConnectorDefinitionReference::BuiltIn {
                key: "billing".to_string(),
                version: NonZeroU64::new(1).expect("fixture version is positive"),
            },
            origin: Some("https://billing.example.test".to_string()),
            non_secret_config: json!({"region": "east"}),
        };
        let create_body = Bytes::from(
            serde_json::to_vec(&create).expect("fixture create request should serialize"),
        );
        let translation = translate(
            &Method::POST,
            &uri("/v1/connectors/connections"),
            &create_body,
            TenantId::from(Uuid::from_u128(88)),
        )
        .expect("connector route should match");
        let RouteTranslation::Forward { method, path, body } = translation else {
            panic!("valid connector create should forward")
        };
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/ConnectorConnections/create");
        let value: Value =
            serde_json::from_slice(&body).expect("translated connector create should remain JSON");
        assert_eq!(
            value,
            serde_json::to_value(create).expect("fixture serializes")
        );
        assert!(value.get("tenant_id").is_none());
        assert!(value.get("identity_id").is_none());

        let mutation = Bytes::from_static(br#"{"expected_generation":3}"#);
        let translation = translate(
            &Method::POST,
            &uri(&format!(
                "/v1/connectors/connections/{connection_id}/activate"
            )),
            &mutation,
            TenantId::from(Uuid::from_u128(99)),
        )
        .expect("connector activation route should match");
        let RouteTranslation::Forward { path, body, .. } = translation else {
            panic!("valid connector activation should forward")
        };
        assert_eq!(path, "/ConnectorConnections/activate");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("activation should be JSON"),
            json!({"connection_id": connection_id, "expected_generation": 3})
        );
    }

    #[test]
    fn connector_use_translation_accepts_only_closed_typed_subjects() {
        // Pins: direct grants derive the connection from the path and accept
        // only the closed operator/agent/contact wire subject vocabulary.
        let connection_id = connection_id();
        let request = ConnectorConnectionUseRequest {
            subject: ConnectorUseSubject::Agent {
                id: Uuid::from_u128(101),
            },
        };
        let body = Bytes::from(
            serde_json::to_vec(&request).expect("fixture use request should serialize"),
        );
        let translation = translate(
            &Method::POST,
            &uri(&format!(
                "/v1/connectors/connections/{connection_id}/use/grant"
            )),
            &body,
            TenantId::from(Uuid::nil()),
        )
        .expect("connector use route should match");
        let RouteTranslation::Forward { path, body, .. } = translation else {
            panic!("valid grant should forward")
        };
        assert_eq!(path, "/ConnectorConnections/grant_use");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("grant should be JSON"),
            json!({
                "connection_id": connection_id,
                "subject": {"type": "agent", "id": Uuid::from_u128(101)},
            })
        );

        let invalid = Bytes::from_static(
            br#"{"subject":{"type":"service","id":"00000000-0000-0000-0000-000000000001"}}"#,
        );
        assert_eq!(
            translate(
                &Method::POST,
                &uri(&format!(
                    "/v1/connectors/connections/{connection_id}/use/grant"
                )),
                &invalid,
                TenantId::from(Uuid::nil()),
            ),
            Some(RouteTranslation::BadRequest(
                "invalid connector use request"
            ))
        );
    }

    #[test]
    fn connector_delete_route_is_explicit_and_http_delete_remains_disconnect() {
        // Pins: destructive deletion requires its explicit child route; the
        // conventional HTTP DELETE remains the retained-record disconnect.
        let connection_id = connection_id();
        let body = Bytes::from_static(br#"{"expected_generation":9}"#);

        let explicit_delete = translate(
            &Method::POST,
            &uri(&format!(
                "/v1/connectors/connections/{connection_id}/delete"
            )),
            &body,
            TenantId::from(Uuid::nil()),
        )
        .expect("explicit connector delete route should match");
        let RouteTranslation::Forward { path, body, .. } = explicit_delete else {
            panic!("explicit connector delete should forward")
        };
        assert_eq!(path, "/ConnectorConnections/delete");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("delete should be JSON"),
            json!({"connection_id": connection_id, "expected_generation": 9})
        );

        let ordinary_disconnect = translate(
            &Method::DELETE,
            &uri(&format!("/v1/connectors/connections/{connection_id}")),
            &Bytes::from_static(br#"{"expected_generation":9}"#),
            TenantId::from(Uuid::nil()),
        )
        .expect("ordinary connector HTTP DELETE route should match");
        let RouteTranslation::Forward { path, .. } = ordinary_disconnect else {
            panic!("ordinary connector HTTP DELETE should forward")
        };
        assert_eq!(path, "/ConnectorConnections/disconnect");
    }

    #[test]
    fn connector_routes_reject_wrong_method_query_and_untyped_payloads() {
        // Pins: management paths are an exact allowlist and cannot smuggle
        // tenant, credential, or definition bytes through unknown fields.
        let connection_id = connection_id();
        assert_eq!(
            translate(
                &Method::GET,
                &uri("/v1/connectors/connections?tenant_id=forged"),
                &Bytes::new(),
                TenantId::from(Uuid::nil()),
            ),
            Some(RouteTranslation::BadRequest(
                "connector routes do not accept query parameters"
            ))
        );
        assert_eq!(
            translate(
                &Method::PATCH,
                &uri(&format!("/v1/connectors/connections/{connection_id}")),
                &Bytes::new(),
                TenantId::from(Uuid::nil()),
            ),
            Some(RouteTranslation::NotFound)
        );
        let invalid_create = Bytes::from(
            serde_json::to_vec(&json!({
                "connection_id": connection_id,
                "display_name": "Billing",
                "definition_ref": {"kind": "built_in", "key": "billing", "version": 1},
                "credential": "must-not-forward",
            }))
            .expect("invalid fixture should serialize"),
        );
        assert_eq!(
            translate(
                &Method::POST,
                &uri("/v1/connectors/connections"),
                &invalid_create,
                TenantId::from(Uuid::nil()),
            ),
            Some(RouteTranslation::BadRequest(
                "invalid connector create request"
            ))
        );
    }

    #[test]
    fn credential_request_headers_enforce_json_and_prebuffer_size_bound() {
        // Pins: unsupported content types and known oversized bodies fail before
        // the opaque credential body is read or sent to private ingress.
        let mut headers = HeaderMap::new();
        let response = validate_credential_request_headers(&headers)
            .expect_err("missing content type should be rejected");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&(MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES + 1).to_string())
                .expect("fixture content length should parse"),
        );
        let response = validate_credential_request_headers(&headers)
            .expect_err("known oversized body should be rejected");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        headers.remove(header::CONTENT_LENGTH);
        headers.append(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = validate_credential_request_headers(&headers)
            .expect_err("ambiguous duplicate content type should be rejected");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        headers.remove(header::CONTENT_TYPE);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("65536"));
        validate_credential_request_headers(&headers)
            .expect("exact maximum JSON request should pass header admission");
    }

    #[test]
    fn credential_rejection_responses_never_reflect_upstream_text() {
        // Pins: the public error contract is selected only from an allowlisted
        // status, so upstream bodies and raw messages have no response carrier.
        for (upstream, expected) in [
            (StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST),
            (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
            (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
            (StatusCode::CONFLICT, StatusCode::CONFLICT),
            (StatusCode::TOO_MANY_REQUESTS, StatusCode::TOO_MANY_REQUESTS),
            (StatusCode::INTERNAL_SERVER_ERROR, StatusCode::BAD_GATEWAY),
        ] {
            assert_eq!(connector_rejection_response(upstream).status(), expected);
        }
    }
}
