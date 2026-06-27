//! Public workflow route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;

use super::{RouteTranslation, translate_json_object_with_tenant_id};

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method != Method::POST {
        return None;
    }

    let translation = match uri.path() {
        "/v1/workflows/run" => {
            translate_json_object_with_tenant_id(body, "/Workflows/run", tenant_id)
        }
        "/v1/workflows/status" => {
            translate_json_object_with_tenant_id(body, "/Workflows/status", tenant_id)
        }
        "/v1/workflows/cancel" => {
            translate_json_object_with_tenant_id(body, "/Workflows/cancel", tenant_id)
        }
        "/v1/workflows/decide-review" => {
            translate_json_object_with_tenant_id(body, "/Workflows/decide_review", tenant_id)
        }
        "/v1/workflows/signal" => {
            translate_json_object_with_tenant_id(body, "/Workflows/signal", tenant_id)
        }
        _ => return None,
    };
    Some(translation)
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

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
}
