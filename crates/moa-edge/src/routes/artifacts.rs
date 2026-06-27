//! Public artifact, skill, and learning-review route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;

use super::{
    RouteTranslation, tenant_id_field, translate_json_object_with_fields,
    translate_json_object_with_tenant_id, translate_json_object_with_tenant_scope,
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
        "/v1/skills/export" => {
            translate_json_object_with_tenant_id(body, "/Skills/export", tenant_id)
        }
        "/v1/skills/import" => {
            translate_json_object_with_tenant_scope(body, "/Skills/import", tenant_id)
        }
        "/v1/skills/list" => translate_json_object_with_tenant_id(body, "/Skills/list", tenant_id),
        "/v1/artifacts/import" => {
            translate_json_object_with_tenant_scope(body, "/Artifacts/import", tenant_id)
        }
        "/v1/artifacts/export" => {
            translate_json_object_with_tenant_id(body, "/Artifacts/export", tenant_id)
        }
        "/v1/artifacts/list" => {
            translate_json_object_with_tenant_id(body, "/Artifacts/list", tenant_id)
        }
        "/v1/artifacts/validate" => {
            translate_json_object_with_tenant_id(body, "/Artifacts/validate", tenant_id)
        }
        "/v1/artifacts/publish" => {
            translate_json_object_with_tenant_scope(body, "/Artifacts/publish", tenant_id)
        }
        "/v1/learning-candidates/get" => {
            translate_json_object_with_tenant_id(body, "/LearningReview/get", tenant_id)
        }
        "/v1/learning-candidates/accept-skill" => translate_json_object_with_fields(
            body,
            "/LearningReview/accept_skill",
            "bad tenant route body",
            "tenant route body must be object",
            "serialize tenant route body failed",
            [
                tenant_id_field(tenant_id),
                ("action", serde_json::json!("accept")),
                ("reviewer_subject", serde_json::json!("edge")),
            ],
        ),
        "/v1/learning-candidates/reject" => translate_json_object_with_fields(
            body,
            "/LearningReview/reject",
            "bad tenant route body",
            "tenant route body must be object",
            "serialize tenant route body failed",
            [
                tenant_id_field(tenant_id),
                ("action", serde_json::json!("reject")),
                ("reviewer_subject", serde_json::json!("edge")),
            ],
        ),
        _ => return None,
    };
    Some(translation)
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, test_tenant_scope_json, translate};

    #[test]
    fn skills_public_routes_translate_to_restate_handlers() {
        // Pins: hosted skills edge routes forward to the internal Skills service paths.
        let cases = [
            (
                "/v1/skills/export",
                "/Skills/export",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/skills/import",
                "/Skills/import",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "packages": []
                }),
            ),
            (
                "/v1/skills/list",
                "/Skills/list",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            if public_path == "/v1/skills/import" {
                object.remove("scope");
            }
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

    #[test]
    fn artifact_public_routes_translate_to_restate_handlers() {
        // Pins: hosted artifact edge routes forward to the internal Artifacts service paths.
        let cases = [
            (
                "/v1/artifacts/import",
                "/Artifacts/import",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "source_format": "json",
                    "source_text": "{}"
                }),
            ),
            (
                "/v1/artifacts/export",
                "/Artifacts/export",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/artifacts/list",
                "/Artifacts/list",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/artifacts/validate",
                "/Artifacts/validate",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "source_format": "json",
                    "source_text": "{}"
                }),
            ),
            (
                "/v1/artifacts/publish",
                "/Artifacts/publish",
                serde_json::json!({
                    "scope": test_tenant_scope_json(),
                    "revision_uid": "11111111-1111-1111-1111-111111111111"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            object.remove("scope");
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

    #[test]
    fn learning_candidate_public_routes_translate_to_restate_handlers() {
        // Pins: hosted learning-review edge routes forward to the internal LearningReview service paths.
        let cases = [
            (
                "/v1/learning-candidates/get",
                "/LearningReview/get",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/learning-candidates/accept-skill",
                "/LearningReview/accept_skill",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "accept",
                    "reviewer_subject": "edge"
                }),
            ),
            (
                "/v1/learning-candidates/reject",
                "/LearningReview/reject",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "reject",
                    "reviewer_subject": "edge"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body =
                Bytes::from_static(br#"{"candidate_id":"11111111-1111-1111-1111-111111111111"}"#);

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
