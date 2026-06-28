//! Hermetic negative-path tests for Auth0 JWT validation.
//!
//! These mirror the live `auth0_live.rs` rejection cases against a fake JWKS
//! endpoint so CI proves the provider *rejects* bad tokens, not just that it
//! accepts good ones. JWT validation fails before any database access, so the
//! provider is built over a lazy pool and these tests never touch Postgres.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use httpmock::{Method::GET, MockServer};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use moa_auth_providers_auth0::Auth0AuthProvider;
use moa_core::traits::{AuthError, AuthProvider, Credential};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, rand_core::OsRng};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

const KID: &str = "test-key-1";
const AUDIENCE: &str = "https://api.moa.example.test";

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

/// A provider plus the JWKS mock server it trusts, wired to a never-queried pool.
struct Fixture {
    provider: Auth0AuthProvider,
    issuer: String,
    // Held only to keep the JWKS endpoint serving across the authenticate call;
    // dropping it would turn every rejection into a JWKS-unavailable error.
    #[allow(dead_code)]
    server: MockServer,
}

/// Build a provider whose JWKS publishes `publish_key`'s public half under `KID`.
fn fixture(publish_key: &RsaPrivateKey) -> Fixture {
    let server = MockServer::start();
    let public = publish_key.to_public_key();
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "kid": KID,
            "use": "sig",
            "alg": "RS256",
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
        }]
    });
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/jwks.json");
        then.status(200).json_body(jwks);
    });

    let issuer = format!("{}/", server.base_url());
    // A lazy pool never connects unless queried; valid tokens are the only path
    // that reaches the database, and these tests only mint invalid tokens.
    let pool = PgPool::connect_lazy("postgres://moa:moa@127.0.0.1:1/moa_unused")
        .expect("lazy pool URL parses without connecting");
    let provider = Auth0AuthProvider::new_with_jwks_url(
        issuer.clone(),
        AUDIENCE.to_string(),
        server.url("/.well-known/jwks.json"),
        Arc::new(pool),
    );

    Fixture {
        provider,
        issuer,
        server,
    }
}

/// Sign an RS256 token with `signing_key` under `KID`.
fn sign(signing_key: &RsaPrivateKey, iss: &str, aud: &str, exp: i64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let claims = Claims {
        sub: "auth0|negative-subject".to_string(),
        iss: iss.to_string(),
        aud: aud.to_string(),
        exp,
        tenant_id: Uuid::from_u128(0x200).to_string(),
        identity_type: "user".to_string(),
    };
    let pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode RSA key as PKCS8 PEM");
    let encoding_key = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("build RSA encoding key");
    encode(&header, &claims, &encoding_key).expect("sign JWT")
}

#[tokio::test]
async fn jwt_validation_rejects_expired_token() {
    // Pins: a token whose exp is well past (beyond the 30s leeway) is Expired.
    let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
    let fx = fixture(&key);
    let expired = Utc::now().timestamp() - 3_600;
    let token = sign(&key, &fx.issuer, AUDIENCE, expired);

    let error = fx
        .provider
        .authenticate(&Credential::BearerJwt(token))
        .await
        .expect_err("expired token must be rejected");

    assert!(
        matches!(error, AuthError::Expired),
        "expected Expired, got {error:?}"
    );
}

#[tokio::test]
async fn jwt_validation_rejects_wrong_audience() {
    // Pins: a correctly-signed token minted for another audience is Rejected.
    let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
    let fx = fixture(&key);
    let exp = Utc::now().timestamp() + 600;
    let token = sign(&key, &fx.issuer, "https://attacker.example/api", exp);

    let error = fx
        .provider
        .authenticate(&Credential::BearerJwt(token))
        .await
        .expect_err("wrong-audience token must be rejected");

    assert!(
        matches!(error, AuthError::Rejected),
        "expected Rejected, got {error:?}"
    );
}

#[tokio::test]
async fn jwt_validation_rejects_wrong_issuer() {
    // Pins: a correctly-signed token from a different issuer is Rejected.
    let key = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
    let fx = fixture(&key);
    let exp = Utc::now().timestamp() + 600;
    let token = sign(&key, "https://attacker.example/", AUDIENCE, exp);

    let error = fx
        .provider
        .authenticate(&Credential::BearerJwt(token))
        .await
        .expect_err("wrong-issuer token must be rejected");

    assert!(
        matches!(error, AuthError::Rejected),
        "expected Rejected, got {error:?}"
    );
}

#[tokio::test]
async fn jwt_validation_rejects_bad_signature() {
    // Pins: a token signed by a key that is not the one published under KID fails
    // signature verification (the JWKS key is found by kid, but the signature is
    // wrong) and is Rejected — not silently accepted.
    let published = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
    let attacker = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA attacker key");
    let fx = fixture(&published);
    let exp = Utc::now().timestamp() + 600;
    let token = sign(&attacker, &fx.issuer, AUDIENCE, exp);

    let error = fx
        .provider
        .authenticate(&Credential::BearerJwt(token))
        .await
        .expect_err("forged-signature token must be rejected");

    assert!(
        matches!(error, AuthError::Rejected),
        "expected Rejected, got {error:?}"
    );
}
