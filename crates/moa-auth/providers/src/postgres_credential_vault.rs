//! Durable, Postgres-backed [`CredentialVault`] for tenant connection secrets.
//!
//! This is the single owner of MOA-managed connector material. It sits beside
//! the token vault and deliberately does *not* reuse that table: the token vault
//! is user/OAuth-shaped and keyed by `(tenant, user, connection_name)`, which
//! cannot express a versioned, rotatable, tenant-connection credential series.
//!
//! Storage access flows through [`ScopedConn`] under the `moa_app` role so the
//! forced row-level-security policies on both tables are enforced. Material is
//! envelope-encrypted through the explicitly injected KMS before insertion and
//! opened only at resolve time, so neither table ever sees plaintext.
//!
//! Every operation is replay-safe. The caller supplies a stable operation id and
//! a canonical request hash; the audit table's `(tenant_id, operation_id)`
//! unique index is the replay key. Replaying the same pair returns the original
//! outcome from one audit row; reusing an id with different inputs is a typed
//! [`CredentialError::IdempotencyConflict`] rather than a silent overwrite.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::error::MoaError;
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialSource, CredentialVersion, DeploymentSecrets,
    RedactedSecret,
};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::{Ciphertext, EncryptionContext, KeyManagementProvider};
use moa_db::ScopedConn;
use secrecy::{ExposeSecret, SecretString};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Classification bound into every credential ciphertext.
const CREDENTIAL_PII_CLASS: &str = "tenant_credential";

/// Transaction-local flag that unlocks audit deletion for the purge path.
const PURGE_GUC: &str = "moa.credential_purge";

/// Non-secret row describing one stored credential version.
struct VersionRow {
    credential_uid: Uuid,
    tenant_id: Uuid,
    connection_uid: Uuid,
    kind: CredentialKind,
    version: i64,
    active: bool,
    revoked: bool,
    material_sealed: Vec<u8>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of recording one operation in the append-only audit.
enum AuditRecord {
    /// The audit row was inserted by this call; the operation must run.
    Fresh,
    /// An identical operation was already recorded; return its recorded result.
    Replay { credential_uid: Option<Uuid> },
}

/// Postgres-backed implementation of [`CredentialVault`].
pub struct PostgresCredentialVault {
    pool: Arc<PgPool>,
    kms: Arc<dyn KeyManagementProvider>,
    deployment: DeploymentSecrets,
}

impl PostgresCredentialVault {
    /// Constructs the vault with its required key-management provider.
    #[must_use]
    pub fn new(pool: Arc<PgPool>, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            kms,
            deployment: DeploymentSecrets::new(),
        }
    }

    /// Attaches the deployment-owned operator secrets.
    #[must_use]
    pub fn with_deployment_secrets(mut self, deployment: DeploymentSecrets) -> Self {
        self.deployment = deployment;
        self
    }

    /// Rejects a principal that may not perform the context's operation.
    fn authorize_principal(ctx: &CredentialContext) -> Result<(), CredentialError> {
        if ctx.principal.permits(ctx.operation) {
            Ok(())
        } else {
            Err(CredentialError::Unauthorized)
        }
    }

    /// Records one operation in the append-only audit inside `conn`.
    ///
    /// The unique `(tenant_id, operation_id)` index is the replay key: a repeated
    /// call with the same request hash and operation reports [`AuditRecord::Replay`]
    /// with the originally recorded credential, while a changed hash or operation
    /// is an idempotency conflict.
    async fn record_operation(
        conn: &mut ScopedConn<'_>,
        ctx: &CredentialContext,
        credential_uid: Option<Uuid>,
        connection_uid: Option<Uuid>,
        kind: Option<CredentialKind>,
        version: Option<i64>,
    ) -> Result<AuditRecord, CredentialError> {
        let (principal_kind, principal_id, delegated_by, service_actor) = match ctx.principal {
            CredentialPrincipal::Caller {
                identity_id,
                delegated_by,
            } => ("caller", Some(identity_id), delegated_by, None),
            CredentialPrincipal::Service { actor } => ("service", None, None, Some(actor.as_str())),
        };

        let inserted = sqlx::query(
            r#"
            INSERT INTO tenant_credential_operations (
                tenant_id, operation_id, request_hash, operation, credential_uid,
                connection_uid, kind, version, principal_kind, principal_id,
                delegated_by, service_actor, outcome
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'succeeded')
            ON CONFLICT (tenant_id, operation_id) DO NOTHING
            RETURNING operation_uid
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .bind(&ctx.request_hash)
        .bind(ctx.operation.as_str())
        .bind(credential_uid)
        .bind(connection_uid)
        .bind(kind.map(CredentialKind::as_str))
        .bind(version)
        .bind(principal_kind)
        .bind(principal_id)
        .bind(delegated_by)
        .bind(service_actor)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        if inserted.is_some() {
            return Ok(AuditRecord::Fresh);
        }

        let existing = sqlx::query(
            r#"
            SELECT request_hash, operation, credential_uid
            FROM tenant_credential_operations
            WHERE tenant_id = $1 AND operation_id = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or(CredentialError::Storage(
            "audit row vanished between insert and read".to_string(),
        ))?;

        let recorded_hash: String = existing.try_get("request_hash").map_err(map_sqlx_error)?;
        let recorded_operation: String = existing.try_get("operation").map_err(map_sqlx_error)?;
        if recorded_hash != ctx.request_hash || recorded_operation != ctx.operation.as_str() {
            return Err(CredentialError::IdempotencyConflict);
        }
        Ok(AuditRecord::Replay {
            credential_uid: existing.try_get("credential_uid").map_err(map_sqlx_error)?,
        })
    }

    /// Loads one credential version by reference inside `conn`.
    async fn load_version(
        conn: &mut ScopedConn<'_>,
        reference: CredentialRef,
    ) -> Result<VersionRow, CredentialError> {
        let row = sqlx::query(
            r#"
            SELECT credential_uid, tenant_id, connection_uid, kind, version,
                   active, revoked, material_sealed, created_at
            FROM tenant_credential_versions
            WHERE credential_uid = $1
            "#,
        )
        .bind(reference.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or(CredentialError::NotFound)?;

        let kind_name: String = row.try_get("kind").map_err(map_sqlx_error)?;
        Ok(VersionRow {
            credential_uid: row.try_get("credential_uid").map_err(map_sqlx_error)?,
            tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
            connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
            kind: CredentialKind::from_str_exact(&kind_name).ok_or_else(|| {
                CredentialError::Storage("stored credential kind is not recognized".to_string())
            })?,
            version: row.try_get("version").map_err(map_sqlx_error)?,
            active: row.try_get("active").map_err(map_sqlx_error)?,
            revoked: row.try_get("revoked").map_err(map_sqlx_error)?,
            material_sealed: row.try_get("material_sealed").map_err(map_sqlx_error)?,
            created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        })
    }

    /// Seals material for one credential identity.
    async fn seal(
        &self,
        identity: CredentialIdentity,
        material: &SecretString,
    ) -> Result<(Vec<u8>, String), CredentialError> {
        let ctx = credential_encryption_context(identity);
        let sealed =
            moa_crypto::encrypt(self.kms.as_ref(), material.expose_secret().as_bytes(), &ctx)
                .await
                .map_err(|error| CredentialError::Storage(format!("seal credential: {error}")))?;
        let key_id = sealed.key_handle.as_str().to_string();
        Ok((sealed.to_bytes(), key_id))
    }

    /// Opens sealed material for one stored version.
    async fn open(&self, row: &VersionRow) -> Result<RedactedSecret, CredentialError> {
        let identity = CredentialIdentity {
            tenant_id: TenantId::from(row.tenant_id),
            connection_uid: row.connection_uid,
            kind: row.kind,
        };
        let ctx = credential_encryption_context(identity);
        let ciphertext = Ciphertext::from_bytes(&row.material_sealed)
            .map_err(|error| CredentialError::Storage(format!("decode credential: {error}")))?;
        let plaintext = moa_crypto::decrypt(self.kms.as_ref(), &ciphertext, &ctx)
            .await
            .map_err(|error| CredentialError::Storage(format!("open credential: {error}")))?;
        let plaintext = String::from_utf8(plaintext).map_err(|_| {
            CredentialError::Storage("stored credential is not valid UTF-8".to_string())
        })?;
        Ok(RedactedSecret::new(plaintext))
    }

    /// Builds the non-secret description of one stored version.
    fn version_from_row(row: &VersionRow) -> CredentialVersion {
        CredentialVersion {
            reference: CredentialRef::from_uuid(row.credential_uid),
            identity: CredentialIdentity {
                tenant_id: TenantId::from(row.tenant_id),
                connection_uid: row.connection_uid,
                kind: row.kind,
            },
            version: row.version,
            active: row.active,
            revoked: row.revoked,
            created_at: row.created_at,
        }
    }

    /// Opens a tenant-scoped `moa_app` transaction for `ctx`.
    async fn begin(&self, ctx: &CredentialContext) -> Result<ScopedConn<'_>, CredentialError> {
        ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(ctx.tenant_id), true)
            .await
            .map_err(map_db_error)
    }
}

#[async_trait]
impl CredentialVault for PostgresCredentialVault {
    async fn create(
        &self,
        identity: CredentialIdentity,
        material: SecretString,
        ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Self::authorize_principal(ctx)?;
        if identity.tenant_id != ctx.tenant_id {
            return Err(CredentialError::WrongTenant);
        }
        let (sealed, key_id) = self.seal(identity, &material).await?;

        let mut conn = self.begin(ctx).await?;
        let credential_uid = Uuid::now_v7();
        match Self::record_operation(
            &mut conn,
            ctx,
            Some(credential_uid),
            Some(identity.connection_uid),
            Some(identity.kind),
            Some(1),
        )
        .await?
        {
            AuditRecord::Replay { credential_uid } => {
                let reference = credential_uid.ok_or(CredentialError::NotFound)?;
                let row =
                    Self::load_version(&mut conn, CredentialRef::from_uuid(reference)).await?;
                conn.commit().await.map_err(map_db_error)?;
                return Ok(Self::version_from_row(&row));
            }
            AuditRecord::Fresh => {}
        }

        sqlx::query(
            r#"
            INSERT INTO tenant_credential_versions (
                credential_uid, tenant_id, connection_uid, kind, version,
                material_sealed, kms_key_id, active, revoked, owner_identity_id
            )
            VALUES ($1, $2, $3, $4, 1, $5, $6, TRUE, FALSE, $7)
            "#,
        )
        .bind(credential_uid)
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(&sealed)
        .bind(&key_id)
        .bind(ctx.principal.owner_identity())
        .execute(conn.as_mut())
        .await
        .map_err(map_unique_violation)?;

        let row = Self::load_version(&mut conn, CredentialRef::from_uuid(credential_uid)).await?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(Self::version_from_row(&row))
    }

    async fn resolve(
        &self,
        source: &CredentialSource,
        ctx: &CredentialContext,
    ) -> Result<RedactedSecret, CredentialError> {
        Self::authorize_principal(ctx)?;
        let reference = match source {
            CredentialSource::Deployment { secret } => {
                // Deployment secrets have no tenant credential row and therefore
                // no per-tenant audit row; they are operator configuration.
                return self.deployment.resolve(*secret);
            }
            CredentialSource::TenantConnection { reference } => *reference,
        };

        let mut conn = self.begin(ctx).await?;
        let row = Self::load_version(&mut conn, reference).await?;
        if row.tenant_id != ctx.tenant_id.0 {
            return Err(CredentialError::WrongTenant);
        }
        if row.revoked {
            return Err(CredentialError::Revoked);
        }
        if !row.active {
            return Err(CredentialError::StaleVersion);
        }

        Self::record_operation(
            &mut conn,
            ctx,
            Some(row.credential_uid),
            Some(row.connection_uid),
            Some(row.kind),
            Some(row.version),
        )
        .await?;
        // The audit row is committed before any plaintext exists in memory, so a
        // resolution can never be observed by the caller without a durable record.
        conn.commit().await.map_err(map_db_error)?;

        self.open(&row).await
    }

    async fn describe_batch(
        &self,
        references: &[(Uuid, CredentialRef)],
        ctx: &CredentialContext,
    ) -> Result<Vec<(Uuid, CredentialVersion)>, CredentialError> {
        Self::authorize_principal(ctx)?;
        if references.is_empty() {
            return Ok(Vec::new());
        }

        let connection_uids = references
            .iter()
            .map(|(connection_uid, _)| *connection_uid)
            .collect::<Vec<_>>();
        let credential_uids = references
            .iter()
            .map(|(_, reference)| reference.as_uuid())
            .collect::<Vec<_>>();
        let mut conn = self.begin(ctx).await?;
        let rows = sqlx::query(
            r#"
            SELECT requested.connection_uid,
                   stored.credential_uid,
                   stored.tenant_id,
                   stored.kind,
                   stored.version,
                   stored.active,
                   stored.revoked,
                   stored.created_at
            FROM unnest($1::UUID[], $2::UUID[])
                 AS requested(connection_uid, credential_uid)
            JOIN tenant_credential_versions AS stored
              ON stored.connection_uid = requested.connection_uid
             AND stored.credential_uid = requested.credential_uid
            WHERE stored.tenant_id = $3
            ORDER BY requested.connection_uid, requested.credential_uid
            "#,
        )
        .bind(&connection_uids)
        .bind(&credential_uids)
        .bind(ctx.tenant_id.0)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;

        rows.into_iter()
            .map(|row| {
                let connection_uid: Uuid = row.try_get("connection_uid").map_err(map_sqlx_error)?;
                let credential_uid: Uuid = row.try_get("credential_uid").map_err(map_sqlx_error)?;
                let tenant_id: Uuid = row.try_get("tenant_id").map_err(map_sqlx_error)?;
                let kind_name: String = row.try_get("kind").map_err(map_sqlx_error)?;
                let kind = CredentialKind::from_str_exact(&kind_name).ok_or_else(|| {
                    CredentialError::Storage("stored credential kind is not recognized".to_string())
                })?;
                Ok((
                    connection_uid,
                    CredentialVersion {
                        reference: CredentialRef::from_uuid(credential_uid),
                        identity: CredentialIdentity {
                            tenant_id: TenantId::from(tenant_id),
                            connection_uid,
                            kind,
                        },
                        version: row.try_get("version").map_err(map_sqlx_error)?,
                        active: row.try_get("active").map_err(map_sqlx_error)?,
                        revoked: row.try_get("revoked").map_err(map_sqlx_error)?,
                        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
                    },
                ))
            })
            .collect()
    }

    async fn rotate(
        &self,
        current: CredentialRef,
        material: SecretString,
        ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Self::authorize_principal(ctx)?;

        let mut conn = self.begin(ctx).await?;
        let existing = Self::load_version(&mut conn, current).await?;
        if existing.tenant_id != ctx.tenant_id.0 {
            return Err(CredentialError::WrongTenant);
        }
        let identity = CredentialIdentity {
            tenant_id: TenantId::from(existing.tenant_id),
            connection_uid: existing.connection_uid,
            kind: existing.kind,
        };
        let next_version = existing.version + 1;
        let credential_uid = Uuid::now_v7();

        match Self::record_operation(
            &mut conn,
            ctx,
            Some(credential_uid),
            Some(identity.connection_uid),
            Some(identity.kind),
            Some(next_version),
        )
        .await?
        {
            AuditRecord::Replay { credential_uid } => {
                let reference = credential_uid.ok_or(CredentialError::NotFound)?;
                let row =
                    Self::load_version(&mut conn, CredentialRef::from_uuid(reference)).await?;
                conn.commit().await.map_err(map_db_error)?;
                return Ok(Self::version_from_row(&row));
            }
            AuditRecord::Fresh => {}
        }

        // Compare-and-swap: only the caller holding the currently-active version
        // may supersede it, so a concurrent rotation cannot be silently lost.
        let deactivated = sqlx::query(
            r#"
            UPDATE tenant_credential_versions
            SET active = FALSE, updated_at = NOW()
            WHERE credential_uid = $1 AND active = TRUE AND revoked = FALSE
            "#,
        )
        .bind(current.as_uuid())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if deactivated.rows_affected() == 0 {
            return Err(CredentialError::VersionConflict);
        }

        let (sealed, key_id) = self.seal(identity, &material).await?;
        sqlx::query(
            r#"
            INSERT INTO tenant_credential_versions (
                credential_uid, tenant_id, connection_uid, kind, version,
                material_sealed, kms_key_id, active, revoked, owner_identity_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, TRUE, FALSE, $8)
            "#,
        )
        .bind(credential_uid)
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(next_version)
        .bind(&sealed)
        .bind(&key_id)
        .bind(ctx.principal.owner_identity())
        .execute(conn.as_mut())
        .await
        .map_err(map_unique_violation)?;

        let row = Self::load_version(&mut conn, CredentialRef::from_uuid(credential_uid)).await?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(Self::version_from_row(&row))
    }

    async fn revoke(
        &self,
        reference: CredentialRef,
        ctx: &CredentialContext,
    ) -> Result<(), CredentialError> {
        Self::authorize_principal(ctx)?;

        let mut conn = self.begin(ctx).await?;
        let existing = Self::load_version(&mut conn, reference).await?;
        if existing.tenant_id != ctx.tenant_id.0 {
            return Err(CredentialError::WrongTenant);
        }

        match Self::record_operation(
            &mut conn,
            ctx,
            Some(existing.credential_uid),
            Some(existing.connection_uid),
            Some(existing.kind),
            Some(existing.version),
        )
        .await?
        {
            AuditRecord::Replay { .. } => {
                conn.commit().await.map_err(map_db_error)?;
                return Ok(());
            }
            AuditRecord::Fresh => {}
        }

        sqlx::query(
            r#"
            UPDATE tenant_credential_versions
            SET active = FALSE, revoked = TRUE, revoked_at = NOW(), updated_at = NOW()
            WHERE credential_uid = $1 AND revoked = FALSE
            "#,
        )
        .bind(reference.as_uuid())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    async fn delete_connection(
        &self,
        connection_uid: Uuid,
        ctx: &CredentialContext,
    ) -> Result<u64, CredentialError> {
        Self::authorize_principal(ctx)?;

        let mut conn = self.begin(ctx).await?;
        // Unlock audit deletion for this transaction only. Ordinary resolve and
        // rotate traffic never sets this, so it cannot erase audit history.
        sqlx::query("SELECT set_config($1, 'true', true)")
            .bind(PURGE_GUC)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;

        let deleted = sqlx::query(
            r#"
            DELETE FROM tenant_credential_versions
            WHERE tenant_id = $1 AND connection_uid = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(connection_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        sqlx::query(
            r#"
            DELETE FROM tenant_credential_operations
            WHERE tenant_id = $1 AND connection_uid = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(connection_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        conn.commit().await.map_err(map_db_error)?;
        Ok(deleted)
    }

    async fn purge_tenant(
        &self,
        limit: u32,
        ctx: &CredentialContext,
    ) -> Result<u64, CredentialError> {
        Self::authorize_principal(ctx)?;
        if ctx.operation != CredentialOperation::Delete {
            return Err(CredentialError::Unauthorized);
        }
        let limit = i64::from(limit.max(1));

        let mut conn = self.begin(ctx).await?;
        // Same narrowly scoped unlock as the connection sweep: audit deletion is
        // reachable only from a transaction that explicitly opts in.
        sqlx::query("SELECT set_config($1, 'true', true)")
            .bind(PURGE_GUC)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;

        // Bounded batch. Forced RLS already pins every statement to the context's
        // tenant; the explicit predicate keeps intent readable and survives a
        // policy edit. Versions go first so a crash never leaves a version whose
        // audit projection has already been removed.
        let versions_removed = sqlx::query(
            r#"
            DELETE FROM tenant_credential_versions
            WHERE credential_uid IN (
                SELECT credential_uid
                FROM tenant_credential_versions
                WHERE tenant_id = $1
                ORDER BY created_at
                LIMIT $2
            )
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(limit)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        // Only once no versions remain does the audit projection get swept, so a
        // resumed purge never orphans a version's history before the version.
        let audit_removed = if versions_removed == 0 {
            sqlx::query(
                r#"
                DELETE FROM tenant_credential_operations
                WHERE operation_uid IN (
                    SELECT operation_uid
                    FROM tenant_credential_operations
                    WHERE tenant_id = $1
                    ORDER BY created_at
                    LIMIT $2
                )
                "#,
            )
            .bind(ctx.tenant_id.0)
            .bind(limit)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
            .rows_affected()
        } else {
            0
        };

        conn.commit().await.map_err(map_db_error)?;
        // The total is what makes "loop until 0" terminate only when both the
        // versions and their permitted audit projection are gone.
        Ok(versions_removed + audit_removed)
    }
}

/// Builds the authenticated encryption context binding one credential series.
///
/// Binding tenant, connection, and kind means a row copied to another connection
/// or relabelled to another kind cannot be decrypted at all.
fn credential_encryption_context(identity: CredentialIdentity) -> EncryptionContext {
    EncryptionContext::new(
        identity.tenant_id.0,
        identity.connection_uid,
        identity.kind.as_str(),
        CREDENTIAL_PII_CLASS,
    )
}

/// Maps a unique-violation to the typed version conflict, others to storage.
fn map_unique_violation(error: sqlx::Error) -> CredentialError {
    let is_unique_violation = error
        .as_database_error()
        .and_then(|db| db.code())
        .as_deref()
        == Some("23505");
    if is_unique_violation {
        return CredentialError::VersionConflict;
    }
    map_sqlx_error(error)
}

/// Maps a raw sqlx error to a typed, secret-free storage failure.
fn map_sqlx_error(error: sqlx::Error) -> CredentialError {
    CredentialError::Storage(format!("db: {error}"))
}

/// Maps a [`moa_db`] storage error to a typed, secret-free storage failure.
fn map_db_error(error: MoaError) -> CredentialError {
    CredentialError::Storage(format!("db: {error}"))
}
