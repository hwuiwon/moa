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
        // Index-rebuild control. The tenant is stamped server-side from the
        // authenticated caller rather than read from the body, and every one of
        // these handlers re-checks tenant-admin authority before it touches
        // rebuild state.
        "/v1/memory/index-rebuild/start" => translate_json_object_with_tenant_id(
            body,
            "/GraphMemoryMaint/start_index_rebuild",
            tenant_id,
        ),
        "/v1/memory/index-rebuild/status" => translate_json_object_with_tenant_id(
            body,
            "/GraphMemoryMaint/index_rebuild_status",
            tenant_id,
        ),
        "/v1/memory/index-rebuild/cancel" => translate_json_object_with_tenant_id(
            body,
            "/GraphMemoryMaint/cancel_index_rebuild",
            tenant_id,
        ),
        "/v1/memory/index-rebuild/rollback" => translate_json_object_with_tenant_id(
            body,
            "/GraphMemoryMaint/rollback_index_rebuild",
            tenant_id,
        ),
        "/v1/memory/index-rebuild/finalize" => translate_json_object_with_tenant_id(
            body,
            "/GraphMemoryMaint/finalize_index_rebuild",
            tenant_id,
        ),
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
    fn index_rebuild_routes_stamp_the_tenant_server_side() {
        // Pins: a caller cannot name the tenant whose index is rebuilt. The
        // edge overwrites `tenant_id` from the authenticated request, so a
        // forged body cannot start, inspect, cancel, roll back, or finalize a
        // rebuild in someone else's tenant.
        let cases = [
            (
                "/v1/memory/index-rebuild/start",
                "/GraphMemoryMaint/start_index_rebuild",
            ),
            (
                "/v1/memory/index-rebuild/status",
                "/GraphMemoryMaint/index_rebuild_status",
            ),
            (
                "/v1/memory/index-rebuild/cancel",
                "/GraphMemoryMaint/cancel_index_rebuild",
            ),
            (
                "/v1/memory/index-rebuild/rollback",
                "/GraphMemoryMaint/rollback_index_rebuild",
            ),
            (
                "/v1/memory/index-rebuild/finalize",
                "/GraphMemoryMaint/finalize_index_rebuild",
            ),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from(
                serde_json::json!({
                    "tenant_id": "99999999-9999-9999-9999-999999999999",
                    "kind": "reembed",
                    "operation_uid": "33333333-3333-3333-3333-333333333333"
                })
                .to_string(),
            );

            match translate(&Method::POST, &uri, &body) {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(
                        forwarded.get("tenant_id"),
                        Some(&test_tenant_json()),
                        "{public_path} must overwrite a caller-supplied tenant"
                    );
                }
                other => panic!("{public_path} should forward, got {other:?}"),
            }
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
