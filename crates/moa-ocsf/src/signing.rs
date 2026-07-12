//! Per-tenant HMAC-SHA256 signing and key rotation.

use crate::jcs::{self, JcsError};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, Mac};
use moka::future::Cache;
use rand::{RngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use sha2::Sha256;
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Maximum number of tenant signing keys held in the in-process cache.
const SIGNING_KEY_CACHE_CAPACITY: u64 = 10_000;
/// How long a cached active signing key is trusted before it is re-read.
const SIGNING_KEY_CACHE_TTL: Duration = Duration::from_secs(300);

static SIGNING_KEY_CACHE: OnceLock<Cache<Uuid, ActiveKey>> = OnceLock::new();

fn signing_key_cache() -> &'static Cache<Uuid, ActiveKey> {
    SIGNING_KEY_CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(SIGNING_KEY_CACHE_CAPACITY)
            .time_to_live(SIGNING_KEY_CACHE_TTL)
            .build()
    })
}

/// Signing failures.
#[derive(Debug, Error)]
pub enum SigningError {
    /// The tenant has no active signing key.
    #[error("no active signing key for tenant {0}")]
    NoActiveKey(Uuid),
    /// Key material could not be decoded or used.
    #[error("invalid key material: {0}")]
    InvalidKey(String),
    /// Canonicalization failed.
    #[error("canonicalization: {0}")]
    Canonicalization(#[from] JcsError),
    /// Database operation failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

/// Active tenant signing key.
#[derive(Debug, Clone)]
pub(crate) struct ActiveKey {
    /// Signing key row id.
    pub key_id: Uuid,
    /// Base64-encoded HMAC key material.
    pub key: SecretString,
}

/// Fetch the active signing-key row for a tenant, if one exists.
///
/// Works against any `sqlx` executor (a pool or a transaction connection) so the
/// active-key SELECT lives in exactly one place.
async fn fetch_active_key_row<'e, E>(
    exec: E,
    tenant_id: Uuid,
) -> Result<Option<(Uuid, String)>, SigningError>
where
    E: PgExecutor<'e>,
{
    let row = sqlx::query_as(
        "SELECT id, key_b64 FROM tenant_signing_keys WHERE tenant_id = $1 AND active = TRUE LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(exec)
    .await?;
    Ok(row)
}

/// Return the active signing key for a tenant.
pub(crate) async fn active_key_for(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<ActiveKey, SigningError> {
    let row = fetch_active_key_row(pool, tenant_id).await?;
    active_key_from_row(row, tenant_id)
}

/// Ensure a tenant has an active signing key and return its id.
pub async fn ensure_key(pool: &PgPool, tenant_id: Uuid) -> Result<Uuid, SigningError> {
    match active_key_for(pool, tenant_id).await {
        Ok(active) => Ok(active.key_id),
        Err(SigningError::NoActiveKey(_)) => rotate_key(pool, tenant_id).await,
        Err(error) => Err(error),
    }
}

/// Generate a fresh active signing key for a tenant.
pub async fn rotate_key(pool: &PgPool, tenant_id: Uuid) -> Result<Uuid, SigningError> {
    let mut tx = pool.begin().await?;
    let key_id = rotate_key_tx(&mut tx, tenant_id).await?;
    tx.commit().await?;
    signing_key_cache().invalidate(&tenant_id).await;
    Ok(key_id)
}

/// Generate a fresh active signing key inside an existing transaction.
pub(crate) async fn rotate_key_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<Uuid, SigningError> {
    let mut key_bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut key_bytes);
    let key_b64 = B64.encode(key_bytes);
    let key_id = Uuid::new_v4();

    sqlx::query(
        r#"
        UPDATE tenant_signing_keys
        SET active = FALSE, deactivated_at = NOW()
        WHERE tenant_id = $1 AND active = TRUE
        "#,
    )
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO tenant_signing_keys (id, tenant_id, key_b64, active)
        VALUES ($1, $2, $3, TRUE)
        "#,
    )
    .bind(key_id)
    .bind(tenant_id)
    .bind(key_b64)
    .execute(&mut **tx)
    .await?;

    Ok(key_id)
}

/// Sign a JSON event using the tenant's active key, reusing a cached key.
///
/// Unlike [`sign`], this never opens a transaction on the hot path: the active
/// key is read once per tenant and then served from an in-process cache with a
/// short TTL. It is used by the background audit writer, where per-event key
/// SELECTs would dominate the cost. If the tenant has no active key yet one is
/// created (a rare, first-event write).
pub async fn sign_cached(
    pool: &PgPool,
    tenant_id: Uuid,
    event_json: &Value,
) -> Result<(Uuid, String, Vec<u8>), SigningError> {
    let active = active_key_cached(pool, tenant_id).await?;
    let key_bytes = B64
        .decode(active.key.expose_secret())
        .map_err(|error| SigningError::InvalidKey(error.to_string()))?;
    let event_jcs = jcs::canonicalize(event_json)?;
    let signature_hex = hmac_hex(&key_bytes, &event_jcs)?;
    Ok((active.key_id, signature_hex, event_jcs))
}

/// Return the tenant's active signing key from cache, reading or creating it on
/// a miss. Cached entries are invalidated by [`rotate_key`].
async fn active_key_cached(pool: &PgPool, tenant_id: Uuid) -> Result<ActiveKey, SigningError> {
    if let Some(active) = signing_key_cache().get(&tenant_id).await {
        return Ok(active);
    }
    let active = match active_key_for(pool, tenant_id).await {
        Ok(active) => active,
        Err(SigningError::NoActiveKey(_)) => {
            if let Err(error) = rotate_key(pool, tenant_id).await
                && !matches!(
                    &error,
                    SigningError::Database(database_error)
                        if is_unique_violation(database_error)
                )
            {
                return Err(error);
            }
            // Another first-event signer created the tenant's active key
            // concurrently. Refetch it instead of dropping this audit row.
            active_key_for(pool, tenant_id).await?
        }
        Err(error) => return Err(error),
    };
    signing_key_cache().insert(tenant_id, active.clone()).await;
    Ok(active)
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("23505")
    )
}

/// Sign a JSON event inside an existing transaction.
pub(crate) async fn sign_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    event_json: &Value,
) -> Result<(Uuid, String, Vec<u8>), SigningError> {
    let active = active_or_create_key_tx(tx, tenant_id).await?;
    let key_bytes = B64
        .decode(active.key.expose_secret())
        .map_err(|error| SigningError::InvalidKey(error.to_string()))?;
    let event_jcs = jcs::canonicalize(event_json)?;
    let signature_hex = hmac_hex(&key_bytes, &event_jcs)?;
    Ok((active.key_id, signature_hex, event_jcs))
}

/// Verify an existing signed event.
pub async fn verify(
    pool: &PgPool,
    signing_key_id: Uuid,
    event_jcs: &[u8],
    signature_hex: &str,
) -> Result<bool, SigningError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT key_b64 FROM tenant_signing_keys WHERE id = $1")
            .bind(signing_key_id)
            .fetch_optional(pool)
            .await?;
    let Some((key_b64,)) = row else {
        return Ok(false);
    };
    let key_bytes = B64
        .decode(key_b64)
        .map_err(|error| SigningError::InvalidKey(error.to_string()))?;
    let expected = hmac_hex(&key_bytes, event_jcs)?;
    Ok(constant_time_eq::constant_time_eq(
        expected.as_bytes(),
        signature_hex.as_bytes(),
    ))
}

async fn active_or_create_key_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<ActiveKey, SigningError> {
    if let Some(row) = fetch_active_key_row(&mut **tx, tenant_id).await? {
        return active_key_from_row(Some(row), tenant_id);
    }
    rotate_key_tx(tx, tenant_id).await?;
    let row = fetch_active_key_row(&mut **tx, tenant_id).await?;
    active_key_from_row(row, tenant_id)
}

fn active_key_from_row(
    row: Option<(Uuid, String)>,
    tenant_id: Uuid,
) -> Result<ActiveKey, SigningError> {
    let (key_id, key_b64) = row.ok_or(SigningError::NoActiveKey(tenant_id))?;
    Ok(ActiveKey {
        key_id,
        key: SecretString::new(key_b64.into_boxed_str()),
    })
}

fn hmac_hex(key_bytes: &[u8], payload: &[u8]) -> Result<String, SigningError> {
    let mut mac = HmacSha256::new_from_slice(key_bytes)
        .map_err(|error| SigningError::InvalidKey(error.to_string()))?;
    mac.update(payload);
    Ok(hex::encode(mac.finalize().into_bytes()))
}
