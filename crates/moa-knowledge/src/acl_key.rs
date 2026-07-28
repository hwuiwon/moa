//! Versioned, KMS-backed key owner for source-ACL principal fingerprints.
//!
//! Provider ACLs name people: `alice@example.com`, `sales@example.com`,
//! `example.com`. Storing those strings would put a directory of every tenant's
//! staff into the retrieval path's hot tables, where they would leak through
//! query plans, index dumps, backups, and error text. Instead every principal is
//! canonicalized once and reduced to `HMAC-SHA256(tenant ACL key, canonical
//! principal)`.
//!
//! The key itself never lives in Postgres in the clear: it is a KMS-wrapped data
//! key, unwrapped on first use and cached in memory for the process lifetime.
//! Rotation mints a new version; because the version is encoded into the
//! fingerprint, entries and bindings minted under an old key simply stop
//! matching, which fails closed.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{SOURCE_PRINCIPAL_DIGEST_BYTES, SourcePrincipalFingerprint};
use moa_crypto::{EncryptionContext, KeyHandle, KeyManagementProvider, WrappedDek};
use moa_db::ScopedConn;
use sha2::Sha256;
use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::domain::CanonicalSourcePrincipal;
use crate::error::{Error, Result};

/// Fixed data-subject id for the per-tenant source-ACL key.
///
/// The ACL key is tenant infrastructure, not one contact's data, so it is bound
/// to a stable synthetic subject rather than to any person. A contact erasure
/// must not shred the key that every other member's admission depends on.
const ACL_KEY_SUBJECT: Uuid = Uuid::from_u128(0x6163_6c2d_6b65_795f_6f77_6e65_725f_7631);

/// One tenant's source-ACL MAC key at one version.
pub struct SourceAclKey {
    key_version: u16,
    material: Zeroizing<Vec<u8>>,
}

impl SourceAclKey {
    /// Creates a key from unwrapped material.
    ///
    /// The material is moved into [`Zeroizing`] here rather than at the call
    /// site, so no caller can hold an un-scrubbed copy of a MAC key.
    #[must_use]
    pub fn new(key_version: u16, material: Vec<u8>) -> Self {
        Self {
            key_version,
            material: Zeroizing::new(material),
        }
    }

    /// Returns the version this key was minted at.
    #[must_use]
    pub fn key_version(&self) -> u16 {
        self.key_version
    }

    /// Reduces one canonical principal to its keyed opaque fingerprint.
    ///
    /// The version is mixed into the MAC input as well as into the encoded
    /// fingerprint, so a rotated key cannot produce a value that collides with
    /// one minted under the previous key.
    #[must_use]
    pub fn fingerprint(&self, principal: &CanonicalSourcePrincipal) -> SourcePrincipalFingerprint {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.material)
            .expect("HMAC-SHA256 accepts any key length");
        mac.update(b"moa/source-acl/principal/v1");
        mac.update(&self.key_version.to_be_bytes());
        mac.update(&principal.canonical_bytes());
        let digest: [u8; SOURCE_PRINCIPAL_DIGEST_BYTES] = mac.finalize().into_bytes().into();
        SourcePrincipalFingerprint::from_digest(self.key_version, digest)
    }
}

impl std::fmt::Debug for SourceAclKey {
    /// Prints the version only; key material must never reach a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceAclKey")
            .field("key_version", &self.key_version)
            .finish_non_exhaustive()
    }
}

/// Owner of a tenant's versioned source-ACL fingerprint keys.
#[async_trait]
pub trait SourceAclKeyOwner: Send + Sync {
    /// Returns the tenant's current key, creating the first version on demand.
    async fn current_key(&self, tenant_id: TenantId) -> Result<Arc<SourceAclKey>>;

    /// Returns one specific stored key version.
    async fn key_version(&self, tenant_id: TenantId, key_version: u16)
    -> Result<Arc<SourceAclKey>>;
}

/// Postgres-persisted, KMS-wrapped implementation of [`SourceAclKeyOwner`].
pub struct KmsSourceAclKeyOwner {
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    assume_app_role: bool,
    cache: RwLock<HashMap<(Uuid, u16), Arc<SourceAclKey>>>,
}

impl KmsSourceAclKeyOwner {
    /// Creates a key owner over the deployment KMS and the shared pool.
    #[must_use]
    pub fn new(pool: PgPool, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            kms,
            assume_app_role: false,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Creates a key owner that assumes `moa_app` inside each transaction.
    #[must_use]
    pub fn new_for_app_role(pool: PgPool, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            kms,
            assume_app_role: true,
            cache: RwLock::new(HashMap::new()),
        }
    }

    fn encryption_context(tenant_id: TenantId, key_version: u16) -> EncryptionContext {
        EncryptionContext::new(
            tenant_id.0,
            ACL_KEY_SUBJECT,
            format!("source-acl-key:{key_version}"),
            "restricted",
        )
    }

    async fn cached(&self, tenant_id: TenantId, key_version: u16) -> Option<Arc<SourceAclKey>> {
        self.cache
            .read()
            .await
            .get(&(tenant_id.0, key_version))
            .cloned()
    }

    async fn store_cached(&self, tenant_id: TenantId, key: Arc<SourceAclKey>) {
        self.cache
            .write()
            .await
            .insert((tenant_id.0, key.key_version()), key);
    }

    async fn unwrap_row(&self, tenant_id: TenantId, row: StoredAclKeyRow) -> Result<SourceAclKey> {
        let key_version = u16::try_from(row.key_version).map_err(|_| {
            Error::Repository(format!(
                "source ACL key version {} is outside the encodable range",
                row.key_version
            ))
        })?;
        let plaintext = self
            .kms
            .decrypt_data_key(
                &WrappedDek::new(row.wrapped_key),
                &KeyHandle::new(row.key_handle),
                &Self::encryption_context(tenant_id, key_version),
            )
            .await
            .map_err(|error| {
                Error::Repository(format!("failed to unwrap the source ACL key: {error}"))
            })?;
        Ok(SourceAclKey::new(key_version, plaintext.expose().to_vec()))
    }

    /// Mints the tenant's first ACL key, or returns the one a concurrent caller
    /// already inserted.
    async fn mint_first_key(&self, tenant_id: TenantId) -> Result<SourceAclKey> {
        let key_version: u16 = 1;
        let generated = self
            .kms
            .generate_data_key(&Self::encryption_context(tenant_id, key_version))
            .await
            .map_err(|error| {
                Error::Repository(format!("failed to generate the source ACL key: {error}"))
            })?;

        let mut conn = self.begin().await?;
        let inserted = sqlx::query_as::<_, StoredAclKeyRow>(
            r#"
            INSERT INTO moa.knowledge_source_acl_keys (
                tenant_id, key_version, key_handle, wrapped_key
            )
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (tenant_id, key_version) DO NOTHING
            RETURNING key_version, key_handle, wrapped_key
            "#,
        )
        .bind(tenant_id.0)
        .bind(i32::from(key_version))
        .bind(generated.handle.as_str())
        .bind(generated.wrapped.as_bytes())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let row = match inserted {
            Some(row) => row,
            // Another process won the race; its key is authoritative because
            // fingerprints already minted under it are stored.
            None => sqlx::query_as::<_, StoredAclKeyRow>(
                r#"
                SELECT key_version, key_handle, wrapped_key
                FROM moa.knowledge_source_acl_keys
                WHERE tenant_id = $1 AND key_version = $2
                "#,
            )
            .bind(tenant_id.0)
            .bind(i32::from(key_version))
            .fetch_one(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?,
        };
        conn.commit().await.map_err(map_moa_error)?;
        self.unwrap_row(tenant_id, row).await
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(
            &self.pool,
            &moa_core::types::memory::RlsContext::tenant(TenantId::from(Uuid::nil())),
            self.assume_app_role,
        )
        .await
        .map_err(map_moa_error)
    }

    async fn begin_for_tenant(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(
            &self.pool,
            &moa_core::types::memory::RlsContext::tenant(tenant_id),
            self.assume_app_role,
        )
        .await
        .map_err(map_moa_error)
    }
}

#[async_trait]
impl SourceAclKeyOwner for KmsSourceAclKeyOwner {
    async fn current_key(&self, tenant_id: TenantId) -> Result<Arc<SourceAclKey>> {
        let mut conn = self.begin_for_tenant(tenant_id).await?;
        let row = sqlx::query_as::<_, StoredAclKeyRow>(
            r#"
            SELECT key_version, key_handle, wrapped_key
            FROM moa.knowledge_source_acl_keys
            WHERE tenant_id = $1
            ORDER BY key_version DESC
            LIMIT 1
            "#,
        )
        .bind(tenant_id.0)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;

        let key = match row {
            Some(row) => {
                if let Some(cached) = self
                    .cached(
                        tenant_id,
                        u16::try_from(row.key_version).unwrap_or(u16::MAX),
                    )
                    .await
                {
                    return Ok(cached);
                }
                self.unwrap_row(tenant_id, row).await?
            }
            None => self.mint_first_key(tenant_id).await?,
        };
        let key = Arc::new(key);
        self.store_cached(tenant_id, Arc::clone(&key)).await;
        Ok(key)
    }

    async fn key_version(
        &self,
        tenant_id: TenantId,
        key_version: u16,
    ) -> Result<Arc<SourceAclKey>> {
        if let Some(cached) = self.cached(tenant_id, key_version).await {
            return Ok(cached);
        }
        let mut conn = self.begin_for_tenant(tenant_id).await?;
        let row = sqlx::query_as::<_, StoredAclKeyRow>(
            r#"
            SELECT key_version, key_handle, wrapped_key
            FROM moa.knowledge_source_acl_keys
            WHERE tenant_id = $1 AND key_version = $2
            "#,
        )
        .bind(tenant_id.0)
        .bind(i32::from(key_version))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_moa_error)?;
        let Some(row) = row else {
            return Err(Error::Repository(format!(
                "source ACL key version {key_version} is not provisioned for this tenant"
            )));
        };
        let key = Arc::new(self.unwrap_row(tenant_id, row).await?);
        self.store_cached(tenant_id, Arc::clone(&key)).await;
        Ok(key)
    }
}

#[derive(Debug, sqlx::FromRow)]
struct StoredAclKeyRow {
    key_version: i32,
    key_handle: String,
    wrapped_key: Vec<u8>,
}

fn map_sqlx_error(error: sqlx::Error) -> Error {
    Error::Repository(error.to_string())
}

fn map_moa_error(error: moa_core::error::MoaError) -> Error {
    Error::Repository(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourcePrincipalKind;

    fn key(version: u16, byte: u8) -> SourceAclKey {
        SourceAclKey::new(version, vec![byte; 32])
    }

    #[test]
    fn fingerprints_are_deterministic_and_key_separated() {
        // Pins: the same principal under the same key always fingerprints the
        // same, a different principal never collides, and a rotated key version
        // produces a different value so old entries stop matching (fail closed).
        let alice =
            CanonicalSourcePrincipal::new("drive", SourcePrincipalKind::User, "alice@example.com")
                .expect("normalizes");
        let bob =
            CanonicalSourcePrincipal::new("drive", SourcePrincipalKind::User, "bob@example.com")
                .expect("normalizes");

        let key_v1 = key(1, 0xAB);
        assert_eq!(key_v1.fingerprint(&alice), key_v1.fingerprint(&alice));
        assert_ne!(key_v1.fingerprint(&alice), key_v1.fingerprint(&bob));
        assert_eq!(key_v1.fingerprint(&alice).key_version(), 1);

        let key_v2 = key(2, 0xAB);
        assert_ne!(
            key_v1.fingerprint(&alice),
            key_v2.fingerprint(&alice),
            "a rotated version must not reproduce the previous fingerprint"
        );

        let other_material = key(1, 0xCD);
        assert_ne!(
            key_v1.fingerprint(&alice),
            other_material.fingerprint(&alice),
            "a different tenant key must not reproduce another tenant's fingerprint"
        );
    }

    #[test]
    fn debug_never_reveals_key_material() {
        // Pins: the key type cannot leak into a log line through Debug.
        let rendered = format!("{:?}", key(3, 0x5A));
        assert!(rendered.contains("key_version: 3"));
        assert!(!rendered.contains("90"), "unexpected material: {rendered}");
        assert!(!rendered.contains("5a"), "unexpected material: {rendered}");
    }
}
