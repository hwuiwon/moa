//! Tests for trusted identity-header extraction.

use moa_core::TenantId;
use moa_core::traits::IdentityType;
use moa_orchestrator::ctx::{IdentityHeaderError, extract_identity};
use restate_sdk::prelude::HeaderMap;
use uuid::Uuid;

#[test]
fn missing_headers_returns_missing_error() {
    // Pins: calls that bypass moa-edge identity injection fail closed.
    let headers = HeaderMap::with_capacity(0);

    let error = extract_identity(&headers).expect_err("missing identity headers should fail");

    assert_eq!(error, IdentityHeaderError::Missing("x-moa-identity-type"));
}

#[test]
fn full_header_set_produces_expected_identity() {
    // Pins: all five trusted headers map onto the exact Identity fields.
    let identity_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
        .expect("identity UUID fixture parses");
    let tenant_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
        .expect("tenant UUID fixture parses");
    let api_key_id = Uuid::parse_str("33333333-3333-3333-3333-333333333333")
        .expect("api key UUID fixture parses");
    let acting_user_id = Uuid::parse_str("44444444-4444-4444-4444-444444444444")
        .expect("acting user UUID fixture parses");
    let mut headers = HeaderMap::with_capacity(5);
    headers.insert("x-moa-identity-type", "agent".to_string());
    headers.insert("x-moa-identity-id", identity_id.to_string());
    headers.insert("x-moa-tenant-id", tenant_id.to_string());
    headers.insert("x-moa-api-key-id", api_key_id.to_string());
    headers.insert("x-moa-acting-on-behalf-of", acting_user_id.to_string());

    let identity = extract_identity(&headers)
        .expect("full identity headers should parse")
        .expect("full identity headers should produce identity");

    assert_eq!(identity.identity_type, IdentityType::Agent);
    assert_eq!(identity.id, identity_id);
    assert_eq!(identity.tenant_id, TenantId::from(tenant_id));
    assert_eq!(identity.api_key_id, Some(api_key_id));
    assert_eq!(identity.acting_on_behalf_of, Some(acting_user_id));
}

#[test]
fn partial_header_set_returns_malformed_error() {
    // Pins: partial identity is rejected instead of producing an ambiguous principal.
    let mut headers = HeaderMap::with_capacity(1);
    headers.insert("x-moa-identity-type", "operator".to_string());

    let error =
        extract_identity(&headers).expect_err("partial identity header set should be malformed");

    assert_eq!(
        error,
        IdentityHeaderError::Malformed("partial identity headers; require all of type/id/tenant")
    );
}

#[test]
fn unknown_identity_type_returns_unknown_type_error() {
    // Pins: identity type values are a closed set.
    let mut headers = HeaderMap::with_capacity(3);
    headers.insert("x-moa-identity-type", "robot".to_string());
    headers.insert(
        "x-moa-identity-id",
        "11111111-1111-1111-1111-111111111111".to_string(),
    );
    headers.insert(
        "x-moa-tenant-id",
        "22222222-2222-2222-2222-222222222222".to_string(),
    );

    let error = extract_identity(&headers).expect_err("unknown identity type should be rejected");

    assert_eq!(error, IdentityHeaderError::UnknownType("robot".to_string()));
}
