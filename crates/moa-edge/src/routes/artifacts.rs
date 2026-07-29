//! Public artifact, skill, and learning-review route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::types::identifiers::TenantId;
use moa_execution::wire::ExecutionTemplateAdmissionRequest;

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
        "/v1/capabilities/list" => {
            translate_json_object_with_tenant_id(body, "/Execution/list_capabilities", tenant_id)
        }
        "/v1/execution-runs/start" => translate_execution_template_admission(body, tenant_id),
        "/v1/execution-runs/list" => {
            translate_json_object_with_tenant_id(body, "/Execution/list_runs", tenant_id)
        }
        "/v1/execution-runs/status" => {
            translate_json_object_with_tenant_id(body, "/Execution/status", tenant_id)
        }
        "/v1/execution-runs/tasks/list" => {
            translate_execution_run_nested(body, "/Execution/list_tasks", tenant_id)
        }
        "/v1/execution-runs/cancel" => {
            translate_execution_run_nested(body, "/Execution/cancel", tenant_id)
        }
        "/v1/execution-runs/decide-review" => {
            translate_json_object_with_tenant_id(body, "/Execution/decide_review", tenant_id)
        }
        "/v1/execution-runs/signal" => {
            translate_json_object_with_tenant_id(body, "/Execution/deliver_signal", tenant_id)
        }
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
        // Rollback acceptance is its own route, not an `action` on the accept
        // route: it archives a serving revision, so routing it by a field in a
        // caller-supplied body would put a destructive operation one typo away
        // from a draft promotion. The service still re-checks the proposal kind.
        "/v1/learning-candidates/accept-rollback" => translate_json_object_with_fields(
            body,
            "/LearningReview/accept_rollback",
            "bad tenant route body",
            "tenant route body must be object",
            "serialize tenant route body failed",
            [
                tenant_id_field(tenant_id),
                ("action", serde_json::json!("accept")),
                ("reviewer_subject", serde_json::json!("edge")),
            ],
        ),
        "/v1/learning-candidates/dismiss" => translate_json_object_with_fields(
            body,
            "/LearningReview/dismiss",
            "bad tenant route body",
            "tenant route body must be object",
            "serialize tenant route body failed",
            [
                tenant_id_field(tenant_id),
                ("action", serde_json::json!("dismiss")),
                ("reviewer_subject", serde_json::json!("edge")),
            ],
        ),
        _ => return None,
    };
    Some(translation)
}

fn translate_execution_template_admission(body: &Bytes, tenant_id: TenantId) -> RouteTranslation {
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad tenant route body"),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest("tenant route body must be object");
    };
    object.insert("tenant_id".to_string(), serde_json::json!(tenant_id));
    let request = match serde_json::from_value::<ExecutionTemplateAdmissionRequest>(value) {
        Ok(request) => request,
        Err(_) => return RouteTranslation::BadRequest("invalid execution admission request"),
    };
    let path = format!("/Session/{}/admit_execution_template", request.session_id);
    match serde_json::to_vec(&request) {
        Ok(body) => RouteTranslation::Forward {
            method: Method::POST,
            path,
            body,
        },
        Err(_) => RouteTranslation::BadRequest("serialize tenant route body failed"),
    }
}

fn translate_execution_run_nested(
    body: &Bytes,
    path: &'static str,
    tenant_id: TenantId,
) -> RouteTranslation {
    let mut value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad tenant route body"),
    };
    let Some(object) = value.as_object_mut() else {
        return RouteTranslation::BadRequest("tenant route body must be object");
    };
    let Some(run) = object
        .get_mut("run")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return RouteTranslation::BadRequest("execution request requires run object");
    };
    run.insert("tenant_id".to_string(), serde_json::json!(tenant_id));
    match serde_json::to_vec(&value) {
        Ok(body) => RouteTranslation::Forward {
            method: Method::POST,
            path: path.to_string(),
            body,
        },
        Err(_) => RouteTranslation::BadRequest("serialize tenant route body failed"),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, test_tenant_scope_json, translate};

    #[test]
    fn skills_and_capabilities_public_routes_translate_to_restate_handlers() {
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
            (
                "/v1/capabilities/list",
                "/Execution/list_capabilities",
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
    fn legacy_execution_lifecycle_routes_removed() {
        // Pins: retired skill-lifecycle REST paths have no redirects or aliases after
        // the execution cutover, while exact pinned-template admission is Session-owned.
        for suffix in [
            "run",
            "status",
            "runs/list",
            "cancel",
            "signal",
            "decide-review",
        ] {
            let public_path = format!("/v1/skills/{suffix}");
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            assert_eq!(
                translate(&Method::POST, &uri, &Bytes::from_static(br#"{}"#)),
                RouteTranslation::NotFound,
                "{public_path} must not remain as an alias"
            );
        }

        let session_id = "11111111-1111-1111-1111-111111111111";
        let uri = "/v1/execution-runs/start"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from(
            serde_json::json!({
                "contact_id": null,
                "session_id": session_id,
                "template": {
                    "skill_ref": "skill://triage",
                    "revision_uid": "22222222-2222-2222-2222-222222222222"
                },
                "objective": "Triage the incident",
                "input": {"severity": "high"},
                "idempotency_key": "incident-42"
            })
            .to_string(),
        );
        let RouteTranslation::Forward {
            method,
            path,
            body: forwarded_body,
        } = translate(&Method::POST, &uri, &body)
        else {
            panic!("execution start should translate to Session admission");
        };
        assert_eq!(method, Method::POST);
        assert_eq!(
            path,
            format!("/Session/{session_id}/admit_execution_template")
        );
        let forwarded: serde_json::Value =
            serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
        assert_eq!(forwarded["tenant_id"], test_tenant_json());
        assert_eq!(forwarded["session_id"], serde_json::json!(session_id));

        let forbidden_fields = [
            ["compiled", "plan", "id"].join("_"),
            ["raw", "plan"].join("_"),
            "plan".to_string(),
        ];
        for forbidden in forbidden_fields {
            let mut invalid = serde_json::from_slice::<serde_json::Value>(&body)
                .expect("admission fixture is JSON");
            invalid[&forbidden] = serde_json::json!({"nodes": []});
            assert_eq!(
                translate(&Method::POST, &uri, &Bytes::from(invalid.to_string())),
                RouteTranslation::BadRequest("invalid execution admission request"),
                "execution admission must reject caller-supplied {forbidden}"
            );
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
            // Rollback and dismiss reach their OWN handlers. If either collapsed
            // onto accept_skill, accepting a rollback would run the draft-publish
            // path against a revision nobody proposed publishing.
            (
                "/v1/learning-candidates/accept-rollback",
                "/LearningReview/accept_rollback",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "accept",
                    "reviewer_subject": "edge"
                }),
            ),
            (
                "/v1/learning-candidates/dismiss",
                "/LearningReview/dismiss",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "candidate_id": "11111111-1111-1111-1111-111111111111",
                    "action": "dismiss",
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
