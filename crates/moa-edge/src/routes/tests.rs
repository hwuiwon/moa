//! Behavior tests for public edge route translation and request credential extraction.

use async_trait::async_trait;
use axum::http::header::{AUTHORIZATION, COOKIE};
use moa_core::traits::{AuthError, Identity, IdentityType};
use moa_core::types::identifiers::TenantId;

use super::*;

struct StrictAuth;

#[async_trait]
impl AuthProvider for StrictAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Err(AuthError::Rejected)
    }

    fn name(&self) -> &'static str {
        "strict"
    }
}

struct DisabledAuth;

#[async_trait]
impl AuthProvider for DisabledAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Ok(Identity {
            identity_type: IdentityType::Service,
            id: Uuid::nil(),
            tenant_id: TenantId::from(Uuid::nil()),
            api_key_id: None,
            acting_on_behalf_of: None,
        })
    }

    fn name(&self) -> &'static str {
        "disabled"
    }

    fn requires_credentials(&self) -> bool {
        false
    }
}

#[test]
fn strict_auth_requires_authorization_header() {
    // Pins: normal auth providers still reject requests before authentication when no credential is present.
    let headers = HeaderMap::new();

    assert!(credential_for_request(&StrictAuth, &headers).is_none());
}

#[test]
fn disabled_auth_allows_missing_authorization_header() {
    // Pins: auth.provider=disabled can pass through edge requests with no Authorization header.
    let headers = HeaderMap::new();

    assert_eq!(
        credential_for_request(&DisabledAuth, &headers),
        Some(Credential::ApiKey(String::new()))
    );
}

#[test]
fn authorization_header_wins_when_disabled_auth_is_configured() {
    // Pins: disabled auth still forwards an explicitly supplied credential when present.
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        "Bearer moa_dev_example"
            .parse()
            .expect("test auth header should parse"),
    );

    assert_eq!(
        credential_for_request(&DisabledAuth, &headers),
        Some(Credential::ApiKey("moa_dev_example".to_string()))
    );
}

#[test]
fn user_session_bearer_token_is_not_treated_as_api_key_or_jwt() {
    // Pins: dashboard login tokens route to the local user-session auth path, not API-key or OIDC auth.
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        "Bearer user_session_example"
            .parse()
            .expect("test auth header should parse"),
    );

    assert_eq!(
        credential_for_request(&StrictAuth, &headers),
        Some(Credential::UserSessionToken(
            "user_session_example".to_string()
        ))
    );
}

#[test]
fn user_session_cookie_is_accepted_when_authorization_header_is_missing() {
    // Pins: browser dashboard requests can authenticate with the HttpOnly session cookie alone.
    let mut headers = HeaderMap::new();
    headers.insert(
        COOKIE,
        "__Host-user_session=user_session_example; other=value"
            .parse()
            .expect("test cookie header should parse"),
    );

    assert_eq!(
        credential_for_request(&StrictAuth, &headers),
        Some(Credential::UserSessionToken(
            "user_session_example".to_string()
        ))
    );
}

#[test]
fn authorization_header_wins_over_session_cookie() {
    // Pins: API clients can still send explicit bearer credentials without being shadowed by a browser cookie.
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        "Bearer moa_dev_example"
            .parse()
            .expect("test auth header should parse"),
    );
    headers.insert(
        COOKIE,
        "__Host-user_session=user_session_example"
            .parse()
            .expect("test cookie header should parse"),
    );

    assert_eq!(
        credential_for_request(&StrictAuth, &headers),
        Some(Credential::ApiKey("moa_dev_example".to_string()))
    );
}

#[test]
fn oauth_access_token_bearer_routes_to_opaque_oauth_credential_not_api_key() {
    // Pins: a moa_oauth_at_ bearer dispatches to the opaque OAuth resolver, not
    // the API-key path it shares the moa_ namespace with, nor the JWT path.
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        "Bearer moa_oauth_at_examplevalue"
            .parse()
            .expect("test auth header should parse"),
    );

    assert_eq!(
        credential_for_request(&StrictAuth, &headers),
        Some(Credential::OAuthAccessToken(
            "moa_oauth_at_examplevalue".to_string()
        ))
    );
}

#[test]
fn only_send_message_takes_the_tenant_flow_control_scope() {
    // Pins: posting a message starts a turn and is enrolled in per-tenant admission
    // control, while contact-session reads and lifecycle calls (progress,
    // authorize_session, init_session) stay unscoped so a status poll or session setup
    // never consumes a tenant's concurrency slot.
    let tenant = TenantId::from(
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("tenant uuid parses"),
    );

    assert_eq!(
        contacts_scope("send_message", tenant),
        IngressScope::Tenant(tenant)
    );
    for read_handler in ["progress", "authorize_session", "init_session"] {
        assert_eq!(
            contacts_scope(read_handler, tenant),
            IngressScope::Unscoped,
            "{read_handler} must not consume tenant concurrency"
        );
    }
}

#[test]
fn admission_429_terminal_body_produces_retry_after_seconds() {
    // Pins: Restate terminal errors cannot add arbitrary HTTP headers, so
    // edge translates the bounded retry delay carried by the admission
    // error into the public Retry-After response header.
    assert_eq!(
        retry_after_from_terminal_body(
            "turn admission fleet budget is saturated; retry_after_ms=2500"
        ),
        Some("3".to_string())
    );
    assert_eq!(
        retry_after_from_terminal_body("unrelated upstream failure"),
        None
    );
}

#[test]
fn edge_proxy_security_rejects_unknown_v1_route() {
    // Pins: the public catch-all is an allowlist; unknown /v1 paths return 404 instead of
    // forwarding the caller's path unchanged to Restate.
    let unknown = "/v1/internal/restate/call/SessionStore/append_event"
        .parse::<Uri>()
        .expect("unknown route URI should parse");

    assert_eq!(
        translate_public_route(
            &Method::POST,
            &unknown,
            &Bytes::new(),
            test_support::test_tenant_id()
        ),
        RouteTranslation::NotFound
    );

    let session_id = "11111111-1111-1111-1111-111111111111";
    let known = format!("/v1/sessions/{session_id}/progress")
        .parse::<Uri>()
        .expect("known route URI should parse");
    match translate_public_route(
        &Method::POST,
        &known,
        &Bytes::from_static(br#"{}"#),
        test_support::test_tenant_id(),
    ) {
        RouteTranslation::Forward { method, path, .. } => {
            assert_eq!(method, Method::POST);
            assert_eq!(path, format!("/Session/{session_id}/progress"));
        }
        RouteTranslation::NotFound => panic!("known session route should still translate"),
        RouteTranslation::BadRequest(message) => {
            panic!("known session route should not fail translation: {message}")
        }
    }
}

#[test]
fn edge_proxy_security_rejects_literal_dot_segment_internal_route() {
    // Pins: literal and encoded dot-segment attempts do not translate to an internal
    // Restate service path before the proxy gets a chance to normalize the URL.
    for path in [
        "/v1/../../restate/call/SessionStore/append_event",
        "/v1/%2e%2e/%2e%2e/restate/call/SessionStore/append_event",
    ] {
        let uri = path.parse::<Uri>().expect("attack route URI should parse");

        assert_eq!(
            translate_public_route(
                &Method::POST,
                &uri,
                &Bytes::new(),
                test_support::test_tenant_id(),
            ),
            RouteTranslation::NotFound,
            "{path} must not translate to an upstream service path"
        );
    }
}
