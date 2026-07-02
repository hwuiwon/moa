//! Restate ingress path construction for the MOA edge.
//!
//! Restate 1.7 retired the unversioned `/{Service}/{key}/{handler}` ingress
//! paths in favor of explicit schemes: `/restate/call/...` for request-response
//! invocations, `/restate/send/...` for fire-and-forget, and a
//! `/restate/scope/{scopeKey}/...` prefix that enrolls an invocation in
//! cluster-wide flow control. The edge only ever issues request-response calls
//! to the ingress today, so this module builds the `call` form.
//!
//! Route translation produces a *service path* — the `/{Service}/{handler}` (or
//! keyed `/{Service}/{key}/{handler}`) segment naming the target handler — and
//! this module prefixes it with the ingress scheme. Keeping the scheme here
//! means the per-route translation layer never encodes Restate's wire contract,
//! and every forwarded call routes through one place that can add a flow-control
//! scope.

use moa_core::TenantId;

/// Flow-control scope applied to a Restate ingress call.
///
/// Restate admission control keys concurrency on an opaque scope segment. Under
/// the cluster rule book's `*` default rule, every distinct scope key gets its
/// own concurrency counter. The edge scopes only the invocations that start
/// expensive agent work, keyed per tenant, so one tenant cannot exhaust cluster
/// capacity; cheap reads and status polls stay [`IngressScope::Unscoped`] and
/// never consume a tenant's concurrency slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressScope {
    /// No flow-control scope; the call bypasses per-tenant admission control.
    Unscoped,
    /// Per-tenant admission-control scope, rendered as the single path segment
    /// `tenant-{tenant_id}`.
    Tenant(TenantId),
}

impl IngressScope {
    /// Render the opaque scope-key path segment, or `None` when unscoped.
    ///
    /// A tenant id is a hyphenated UUID and contains no `/`, so `tenant-{id}` is
    /// always a single path segment as the scoped ingress form requires.
    fn scope_key(&self) -> Option<String> {
        match self {
            IngressScope::Unscoped => None,
            IngressScope::Tenant(tenant_id) => Some(format!("tenant-{tenant_id}")),
        }
    }
}

/// Build the Restate 1.7 request-response ingress path for a service handler.
///
/// `service_path` is the leading-slash `/{Service}/{handler}` (or keyed
/// `/{Service}/{key}/{handler}`) segment. Unscoped calls become
/// `/restate/call{service_path}`; tenant-scoped calls become
/// `/restate/scope/tenant-{tenant_id}/call{service_path}`.
pub(crate) fn call_path(scope: &IngressScope, service_path: &str) -> String {
    match scope.scope_key() {
        Some(scope_key) => format!("/restate/scope/{scope_key}/call{service_path}"),
        None => format!("/restate/call{service_path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{IngressScope, call_path};
    use moa_core::TenantId;
    use uuid::Uuid;

    fn tenant() -> TenantId {
        TenantId::from(
            Uuid::parse_str("11111111-2222-3333-4444-555555555555").expect("tenant uuid parses"),
        )
    }

    #[test]
    fn unscoped_service_call_uses_restate_call_prefix() {
        // Pins: an unscoped read forwards to the v1.7 `/restate/call` form with the
        // service path appended unchanged, so a cheap poll never carries a scope segment.
        assert_eq!(
            call_path(&IngressScope::Unscoped, "/Session/abc/progress"),
            "/restate/call/Session/abc/progress"
        );
    }

    #[test]
    fn tenant_scoped_service_call_uses_scope_prefix() {
        // Pins: a turn-starting call is enrolled in per-tenant flow control via the
        // `/restate/scope/{scopeKey}/call` form, where scopeKey is `tenant-{uuid}`.
        assert_eq!(
            call_path(&IngressScope::Tenant(tenant()), "/Contacts/send_message"),
            "/restate/scope/tenant-11111111-2222-3333-4444-555555555555/call/Contacts/send_message"
        );
    }

    #[test]
    fn tenant_scope_key_is_a_single_path_segment() {
        // Pins: the scope key stays one path segment (no interior `/`) so Restate reads
        // exactly `tenant-{uuid}` as the opaque scopeKey and the service path after `/call`.
        let path = call_path(&IngressScope::Tenant(tenant()), "/Contacts/send_message");
        let scope_key = path
            .strip_prefix("/restate/scope/")
            .and_then(|rest| rest.split_once("/call/"))
            .map(|(scope_key, _)| scope_key)
            .expect("scoped path has a scope key before /call/");
        assert_eq!(scope_key, "tenant-11111111-2222-3333-4444-555555555555");
        assert!(!scope_key.contains('/'), "scope key must be one segment");
    }
}
