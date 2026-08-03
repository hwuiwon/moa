//! Trusted request-header and trace helpers for Restate handlers.

use moa_core::traits::Identity;
use uuid::Uuid;

/// Identity-header extraction failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityHeaderError {
    /// A required identity header was absent.
    #[error("missing identity header: {0}")]
    Missing(&'static str),
    /// One or more identity headers were malformed.
    #[error("malformed identity header: {0}")]
    Malformed(&'static str),
    /// Identity type was present but not recognized.
    #[error("unknown identity type: {0}")]
    UnknownType(String),
}

/// Common header access for all Restate handler context variants.
pub trait RequestHeaders {
    /// Returns the request headers attached to the current Restate invocation.
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap;
}

impl RequestHeaders for restate_sdk::context::Context<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::ObjectContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::SharedObjectContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::WorkflowContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::SharedWorkflowContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

/// Extract a trusted identity from Restate request headers.
///
/// The core identity header set is all-or-nothing: either
/// `x-moa-identity-type`, `x-moa-identity-id`, and `x-moa-tenant-id` are all
/// present, or none are present. Optional API-key and delegation headers are
/// parsed only after the core set is valid.
pub fn extract_identity(
    headers: &restate_sdk::context::HeaderMap,
) -> Result<Option<Identity>, IdentityHeaderError> {
    let get = |name: &'static str| headers.get(name).map(String::as_str);
    let raw_type = get("x-moa-identity-type");
    let raw_id = get("x-moa-identity-id");
    let raw_tenant = get("x-moa-tenant-id");

    let (raw_type, raw_id, raw_tenant) = match (raw_type, raw_id, raw_tenant) {
        (None, None, None) => {
            return Err(IdentityHeaderError::Missing("x-moa-identity-type"));
        }
        (Some(raw_type), Some(raw_id), Some(raw_tenant)) => (raw_type, raw_id, raw_tenant),
        _ => {
            return Err(IdentityHeaderError::Malformed(
                "partial identity headers; require all of type/id/tenant",
            ));
        }
    };

    let identity_type = raw_type.parse().map_err(IdentityHeaderError::UnknownType)?;
    let id = parse_uuid(raw_id, "x-moa-identity-id")?;
    let tenant_id =
        moa_core::types::identifiers::TenantId::from(parse_uuid(raw_tenant, "x-moa-tenant-id")?);
    let api_key_id = get("x-moa-api-key-id")
        .map(|value| parse_uuid(value, "x-moa-api-key-id"))
        .transpose()?;
    let acting_on_behalf_of = get("x-moa-acting-on-behalf-of")
        .map(|value| parse_uuid(value, "x-moa-acting-on-behalf-of"))
        .transpose()?;

    Ok(Some(Identity {
        identity_type,
        id,
        tenant_id,
        api_key_id,
        acting_on_behalf_of,
    }))
}

/// Adopts and links an inbound W3C trace context on the current span.
///
/// Restate forwards custom request headers verbatim across invocations, so an
/// upstream hop that injected `traceparent` through the identity-header helpers
/// remains causally connected. Restate's endpoint span may already own the local
/// parent edge, so the explicit link preserves the remote hop even when parent
/// adoption cannot replace that edge. A missing or malformed `traceparent` is a
/// no-op.
pub(crate) fn adopt_incoming_trace_parent(ctx: &impl RequestHeaders) {
    let headers = ctx.request_headers();
    let span = tracing::Span::current();
    let _ = moa_observability::adopt_remote_parent(&span, |name| headers.get(name).cloned());
    let _ = moa_observability::propagation::link_remote_context(&span, |name| {
        headers.get(name).cloned()
    });
}

/// Extract identity from a Restate handler context.
pub fn current_identity(
    ctx: &impl RequestHeaders,
) -> Result<Option<Identity>, IdentityHeaderError> {
    let identity = extract_identity(ctx.request_headers())?;
    tracing::debug!(
        has_identity = identity.is_some(),
        "extracted request identity"
    );
    Ok(identity)
}

fn parse_uuid(value: &str, header: &'static str) -> Result<Uuid, IdentityHeaderError> {
    Uuid::parse_str(value).map_err(|_| IdentityHeaderError::Malformed(header))
}
