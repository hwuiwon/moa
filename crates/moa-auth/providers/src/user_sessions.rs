//! Opaque local user-session token generation, storage, and validation.

use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng as SaltOsRng};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use chrono::{DateTime, Utc};
use rand::Rng;
use secrecy::SecretString;
use sqlx::PgExecutor;
use thiserror::Error;
use uuid::Uuid;

const RANDOM_LEN: usize = 48;
const PREFIX_RANDOM_LEN: usize = 12;
const TOKEN_MARKER: &str = "user_session_";
const CHARSET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

type SessionLookupRow = (Uuid, String, Uuid, Uuid);

/// New user session token data.
#[derive(Debug, Clone, Copy)]
pub struct NewUserSessionToken {
    /// Tenant boundary for this login token.
    pub tenant_id: Uuid,
    /// User that owns the login token.
    pub user_id: Uuid,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Issued user session token, returned exactly once.
pub struct IssuedUserSessionToken {
    /// User session token row ID.
    pub id: Uuid,
    /// Full bearer token value.
    pub token: SecretString,
    /// Non-secret lookup prefix.
    pub prefix: String,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Resolved local user session token identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUserSessionToken {
    /// User session token row ID.
    pub id: Uuid,
    /// Tenant boundary.
    pub tenant_id: Uuid,
    /// User identity.
    pub user_id: Uuid,
}

/// User-session token failures.
#[derive(Debug, Error)]
pub enum UserSessionTokenError {
    /// Token shape was invalid.
    #[error("malformed token: {0}")]
    Malformed(&'static str),
    /// Argon2 hash or verify failed.
    #[error("hash error: {0}")]
    Hash(String),
    /// Database query failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Token was unknown, expired, revoked, or owned by an inactive user.
    #[error("not found, expired, or revoked")]
    NotFoundExpiredOrRevoked,
}

/// Return whether a bearer token has the local user-session token prefix.
#[must_use]
pub fn looks_like_user_session_token(token: &str) -> bool {
    token.starts_with(TOKEN_MARKER)
}

/// Create and persist a new user session token.
pub async fn create<'executor, Executor>(
    exec: Executor,
    new: NewUserSessionToken,
) -> Result<IssuedUserSessionToken, UserSessionTokenError>
where
    Executor: PgExecutor<'executor>,
{
    let token = generate();
    let prefix = prefix_of(&token)?;
    // Argon2 hashing is CPU-heavy; run it off the async runtime so session-token
    // creation does not stall other tasks on the worker thread (mirrors verify).
    let token_for_hash = token.clone();
    let hash = tokio::task::spawn_blocking(move || hash_token(&token_for_hash))
        .await
        .map_err(|error| UserSessionTokenError::Hash(format!("hash task failed: {error}")))??;
    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO user_session_tokens
            (id, token_prefix, token_hash, tenant_id, user_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(id)
    .bind(&prefix)
    .bind(&hash)
    .bind(new.tenant_id)
    .bind(new.user_id)
    .bind(new.expires_at)
    .execute(exec)
    .await?;

    Ok(IssuedUserSessionToken {
        id,
        token: SecretString::new(token.into_boxed_str()),
        prefix,
        expires_at: new.expires_at,
    })
}

/// Validate a presented user session token and return its owner identity.
pub async fn validate(
    pool: &sqlx::PgPool,
    presented: &str,
) -> Result<ResolvedUserSessionToken, UserSessionTokenError> {
    let prefix = prefix_of(presented)?;
    let row: Option<SessionLookupRow> = sqlx::query_as(
        r#"
        SELECT token.id, token.token_hash, token.tenant_id, token.user_id
        FROM user_session_tokens token
        JOIN users u
          ON u.id = token.user_id
         AND u.tenant_id = token.tenant_id
        WHERE token.token_prefix = $1
          AND token.revoked_at IS NULL
          AND token.expires_at > NOW()
          AND u.active = true
        "#,
    )
    .bind(&prefix)
    .fetch_optional(pool)
    .await?;

    let Some((id, hash, tenant_id, user_id)) = row else {
        return Err(UserSessionTokenError::NotFoundExpiredOrRevoked);
    };
    let presented_owned = presented.to_string();
    let verified = tokio::task::spawn_blocking(move || verify_token(&presented_owned, &hash))
        .await
        .map_err(|error| UserSessionTokenError::Hash(format!("verify task failed: {error}")))??;
    if !verified {
        return Err(UserSessionTokenError::NotFoundExpiredOrRevoked);
    }

    if let Err(error) = sqlx::query(
        r#"
        UPDATE user_session_tokens
        SET last_used_at = NOW()
        WHERE id = $1
          AND (
              last_used_at IS NULL
              OR last_used_at < NOW() - INTERVAL '5 minutes'
          )
        "#,
    )
    .bind(id)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %error, token_id = %id, "failed to update user session last_used_at");
    }

    Ok(ResolvedUserSessionToken {
        id,
        tenant_id,
        user_id,
    })
}

/// Revoke a presented user session token after validating it.
pub async fn revoke_presented(
    pool: &sqlx::PgPool,
    presented: &str,
    reason: &str,
) -> Result<ResolvedUserSessionToken, UserSessionTokenError> {
    let resolved = validate(pool, presented).await?;
    sqlx::query(
        r#"
        UPDATE user_session_tokens
        SET revoked_at = COALESCE(revoked_at, NOW()),
            revoked_reason = COALESCE(revoked_reason, $2)
        WHERE id = $1
        "#,
    )
    .bind(resolved.id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(resolved)
}

fn generate() -> String {
    let mut rng = rand::rngs::OsRng;
    let random: String = (0..RANDOM_LEN)
        .map(|_| {
            let index = rng.gen_range(0..CHARSET.len());
            CHARSET[index] as char
        })
        .collect();
    format!("{TOKEN_MARKER}{random}")
}

fn prefix_of(token: &str) -> Result<String, UserSessionTokenError> {
    let random = token
        .strip_prefix(TOKEN_MARKER)
        .ok_or(UserSessionTokenError::Malformed(
            "missing user_session_ prefix",
        ))?;
    if random.len() != RANDOM_LEN || !random.chars().all(|value| value.is_ascii_alphanumeric()) {
        return Err(UserSessionTokenError::Malformed(
            "random must be 48 base62 characters",
        ));
    }
    Ok(format!("{TOKEN_MARKER}{}", &random[..PREFIX_RANDOM_LEN]))
}

fn hash_token(token: &str) -> Result<String, UserSessionTokenError> {
    let salt = SaltString::generate(&mut SaltOsRng);
    Argon2::default()
        .hash_password(token.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| UserSessionTokenError::Hash(error.to_string()))
}

fn verify_token(token: &str, phc: &str) -> Result<bool, UserSessionTokenError> {
    let parsed =
        PasswordHash::new(phc).map_err(|error| UserSessionTokenError::Hash(error.to_string()))?;
    match Argon2::default().verify_password(token.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(error) => Err(UserSessionTokenError::Hash(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::{TOKEN_MARKER, looks_like_user_session_token, prefix_of};

    #[test]
    fn user_session_tokens_use_customer_neutral_prefix() {
        // Pins: login tokens returned by account APIs do not expose the internal product name.
        let token = format!("{TOKEN_MARKER}{}", "a".repeat(48));

        assert!(looks_like_user_session_token(&token));
        assert_eq!(
            prefix_of(&token).expect("well-formed token should produce lookup prefix"),
            format!("{TOKEN_MARKER}{}", "a".repeat(12))
        );
        assert!(!looks_like_user_session_token("moa_session_example"));
    }
}
