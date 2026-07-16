//! Helpers for propagating trusted MOA identity headers across Restate calls.

use moa_core::traits::{Identity, IdentityType};
use reqwest::RequestBuilder;
use restate_sdk::prelude::Request;

/// Marks a generated Restate request as safe for deterministic journal replay.
///
/// Attempt-local OpenTelemetry headers must not be attached to Restate commands:
/// a process restart creates a new handler span, which would rebuild a different
/// command and trigger a journal mismatch. Durable callbacks that require exact
/// W3C causality persist and reinject a validated context explicitly.
pub(crate) fn replay_safe_request<'a, Req, Res>(
    request: Request<'a, Req, Res>,
) -> Request<'a, Req, Res> {
    request
}

/// Attaches the active span's W3C trace context to a raw HTTP request.
pub(crate) fn with_reqwest_trace_headers(request: RequestBuilder) -> RequestBuilder {
    moa_observability::propagation::with_reqwest_trace_headers(request)
}

/// Attaches trusted identity headers to a generated Restate client request.
pub(crate) fn with_identity_headers<'a, Req, Res>(
    request: Request<'a, Req, Res>,
    identity: &Identity,
) -> Request<'a, Req, Res> {
    let request = request
        .header(
            "x-moa-identity-type".to_string(),
            identity_type_header(identity.identity_type).to_string(),
        )
        .header("x-moa-identity-id".to_string(), identity.id.to_string())
        .header(
            "x-moa-tenant-id".to_string(),
            identity.tenant_id.to_string(),
        );
    let request = if let Some(api_key_id) = identity.api_key_id {
        request.header("x-moa-api-key-id".to_string(), api_key_id.to_string())
    } else {
        request
    };
    let request = if let Some(user_id) = identity.acting_on_behalf_of {
        request.header("x-moa-acting-on-behalf-of".to_string(), user_id.to_string())
    } else {
        request
    };
    replay_safe_request(request)
}

/// Attaches trusted identity headers to an HTTP Restate ingress request.
///
/// Also injects the active span's W3C trace context so the receiving Restate
/// handler continues the same end-to-end trace.
pub(crate) fn with_reqwest_identity_headers(
    request: RequestBuilder,
    identity: &Identity,
) -> RequestBuilder {
    let request = request
        .header(
            "x-moa-identity-type",
            identity_type_header(identity.identity_type),
        )
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string());
    let request = if let Some(api_key_id) = identity.api_key_id {
        request.header("x-moa-api-key-id", api_key_id.to_string())
    } else {
        request
    };
    let request = if let Some(user_id) = identity.acting_on_behalf_of {
        request.header("x-moa-acting-on-behalf-of", user_id.to_string())
    } else {
        request
    };
    with_reqwest_trace_headers(request)
}

fn identity_type_header(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::Operator => "operator",
        IdentityType::Contact => "contact",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}
