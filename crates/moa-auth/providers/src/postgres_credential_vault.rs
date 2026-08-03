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
    CredentialPrincipal, CredentialRef, CredentialSlotName, CredentialSource,
    CredentialStagingToken, CredentialVersion, DeploymentSecrets, RedactedSecret,
};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::{Ciphertext, EncryptionContext, KeyManagementProvider};
use moa_db::ScopedConn;
use secrecy::{ExposeSecret, SecretString};
use sqlx::{PgPool, Row, postgres::PgRow};
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
    slot_name: CredentialSlotName,
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
    Replay {
        credential_uid: Option<Uuid>,
        expected_prior_credential_uid: Option<Uuid>,
    },
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

    /// Requires the context to name the exact mutating operation being called.
    fn authorize_operation(
        ctx: &CredentialContext,
        expected: CredentialOperation,
    ) -> Result<(), CredentialError> {
        Self::authorize_principal(ctx)?;
        if ctx.operation == expected {
            Ok(())
        } else {
            Err(CredentialError::Unauthorized)
        }
    }

    /// Loads and validates an existing replay record for one exact selector.
    async fn load_operation_replay(
        conn: &mut ScopedConn<'_>,
        ctx: &CredentialContext,
        identity: &CredentialIdentity,
    ) -> Result<Option<AuditRecord>, CredentialError> {
        let existing = sqlx::query(
            r#"
            SELECT request_hash, operation, credential_uid,
                   expected_prior_credential_uid, connection_uid, kind, slot_name
            FROM tenant_credential_operations
            WHERE tenant_id = $1 AND operation_id = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let Some(existing) = existing else {
            return Ok(None);
        };

        let recorded_hash: String = existing.try_get("request_hash").map_err(map_sqlx_error)?;
        let recorded_operation: String = existing.try_get("operation").map_err(map_sqlx_error)?;
        let recorded_connection: Option<Uuid> =
            existing.try_get("connection_uid").map_err(map_sqlx_error)?;
        let recorded_kind: Option<String> = existing.try_get("kind").map_err(map_sqlx_error)?;
        let recorded_slot: String = existing.try_get("slot_name").map_err(map_sqlx_error)?;
        if recorded_hash != ctx.request_hash
            || recorded_operation != ctx.operation.as_str()
            || recorded_connection != Some(identity.connection_uid)
            || recorded_kind.as_deref() != Some(identity.kind.as_str())
            || recorded_slot != identity.slot_name.as_str()
        {
            return Err(CredentialError::IdempotencyConflict);
        }
        Ok(Some(AuditRecord::Replay {
            credential_uid: existing.try_get("credential_uid").map_err(map_sqlx_error)?,
            expected_prior_credential_uid: existing
                .try_get("expected_prior_credential_uid")
                .map_err(map_sqlx_error)?,
        }))
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
        credential_uid: Uuid,
        identity: &CredentialIdentity,
        version: i64,
        expected_prior: Option<CredentialRef>,
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
                connection_uid, kind, slot_name, version,
                expected_prior_credential_uid, principal_kind, principal_id,
                delegated_by, service_actor, outcome
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                'succeeded'
            )
            ON CONFLICT (tenant_id, operation_id) DO NOTHING
            RETURNING operation_uid
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .bind(&ctx.request_hash)
        .bind(ctx.operation.as_str())
        .bind(credential_uid)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .bind(version)
        .bind(expected_prior.map(CredentialRef::as_uuid))
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

        let replay = Self::load_operation_replay(conn, ctx, identity)
            .await?
            .ok_or(CredentialError::Storage(
                "audit row vanished between insert and read".to_string(),
            ))?;
        let AuditRecord::Replay {
            expected_prior_credential_uid,
            ..
        } = replay
        else {
            return Err(CredentialError::Storage(
                "audit replay loaded a fresh record".to_string(),
            ));
        };
        if expected_prior_credential_uid != expected_prior.map(CredentialRef::as_uuid) {
            return Err(CredentialError::IdempotencyConflict);
        }
        Ok(replay)
    }

    /// Reserves one replay-stable, connection-wide revocation audit row.
    ///
    /// Returns `true` only to the caller that inserted the audit row and must
    /// perform the matching bulk revocation. An exact replay returns `false`;
    /// reusing the operation id for another selector fails closed.
    async fn reserve_connection_revoke(
        conn: &mut ScopedConn<'_>,
        ctx: &CredentialContext,
        connection_uid: Uuid,
    ) -> Result<bool, CredentialError> {
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
                connection_uid, kind, slot_name, version,
                expected_prior_credential_uid, principal_kind, principal_id,
                delegated_by, service_actor, outcome
            )
            VALUES (
                $1, $2, $3, $4, NULL, $5, NULL, NULL, NULL, NULL,
                $6, $7, $8, $9, 'succeeded'
            )
            ON CONFLICT (tenant_id, operation_id) DO NOTHING
            RETURNING operation_uid
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .bind(&ctx.request_hash)
        .bind(ctx.operation.as_str())
        .bind(connection_uid)
        .bind(principal_kind)
        .bind(principal_id)
        .bind(delegated_by)
        .bind(service_actor)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if inserted.is_some() {
            return Ok(true);
        }

        let existing = sqlx::query(
            r#"
            SELECT request_hash, operation, credential_uid, connection_uid,
                   kind, slot_name, version, expected_prior_credential_uid
            FROM tenant_credential_operations
            WHERE tenant_id = $1 AND operation_id = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            CredentialError::Storage(
                "connection revocation audit vanished between insert and read".to_string(),
            )
        })?;
        let recorded_hash: String = existing.try_get("request_hash").map_err(map_sqlx_error)?;
        let recorded_operation: String = existing.try_get("operation").map_err(map_sqlx_error)?;
        let recorded_credential: Option<Uuid> =
            existing.try_get("credential_uid").map_err(map_sqlx_error)?;
        let recorded_connection: Option<Uuid> =
            existing.try_get("connection_uid").map_err(map_sqlx_error)?;
        let recorded_kind: Option<String> = existing.try_get("kind").map_err(map_sqlx_error)?;
        let recorded_slot: Option<String> =
            existing.try_get("slot_name").map_err(map_sqlx_error)?;
        let recorded_version: Option<i64> = existing.try_get("version").map_err(map_sqlx_error)?;
        let recorded_prior: Option<Uuid> = existing
            .try_get("expected_prior_credential_uid")
            .map_err(map_sqlx_error)?;
        if recorded_hash != ctx.request_hash
            || recorded_operation != CredentialOperation::Revoke.as_str()
            || recorded_credential.is_some()
            || recorded_connection != Some(connection_uid)
            || recorded_kind.is_some()
            || recorded_slot.is_some()
            || recorded_version.is_some()
            || recorded_prior.is_some()
        {
            return Err(CredentialError::IdempotencyConflict);
        }
        Ok(false)
    }

    /// Records one secret-free exact-series readiness check.
    async fn record_status_operation(
        conn: &mut ScopedConn<'_>,
        ctx: &CredentialContext,
        identity: &CredentialIdentity,
    ) -> Result<(), CredentialError> {
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
                connection_uid, kind, slot_name, version,
                expected_prior_credential_uid, principal_kind, principal_id,
                delegated_by, service_actor, outcome
            )
            VALUES (
                $1, $2, $3, $4, NULL, $5, $6, $7, NULL, NULL,
                $8, $9, $10, $11, 'succeeded'
            )
            ON CONFLICT (tenant_id, operation_id) DO NOTHING
            RETURNING operation_uid
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .bind(&ctx.request_hash)
        .bind(ctx.operation.as_str())
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .bind(principal_kind)
        .bind(principal_id)
        .bind(delegated_by)
        .bind(service_actor)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if inserted.is_some() {
            return Ok(());
        }

        let existing = sqlx::query(
            r#"
            SELECT request_hash, operation, credential_uid, connection_uid,
                   kind, slot_name, version, expected_prior_credential_uid
            FROM tenant_credential_operations
            WHERE tenant_id = $1 AND operation_id = $2
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(&ctx.operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            CredentialError::Storage(
                "credential status audit vanished between insert and read".to_string(),
            )
        })?;
        let recorded_hash: String = existing.try_get("request_hash").map_err(map_sqlx_error)?;
        let recorded_operation: String = existing.try_get("operation").map_err(map_sqlx_error)?;
        let recorded_credential: Option<Uuid> =
            existing.try_get("credential_uid").map_err(map_sqlx_error)?;
        let recorded_connection: Option<Uuid> =
            existing.try_get("connection_uid").map_err(map_sqlx_error)?;
        let recorded_kind: Option<String> = existing.try_get("kind").map_err(map_sqlx_error)?;
        let recorded_slot: Option<String> =
            existing.try_get("slot_name").map_err(map_sqlx_error)?;
        let recorded_version: Option<i64> = existing.try_get("version").map_err(map_sqlx_error)?;
        let recorded_prior: Option<Uuid> = existing
            .try_get("expected_prior_credential_uid")
            .map_err(map_sqlx_error)?;
        if recorded_hash != ctx.request_hash
            || recorded_operation != CredentialOperation::Resolve.as_str()
            || recorded_credential.is_some()
            || recorded_connection != Some(identity.connection_uid)
            || recorded_kind.as_deref() != Some(identity.kind.as_str())
            || recorded_slot.as_deref() != Some(identity.slot_name.as_str())
            || recorded_version.is_some()
            || recorded_prior.is_some()
        {
            return Err(CredentialError::IdempotencyConflict);
        }
        Ok(())
    }

    /// Loads one credential version by reference inside `conn`.
    async fn load_version(
        conn: &mut ScopedConn<'_>,
        reference: CredentialRef,
    ) -> Result<VersionRow, CredentialError> {
        let row = sqlx::query(
            r#"
            SELECT credential_uid, tenant_id, connection_uid, kind, slot_name, version,
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
        let slot_name: String = row.try_get("slot_name").map_err(map_sqlx_error)?;
        Ok(VersionRow {
            credential_uid: row.try_get("credential_uid").map_err(map_sqlx_error)?,
            tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
            connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
            kind: CredentialKind::from_str_exact(&kind_name).ok_or_else(|| {
                CredentialError::Storage("stored credential kind is not recognized".to_string())
            })?,
            slot_name: parse_slot_name(slot_name)?,
            version: row.try_get("version").map_err(map_sqlx_error)?,
            active: row.try_get("active").map_err(map_sqlx_error)?,
            revoked: row.try_get("revoked").map_err(map_sqlx_error)?,
            material_sealed: row.try_get("material_sealed").map_err(map_sqlx_error)?,
            created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        })
    }

    /// Loads and locks the active version for one exact credential series.
    async fn load_active_version(
        conn: &mut ScopedConn<'_>,
        identity: &CredentialIdentity,
    ) -> Result<VersionRow, CredentialError> {
        let row = sqlx::query(
            r#"
            SELECT credential_uid, tenant_id, connection_uid, kind, slot_name, version,
                   active, revoked, material_sealed, created_at
            FROM tenant_credential_versions
            WHERE tenant_id = $1
              AND connection_uid = $2
              AND kind = $3
              AND slot_name = $4
              AND active = TRUE
              AND revoked = FALSE
            FOR SHARE
            "#,
        )
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or(CredentialError::NotFound)?;

        let kind_name: String = row.try_get("kind").map_err(map_sqlx_error)?;
        let slot_name: String = row.try_get("slot_name").map_err(map_sqlx_error)?;
        Ok(VersionRow {
            credential_uid: row.try_get("credential_uid").map_err(map_sqlx_error)?,
            tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
            connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
            kind: CredentialKind::from_str_exact(&kind_name).ok_or_else(|| {
                CredentialError::Storage("stored credential kind is not recognized".to_string())
            })?,
            slot_name: parse_slot_name(slot_name)?,
            version: row.try_get("version").map_err(map_sqlx_error)?,
            active: row.try_get("active").map_err(map_sqlx_error)?,
            revoked: row.try_get("revoked").map_err(map_sqlx_error)?,
            material_sealed: row.try_get("material_sealed").map_err(map_sqlx_error)?,
            created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        })
    }

    /// Locks and returns the active version for one exact series, when present.
    async fn load_active_version_for_update(
        conn: &mut ScopedConn<'_>,
        identity: &CredentialIdentity,
    ) -> Result<Option<VersionRow>, CredentialError> {
        let row = sqlx::query(
            r#"
            SELECT credential_uid, tenant_id, connection_uid, kind, slot_name, version,
                   active, revoked, material_sealed, created_at
            FROM tenant_credential_versions
            WHERE tenant_id = $1
              AND connection_uid = $2
              AND kind = $3
              AND slot_name = $4
              AND active = TRUE
              AND revoked = FALSE
            FOR UPDATE
            "#,
        )
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        row.map(version_row_from_sqlx).transpose()
    }

    /// Loads and locks one exact stored version.
    async fn load_version_for_update(
        conn: &mut ScopedConn<'_>,
        reference: CredentialRef,
    ) -> Result<VersionRow, CredentialError> {
        let row = sqlx::query(
            r#"
            SELECT credential_uid, tenant_id, connection_uid, kind, slot_name, version,
                   active, revoked, material_sealed, created_at
            FROM tenant_credential_versions
            WHERE credential_uid = $1
            FOR UPDATE
            "#,
        )
        .bind(reference.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or(CredentialError::NotFound)?;

        version_row_from_sqlx(row)
    }

    /// Serializes staged mutations for one exact credential series.
    async fn lock_staging_series(
        conn: &mut ScopedConn<'_>,
        identity: &CredentialIdentity,
    ) -> Result<(), CredentialError> {
        let lock_key = format!(
            "{}:{}:{}:{}",
            identity.tenant_id.0,
            identity.connection_uid,
            identity.kind.as_str(),
            identity.slot_name.as_str()
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Loads the exact predecessor recorded by the candidate's activation.
    async fn load_activation_predecessor(
        conn: &mut ScopedConn<'_>,
        candidate: CredentialRef,
        tenant_id: TenantId,
    ) -> Result<Option<CredentialRef>, CredentialError> {
        let rows: Vec<Option<Uuid>> = sqlx::query_scalar(
            r#"
            SELECT expected_prior_credential_uid
            FROM tenant_credential_operations
            WHERE credential_uid = $1
              AND tenant_id = $2
              AND operation = 'activate'
              AND outcome = 'succeeded'
            "#,
        )
        .bind(candidate.as_uuid())
        .bind(tenant_id.0)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let [predecessor] = rows.as_slice() else {
            return Err(CredentialError::VersionConflict);
        };
        Ok(predecessor.map(CredentialRef::from_uuid))
    }

    /// Allocates the next monotonic version number inside the series lock.
    async fn next_version(
        conn: &mut ScopedConn<'_>,
        identity: &CredentialIdentity,
    ) -> Result<i64, CredentialError> {
        let current: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(MAX(version), 0)
            FROM tenant_credential_versions
            WHERE tenant_id = $1
              AND connection_uid = $2
              AND kind = $3
              AND slot_name = $4
            "#,
        )
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        current.checked_add(1).ok_or_else(|| {
            CredentialError::Storage("credential version counter overflow".to_string())
        })
    }

    /// Ensures a stored version belongs to the exact requested credential series.
    fn require_identity(
        row: &VersionRow,
        identity: &CredentialIdentity,
    ) -> Result<(), CredentialError> {
        if row.tenant_id != identity.tenant_id.0 {
            return Err(CredentialError::WrongTenant);
        }
        if row.connection_uid != identity.connection_uid {
            return Err(CredentialError::WrongConnection);
        }
        if row.kind != identity.kind {
            return Err(CredentialError::WrongKind);
        }
        if row.slot_name != identity.slot_name {
            return Err(CredentialError::NotFound);
        }
        Ok(())
    }

    /// Seals material for one credential identity.
    async fn seal(
        &self,
        identity: &CredentialIdentity,
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
            slot_name: row.slot_name.clone(),
        };
        let ctx = credential_encryption_context(&identity);
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
                slot_name: row.slot_name.clone(),
            },
            version: row.version,
            active: row.active,
            revoked: row.revoked,
            created_at: row.created_at,
        }
    }

    /// Builds the host-internal handoff for one staged row.
    fn staging_token_from_row(
        row: &VersionRow,
        expected_prior_credential_uid: Option<Uuid>,
    ) -> CredentialStagingToken {
        CredentialStagingToken::new(
            CredentialRef::from_uuid(row.credential_uid),
            CredentialIdentity {
                tenant_id: TenantId::from(row.tenant_id),
                connection_uid: row.connection_uid,
                kind: row.kind,
                slot_name: row.slot_name.clone(),
            },
            row.version,
            expected_prior_credential_uid.map(CredentialRef::from_uuid),
        )
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
        let (sealed, key_id) = self.seal(&identity, &material).await?;

        let mut conn = self.begin(ctx).await?;
        let credential_uid = Uuid::now_v7();
        match Self::record_operation(&mut conn, ctx, credential_uid, &identity, 1, None).await? {
            AuditRecord::Replay { credential_uid, .. } => {
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
                credential_uid, tenant_id, connection_uid, kind, slot_name,
                version, material_sealed, kms_key_id, active, revoked,
                owner_identity_id
            )
            VALUES ($1, $2, $3, $4, $5, 1, $6, $7, TRUE, FALSE, $8)
            "#,
        )
        .bind(credential_uid)
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
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

    async fn stage(
        &self,
        identity: CredentialIdentity,
        material: SecretString,
        ctx: &CredentialContext,
    ) -> Result<CredentialStagingToken, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::Stage)?;
        if identity.tenant_id != ctx.tenant_id {
            return Err(CredentialError::WrongTenant);
        }

        let mut conn = self.begin(ctx).await?;
        Self::lock_staging_series(&mut conn, &identity).await?;
        if let Some(replay) = Self::load_operation_replay(&mut conn, ctx, &identity).await? {
            let AuditRecord::Replay {
                credential_uid,
                expected_prior_credential_uid,
            } = replay
            else {
                return Err(CredentialError::Storage(
                    "staging replay loaded a fresh audit record".to_string(),
                ));
            };
            let reference = credential_uid.ok_or(CredentialError::NotFound)?;
            let row = Self::load_version(&mut conn, CredentialRef::from_uuid(reference)).await?;
            Self::require_identity(&row, &identity)?;
            conn.commit().await.map_err(map_db_error)?;
            return Ok(Self::staging_token_from_row(
                &row,
                expected_prior_credential_uid,
            ));
        }

        let expected_prior = Self::load_active_version_for_update(&mut conn, &identity)
            .await?
            .map(|row| CredentialRef::from_uuid(row.credential_uid));
        let version = Self::next_version(&mut conn, &identity).await?;
        let credential_uid = Uuid::now_v7();
        let (sealed, key_id) = self.seal(&identity, &material).await?;
        match Self::record_operation(
            &mut conn,
            ctx,
            credential_uid,
            &identity,
            version,
            expected_prior,
        )
        .await?
        {
            AuditRecord::Fresh => {}
            AuditRecord::Replay { .. } => {
                return Err(CredentialError::Storage(
                    "staging operation replayed after its series lock".to_string(),
                ));
            }
        }

        sqlx::query(
            r#"
            INSERT INTO tenant_credential_versions (
                credential_uid, tenant_id, connection_uid, kind, slot_name,
                version, material_sealed, kms_key_id, active, revoked,
                owner_identity_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, FALSE, FALSE, $9)
            "#,
        )
        .bind(credential_uid)
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .bind(version)
        .bind(&sealed)
        .bind(&key_id)
        .bind(ctx.principal.owner_identity())
        .execute(conn.as_mut())
        .await
        .map_err(map_unique_violation)?;

        let row = Self::load_version(&mut conn, CredentialRef::from_uuid(credential_uid)).await?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(Self::staging_token_from_row(
            &row,
            expected_prior.map(CredentialRef::as_uuid),
        ))
    }

    async fn activate_staged(
        &self,
        staged: &CredentialStagingToken,
        ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::Activate)?;
        if staged.identity().tenant_id != ctx.tenant_id {
            return Err(CredentialError::WrongTenant);
        }

        let identity = staged.identity();
        let mut conn = self.begin(ctx).await?;
        Self::lock_staging_series(&mut conn, identity).await?;
        if let Some(replay) = Self::load_operation_replay(&mut conn, ctx, identity).await? {
            let AuditRecord::Replay {
                credential_uid,
                expected_prior_credential_uid,
            } = replay
            else {
                return Err(CredentialError::Storage(
                    "activation replay loaded a fresh audit record".to_string(),
                ));
            };
            if credential_uid != Some(staged.staged_reference().as_uuid())
                || expected_prior_credential_uid
                    != staged.expected_prior_active().map(CredentialRef::as_uuid)
            {
                return Err(CredentialError::IdempotencyConflict);
            }
            let row = Self::load_version(&mut conn, staged.staged_reference()).await?;
            Self::require_identity(&row, identity)?;
            if row.version != staged.version() {
                return Err(CredentialError::IdempotencyConflict);
            }
            if row.revoked {
                return Err(CredentialError::Revoked);
            }
            if !row.active {
                return Err(CredentialError::StaleVersion);
            }
            conn.commit().await.map_err(map_db_error)?;
            return Ok(Self::version_from_row(&row));
        }

        let staged_row =
            Self::load_version_for_update(&mut conn, staged.staged_reference()).await?;
        Self::require_identity(&staged_row, identity)?;
        if staged_row.version != staged.version() {
            return Err(CredentialError::IdempotencyConflict);
        }
        if staged_row.revoked {
            return Err(CredentialError::Revoked);
        }
        if staged_row.active {
            return Err(CredentialError::VersionConflict);
        }

        let active = Self::load_active_version_for_update(&mut conn, identity).await?;
        let active_reference = active
            .as_ref()
            .map(|row| CredentialRef::from_uuid(row.credential_uid));
        if active_reference != staged.expected_prior_active() {
            return Err(CredentialError::VersionConflict);
        }

        match Self::record_operation(
            &mut conn,
            ctx,
            staged_row.credential_uid,
            identity,
            staged_row.version,
            staged.expected_prior_active(),
        )
        .await?
        {
            AuditRecord::Fresh => {}
            AuditRecord::Replay { .. } => {
                return Err(CredentialError::Storage(
                    "activation operation replayed after its series lock".to_string(),
                ));
            }
        }

        if let Some(expected_prior) = staged.expected_prior_active() {
            let deactivated = sqlx::query(
                r#"
                UPDATE tenant_credential_versions
                SET active = FALSE, updated_at = NOW()
                WHERE credential_uid = $1
                  AND tenant_id = $2
                  AND connection_uid = $3
                  AND kind = $4
                  AND slot_name = $5
                  AND active = TRUE
                  AND revoked = FALSE
                "#,
            )
            .bind(expected_prior.as_uuid())
            .bind(identity.tenant_id.0)
            .bind(identity.connection_uid)
            .bind(identity.kind.as_str())
            .bind(identity.slot_name.as_str())
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            if deactivated.rows_affected() != 1 {
                return Err(CredentialError::VersionConflict);
            }
        }

        let activated = sqlx::query(
            r#"
            UPDATE tenant_credential_versions
            SET active = TRUE, updated_at = NOW()
            WHERE credential_uid = $1
              AND tenant_id = $2
              AND connection_uid = $3
              AND kind = $4
              AND slot_name = $5
              AND version = $6
              AND active = FALSE
              AND revoked = FALSE
            "#,
        )
        .bind(staged.staged_reference().as_uuid())
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .bind(staged.version())
        .execute(conn.as_mut())
        .await
        .map_err(map_unique_violation)?;
        if activated.rows_affected() != 1 {
            return Err(CredentialError::VersionConflict);
        }

        let row = Self::load_version(&mut conn, staged.staged_reference()).await?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(Self::version_from_row(&row))
    }

    async fn rollback_activation(
        &self,
        candidate: CredentialRef,
        prior_active: Option<CredentialRef>,
        ctx: &CredentialContext,
    ) -> Result<CredentialVersion, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::RollbackActivation)?;
        if prior_active == Some(candidate) {
            return Err(CredentialError::VersionConflict);
        }

        let mut conn = self.begin(ctx).await?;
        // Read only enough non-secret identity to acquire locks in the same
        // advisory-lock-before-row-lock order as staging and activation.
        let observed_candidate = Self::load_version(&mut conn, candidate).await?;
        let identity = CredentialIdentity {
            tenant_id: TenantId::from(observed_candidate.tenant_id),
            connection_uid: observed_candidate.connection_uid,
            kind: observed_candidate.kind,
            slot_name: observed_candidate.slot_name.clone(),
        };
        Self::lock_staging_series(&mut conn, &identity).await?;
        let candidate_row = Self::load_version_for_update(&mut conn, candidate).await?;
        Self::require_identity(&candidate_row, &identity)?;
        if Self::load_activation_predecessor(&mut conn, candidate, identity.tenant_id).await?
            != prior_active
        {
            return Err(CredentialError::VersionConflict);
        }

        if let Some(replay) = Self::load_operation_replay(&mut conn, ctx, &identity).await? {
            let AuditRecord::Replay {
                credential_uid,
                expected_prior_credential_uid,
            } = replay
            else {
                return Err(CredentialError::Storage(
                    "activation rollback replay loaded a fresh audit record".to_string(),
                ));
            };
            if credential_uid != Some(candidate.as_uuid())
                || expected_prior_credential_uid != prior_active.map(CredentialRef::as_uuid)
            {
                return Err(CredentialError::IdempotencyConflict);
            }
            if candidate_row.active || !candidate_row.revoked {
                return Err(CredentialError::VersionConflict);
            }
            let active = Self::load_active_version_for_update(&mut conn, &identity).await?;
            let active_reference = active
                .as_ref()
                .map(|row| CredentialRef::from_uuid(row.credential_uid));
            if active_reference != prior_active {
                return Err(CredentialError::VersionConflict);
            }
            conn.commit().await.map_err(map_db_error)?;
            return Ok(Self::version_from_row(&candidate_row));
        }

        if candidate_row.revoked {
            return Err(CredentialError::Revoked);
        }
        if !candidate_row.active {
            return Err(CredentialError::StaleVersion);
        }

        if let Some(prior) = prior_active {
            let prior_row = Self::load_version_for_update(&mut conn, prior).await?;
            Self::require_identity(&prior_row, &identity)?;
            if prior_row.revoked {
                return Err(CredentialError::Revoked);
            }
            if prior_row.active {
                return Err(CredentialError::VersionConflict);
            }
        }

        match Self::record_operation(
            &mut conn,
            ctx,
            candidate_row.credential_uid,
            &identity,
            candidate_row.version,
            prior_active,
        )
        .await?
        {
            AuditRecord::Fresh => {}
            AuditRecord::Replay { .. } => {
                return Err(CredentialError::Storage(
                    "activation rollback replayed after its series lock".to_string(),
                ));
            }
        }

        let deactivated = sqlx::query(
            r#"
            UPDATE tenant_credential_versions
            SET active = FALSE, revoked = TRUE, revoked_at = NOW(), updated_at = NOW()
            WHERE credential_uid = $1
              AND tenant_id = $2
              AND connection_uid = $3
              AND kind = $4
              AND slot_name = $5
              AND version = $6
              AND active = TRUE
              AND revoked = FALSE
            "#,
        )
        .bind(candidate.as_uuid())
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .bind(candidate_row.version)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if deactivated.rows_affected() != 1 {
            return Err(CredentialError::VersionConflict);
        }

        if let Some(prior) = prior_active {
            let restored = sqlx::query(
                r#"
                UPDATE tenant_credential_versions
                SET active = TRUE, updated_at = NOW()
                WHERE credential_uid = $1
                  AND tenant_id = $2
                  AND connection_uid = $3
                  AND kind = $4
                  AND slot_name = $5
                  AND active = FALSE
                  AND revoked = FALSE
                "#,
            )
            .bind(prior.as_uuid())
            .bind(identity.tenant_id.0)
            .bind(identity.connection_uid)
            .bind(identity.kind.as_str())
            .bind(identity.slot_name.as_str())
            .execute(conn.as_mut())
            .await
            .map_err(map_unique_violation)?;
            if restored.rows_affected() != 1 {
                return Err(CredentialError::VersionConflict);
            }
        }

        let row = Self::load_version(&mut conn, candidate).await?;
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
        let identity = CredentialIdentity {
            tenant_id: TenantId::from(row.tenant_id),
            connection_uid: row.connection_uid,
            kind: row.kind,
            slot_name: row.slot_name.clone(),
        };

        Self::record_operation(
            &mut conn,
            ctx,
            row.credential_uid,
            &identity,
            row.version,
            None,
        )
        .await?;
        // The audit row is committed before any plaintext exists in memory, so a
        // resolution can never be observed by the caller without a durable record.
        conn.commit().await.map_err(map_db_error)?;

        self.open(&row).await
    }

    async fn has_active(
        &self,
        identity: &CredentialIdentity,
        ctx: &CredentialContext,
    ) -> Result<bool, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::Resolve)?;
        if identity.tenant_id != ctx.tenant_id {
            return Err(CredentialError::WrongTenant);
        }

        let mut conn = self.begin(ctx).await?;
        let active: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM tenant_credential_versions
                WHERE tenant_id = $1
                  AND connection_uid = $2
                  AND kind = $3
                  AND slot_name = $4
                  AND active = TRUE
                  AND revoked = FALSE
            )
            "#,
        )
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        Self::record_status_operation(&mut conn, ctx, identity).await?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(active)
    }

    async fn has_active_batch(
        &self,
        identities: &[CredentialIdentity],
        ctx: &CredentialContext,
    ) -> Result<Vec<bool>, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::Resolve)?;
        if identities
            .iter()
            .any(|identity| identity.tenant_id != ctx.tenant_id)
        {
            return Err(CredentialError::WrongTenant);
        }
        if identities.is_empty() {
            return Ok(Vec::new());
        }

        let connection_uids = identities
            .iter()
            .map(|identity| identity.connection_uid)
            .collect::<Vec<_>>();
        let kinds = identities
            .iter()
            .map(|identity| identity.kind.as_str())
            .collect::<Vec<_>>();
        let slot_names = identities
            .iter()
            .map(|identity| identity.slot_name.as_str())
            .collect::<Vec<_>>();
        let mut conn = self.begin(ctx).await?;
        let active = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM tenant_credential_versions AS stored
                WHERE stored.tenant_id = $4
                  AND stored.connection_uid = requested.connection_uid
                  AND stored.kind = requested.kind
                  AND stored.slot_name = requested.slot_name
                  AND stored.active = TRUE
                  AND stored.revoked = FALSE
            )
            FROM unnest($1::UUID[], $2::TEXT[], $3::TEXT[])
                 WITH ORDINALITY AS requested(connection_uid, kind, slot_name, position)
            ORDER BY requested.position
            "#,
        )
        .bind(&connection_uids)
        .bind(&kinds)
        .bind(&slot_names)
        .bind(ctx.tenant_id.0)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(active)
    }

    async fn resolve_active(
        &self,
        identity: &CredentialIdentity,
        ctx: &CredentialContext,
    ) -> Result<RedactedSecret, CredentialError> {
        Self::authorize_principal(ctx)?;
        if identity.tenant_id != ctx.tenant_id {
            return Err(CredentialError::WrongTenant);
        }

        let mut conn = self.begin(ctx).await?;
        let selected = Self::load_active_version(&mut conn, identity).await?;
        let row = match Self::record_operation(
            &mut conn,
            ctx,
            selected.credential_uid,
            identity,
            selected.version,
            None,
        )
        .await?
        {
            AuditRecord::Fresh => selected,
            AuditRecord::Replay { credential_uid, .. } => {
                let credential_uid = credential_uid.ok_or(CredentialError::NotFound)?;
                let replayed =
                    Self::load_version(&mut conn, CredentialRef::from_uuid(credential_uid)).await?;
                Self::require_identity(&replayed, identity)?;
                if replayed.revoked {
                    return Err(CredentialError::Revoked);
                }
                if !replayed.active {
                    return Err(CredentialError::StaleVersion);
                }
                replayed
            }
        };
        // Commit the selected version and caller provenance before material is
        // decrypted into process memory. A decryption failure still leaves the
        // durable evidence that the resolution was attempted.
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
                   stored.slot_name,
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
                let slot_name: String = row.try_get("slot_name").map_err(map_sqlx_error)?;
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
                            slot_name: parse_slot_name(slot_name)?,
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
            slot_name: existing.slot_name.clone(),
        };
        let next_version = existing.version + 1;
        let credential_uid = Uuid::now_v7();

        match Self::record_operation(
            &mut conn,
            ctx,
            credential_uid,
            &identity,
            next_version,
            None,
        )
        .await?
        {
            AuditRecord::Replay { credential_uid, .. } => {
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

        let (sealed, key_id) = self.seal(&identity, &material).await?;
        sqlx::query(
            r#"
            INSERT INTO tenant_credential_versions (
                credential_uid, tenant_id, connection_uid, kind, slot_name,
                version, material_sealed, kms_key_id, active, revoked,
                owner_identity_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, TRUE, FALSE, $9)
            "#,
        )
        .bind(credential_uid)
        .bind(identity.tenant_id.0)
        .bind(identity.connection_uid)
        .bind(identity.kind.as_str())
        .bind(identity.slot_name.as_str())
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
        let identity = CredentialIdentity {
            tenant_id: TenantId::from(existing.tenant_id),
            connection_uid: existing.connection_uid,
            kind: existing.kind,
            slot_name: existing.slot_name.clone(),
        };

        match Self::record_operation(
            &mut conn,
            ctx,
            existing.credential_uid,
            &identity,
            existing.version,
            None,
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

    async fn revoke_connection(
        &self,
        connection_uid: Uuid,
        ctx: &CredentialContext,
    ) -> Result<u64, CredentialError> {
        Self::authorize_operation(ctx, CredentialOperation::Revoke)?;

        let mut conn = self.begin(ctx).await?;
        if !Self::reserve_connection_revoke(&mut conn, ctx, connection_uid).await? {
            conn.commit().await.map_err(map_db_error)?;
            return Ok(0);
        }

        let revoked = sqlx::query(
            r#"
            UPDATE tenant_credential_versions
            SET active = FALSE, revoked = TRUE, revoked_at = NOW(), updated_at = NOW()
            WHERE tenant_id = $1
              AND connection_uid = $2
              AND revoked = FALSE
            "#,
        )
        .bind(ctx.tenant_id.0)
        .bind(connection_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await.map_err(map_db_error)?;
        Ok(revoked)
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
/// Binding tenant, connection, kind, and slot means a row copied or relabelled
/// into another credential series cannot be decrypted at all. The conventional
/// primary slot retains the pre-slot record identifier so credentials sealed
/// before named slots were introduced remain resolvable after migration.
fn credential_encryption_context(identity: &CredentialIdentity) -> EncryptionContext {
    let record_id = if identity.slot_name == CredentialSlotName::PRIMARY {
        identity.kind.as_str().to_string()
    } else {
        format!(
            "{}:slot:{}",
            identity.kind.as_str(),
            identity.slot_name.as_str()
        )
    };
    EncryptionContext::new(
        identity.tenant_id.0,
        identity.connection_uid,
        record_id,
        CREDENTIAL_PII_CLASS,
    )
}

/// Parses a persisted slot name without permitting a storage default.
fn parse_slot_name(value: String) -> Result<CredentialSlotName, CredentialError> {
    CredentialSlotName::try_from(value).map_err(|_| {
        CredentialError::Storage("stored credential slot name is not recognized".to_string())
    })
}

/// Converts one complete credential-version query row into the internal shape.
fn version_row_from_sqlx(row: PgRow) -> Result<VersionRow, CredentialError> {
    let kind_name: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let slot_name: String = row.try_get("slot_name").map_err(map_sqlx_error)?;
    Ok(VersionRow {
        credential_uid: row.try_get("credential_uid").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
        kind: CredentialKind::from_str_exact(&kind_name).ok_or_else(|| {
            CredentialError::Storage("stored credential kind is not recognized".to_string())
        })?,
        slot_name: parse_slot_name(slot_name)?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        active: row.try_get("active").map_err(map_sqlx_error)?,
        revoked: row.try_get("revoked").map_err(map_sqlx_error)?,
        material_sealed: row.try_get("material_sealed").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
    })
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
