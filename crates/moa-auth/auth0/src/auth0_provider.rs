//! Auth0 JWT authentication provider.
//!
//! Auth0-issued bearer JWTs are validated with RS256 against the tenant JWKS.
//! The provider maps the external `sub` claim to MOA's internal UUID through
//! the `auth0_user_map` table, creating a local row on first login.

use crate::jwks_cache::JwksCache;
use async_trait::async_trait;
use jsonwebtoken::{Algorithm, Validation, decode, decode_header};
use moa_core::TenantId;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use moka::future::Cache;
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

/// Maximum number of `(sub, tenant) -> user_id` mappings held in-process.
const USER_ID_CACHE_CAPACITY: u64 = 50_000;
/// TTL for cached identity resolutions. The mapping is immutable once created,
/// so this only bounds memory footprint, not correctness.
const USER_ID_CACHE_TTL: Duration = Duration::from_secs(3600);

static USER_ID_CACHE: OnceLock<Cache<(String, Uuid), Uuid>> = OnceLock::new();

fn user_id_cache() -> &'static Cache<(String, Uuid), Uuid> {
    USER_ID_CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(USER_ID_CACHE_CAPACITY)
            .time_to_live(USER_ID_CACHE_TTL)
            .build()
    })
}

/// Auth0-backed JWT authentication provider.
pub struct Auth0AuthProvider {
    jwks: JwksCache,
    issuer: String,
    audience: String,
    pool: Arc<PgPool>,
}

impl Auth0AuthProvider {
    /// Construct a provider for an Auth0 tenant domain and expected audience.
    #[must_use]
    pub fn new(domain: &str, audience: &str, pool: Arc<PgPool>) -> Self {
        let issuer = format!("https://{}/", domain.trim_end_matches('/'));
        let jwks_url = format!("{}.well-known/jwks.json", issuer);
        Self::new_with_jwks_url(issuer, audience.to_string(), jwks_url, pool)
    }

    /// Construct a provider with an explicit issuer and JWKS URL.
    ///
    /// This is useful for tests and local OIDC-compatible fixtures where the
    /// Auth0 domain-derived HTTPS JWKS URL is not available.
    #[must_use]
    pub fn new_with_jwks_url(
        issuer: String,
        audience: String,
        jwks_url: String,
        pool: Arc<PgPool>,
    ) -> Self {
        Self {
            jwks: JwksCache::new(jwks_url, Duration::from_secs(3600)),
            issuer,
            audience,
            pool,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Auth0Claims {
    sub: String,
    #[serde(rename = "https://moa/tenant_id")]
    tenant_id: Option<String>,
    #[serde(rename = "https://moa/identity_type", default)]
    identity_type: Option<String>,
}

#[async_trait]
impl AuthProvider for Auth0AuthProvider {
    async fn authenticate(&self, credential: &Credential) -> Result<Identity, AuthError> {
        let token = match credential {
            Credential::BearerJwt(token) => token,
            Credential::ApiKey(_) | Credential::UserSessionToken(_) => {
                return Err(AuthError::NotConfigured);
            }
        };

        let header = decode_header(token).map_err(|_| AuthError::InvalidFormat)?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Rejected);
        }
        let kid = header.kid.ok_or(AuthError::InvalidFormat)?;
        let key = self
            .jwks
            .key_for(&kid)
            .await
            .map_err(|error| AuthError::Unavailable(format!("jwks: {error}")))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
        validation.leeway = 30;

        let claims = decode::<Auth0Claims>(token, &key, &validation)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
                _ => AuthError::Rejected,
            })?
            .claims;

        let tenant_str = claims.tenant_id.ok_or_else(|| {
            AuthError::Internal(
                "auth0 token missing https://moa/tenant_id namespaced claim; configure the Auth0 Action"
                    .into(),
            )
        })?;
        let tenant_id = Uuid::parse_str(&tenant_str)
            .map_err(|_| AuthError::Internal("tenant_id claim is not a UUID".into()))?;

        let identity_type = parse_identity_type(claims.identity_type.as_deref())?;
        let id =
            resolve_or_provision_static(&self.pool, &claims.sub, tenant_id, identity_type, "auth0")
                .await
                .map_err(|error| AuthError::Internal(format!("resolve sub: {error}")))?;

        Ok(Identity {
            identity_type,
            id,
            tenant_id: TenantId::from(tenant_id),
            api_key_id: None,
            acting_on_behalf_of: None,
        })
    }

    fn name(&self) -> &'static str {
        "auth0"
    }
}

/// Resolve or create the MOA UUID mapped to an external identity provider subject.
///
/// The common case — an already-provisioned subject — is served from an
/// in-process cache and, on a miss, a single non-transactional `SELECT`. A
/// transaction is opened only to provision a first-seen subject.
pub async fn resolve_or_provision_static(
    pool: &PgPool,
    sub: &str,
    tenant_id: Uuid,
    identity_type: IdentityType,
    source: &str,
) -> Result<Uuid, sqlx::Error> {
    let cache_key = (sub.to_string(), tenant_id);
    if let Some(user_id) = user_id_cache().get(&cache_key).await {
        return Ok(user_id);
    }

    // Fast path: existing subject resolves with a plain read, no transaction.
    if let Some((existing_id,)) = sqlx::query_as::<_, (Uuid,)>(
        "SELECT user_id FROM auth0_user_map WHERE sub = $1 AND tenant_id = $2",
    )
    .bind(sub)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?
    {
        user_id_cache().insert(cache_key, existing_id).await;
        return Ok(existing_id);
    }

    let resolved_id = provision_static(pool, sub, tenant_id, identity_type, source).await?;
    user_id_cache().insert(cache_key, resolved_id).await;
    Ok(resolved_id)
}

/// Provision a first-seen subject inside a transaction and return its MOA UUID.
async fn provision_static(
    pool: &PgPool,
    sub: &str,
    tenant_id: Uuid,
    identity_type: IdentityType,
    source: &str,
) -> Result<Uuid, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let new_id = Uuid::new_v4();
    if identity_type == IdentityType::Operator {
        let external_id = format!("{source}:{sub}");
        sqlx::query(
            r#"
            INSERT INTO users (id, tenant_id, email, external_id, created_at, updated_at)
            VALUES ($1, $2, $3, $3, NOW(), NOW())
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(new_id)
        .bind(tenant_id)
        .bind(external_id)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        r#"
        INSERT INTO auth0_user_map (sub, tenant_id, user_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (sub, tenant_id) DO NOTHING
        "#,
    )
    .bind(sub)
    .bind(tenant_id)
    .bind(new_id)
    .execute(&mut *tx)
    .await?;

    let (resolved_id,) = sqlx::query_as::<_, (Uuid,)>(
        "SELECT user_id FROM auth0_user_map WHERE sub = $1 AND tenant_id = $2",
    )
    .bind(sub)
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(resolved_id)
}

pub(crate) fn parse_identity_type(value: Option<&str>) -> Result<IdentityType, AuthError> {
    match value.unwrap_or("operator") {
        "operator" => Ok(IdentityType::Operator),
        "agent" => Ok(IdentityType::Agent),
        "service" => Ok(IdentityType::Service),
        other => Err(AuthError::Internal(format!(
            "unknown identity_type: {other}"
        ))),
    }
}
