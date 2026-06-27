//! Public analytics, experiments, admin, lineage, and privacy route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;

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
        "/v1/analytics/session-stats" => translate_json_object_with_fields(
            body,
            "/Analytics/session_stats",
            "bad session stats body",
            "session stats body must be object",
            "serialize session stats body failed",
            [],
        ),
        "/v1/analytics/tenant-stats" => {
            translate_json_object_with_tenant_id(body, "/Analytics/tenant_stats", tenant_id)
        }
        "/v1/analytics/tool-stats" => {
            translate_json_object_with_tenant_id(body, "/Analytics/tool_stats", tenant_id)
        }
        "/v1/analytics/cache-stats" => {
            translate_json_object_with_tenant_id(body, "/Analytics/cache_stats", tenant_id)
        }
        "/v1/analytics/experiment-stats" => {
            translate_json_object_with_tenant_id(body, "/Analytics/experiment_stats", tenant_id)
        }
        "/v1/analytics/learning-candidates" => {
            translate_json_object_with_tenant_id(body, "/Analytics/learning_candidates", tenant_id)
        }
        "/v1/analytics/session-search" => {
            translate_json_object_with_tenant_id(body, "/Analytics/session_search", tenant_id)
        }
        "/v1/experiments/generate-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/generate_plan", tenant_id)
        }
        "/v1/experiments/run-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/run", tenant_id)
        }
        "/v1/experiments/status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/status", tenant_id)
        }
        "/v1/experiments/list" => {
            translate_json_object_with_tenant_id(body, "/Experiments/list", tenant_id)
        }
        "/v1/experiments/trials" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trials", tenant_id)
        }
        "/v1/experiments/trial-status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trial_status", tenant_id)
        }
        "/v1/experiments/cancel" => {
            translate_json_object_with_tenant_id(body, "/Experiments/cancel", tenant_id)
        }
        "/v1/experiments/propose-improvements" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/propose_improvements",
            tenant_id,
        ),
        "/v1/experiments/scores" => {
            translate_json_object_with_tenant_id(body, "/Experiments/scores", tenant_id)
        }
        "/v1/experiments/compare" => {
            translate_json_object_with_tenant_id(body, "/Experiments/compare", tenant_id)
        }
        "/v1/experiments/agent-revision-simulations" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/run_agent_revision_simulation",
            tenant_id,
        ),
        "/v1/experiments/agent-revision-simulations/compare" => {
            translate_json_object_with_tenant_id(
                body,
                "/Experiments/compare_agent_revision_simulation",
                tenant_id,
            )
        }
        "/v1/admin-maintenance/vector/promote" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/promote_tenant_vector",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/rollback-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/rollback_promotion",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/finalize-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/finalize_promotion",
            tenant_id,
        ),
        "/v1/admin-maintenance/checkpoints/create" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_create".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/list" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_list".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/rollback" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_rollback".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/cleanup" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_cleanup".to_string(),
            body: body.to_vec(),
        },
        "/v1/lineage/explain" => {
            translate_json_object_with_tenant_id(body, "/LineageAdmin/explain", tenant_id)
        }
        "/v1/lineage/query" => {
            translate_json_object_with_tenant_id(body, "/LineageAdmin/query", tenant_id)
        }
        "/v1/lineage/export" => {
            translate_json_object_with_tenant_id(body, "/LineageAdmin/export", tenant_id)
        }
        "/v1/lineage/verify" => {
            translate_json_object_with_tenant_id(body, "/LineageAdmin/verify", tenant_id)
        }
        "/v1/lineage/erase" => {
            translate_json_object_with_tenant_id(body, "/LineageAdmin/erase", tenant_id)
        }
        "/v1/privacy/export" => {
            translate_json_object_with_tenant_id(body, "/Privacy/export", tenant_id)
        }
        "/v1/privacy/erase" => {
            translate_json_object_with_tenant_id(body, "/Privacy/erase", tenant_id)
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
    fn analytics_public_routes_translate_to_restate_handlers() {
        // Pins: hosted analytics edge routes forward to the internal Analytics service paths.
        let cases = [
            (
                "/v1/analytics/session-stats",
                "/Analytics/session_stats",
                serde_json::json!({
                    "session_id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/analytics/tenant-stats",
                "/Analytics/tenant_stats",
                serde_json::json!({ "tenant_id": test_tenant_json(), "days": 14 }),
            ),
            (
                "/v1/analytics/tool-stats",
                "/Analytics/tool_stats",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                "/v1/analytics/cache-stats",
                "/Analytics/cache_stats",
                serde_json::json!({ "tenant_id": test_tenant_json(), "days": 14 }),
            ),
            (
                "/v1/analytics/experiment-stats",
                "/Analytics/experiment_stats",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "from_time": null,
                    "to_time": null,
                    "limit": 20
                }),
            ),
            (
                "/v1/analytics/learning-candidates",
                "/Analytics/learning_candidates",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "status": "proposed",
                    "limit": 20
                }),
            ),
            (
                "/v1/analytics/session-search",
                "/Analytics/session_search",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "query": "refresh token",
                    "from_time": null,
                    "to_time": null,
                    "event_types": ["user_message"],
                    "limit": 10
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if let Some(object) = input_body.as_object_mut() {
                object.remove("tenant_id");
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
    fn eval_public_routes_do_not_translate_to_product_handlers() {
        // Pins: hosted eval is not part of the default public product edge surface.
        let paths = [
            "/v1/evals/plan",
            "/v1/evals/suites/list",
            "/v1/evals/run",
            "/v1/evals/run-status",
            "/v1/evals/datasets/register",
            "/v1/evals/datasets/list",
            "/v1/evals/replay",
            "/v1/evals/scores",
            "/v1/evals/compare",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward {
                    method,
                    path,
                    body: _,
                } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
                }
            }
        }
    }

    #[test]
    fn experiments_public_routes_translate_to_restate_handlers() {
        // Pins: hosted experiment edge routes forward to the internal Experiments service paths.
        let cases = [
            (
                "/v1/experiments/generate-plan",
                "/Experiments/generate_plan",
            ),
            ("/v1/experiments/run-plan", "/Experiments/run"),
            ("/v1/experiments/status", "/Experiments/status"),
            ("/v1/experiments/list", "/Experiments/list"),
            ("/v1/experiments/trials", "/Experiments/trials"),
            ("/v1/experiments/trial-status", "/Experiments/trial_status"),
            ("/v1/experiments/cancel", "/Experiments/cancel"),
            (
                "/v1/experiments/propose-improvements",
                "/Experiments/propose_improvements",
            ),
            ("/v1/experiments/scores", "/Experiments/scores"),
            ("/v1/experiments/compare", "/Experiments/compare"),
            (
                "/v1/experiments/agent-revision-simulations",
                "/Experiments/run_agent_revision_simulation",
            ),
            (
                "/v1/experiments/agent-revision-simulations/compare",
                "/Experiments/compare_agent_revision_simulation",
            ),
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

    #[test]
    fn admin_maintenance_public_routes_translate_to_restate_handlers() {
        // Pins: hosted admin-maintenance routes forward to the internal AdminMaintenance service paths.
        let cases = [
            (
                "/v1/admin-maintenance/vector/promote",
                "/AdminMaintenance/promote_tenant_vector",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "target_backend": "turbopuffer",
                    "validate_percent": 5,
                    "dual_read_hours": 24
                }),
            ),
            (
                "/v1/admin-maintenance/vector/rollback-promotion",
                "/AdminMaintenance/rollback_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "rollback"
                }),
            ),
            (
                "/v1/admin-maintenance/vector/finalize-promotion",
                "/AdminMaintenance/finalize_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "finalize"
                }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/create",
                "/AdminMaintenance/checkpoint_create",
                serde_json::json!({ "label": "before-deploy", "session_id": null }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/list",
                "/AdminMaintenance/checkpoint_list",
                serde_json::json!({}),
            ),
            (
                "/v1/admin-maintenance/checkpoints/rollback",
                "/AdminMaintenance/checkpoint_rollback",
                serde_json::json!({ "id": "br-checkpoint" }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/cleanup",
                "/AdminMaintenance/checkpoint_cleanup",
                serde_json::json!({}),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if public_path.contains("/vector/")
                && let Some(object) = input_body.as_object_mut()
            {
                object.remove("tenant_id");
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
                    if public_path.contains("/vector/") {
                        assert_eq!(forwarded, expected_body, "{public_path} body changed");
                    } else {
                        assert_eq!(
                            forwarded, input_body,
                            "{public_path} body should pass through"
                        );
                    }
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
    fn lineage_and_privacy_public_routes_translate_to_restate_handlers() {
        // Pins: hosted lineage/privacy edge routes forward to internal Restate service paths.
        let cases = [
            (
                "/v1/lineage/explain",
                "/LineageAdmin/explain",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "id": "11111111-1111-1111-1111-111111111111"
                }),
            ),
            (
                "/v1/lineage/query",
                "/LineageAdmin/query",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sql": "SELECT count(*) FROM lineage",
                    "since": "24 hours"
                }),
            ),
            (
                "/v1/lineage/export",
                "/LineageAdmin/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject": "subject-a"
                }),
            ),
            (
                "/v1/lineage/verify",
                "/LineageAdmin/verify",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "window": "hot",
                    "since": "24 hours"
                }),
            ),
            (
                "/v1/lineage/erase",
                "/LineageAdmin/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject": "00ff"
                }),
            ),
            (
                "/v1/privacy/export",
                "/Privacy/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
            (
                "/v1/privacy/erase",
                "/Privacy/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
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
