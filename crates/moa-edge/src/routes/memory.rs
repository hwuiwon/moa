//! Public memory route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::types::identifiers::TenantId;

use super::{
    RouteTranslation, translate_json_object_with_fields, translate_json_object_with_tenant_id,
};

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
        "/v1/memory/search" => translate_session_memory_request(body, "/Memory/search"),
        "/v1/memory/show" => translate_session_memory_request(body, "/Memory/show"),
        "/v1/memory/ingest-documents" => {
            translate_json_object_with_tenant_id(body, "/Memory/ingest_documents", tenant_id)
        }
        "/v1/memory/retrieve-debug" => {
            translate_session_memory_request(body, "/Memory/retrieve_debug")
        }
        _ => return None,
    };
    Some(translation)
}

fn translate_session_memory_request(body: &Bytes, target: &str) -> RouteTranslation {
    translate_json_object_with_fields(
        body,
        target,
        "bad memory route body",
        "memory route body must be object",
        "serialize memory route body failed",
        [],
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

    #[test]
    fn unsupported_index_rebuild_routes_are_absent() {
        // Pins: the edge must not advertise a model-switch workflow whose
        // retrieval-side embedding provider cannot follow the active generation.
        for path in [
            "/v1/memory/index-rebuild/start",
            "/v1/memory/index-rebuild/status",
            "/v1/memory/index-rebuild/cancel",
            "/v1/memory/index-rebuild/rollback",
            "/v1/memory/index-rebuild/finalize",
        ] {
            let uri = path.parse::<Uri>().expect("route path should parse");
            assert!(
                super::translate(
                    &Method::POST,
                    &uri,
                    &Bytes::from_static(b"{}"),
                    moa_core::types::identifiers::TenantId::new(),
                )
                .is_none(),
                "{path} must remain unexposed until model-aware retrieval exists"
            );
        }
    }

    #[test]
    fn memory_public_routes_translate_to_restate_handlers() {
        // Pins: hosted memory edge routes forward to the internal Memory service paths.
        let cases = [
            (
                "/v1/memory/search",
                "/Memory/search",
                serde_json::json!({
                    "session_id": "11111111-1111-1111-1111-111111111111",
                    "query": "auth",
                    "limit": 10
                }),
            ),
            (
                "/v1/memory/show",
                "/Memory/show",
                serde_json::json!({
                    "session_id": "11111111-1111-1111-1111-111111111111",
                    "uid": "22222222-2222-2222-2222-222222222222"
                }),
            ),
            (
                "/v1/memory/ingest-documents",
                "/Memory/ingest_documents",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "documents": [{"source_name": "Auth", "content": "Fact: auth uses JWT"}]
                }),
            ),
            (
                "/v1/memory/retrieve-debug",
                "/Memory/retrieve_debug",
                serde_json::json!({
                    "session_id": "11111111-1111-1111-1111-111111111111",
                    "query": "auth",
                    "limit": 5,
                    "no_flush_wait": true
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            let body = Bytes::from(input_body.to_string());

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
}
