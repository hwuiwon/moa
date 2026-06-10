//! API key format, generation, hashing, validation, and storage helpers.
//!
//! Wire format: `moa_<env>_<random>_<crc32>`.

use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng as SaltOsRng};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Utc};
use moa_core::record_api_key_validation_duration;
use moka::future::Cache;
use rand::Rng;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use thiserror::Error;
use uuid::Uuid;

/// Public GitHub secret-scanning regex for MOA API keys.
pub const GITHUB_SECRET_SCANNING_REGEX: &str =
    r"moa_(live|prod|stg|dev)_[A-Za-z0-9]{32}_[a-f0-9]{8}";

const RANDOM_LEN: usize = 32;
const PREFIX_RANDOM_LEN: usize = 8;
const CHARSET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const VALIDATION_CACHE_CAPACITY: u64 = 10_000;
const VALIDATION_CACHE_TTL: Duration = Duration::from_secs(60);
const VALIDATION_CACHE_DOMAIN: &[u8] = b"moa.api_key.validation.v1";
type ApiKeyLookupRow = (Uuid, String, Option<Uuid>, Option<Uuid>, Uuid);
type ValidationCacheKey = [u8; 32];

static VALIDATION_CACHE: OnceLock<Cache<ValidationCacheKey, ResolvedKey>> = OnceLock::new();
static VALIDATION_KEY_IDS: OnceLock<Cache<Uuid, ValidationCacheKey>> = OnceLock::new();

/// Environment segment embedded in a MOA API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    /// Live environment.
    Live,
    /// Production environment alias.
    Prod,
    /// Staging environment.
    Stg,
    /// Local development environment.
    Dev,
}

impl Env {
    /// Return the key-format string for this environment.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Env::Live => "live",
            Env::Prod => "prod",
            Env::Stg => "stg",
            Env::Dev => "dev",
        }
    }

    /// Parse an environment segment.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "live" => Some(Env::Live),
            "prod" => Some(Env::Prod),
            "stg" => Some(Env::Stg),
            "dev" => Some(Env::Dev),
            _ => None,
        }
    }
}

impl fmt::Display for Env {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Env {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| {
            "expected one of live, prod, stg, or dev for API key environment".to_string()
        })
    }
}

/// Full secret key returned exactly once when a key is created.
pub struct IssuedKey {
    /// Full secret key value.
    pub key: SecretString,
    /// Non-secret lookup prefix.
    pub prefix: String,
    /// API key row ID.
    pub id: Uuid,
}

/// API key operation failures.
#[derive(Debug, Error)]
pub enum ApiKeyError {
    /// Key shape was invalid.
    #[error("malformed key: {0}")]
    Malformed(&'static str),
    /// CRC32 suffix did not match the key body.
    #[error("CRC mismatch (typo?)")]
    CrcMismatch,
    /// Environment segment was not recognized.
    #[error("unknown environment")]
    UnknownEnv,
    /// Argon2 hash or verify failed.
    #[error("hash error: {0}")]
    Hash(String),
    /// Database query failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// SQLx migration failed.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// Key was unknown or already revoked.
    #[error("not found or revoked")]
    NotFoundOrRevoked,
}

/// API key creation request DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyRequest {
    /// Human-readable key name.
    pub name: String,
    /// Optional key description.
    pub description: Option<String>,
    /// Key environment segment.
    pub env: Env,
    /// Optional agent owner. Absent means the caller user owns the key.
    pub for_agent_id: Option<Uuid>,
}

/// API key creation or rotation response DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    /// API key row ID.
    pub id: Uuid,
    /// Full key value, returned exactly once.
    pub key: String,
    /// Non-secret lookup prefix.
    pub prefix: String,
}

/// API key list item DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyListItem {
    /// API key row ID.
    pub id: Uuid,
    /// Human-readable key name.
    pub name: String,
    /// Non-secret lookup prefix.
    pub prefix: String,
    /// Key environment segment.
    pub env: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last successful validation time.
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Generate a new secret key value without persisting it.
#[must_use]
pub fn generate(env: Env) -> SecretString {
    let mut rng = rand::rngs::OsRng;
    let random: String = (0..RANDOM_LEN)
        .map(|_| {
            let index = rng.gen_range(0..CHARSET.len());
            CHARSET[index] as char
        })
        .collect();
    let body = format!("moa_{}_{}", env.as_str(), random);
    let crc = crc32fast::hash(body.as_bytes());
    SecretString::new(format!("{body}_{crc:08x}").into_boxed_str())
}

/// Compute the non-secret 18-character lookup prefix for a key.
pub fn prefix_of(key: &str) -> Result<String, ApiKeyError> {
    let (env, random, _crc) = parse_parts(key)?;
    Ok(format!(
        "moa_{}_{}",
        env.as_str(),
        &random[..PREFIX_RANDOM_LEN]
    ))
}

/// Parse a key into `(env, random, crc)` and verify its CRC suffix.
pub fn parse_parts(key: &str) -> Result<(Env, &str, &str), ApiKeyError> {
    let mut parts = key.splitn(4, '_');
    let marker = parts.next().ok_or(ApiKeyError::Malformed("no segments"))?;
    if marker != "moa" {
        return Err(ApiKeyError::Malformed("missing moa_ prefix"));
    }

    let env_raw = parts.next().ok_or(ApiKeyError::Malformed("missing env"))?;
    let random = parts
        .next()
        .ok_or(ApiKeyError::Malformed("missing random"))?;
    let crc = parts.next().ok_or(ApiKeyError::Malformed("missing crc"))?;

    let env = Env::parse(env_raw).ok_or(ApiKeyError::UnknownEnv)?;
    if random.len() != RANDOM_LEN || !random.chars().all(|value| value.is_ascii_alphanumeric()) {
        return Err(ApiKeyError::Malformed(
            "random must be 32 base62 characters",
        ));
    }
    if crc.len() != 8
        || !crc
            .chars()
            .all(|value| matches!(value, '0'..='9' | 'a'..='f'))
    {
        return Err(ApiKeyError::Malformed("crc must be 8 lowercase hex chars"));
    }

    let body = format!("moa_{}_{}", env.as_str(), random);
    let expected = format!("{:08x}", crc32fast::hash(body.as_bytes()));
    if crc != expected {
        return Err(ApiKeyError::CrcMismatch);
    }

    Ok((env, random, crc))
}

fn hash_key(key: &str) -> Result<String, ApiKeyError> {
    let salt = SaltString::generate(&mut SaltOsRng);
    Argon2::default()
        .hash_password(key.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| ApiKeyError::Hash(error.to_string()))
}

fn verify_key(key: &str, phc: &str) -> Result<bool, ApiKeyError> {
    let parsed = PasswordHash::new(phc).map_err(|error| ApiKeyError::Hash(error.to_string()))?;
    match Argon2::default().verify_password(key.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(error) => Err(ApiKeyError::Hash(error.to_string())),
    }
}

/// New API key row data.
#[derive(Debug, Clone)]
pub struct NewApiKey<'a> {
    /// Tenant owning the key.
    pub tenant_id: Uuid,
    /// User or agent owner.
    pub owner: KeyOwner,
    /// Environment segment.
    pub env: Env,
    /// Human-readable key name.
    pub name: &'a str,
    /// Optional human-readable description.
    pub description: Option<&'a str>,
}

/// API key owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOwner {
    /// User-owned key.
    User(Uuid),
    /// Agent-owned key.
    Agent(Uuid),
}

/// Create and persist a new API key in the caller's transaction.
pub async fn create<'executor, Executor>(
    exec: Executor,
    new: NewApiKey<'_>,
) -> Result<IssuedKey, ApiKeyError>
where
    Executor: PgExecutor<'executor>,
{
    let key = generate(new.env);
    let prefix = prefix_of(key.expose_secret())?;
    let hash = hash_key(key.expose_secret())?;
    let id = Uuid::new_v4();
    let (owner_user_id, owner_agent_id) = match new.owner {
        KeyOwner::User(user_id) => (Some(user_id), None),
        KeyOwner::Agent(agent_id) => (None, Some(agent_id)),
    };

    sqlx::query(
        r#"
        INSERT INTO api_keys
            (id, prefix, hash, owner_user_id, owner_agent_id, tenant_id, name, description, env)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(id)
    .bind(&prefix)
    .bind(&hash)
    .bind(owner_user_id)
    .bind(owner_agent_id)
    .bind(new.tenant_id)
    .bind(new.name)
    .bind(new.description)
    .bind(new.env.as_str())
    .execute(exec)
    .await?;

    Ok(IssuedKey { key, prefix, id })
}

/// Resolved key identity data returned after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    /// API key row ID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// User owner when this is user-owned.
    pub owner_user_id: Option<Uuid>,
    /// Agent owner when this is agent-owned.
    pub owner_agent_id: Option<Uuid>,
}

/// Validate a presented key and return its owner identity.
pub async fn validate(pool: &sqlx::PgPool, presented: &str) -> Result<ResolvedKey, ApiKeyError> {
    let started = Instant::now();
    let cache_key = validation_cache_key(presented);
    let result = if let Some(cached) = validation_cache().get(&cache_key).await {
        validate_cached_resolution(pool, cache_key, cached).await
    } else {
        let result = validate_inner(pool, presented).await;
        if let Ok(resolved) = result.as_ref() {
            cache_successful_validation(cache_key, resolved).await;
        }
        result
    };
    let label = if result.is_ok() { "success" } else { "failure" };
    record_api_key_validation_duration(label, started.elapsed());
    result
}

async fn validate_cached_resolution(
    pool: &sqlx::PgPool,
    cache_key: ValidationCacheKey,
    cached: ResolvedKey,
) -> Result<ResolvedKey, ApiKeyError> {
    if cached_key_is_active(pool, cached.id).await? {
        return Ok(cached);
    }

    validation_cache().invalidate(&cache_key).await;
    validation_key_ids().invalidate(&cached.id).await;
    Err(ApiKeyError::NotFoundOrRevoked)
}

async fn cached_key_is_active(pool: &sqlx::PgPool, key_id: Uuid) -> Result<bool, ApiKeyError> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM api_keys
            WHERE id = $1 AND revoked_at IS NULL
        )
        "#,
    )
    .bind(key_id)
    .fetch_one(pool)
    .await
    .map_err(ApiKeyError::Database)
}

async fn validate_inner(pool: &sqlx::PgPool, presented: &str) -> Result<ResolvedKey, ApiKeyError> {
    let prefix = prefix_of(presented)?;
    let row: Option<ApiKeyLookupRow> = sqlx::query_as(
        r#"
        SELECT id, hash, owner_user_id, owner_agent_id, tenant_id
        FROM api_keys
        WHERE prefix = $1 AND revoked_at IS NULL
        "#,
    )
    .bind(&prefix)
    .fetch_optional(pool)
    .await?;

    let Some((id, hash, owner_user_id, owner_agent_id, tenant_id)) = row else {
        return Err(ApiKeyError::NotFoundOrRevoked);
    };

    if !verify_key(presented, &hash)? {
        return Err(ApiKeyError::NotFoundOrRevoked);
    }

    if let Err(error) = sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %error, key_id = %id, "failed to update api key last_used_at");
    }

    Ok(ResolvedKey {
        id,
        tenant_id,
        owner_user_id,
        owner_agent_id,
    })
}

/// Revoke a key inside the caller's transaction. Idempotent.
pub async fn revoke<'executor, Executor>(
    exec: Executor,
    key_id: Uuid,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Result<(), ApiKeyError>
where
    Executor: PgExecutor<'executor>,
{
    sqlx::query(
        r#"
        WITH updated AS (
            UPDATE api_keys
            SET revoked_at = NOW(), revoked_reason = $2
            WHERE id = $1 AND revoked_at IS NULL
            RETURNING id
        )
        INSERT INTO api_key_revocations (api_key_id, reason, actor_user_id)
        SELECT id, $2, $3 FROM updated
        "#,
    )
    .bind(key_id)
    .bind(reason)
    .bind(actor_user_id)
    .execute(exec)
    .await?;
    invalidate_validation_cache_for_key_id(key_id).await;
    Ok(())
}

fn validation_cache() -> &'static Cache<ValidationCacheKey, ResolvedKey> {
    VALIDATION_CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(VALIDATION_CACHE_CAPACITY)
            .time_to_live(VALIDATION_CACHE_TTL)
            .build()
    })
}

fn validation_key_ids() -> &'static Cache<Uuid, ValidationCacheKey> {
    VALIDATION_KEY_IDS.get_or_init(|| {
        Cache::builder()
            .max_capacity(VALIDATION_CACHE_CAPACITY)
            .time_to_live(VALIDATION_CACHE_TTL)
            .build()
    })
}

fn validation_cache_key(presented: &str) -> ValidationCacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(VALIDATION_CACHE_DOMAIN);
    hasher.update(&(presented.len() as u64).to_le_bytes());
    hasher.update(presented.as_bytes());
    *hasher.finalize().as_bytes()
}

async fn cache_successful_validation(cache_key: ValidationCacheKey, resolved: &ResolvedKey) {
    validation_cache().insert(cache_key, resolved.clone()).await;
    validation_key_ids().insert(resolved.id, cache_key).await;
}

async fn invalidate_validation_cache_for_key_id(key_id: Uuid) {
    if let Some(cache_key) = validation_key_ids().get(&key_id).await {
        validation_cache().invalidate(&cache_key).await;
    }
    validation_key_ids().invalidate(&key_id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_has_correct_shape() {
        // Pins: generated dev keys match the partner-scanning token shape.
        let key = generate(Env::Dev);
        let value = key.expose_secret();
        assert_eq!(&value[..8], "moa_dev_");
        let parts: Vec<&str> = value.split('_').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "moa");
        assert_eq!(parts[1], "dev");
        assert_eq!(parts[2].len(), 32);
        assert_eq!(parts[3].len(), 8);
    }

    #[test]
    fn round_trip_parse_succeeds() {
        // Pins: parser returns the exact embedded environment and segment sizes.
        let key = generate(Env::Prod);
        let (env, random, crc) = parse_parts(key.expose_secret()).expect("generated key parses");
        assert_eq!(env, Env::Prod);
        assert_eq!(random.len(), 32);
        assert_eq!(crc.len(), 8);
    }

    #[test]
    fn flipping_a_random_char_breaks_crc() {
        // Pins: the CRC catches one-character paste mistakes in the random segment.
        let key = generate(Env::Dev);
        let mut chars: Vec<char> = key.expose_secret().chars().collect();
        let random_index = "moa_dev_".len() + 5;
        chars[random_index] = if chars[random_index] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        let error = parse_parts(&tampered).expect_err("tampered key should fail CRC");
        match error {
            ApiKeyError::CrcMismatch => {}
            other => panic!("expected CRC mismatch, got {other:?}"),
        }
    }

    #[test]
    fn hash_and_verify_round_trip() {
        // Pins: argon2 verification accepts the original key and rejects a modified key.
        let key = generate(Env::Dev);
        let hash = hash_key(key.expose_secret()).expect("hash generated key");
        let verified = verify_key(key.expose_secret(), &hash).expect("verify original key");
        assert!(verified, "verify should accept the original key");

        let wrong = key.expose_secret().replace("moa_dev_", "moa_stg_");
        let verified_wrong = verify_key(&wrong, &hash).expect("verify tampered key");
        assert!(!verified_wrong, "verify should reject the tampered key");
    }

    #[test]
    fn prefix_uses_first_eight_random_chars() {
        // Pins: lookup prefix is stable and excludes the secret tail plus CRC.
        let key = "moa_dev_01234567ABCDEFGHIJKLMNOPQRSTUVWX_f3deaf6b";
        let prefix = prefix_of(key).expect("fixture key has valid crc");
        assert_eq!(prefix, "moa_dev_01234567");
    }

    #[test]
    fn validation_cache_key_is_deterministic_and_secret_sensitive() {
        // Pins: cache identity is a stable digest of the whole presented key, not the raw key.
        let key = "moa_dev_01234567ABCDEFGHIJKLMNOPQRSTUVWX_f3deaf6b";
        let changed = "moa_dev_01234567ABCDEFGHIJKLMNOPQRSTUVWY_01234567";

        assert_eq!(validation_cache_key(key), validation_cache_key(key));
        assert_ne!(validation_cache_key(key), validation_cache_key(changed));
    }

    #[tokio::test]
    async fn validation_cache_invalidates_by_key_id() {
        // Pins: in-process revocation removes cached successful validation results.
        let cache_key = validation_cache_key("moa_dev_cached_key");
        let resolved = ResolvedKey {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            owner_user_id: Some(Uuid::new_v4()),
            owner_agent_id: None,
        };

        cache_successful_validation(cache_key, &resolved).await;
        assert_eq!(
            validation_cache().get(&cache_key).await,
            Some(resolved.clone())
        );

        invalidate_validation_cache_for_key_id(resolved.id).await;

        assert_eq!(validation_cache().get(&cache_key).await, None);
        assert_eq!(validation_key_ids().get(&resolved.id).await, None);
    }
}
