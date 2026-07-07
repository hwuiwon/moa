//! Public auth, approval, and contact-token route translation.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::TenantId;
use uuid::Uuid;

use super::{
    RouteTranslation, tenant_id_field, translate_empty_json_body_with_fields,
    translate_json_object_with_fields,
};

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method == Method::GET && uri.path() == "/v1/authz-challenges" {
        return Some(RouteTranslation::Forward {
            method: Method::POST,
            path: "/AuthzChallenges/list_mine".to_string(),
            body: Vec::new(),
        });
    }
    if *method == Method::POST
        && let Some(id) = uri
            .path()
            .strip_prefix("/v1/authz-challenges/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let challenge_id = match Uuid::parse_str(id) {
            Ok(value) => value,
            Err(_) => return Some(RouteTranslation::BadRequest("bad authz challenge id")),
        };
        return Some(translate_json_object_with_fields(
            body,
            "/AuthzChallenges/decide",
            "bad decision body",
            "decision body must be object",
            "serialize authz challenge decision body failed",
            [("id", serde_json::json!(challenge_id))],
        ));
    }
    if *method == Method::GET && uri.path() == "/v1/action-reviews" {
        return Some(translate_empty_json_body_with_fields(
            "/ActionReviews/list_pending",
            "bad action review list body",
            [tenant_id_field(tenant_id)],
        ));
    }
    if *method == Method::POST
        && let Some(rest) = uri
            .path()
            .strip_prefix("/v1/action-reviews/")
            .and_then(|rest| rest.strip_suffix("/decision"))
    {
        let review_id = match Uuid::parse_str(rest) {
            Ok(value) => value,
            Err(_) => return Some(RouteTranslation::BadRequest("bad action review id")),
        };
        return Some(translate_json_object_with_fields(
            body,
            "/ActionReviews/decide",
            "bad action review decision body",
            "action review decision body must be object",
            "serialize action review decision body failed",
            [
                tenant_id_field(tenant_id),
                ("review_id", serde_json::json!(review_id)),
            ],
        ));
    }
    if *method == Method::POST && uri.path() == "/v1/contacts/tokens" {
        return Some(translate_json_object_with_fields(
            body,
            "/Contacts/issue_token",
            "bad contact token body",
            "contact token body must be object",
            "serialize contact token body failed",
            [tenant_id_field(tenant_id)],
        ));
    }
    if *method == Method::POST && uri.path() == "/v1/authz/tuple-write" {
        return Some(RouteTranslation::BadRequest(
            "raw authz tuple writes are not supported",
        ));
    }
    if *method == Method::POST && uri.path() == "/v1/authz/api-key-tenant-roles" {
        return Some(translate_api_key_tenant_role(body));
    }
    None
}

fn translate_api_key_tenant_role(body: &Bytes) -> RouteTranslation {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad API-key tenant-role body"),
    };
    let Some(object) = value.as_object() else {
        return RouteTranslation::BadRequest("API-key tenant-role body must be object");
    };
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "operation" | "api_key_id" | "tenant_id" | "relation"
        ) {
            return RouteTranslation::BadRequest("unsupported API-key tenant-role field");
        }
    }
    match object.get("operation").and_then(serde_json::Value::as_str) {
        Some("grant_api_key_tenant_role" | "revoke_api_key_tenant_role") => {}
        _ => return RouteTranslation::BadRequest("unsupported API-key tenant-role operation"),
    }
    match object.get("relation").and_then(serde_json::Value::as_str) {
        Some("admin" | "operator") => {}
        _ => return RouteTranslation::BadRequest("unsupported API-key tenant-role relation"),
    }
    for field in ["api_key_id", "tenant_id"] {
        let Some(value) = object.get(field).and_then(serde_json::Value::as_str) else {
            return RouteTranslation::BadRequest("API-key tenant-role id must be a UUID");
        };
        if Uuid::parse_str(value).is_err() {
            return RouteTranslation::BadRequest("API-key tenant-role id must be a UUID");
        }
    }
    RouteTranslation::Forward {
        method: Method::POST,
        path: "/Authz/write_tuple".to_string(),
        body: body.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

    #[test]
    fn contact_token_route_translates_to_contacts_service() {
        // Pins: contact token issuance derives the tenant from authenticated identity, not a workspace path.
        let uri = "/v1/contacts/tokens"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(br#"{"display_name":"Ada"}"#);

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Contacts/issue_token");
                let value: serde_json::Value =
                    serde_json::from_slice(&body).expect("translated body should be json");
                assert_eq!(
                    value,
                    serde_json::json!({
                        "display_name": "Ada",
                        "tenant_id": test_tenant_json()
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("contact token route should translate to Contacts service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("contact token route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn action_review_public_routes_translate_to_restate_handlers() {
        // Pins: tenant-admin action-review routes forward to the internal ActionReviews service.
        let list_uri = "/v1/action-reviews"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/list_pending");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("list body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({ "tenant_id": test_tenant_json() })
                );
            }
            RouteTranslation::NoChange => {
                panic!("action review list should translate to ActionReviews service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("action review list should not fail translation: {message}")
            }
        }

        let decision_uri = "/v1/action-reviews/11111111-1111-1111-1111-111111111111/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"decision":"cleared","reason":null}"#);
        let decision_translation = translate(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/ActionReviews/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "tenant_id": test_tenant_json(),
                        "review_id": "11111111-1111-1111-1111-111111111111",
                        "decision": "cleared",
                        "reason": null
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("action review decision should translate to ActionReviews service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("action review decision should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn authz_challenge_public_routes_translate_to_restate_handlers() {
        // Pins: builtin async-authz challenge routes stay separate from action reviews.
        let list_uri = "/v1/authz-challenges"
            .parse::<Uri>()
            .expect("route path should parse");
        let list_translation = translate(&Method::GET, &list_uri, &Bytes::new());
        match list_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/AuthzChallenges/list_mine");
                assert!(
                    body.is_empty(),
                    "authz challenge list should not synthesize a request body"
                );
            }
            RouteTranslation::NoChange => {
                panic!("authz challenge list should translate to AuthzChallenges service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("authz challenge list should not fail translation: {message}")
            }
        }

        let decision_uri = "/v1/authz-challenges/22222222-2222-2222-2222-222222222222/decision"
            .parse::<Uri>()
            .expect("route path should parse");
        let decision_body = Bytes::from_static(br#"{"outcome":"approved","reason":null}"#);
        let decision_translation = translate(&Method::POST, &decision_uri, &decision_body);
        match decision_translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/AuthzChallenges/decide");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("decision body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "id": "22222222-2222-2222-2222-222222222222",
                        "outcome": "approved",
                        "reason": null
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("authz challenge decision should translate to AuthzChallenges service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("authz challenge decision should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn raw_authz_tuple_route_fails_closed() {
        // Pins: public HTTP no longer exposes arbitrary OpenFGA tuple strings.
        let uri = "/v1/authz/tuple-write"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(
            br#"{"user":"user:11111111-1111-1111-1111-111111111111","relation":"participant","object":"session:22222222-2222-2222-2222-222222222222"}"#,
        );

        let translation = translate(&Method::POST, &uri, &body);

        assert_eq!(
            translation,
            RouteTranslation::BadRequest("raw authz tuple writes are not supported")
        );
    }

    #[test]
    fn api_key_tenant_role_route_translates_typed_body() {
        // Pins: the public authz route supports only typed API-key tenant role writes.
        let uri = "/v1/authz/api-key-tenant-roles"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(
            br#"{"operation":"grant_api_key_tenant_role","api_key_id":"11111111-1111-1111-1111-111111111111","tenant_id":"22222222-2222-2222-2222-222222222222","relation":"admin"}"#,
        );

        let translation = translate(&Method::POST, &uri, &body);

        match translation {
            RouteTranslation::Forward { method, path, body } => {
                assert_eq!(method, Method::POST);
                assert_eq!(path, "/Authz/write_tuple");
                let forwarded: serde_json::Value =
                    serde_json::from_slice(&body).expect("forwarded body should be valid JSON");
                assert_eq!(
                    forwarded,
                    serde_json::json!({
                        "operation": "grant_api_key_tenant_role",
                        "api_key_id": "11111111-1111-1111-1111-111111111111",
                        "tenant_id": "22222222-2222-2222-2222-222222222222",
                        "relation": "admin"
                    })
                );
            }
            RouteTranslation::NoChange => {
                panic!("API-key tenant-role route should translate to Authz service")
            }
            RouteTranslation::BadRequest(message) => {
                panic!("API-key tenant-role route should not fail translation: {message}")
            }
        }
    }

    #[test]
    fn api_key_tenant_role_route_rejects_raw_tuple_shape() {
        // Pins: stale `user:<id>` subject bodies cannot be smuggled through the typed route.
        let uri = "/v1/authz/api-key-tenant-roles"
            .parse::<Uri>()
            .expect("route path should parse");
        let body = Bytes::from_static(
            br#"{"user":"user:11111111-1111-1111-1111-111111111111","relation":"admin","object":"tenant:22222222-2222-2222-2222-222222222222"}"#,
        );

        let translation = translate(&Method::POST, &uri, &body);

        assert_eq!(
            translation,
            RouteTranslation::BadRequest("unsupported API-key tenant-role field")
        );
    }
}
