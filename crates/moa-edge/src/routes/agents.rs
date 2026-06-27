//! Public agent and configured-agent route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;
use uuid::Uuid;

use super::{
    RouteTranslation, tenant_id_field, translate_agent_act_as,
    translate_empty_json_body_with_fields, translate_json_object_with_fields, translate_uuid_path,
};

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if let Some(translation) = translate_configured_agent_route(method, uri, body, tenant_id) {
        return Some(translation);
    }
    if *method == Method::POST && uri.path() == "/v1/agents" {
        return Some(RouteTranslation::Forward {
            method: Method::POST,
            path: "/Agents/register".to_string(),
            body: body.to_vec(),
        });
    }
    if *method == Method::GET && uri.path() == "/v1/agents" {
        return Some(RouteTranslation::Forward {
            method: Method::POST,
            path: "/Agents/list".to_string(),
            body: Vec::new(),
        });
    }
    if let Some(rest) = uri.path().strip_prefix("/v1/agents/") {
        if *method == Method::GET {
            return Some(translate_uuid_path(rest, "/Agents/get"));
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/deactivate")
        {
            return Some(translate_uuid_path(id, "/Agents/deactivate"));
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/can-act-as")
        {
            return Some(translate_agent_act_as(id, body, "/Agents/grant_can_act_as"));
        }
        if *method == Method::POST
            && let Some(id) = rest.strip_suffix("/revoke-can-act-as")
        {
            return Some(translate_agent_act_as(
                id,
                body,
                "/Agents/revoke_can_act_as",
            ));
        }
    }
    None
}

fn translate_configured_agent_route(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    let rest = uri.path().strip_prefix("/v1/")?;
    let mut segments = rest.split('/');

    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some("agent-definitions"), None, None, None) if *method == Method::GET => {
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_definitions",
                "bad agent definitions list body",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::GET => {
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_installations",
                "bad agent installations list body",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), None, None, None) if *method == Method::POST => {
            Some(translate_json_object_with_fields(
                body,
                "/AgentDefinitions/install",
                "bad agent install body",
                "agent install body must be object",
                "serialize agent install body failed",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-installations"), Some(installation_uid), Some("deployments"), None)
            if *method == Method::GET =>
        {
            let installation_uid = match Uuid::parse_str(installation_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent installation id")),
            };
            Some(translate_empty_json_body_with_fields(
                "/AgentDefinitions/list_deployments",
                "bad agent deployments list body",
                [
                    tenant_id_field(tenant_id),
                    ("installation_uid", serde_json::json!(installation_uid)),
                ],
            ))
        }
        (Some("agent-installations"), Some(installation_uid), Some("deployments"), None)
            if *method == Method::POST =>
        {
            let installation_uid = match Uuid::parse_str(installation_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent installation id")),
            };
            Some(translate_json_object_with_fields(
                body,
                "/AgentDefinitions/deploy",
                "bad agent deploy body",
                "agent deploy body must be object",
                "serialize agent deploy body failed",
                [
                    tenant_id_field(tenant_id),
                    ("installation_uid", serde_json::json!(installation_uid)),
                ],
            ))
        }
        (Some("agent-simulations"), None, None, None) if *method == Method::POST => {
            Some(translate_json_object_with_fields(
                body,
                "/Experiments/run_agent_revision_simulation",
                "bad agent simulation body",
                "agent simulation body must be object",
                "serialize agent simulation body failed",
                [tenant_id_field(tenant_id)],
            ))
        }
        (Some("agent-simulations"), Some(run_uid), Some("compare"), None)
            if *method == Method::POST =>
        {
            let run_uid = match Uuid::parse_str(run_uid) {
                Ok(value) => value,
                Err(_) => return Some(RouteTranslation::BadRequest("bad agent simulation run id")),
            };
            Some(translate_json_object_with_fields(
                body,
                "/Experiments/compare_agent_revision_simulation",
                "bad agent simulation compare body",
                "agent simulation compare body must be object",
                "serialize agent simulation compare body failed",
                [
                    tenant_id_field(tenant_id),
                    ("run_uid", serde_json::json!(run_uid)),
                ],
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};
    use uuid::Uuid;

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

    #[test]
    fn configured_agent_public_routes_translate_to_restate_handlers() {
        // Pins: tenant-configurable agent product routes reach AgentDefinitions and simulation handlers.
        let installation_uid = "11111111-1111-1111-1111-111111111111";
        let revision_uid = "22222222-2222-2222-2222-222222222222";
        let run_uid = "33333333-3333-3333-3333-333333333333";
        let cases = vec![
            (
                Method::GET,
                "/v1/agent-definitions".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_definitions",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                Method::GET,
                "/v1/agent-installations".to_string(),
                Bytes::new(),
                "/AgentDefinitions/list_installations",
                serde_json::json!({ "tenant_id": test_tenant_json() }),
            ),
            (
                Method::POST,
                "/v1/agent-installations".to_string(),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","metadata":{{"tier":"gold"}}}}"#
                )),
                "/AgentDefinitions/install",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "revision_uid": revision_uid,
                    "metadata": { "tier": "gold" }
                }),
            ),
            (
                Method::GET,
                format!("/v1/agent-installations/{installation_uid}/deployments"),
                Bytes::new(),
                "/AgentDefinitions/list_deployments",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "installation_uid": installation_uid
                }),
            ),
            (
                Method::POST,
                format!("/v1/agent-installations/{installation_uid}/deployments"),
                Bytes::from(format!(
                    r#"{{"revision_uid":"{revision_uid}","reason":"candidate passed"}}"#
                )),
                "/AgentDefinitions/deploy",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "installation_uid": installation_uid,
                    "revision_uid": revision_uid,
                    "reason": "candidate passed"
                }),
            ),
            (
                Method::POST,
                "/v1/agent-simulations".to_string(),
                Bytes::from(format!(
                    r#"{{"name":"compare support","plan_revision_uid":"{revision_uid}","base":{{"variant_key":"base","revision_uid":"{revision_uid}"}}}}"#
                )),
                "/Experiments/run_agent_revision_simulation",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "name": "compare support",
                    "plan_revision_uid": revision_uid,
                    "base": {
                        "variant_key": "base",
                        "revision_uid": revision_uid
                    }
                }),
            ),
            (
                Method::POST,
                format!("/v1/agent-simulations/{run_uid}/compare"),
                Bytes::from_static(
                    br#"{"base_variant_key":"base","candidate_variant_keys":["candidate"]}"#,
                ),
                "/Experiments/compare_agent_revision_simulation",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "run_uid": run_uid,
                    "base_variant_key": "base",
                    "candidate_variant_keys": ["candidate"]
                }),
            ),
        ];

        for (method, public_path, body, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");

            let translation = translate(&method, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method: forwarded_method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(
                        forwarded_method,
                        Method::POST,
                        "{public_path} must use POST"
                    );
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
    fn authenticated_agent_session_route_translates_to_session_store() {
        // Pins: authenticated configured-agent sessions use SessionStore, not the contact session route.
        let uri = "/v1/sessions/create-agent"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"meta":{},"agent":{}}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/SessionStore/create_agent_session");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&forwarded_body).expect("forwarded body should be JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "meta": {
                            "tenant_id": test_tenant_json()
                        },
                        "agent": {}
                    })
                );
            }
            RouteTranslation::NoChange => panic!("agent session route should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent session route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn agent_public_routes_translate_to_restate_handlers() {
        // Pins: public agent lifecycle routes forward to the remaining Agents service.
        let body = Bytes::from_static(br#"{"display_name":"reviewer"}"#);
        let create_uri = "/v1/agents"
            .parse::<Uri>()
            .expect("agent register path should parse");
        match translate(&Method::POST, &create_uri, &body) {
            RouteTranslation::Forward {
                method,
                path,
                body: forwarded_body,
            } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Agents/register");
                assert_eq!(forwarded_body, body.to_vec());
            }
            RouteTranslation::NoChange => panic!("agent register should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent register should not fail translation: {message}")
            }
        }

        let list_uri = "/v1/agents"
            .parse::<Uri>()
            .expect("agent list path should parse");
        match translate(&Method::GET, &list_uri, &Bytes::new()) {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Agents/list");
                assert!(body.is_empty(), "agent list should not synthesize a body");
            }
            RouteTranslation::NoChange => panic!("agent list should translate"),
            RouteTranslation::BadRequest(message) => {
                panic!("agent list should not fail translation: {message}")
            }
        }

        let agent_id = "11111111-1111-1111-1111-111111111111";
        let uuid_cases = [
            (Method::GET, format!("/v1/agents/{agent_id}"), "/Agents/get"),
            (
                Method::POST,
                format!("/v1/agents/{agent_id}/deactivate"),
                "/Agents/deactivate",
            ),
        ];
        for (method, public_path, internal_path) in uuid_cases {
            let uri = public_path.parse::<Uri>().expect("agent path should parse");
            match translate(&method, &uri, &Bytes::new()) {
                RouteTranslation::Forward { method, path, body } => {
                    assert_eq!(method, Method::POST);
                    assert_eq!(path, internal_path);
                    let forwarded: Uuid =
                        serde_json::from_slice(&body).expect("forwarded UUID should parse");
                    assert_eq!(
                        forwarded,
                        Uuid::parse_str(agent_id).expect("agent fixture UUID should parse")
                    );
                }
                RouteTranslation::NoChange => panic!("{public_path} should translate"),
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }

        let user_id = "22222222-2222-2222-2222-222222222222";
        let act_as_body = Bytes::from(format!(r#"{{"user_id":"{user_id}"}}"#));
        let act_as_cases = [
            (
                format!("/v1/agents/{agent_id}/can-act-as"),
                "/Agents/grant_can_act_as",
            ),
            (
                format!("/v1/agents/{agent_id}/revoke-can-act-as"),
                "/Agents/revoke_can_act_as",
            ),
        ];
        for (public_path, internal_path) in act_as_cases {
            let uri = public_path.parse::<Uri>().expect("agent path should parse");
            match translate(&Method::POST, &uri, &act_as_body) {
                RouteTranslation::Forward { method, path, body } => {
                    assert_eq!(method, Method::POST);
                    assert_eq!(path, internal_path);
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&body).expect("forwarded act-as body should parse");
                    assert_eq!(
                        forwarded,
                        serde_json::json!({
                            "agent_id": agent_id,
                            "user_id": user_id
                        })
                    );
                }
                RouteTranslation::NoChange => panic!("{public_path} should translate"),
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }
}
