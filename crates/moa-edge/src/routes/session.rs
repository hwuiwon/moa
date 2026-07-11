//! Public session route translation.

use std::sync::OnceLock;

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::types::identifiers::TenantId;
use uuid::Uuid;

use super::{RouteTranslation, translate_create_agent_session_route};

/// Default cancel-scope body forwarded when a client omits an explicit scope.
///
/// Derived once from [`moa_core::types::session::CancelScope::default`]'s snake_case serialization (today
/// `"task_tree"`) so a bare "stop" cancels the coordinator turn and the whole child task tree, and
/// the forwarded bytes can never drift from the type. The value is pinned by
/// [`tests::default_cancel_scope_body_matches_task_tree_serialization`].
fn default_cancel_scope_body() -> &'static [u8] {
    static BODY: OnceLock<Vec<u8>> = OnceLock::new();
    BODY.get_or_init(|| {
        serde_json::to_vec(&moa_core::types::session::CancelScope::default())
            .expect("CancelScope serializes to JSON")
    })
    .as_slice()
}

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/sessions/")
            .and_then(|rest| rest.strip_suffix("/progress"))
    {
        let session_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return Some(RouteTranslation::BadRequest("bad session id")),
        };
        return Some(RouteTranslation::Forward {
            method: Method::POST,
            path: format!("/Session/{session_id}/progress"),
            body: body.to_vec(),
        });
    }
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/sessions/")
            .and_then(|rest| rest.strip_suffix("/cancel"))
    {
        let session_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return Some(RouteTranslation::BadRequest("bad session id")),
        };
        // Default to TaskTree when the client sends no body; forward an explicit
        // `"coordinator_only"`/`"task_tree"` scope unchanged for the Session VO to validate.
        let body = if body.is_empty() {
            default_cancel_scope_body().to_vec()
        } else {
            body.to_vec()
        };
        return Some(RouteTranslation::Forward {
            method: Method::POST,
            path: format!("/Session/{session_id}/cancel"),
            body,
        });
    }
    if *method == Method::POST && uri.path() == "/v1/sessions/create-agent" {
        return Some(translate_create_agent_session_route(body, tenant_id));
    }
    None
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::translate;

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
    fn default_cancel_scope_body_matches_task_tree_serialization() {
        // Pins: CancelScope::default() still serializes to the task-tree wire literal, so an
        // omitted scope forwards a whole-tree cancel. The forwarded bytes are derived from the
        // type, so this guards the semantic value that derivation produces, not a duplicate literal.
        assert_eq!(super::default_cancel_scope_body(), b"\"task_tree\"");
    }

    #[test]
    fn session_cancel_public_route_defaults_to_task_tree_when_body_empty() {
        // Pins: a public cancel with no explicit scope cancels the whole task tree (today's stop).
        let session_id = "11111111-1111-1111-1111-111111111111";
        let uri = format!("/v1/sessions/{session_id}/cancel")
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::new();

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Session/11111111-1111-1111-1111-111111111111/cancel");
                assert_eq!(forwarded_body, b"\"task_tree\"");
            }
            RouteTranslation::NoChange => panic!("session cancel route should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("session cancel route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn session_cancel_public_route_forwards_explicit_coordinator_only() {
        // Pins: an explicit `coordinator_only` scope reaches the Session VO unchanged so a user can
        // interrupt the coordinator turn while leaving worker work running.
        let session_id = "11111111-1111-1111-1111-111111111111";
        let uri = format!("/v1/sessions/{session_id}/cancel")
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(b"\"coordinator_only\"");

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                path,
                body: forwarded_body,
                ..
            } => {
                assert_eq!(path, "/Session/11111111-1111-1111-1111-111111111111/cancel");
                assert_eq!(forwarded_body, b"\"coordinator_only\"");
            }
            RouteTranslation::NoChange => panic!("session cancel route should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("session cancel route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn session_cancel_public_route_rejects_bad_session_id() {
        // Pins: malformed public Session/cancel paths do not reach the Restate object namespace.
        let uri = "/v1/sessions/not-a-uuid/cancel"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::new();

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
}
