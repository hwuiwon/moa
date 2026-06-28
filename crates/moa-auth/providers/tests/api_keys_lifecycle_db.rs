//! DB-backed API-key validation lifecycle, hermetic against a local Postgres.
//!
//! Mirrors the `local_auth_live.rs` end-to-end coverage in-process: this pins
//! that a key validates only while active, that revocation makes it stop
//! validating, and that a structurally valid key with the wrong secret (same
//! lookup prefix) is rejected by the hash check rather than accepted.

use std::time::Duration;

use moa_auth_providers::api_keys::{self, ApiKeyError, Env, KeyOwner, NewApiKey};
use secrecy::ExposeSecret;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn api_key_validate_rejects_revoked_and_wrong_hash_db() {
    // Pins: create -> validate(ok) -> wrong-hash(reject) -> revoke -> re-validate
    // (NotFoundOrRevoked). The wrong-hash and post-revocation cases are the
    // security negatives that previously only ran in the live full-stack test.
    let pool = migrated_pool().await;
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();

    let issued = api_keys::create(
        &pool,
        NewApiKey {
            tenant_id,
            owner: KeyOwner::User(owner_id),
            env: Env::Dev,
            name: "lifecycle-test-key",
            description: None,
        },
    )
    .await
    .expect("api key should be created");
    let secret = issued.key.expose_secret().to_string();

    // Active key validates and resolves to its owner identity.
    let resolved = api_keys::validate(&pool, &secret)
        .await
        .expect("freshly created key should validate");
    assert_eq!(resolved.id, issued.id);
    assert_eq!(resolved.tenant_id, tenant_id);
    assert_eq!(resolved.owner_user_id, Some(owner_id));
    assert_eq!(resolved.owner_agent_id, None);

    // A structurally valid key sharing the stored prefix but a different secret
    // must be rejected by the hash check, not accepted on prefix match alone.
    let wrong = wrong_secret_same_prefix(&secret);
    assert_ne!(
        wrong, secret,
        "the forged key must differ from the real one"
    );
    let wrong_hash_err = api_keys::validate(&pool, &wrong)
        .await
        .expect_err("a key with the right prefix but wrong secret must be rejected");
    assert!(
        matches!(wrong_hash_err, ApiKeyError::NotFoundOrRevoked),
        "expected NotFoundOrRevoked, got {wrong_hash_err:?}"
    );

    // Revoke the key, then it must no longer validate.
    api_keys::revoke(
        &pool,
        issued.id,
        "lifecycle test revocation",
        Some(owner_id),
    )
    .await
    .expect("revocation should succeed");
    let revoked_err = api_keys::validate(&pool, &secret)
        .await
        .expect_err("a revoked key must not validate");
    assert!(
        matches!(revoked_err, ApiKeyError::NotFoundOrRevoked),
        "expected NotFoundOrRevoked after revocation, got {revoked_err:?}"
    );
}

/// Build a structurally valid key that shares `secret`'s 18-char lookup prefix
/// but carries a different random body (and a freshly recomputed CRC), so it
/// passes format parsing and the prefix lookup yet fails the hash comparison.
fn wrong_secret_same_prefix(secret: &str) -> String {
    let (env, random, _crc) = api_keys::parse_parts(secret).expect("issued key parses");
    let mut chars: Vec<char> = random.chars().collect();
    // Flip a character beyond the 8-char prefix window so the prefix is preserved
    // but the full secret (and therefore its argon2 hash) differs.
    chars[8] = if chars[8] == 'a' { 'b' } else { 'a' };
    let new_random: String = chars.into_iter().collect();
    let body = format!("moa_{}_{}", env.as_str(), new_random);
    let crc = crc32fast::hash(body.as_bytes());
    format!("{body}_{crc:08x}")
}

async fn migrated_pool() -> sqlx::PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("moa_api_keys_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    moa_migrations::run_auth_schema(&pool, &schema_name)
        .await
        .expect("auth baseline should apply");
    pool
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
