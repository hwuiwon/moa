//! Public session route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;
use uuid::Uuid;

use super::{RouteTranslation, translate_create_agent_session_route};

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
