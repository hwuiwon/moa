//! Restate ingress path construction for the MOA edge.
//!
//! Restate 1.7 retired the unversioned `/{Service}/{key}/{handler}` ingress
//! paths in favor of explicit schemes: `/restate/call/...` for request-response
//! invocations and `/restate/send/...` for fire-and-forget. MOA owns execution
//! admission in `Session` through the shared runtime cache, so edge traffic uses
//! the ordinary ingress schemes without a second Restate queue.
//!
//! Route translation produces a *service path* — the `/{Service}/{handler}` (or
//! keyed `/{Service}/{key}/{handler}`) segment naming the target handler — and
//! this module prefixes it with the ingress scheme. Keeping the scheme here
//! means the per-route translation layer never encodes Restate's wire contract.

/// Build the Restate 1.7 request-response ingress path for a service handler.
///
/// `service_path` is the leading-slash `/{Service}/{handler}` (or keyed
/// `/{Service}/{key}/{handler}`) segment.
pub(crate) fn call_path(service_path: &str) -> String {
    format!("/restate/call{service_path}")
}

/// Build the Restate fire-and-forget ingress path for a keyed workflow handler.
pub(crate) fn send_path(service_path: &str) -> String {
    format!("/restate/send{service_path}")
}

#[cfg(test)]
mod tests {
    use super::{call_path, send_path};

    #[test]
    fn unscoped_service_call_uses_restate_call_prefix() {
        // Pins: an unscoped read forwards to the v1.7 `/restate/call` form with the
        // service path appended unchanged, so a cheap poll never carries a scope segment.
        assert_eq!(
            call_path("/Session/abc/progress"),
            "/restate/call/Session/abc/progress"
        );
    }

    #[test]
    fn workflow_send_uses_restate_fire_and_forget_prefix() {
        // Pins: destructive tenant purge admission returns without waiting for workflow completion.
        assert_eq!(
            send_path("/TenantPurge/tenant-key/run"),
            "/restate/send/TenantPurge/tenant-key/run"
        );
    }
}
