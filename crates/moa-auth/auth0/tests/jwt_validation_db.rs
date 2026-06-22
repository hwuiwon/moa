//! Self-signed JWT validation against a fake JWKS endpoint.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use httpmock::{Method::GET, MockServer};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use moa_auth_providers_auth0::Auth0AuthProvider;
use moa_core::TenantId;
use moa_core::traits::{AuthProvider, Credential, IdentityType};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, rand_core::OsRng};
use serde::Serialize;
use uuid::Uuid;

mod support;

#[derive(Debug, Serialize)]
struct Claims {
    sub: String,
    iss: String,
    aud: String,
    exp: i64,
    #[serde(rename = "https://moa/tenant_id")]
    tenant_id: String,
    #[serde(rename = "https://moa/identity_type")]
    identity_type: String,
}

#[tokio::test]
async fn jwt_validation_accepts_self_signed_auth0_token() {
    // Pins: Auth0AuthProvider validates RS256 JWTs via JWKS and reuses the same sub mapping.
    let pool = support::migrated_auth0_pool().await;
    let server = MockServer::start();
    let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
    let public = key.to_public_key();
    let kid = "test-key-1";
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "kid": kid,
            "use": "sig",
            "alg": "RS256",
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
        }]
    });
    let jwks_mock = server.mock(|when, then| {
        when.method(GET).path("/.well-known/jwks.json");
        then.status(200).json_body(jwks);
    });

    let issuer = format!("{}/", server.base_url());
    let audience = "https://api.moa.example.test".to_string();
    let tenant_id = Uuid::from_u128(0x100);
    let sub = "auth0|test-subject".to_string();
    let token = signed_token(&key, kid, &issuer, &audience, tenant_id, &sub);
    let provider = Auth0AuthProvider::new_with_jwks_url(
        issuer,
        audience,
        server.url("/.well-known/jwks.json"),
        Arc::new(pool.clone()),
    );

    let first = provider
        .authenticate(&Credential::BearerJwt(token.clone()))
        .await
        .expect("valid token should authenticate");
    let second = provider
        .authenticate(&Credential::BearerJwt(token))
        .await
        .expect("cached valid token should authenticate");

    assert_eq!(first.identity_type, IdentityType::User);
    assert_eq!(first.tenant_id, TenantId::from(tenant_id));
    assert_eq!(first.api_key_id, None);
    assert_eq!(first.acting_on_behalf_of, None);
    assert_eq!(second.id, first.id);
    assert_eq!(jwks_mock.hits(), 1);

    let (mapped_id, external_id): (Uuid, String) = sqlx::query_as(
        r#"
        SELECT m.user_id, u.external_id
        FROM auth0_user_map m
        JOIN users u ON u.id = m.user_id
        WHERE m.sub = $1 AND m.tenant_id = $2
        "#,
    )
    .bind(&sub)
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("mapping row should exist");
    assert_eq!(mapped_id, first.id);
    assert_eq!(external_id, "auth0:auth0|test-subject");
}

fn signed_token(
    key: &RsaPrivateKey,
    kid: &str,
    issuer: &str,
    audience: &str,
    tenant_id: Uuid,
    sub: &str,
) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    let claims = Claims {
        sub: sub.to_string(),
        iss: issuer.to_string(),
        aud: audience.to_string(),
        exp: Utc::now().timestamp() + 600,
        tenant_id: tenant_id.to_string(),
        identity_type: "user".to_string(),
    };
    let private_key = key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode RSA key as PKCS8 PEM");
    let encoding_key =
        EncodingKey::from_rsa_pem(private_key.as_bytes()).expect("build RSA encoding key");
    encode(&header, &claims, &encoding_key).expect("sign JWT")
}
