//! Persistent, Postgres-backed [`KeyManagementProvider`].

use async_trait::async_trait;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::key_wrap::{WRAPPING_KEY_LEN, generate_key, unwrap_key, wrap_key};
use moa_crypto::kms::validate_single_subject_batch;
use moa_crypto::{
    DEK_LEN, DataKeyDecryptRequest, EncryptionContext, Error as CryptoError, GeneratedDataKey,
    KeyHandle, KeyManagementProvider, PlaintextDek, WrappedDek,
};
use moa_db::ScopedConn;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::KmsError;
use crate::root_key::RootKeyRing;

const HANDLE_PREFIX: &str = "pg-kek";
const KEK_WRAP_AAD_DOMAIN: &[u8] = b"moa-kms/kek-wrap/v1";
const DEK_WRAP_AAD_DOMAIN: &[u8] = b"moa-kms/dek-wrap/v1";
const MAX_REWRAP_BATCH: u32 = 1_000;

/// Shared database state selecting the root-key generation used for new KEKs.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct RootKeyState {
    /// Database-selected active generation.
    pub active_generation: String,
    /// Monotonic compare-and-swap version.
    pub state_version: i64,
}

/// A [`KeyManagementProvider`] that persists per-subject KEKs in Postgres.
///
/// The immutable [`RootKeyRing`] is process-local secret material, while the
/// active generation, every KEK's wrapping generation, and rewrap progress live
/// in Postgres. Any Kubernetes replica can therefore encrypt, decrypt, or run a
/// bounded rewrap job without sticky routing or a KEK cache.
pub struct PostgresKmsProvider {
    pool: PgPool,
    root_keys: RootKeyRing,
}

#[derive(sqlx::FromRow)]
struct KekRow {
    kek_id: Uuid,
    wrapped_kek: Option<Vec<u8>>,
    root_key_generation: String,
    destroyed: bool,
}

#[derive(sqlx::FromRow)]
struct RewrapRow {
    kek_id: Uuid,
    tenant_id: Uuid,
    subject_id: Uuid,
    wrapped_kek: Vec<u8>,
    root_key_generation: String,
    rewrap_version: i64,
}

impl PostgresKmsProvider {
    /// Construct a provider over `pool` with all mounted root-key generations.
    #[must_use]
    pub fn new(pool: PgPool, root_keys: RootKeyRing) -> Self {
        Self { pool, root_keys }
    }

    /// Check whether this pod's keyring is compatible with shared KMS state.
    ///
    /// Readiness fails unless the database active generation equals the pod's
    /// configured required generation and every generation referenced by a live
    /// KEK is mounted in this process.
    pub async fn check_compatibility(&self) -> Result<(), KmsError> {
        let state = self.check_mounted_compatibility().await?;

        if state.active_generation != self.root_keys.required_generation() {
            return Err(KmsError::RequiredGenerationInactive {
                active: state.active_generation,
                required: self.root_keys.required_generation().to_string(),
            });
        }
        Ok(())
    }

    /// Check that the database-active and live-KEK generations are mounted.
    ///
    /// Unlike [`Self::check_compatibility`], this maintenance check permits the
    /// database active generation to differ from the configured required
    /// generation so a rotation job can activate the required generation.
    pub async fn check_mounted_compatibility(&self) -> Result<RootKeyState, KmsError> {
        let mut conn = self.begin_control_plane().await?;
        self.ensure_state(conn.as_mut()).await?;
        let state = current_state_for_share(conn.as_mut()).await?;
        let mut referenced = referenced_generations(conn.as_mut()).await?;
        conn.commit().await.map_err(map_db_kms)?;

        if !referenced.contains(&state.active_generation) {
            referenced.push(state.active_generation.clone());
        }
        self.require_mounted(&referenced)?;
        Ok(state)
    }

    /// Return the shared active generation and its CAS version.
    pub async fn root_key_state(&self) -> Result<RootKeyState, KmsError> {
        let mut conn = self.begin_control_plane().await?;
        self.ensure_state(conn.as_mut()).await?;
        let state = current_state(conn.as_mut()).await?;
        conn.commit().await.map_err(map_db_kms)?;
        Ok(state)
    }

    /// Activate a mounted generation for all new KEKs.
    ///
    /// The update locks and compare-and-swaps the singleton state row. Existing
    /// KEKs retain their recorded generation until a rewrap job moves them.
    pub async fn activate_generation(&self, generation: &str) -> Result<RootKeyState, KmsError> {
        self.root_keys.material(generation)?;
        let mut conn = self.begin_control_plane().await?;
        self.ensure_state(conn.as_mut()).await?;
        let current = current_state_for_update(conn.as_mut()).await?;
        self.root_keys.material(&current.active_generation)?;
        self.require_mounted(&referenced_generations(conn.as_mut()).await?)?;

        sqlx::query(
            "INSERT INTO moa.kms_root_key_generations (generation) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        let retired: Option<bool> = sqlx::query_scalar(
            "SELECT retired_at IS NOT NULL FROM moa.kms_root_key_generations WHERE generation = $1",
        )
        .bind(generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        if retired == Some(true) {
            return Err(KmsError::RootKeyGenerationRetired(generation.to_string()));
        }

        if current.active_generation == generation {
            conn.commit().await.map_err(map_db_kms)?;
            return Ok(current);
        }
        let result = sqlx::query(
            r#"
            UPDATE moa.kms_root_key_state
            SET active_generation = $1, state_version = state_version + 1, updated_at = NOW()
            WHERE singleton = TRUE AND state_version = $2
            "#,
        )
        .bind(generation)
        .bind(current.state_version)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        if result.rows_affected() != 1 {
            return Err(KmsError::GenerationConflict);
        }
        sqlx::query(
            "UPDATE moa.kms_root_key_generations SET activated_at = NOW() WHERE generation = $1",
        )
        .bind(generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        conn.commit().await.map_err(map_db_kms)?;
        Ok(RootKeyState {
            active_generation: generation.to_string(),
            state_version: current.state_version + 1,
        })
    }

    /// Rewrap at most `limit` live KEKs under the database-active generation.
    ///
    /// Rows are claimed with `FOR UPDATE SKIP LOCKED`; the generation and
    /// `rewrap_version` predicate form a CAS fence. A zero limit is a no-op and
    /// larger values are capped to keep every transaction bounded.
    pub async fn rewrap_batch(&self, limit: u32) -> Result<u64, KmsError> {
        let limit = limit.min(MAX_REWRAP_BATCH);
        if limit == 0 {
            return Ok(0);
        }

        let mut conn = self.begin_control_plane().await?;
        self.ensure_state(conn.as_mut()).await?;
        let state = current_state_for_share(conn.as_mut()).await?;
        let rows: Vec<RewrapRow> = sqlx::query_as(
            r#"
            SELECT kek_id, tenant_id, subject_id, wrapped_kek,
                   root_key_generation, rewrap_version
            FROM moa.kek
            WHERE destroyed_at IS NULL
              AND root_key_generation <> $1
            ORDER BY kek_id
            LIMIT $2
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(&state.active_generation)
        .bind(i64::from(limit))
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;

        for row in &rows {
            let kek = self.unwrap_kek(
                row.tenant_id,
                row.subject_id,
                row.kek_id,
                &row.root_key_generation,
                &row.wrapped_kek,
            )?;
            let wrapped = self.wrap_kek(
                row.tenant_id,
                row.subject_id,
                row.kek_id,
                &state.active_generation,
                &kek,
            )?;
            let result = sqlx::query(
                r#"
                UPDATE moa.kek
                SET wrapped_kek = $1,
                    root_key_generation = $2,
                    rewrap_version = rewrap_version + 1,
                    rewrapped_at = NOW()
                WHERE kek_id = $3
                  AND root_key_generation = $4
                  AND rewrap_version = $5
                "#,
            )
            .bind(wrapped)
            .bind(&state.active_generation)
            .bind(row.kek_id)
            .bind(&row.root_key_generation)
            .bind(row.rewrap_version)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_kms)?;
            if result.rows_affected() != 1 {
                return Err(KmsError::GenerationConflict);
            }
        }
        conn.commit().await.map_err(map_db_kms)?;
        Ok(rows.len() as u64)
    }

    /// Retire an inactive generation after every live KEK has been rewrapped.
    pub async fn retire_generation(&self, generation: &str) -> Result<(), KmsError> {
        let mut conn = self.begin_control_plane().await?;
        self.ensure_state(conn.as_mut()).await?;
        let state = current_state_for_update(conn.as_mut()).await?;
        if state.active_generation == generation {
            return Err(KmsError::ActiveGenerationRetirement(generation.to_string()));
        }
        let references: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.kek WHERE destroyed_at IS NULL AND root_key_generation = $1",
        )
        .bind(generation)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        if references > 0 {
            return Err(KmsError::RootKeyGenerationReferenced {
                generation: generation.to_string(),
                references,
            });
        }
        let result = sqlx::query(
            "UPDATE moa.kms_root_key_generations SET retired_at = COALESCE(retired_at, NOW()) WHERE generation = $1",
        )
        .bind(generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_kms)?;
        if result.rows_affected() != 1 {
            return Err(KmsError::RootKeyGenerationMissing(generation.to_string()));
        }
        conn.commit().await.map_err(map_db_kms)?;
        Ok(())
    }

    fn handle_for(tenant_id: Uuid, subject_id: Uuid, kek_id: Uuid) -> KeyHandle {
        KeyHandle::new(format!("{HANDLE_PREFIX}:{tenant_id}:{subject_id}:{kek_id}"))
    }

    fn parse_handle(handle: &KeyHandle) -> Result<(Uuid, Uuid, Uuid), KmsError> {
        let mut parts = handle.as_str().split(':');
        if parts.next() != Some(HANDLE_PREFIX) {
            return Err(KmsError::InvalidHandle);
        }
        let tenant = parts.next().ok_or(KmsError::InvalidHandle)?;
        let subject = parts.next().ok_or(KmsError::InvalidHandle)?;
        let kek = parts.next().ok_or(KmsError::InvalidHandle)?;
        if parts.next().is_some() {
            return Err(KmsError::InvalidHandle);
        }
        Ok((
            Uuid::parse_str(tenant).map_err(|_| KmsError::InvalidHandle)?,
            Uuid::parse_str(subject).map_err(|_| KmsError::InvalidHandle)?,
            Uuid::parse_str(kek).map_err(|_| KmsError::InvalidHandle)?,
        ))
    }

    fn wrap_kek(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
        kek_id: Uuid,
        generation: &str,
        kek: &[u8; WRAPPING_KEY_LEN],
    ) -> Result<Vec<u8>, KmsError> {
        wrap_key(
            self.root_keys.material(generation)?,
            kek,
            &kek_wrap_aad(tenant_id, subject_id, kek_id),
        )
        .map_err(|_| KmsError::KekWrap)
    }

    fn unwrap_kek(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
        kek_id: Uuid,
        generation: &str,
        wrapped: &[u8],
    ) -> Result<Zeroizing<[u8; WRAPPING_KEY_LEN]>, KmsError> {
        let opened = unwrap_key(
            self.root_keys.material(generation)?,
            wrapped,
            &kek_wrap_aad(tenant_id, subject_id, kek_id),
        )
        .map_err(|error| {
            if matches!(error, CryptoError::MalformedWrappedKey) {
                KmsError::MalformedWrappedKek
            } else {
                KmsError::KekUnwrap
            }
        })?;
        let kek = opened
            .as_slice()
            .try_into()
            .map_err(|_| KmsError::KekUnwrap)?;
        Ok(Zeroizing::new(kek))
    }

    async fn begin_tenant(&self, tenant_id: Uuid) -> Result<ScopedConn<'_>, CryptoError> {
        ScopedConn::begin_as_app(
            &self.pool,
            &RlsContext::tenant(TenantId::from(tenant_id)),
            true,
        )
        .await
        .map_err(map_db)
    }

    async fn begin_control_plane(&self) -> Result<ScopedConn<'_>, KmsError> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_db_kms)?;
        conn.assume_app_role().await.map_err(map_db_kms)?;
        Ok(conn)
    }

    async fn ensure_state(&self, conn: &mut PgConnection) -> Result<(), KmsError> {
        let initialized: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.kms_root_key_state WHERE singleton = TRUE)",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_kms)?;
        if initialized {
            return Ok(());
        }

        let required = self.root_keys.required_generation();
        sqlx::query(
            "INSERT INTO moa.kms_root_key_generations (generation) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(required)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_kms)?;
        sqlx::query(
            r#"
            INSERT INTO moa.kms_root_key_state (singleton, active_generation)
            VALUES (TRUE, $1)
            ON CONFLICT (singleton) DO NOTHING
            "#,
        )
        .bind(required)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_kms)?;
        sqlx::query(
            r#"
            UPDATE moa.kms_root_key_generations
            SET activated_at = COALESCE(activated_at, NOW())
            WHERE generation = (
                SELECT active_generation
                FROM moa.kms_root_key_state
                WHERE singleton = TRUE
            )
            "#,
        )
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_kms)?;
        Ok(())
    }

    fn require_mounted(&self, generations: &[String]) -> Result<(), KmsError> {
        for generation in generations {
            self.root_keys.material(generation)?;
        }
        Ok(())
    }

    async fn load_or_create_kek(
        &self,
        conn: &mut PgConnection,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> Result<(KekRow, Zeroizing<[u8; WRAPPING_KEY_LEN]>), CryptoError> {
        self.ensure_state(conn).await?;
        let state = current_state_for_share(conn).await?;
        let candidate_id = Uuid::new_v4();
        let candidate_kek = generate_key();
        let wrapped_candidate = self.wrap_kek(
            tenant_id,
            subject_id,
            candidate_id,
            &state.active_generation,
            &candidate_kek,
        )?;
        sqlx::query(
            r#"
            INSERT INTO moa.kek
                (kek_id, tenant_id, subject_id, wrapped_kek, root_key_generation)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (tenant_id, subject_id) DO NOTHING
            "#,
        )
        .bind(candidate_id)
        .bind(tenant_id)
        .bind(subject_id)
        .bind(wrapped_candidate)
        .bind(&state.active_generation)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
        let row: KekRow = sqlx::query_as(
            r#"
            SELECT kek_id, wrapped_kek, root_key_generation,
                   (destroyed_at IS NOT NULL) AS destroyed
            FROM moa.kek
            WHERE tenant_id = $1 AND subject_id = $2
            "#,
        )
        .bind(tenant_id)
        .bind(subject_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx)?;
        let handle = Self::handle_for(tenant_id, subject_id, row.kek_id);
        if row.destroyed {
            return Err(CryptoError::CryptoShredded(handle));
        }
        let wrapped = row
            .wrapped_kek
            .as_deref()
            .ok_or(KmsError::MalformedWrappedKek)?;
        let kek = self.unwrap_kek(
            tenant_id,
            subject_id,
            row.kek_id,
            &row.root_key_generation,
            wrapped,
        )?;
        Ok((row, kek))
    }

    async fn tombstone_subject(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> Result<(), CryptoError> {
        let mut conn = self.begin_tenant(tenant_id).await?;
        self.ensure_state(conn.as_mut()).await?;
        let state = current_state_for_share(conn.as_mut()).await?;
        sqlx::query(
            r#"
            INSERT INTO moa.kek
                (kek_id, tenant_id, subject_id, wrapped_kek, root_key_generation, destroyed_at)
            VALUES ($1, $2, $3, NULL, $4, NOW())
            ON CONFLICT (tenant_id, subject_id) DO UPDATE SET
                destroyed_at = COALESCE(moa.kek.destroyed_at, NOW()),
                wrapped_kek = NULL
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(tenant_id)
        .bind(subject_id)
        .bind(&state.active_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await.map_err(map_db)?;
        tracing::info!(%tenant_id, %subject_id, "crypto-shred: tombstoned subject KEK");
        Ok(())
    }
}

#[async_trait]
impl KeyManagementProvider for PostgresKmsProvider {
    async fn generate_data_keys(
        &self,
        contexts: &[EncryptionContext],
    ) -> Result<Vec<GeneratedDataKey>, CryptoError> {
        let Some(first) = contexts.first() else {
            return Ok(Vec::new());
        };
        validate_single_subject_batch(contexts.iter())?;
        let mut conn = self.begin_tenant(first.tenant_id).await?;
        let (row, kek) = self
            .load_or_create_kek(conn.as_mut(), first.tenant_id, first.subject_id)
            .await?;
        conn.commit().await.map_err(map_db)?;
        let handle = Self::handle_for(first.tenant_id, first.subject_id, row.kek_id);

        contexts
            .iter()
            .map(|ctx| {
                let dek = generate_key();
                let wrapped = wrap_key(&kek, dek.as_ref(), &dek_wrap_aad(ctx))?;
                Ok(GeneratedDataKey {
                    plaintext: PlaintextDek::new(*dek),
                    wrapped: WrappedDek::new(wrapped),
                    handle: handle.clone(),
                })
            })
            .collect()
    }

    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> Result<Vec<PlaintextDek>, CryptoError> {
        let Some(first) = requests.first() else {
            return Ok(Vec::new());
        };
        validate_single_subject_batch(requests.iter().map(|request| &request.context))?;
        let (tenant_id, subject_id, kek_id) = Self::parse_handle(&first.handle)?;
        if tenant_id != first.context.tenant_id || subject_id != first.context.subject_id {
            return Err(CryptoError::ContextMismatch);
        }
        for request in requests.iter().skip(1) {
            if Self::parse_handle(&request.handle)? != (tenant_id, subject_id, kek_id) {
                return Err(CryptoError::InvalidBatch(
                    "all requests must use the same key handle".to_string(),
                ));
            }
        }

        let mut conn = self.begin_tenant(tenant_id).await?;
        let row: Option<KekRow> = sqlx::query_as(
            r#"
            SELECT kek_id, wrapped_kek, root_key_generation,
                   (destroyed_at IS NOT NULL) AS destroyed
            FROM moa.kek
            WHERE kek_id = $1 AND tenant_id = $2 AND subject_id = $3
            "#,
        )
        .bind(kek_id)
        .bind(tenant_id)
        .bind(subject_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let Some(row) = row else {
            return Err(CryptoError::UnknownKey(first.handle.clone()));
        };
        if row.destroyed {
            return Err(CryptoError::CryptoShredded(first.handle.clone()));
        }
        let wrapped_kek = row
            .wrapped_kek
            .as_deref()
            .ok_or(KmsError::MalformedWrappedKek)?;
        let kek = self.unwrap_kek(
            tenant_id,
            subject_id,
            row.kek_id,
            &row.root_key_generation,
            wrapped_kek,
        )?;
        conn.commit().await.map_err(map_db)?;

        requests
            .iter()
            .map(|request| {
                let dek = unwrap_key(
                    &kek,
                    request.wrapped.as_bytes(),
                    &dek_wrap_aad(&request.context),
                )?;
                let dek: [u8; DEK_LEN] = dek
                    .as_slice()
                    .try_into()
                    .map_err(|_| CryptoError::Decryption)?;
                Ok(PlaintextDek::new(dek))
            })
            .collect()
    }

    async fn destroy_key(&self, handle: &KeyHandle) -> Result<(), CryptoError> {
        let (tenant_id, subject_id, kek_id) = Self::parse_handle(handle)?;
        let mut conn = self.begin_tenant(tenant_id).await?;
        sqlx::query(
            r#"
            UPDATE moa.kek
            SET destroyed_at = COALESCE(destroyed_at, NOW()), wrapped_kek = NULL
            WHERE kek_id = $1 AND tenant_id = $2 AND subject_id = $3
            "#,
        )
        .bind(kek_id)
        .bind(tenant_id)
        .bind(subject_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn destroy_subject_key(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> Result<(), CryptoError> {
        self.tombstone_subject(tenant_id, subject_id).await
    }

    fn is_durable(&self) -> bool {
        true
    }
}

async fn current_state(conn: &mut PgConnection) -> Result<RootKeyState, KmsError> {
    sqlx::query_as(
        "SELECT active_generation, state_version FROM moa.kms_root_key_state WHERE singleton = TRUE",
    )
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_kms)
}

async fn current_state_for_share(conn: &mut PgConnection) -> Result<RootKeyState, KmsError> {
    sqlx::query_as(
        "SELECT active_generation, state_version FROM moa.kms_root_key_state WHERE singleton = TRUE FOR SHARE",
    )
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_kms)
}

async fn current_state_for_update(conn: &mut PgConnection) -> Result<RootKeyState, KmsError> {
    sqlx::query_as(
        "SELECT active_generation, state_version FROM moa.kms_root_key_state WHERE singleton = TRUE FOR UPDATE",
    )
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_kms)
}

async fn referenced_generations(conn: &mut PgConnection) -> Result<Vec<String>, KmsError> {
    sqlx::query_scalar(
        "SELECT DISTINCT root_key_generation FROM moa.kek WHERE destroyed_at IS NULL ORDER BY root_key_generation",
    )
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_kms)
}

fn kek_wrap_aad(tenant_id: Uuid, subject_id: Uuid, kek_id: Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(KEK_WRAP_AAD_DOMAIN.len() + 48);
    aad.extend_from_slice(KEK_WRAP_AAD_DOMAIN);
    aad.extend_from_slice(tenant_id.as_bytes());
    aad.extend_from_slice(subject_id.as_bytes());
    aad.extend_from_slice(kek_id.as_bytes());
    aad
}

fn dek_wrap_aad(ctx: &EncryptionContext) -> Vec<u8> {
    fn push(out: &mut Vec<u8>, field: &[u8]) {
        out.extend_from_slice(&(field.len() as u64).to_be_bytes());
        out.extend_from_slice(field);
    }
    let mut aad = Vec::with_capacity(
        DEK_WRAP_AAD_DOMAIN.len() + 4 * 8 + 32 + ctx.record_id.len() + ctx.pii_class.len(),
    );
    aad.extend_from_slice(DEK_WRAP_AAD_DOMAIN);
    push(&mut aad, ctx.tenant_id.as_bytes());
    push(&mut aad, ctx.subject_id.as_bytes());
    push(&mut aad, ctx.record_id.as_bytes());
    push(&mut aad, ctx.pii_class.as_bytes());
    aad
}

fn map_db(error: moa_core::error::MoaError) -> CryptoError {
    KmsError::Database(error.to_string()).into()
}

fn map_db_kms(error: moa_core::error::MoaError) -> KmsError {
    KmsError::Database(error.to_string())
}

fn map_sqlx(error: sqlx::Error) -> CryptoError {
    KmsError::Database(error.to_string()).into()
}

fn map_sqlx_kms(error: sqlx::Error) -> KmsError {
    KmsError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    use super::*;

    fn ring() -> RootKeyRing {
        RootKeyRing::from_directory_entries(
            PathBuf::from("/keys"),
            "primary",
            [("primary", BASE64.encode([1_u8; WRAPPING_KEY_LEN]))],
        )
        .expect("keyring")
    }

    #[test]
    fn handle_round_trips_offline() {
        // Pins: a handle encodes and parses back to the same identity triple.
        let tenant = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let kek = Uuid::new_v4();
        let handle = PostgresKmsProvider::handle_for(tenant, subject, kek);
        assert_eq!(
            PostgresKmsProvider::parse_handle(&handle).expect("parse"),
            (tenant, subject, kek)
        );
    }

    #[test]
    fn malformed_handle_is_rejected_offline() {
        // Pins: handles lacking the prefix or exact three UUID segments fail.
        for bad in ["", "pg-kek:", "pg-kek:not-a-uuid:x:y", "local-kek:a:b"] {
            assert!(PostgresKmsProvider::parse_handle(&KeyHandle::new(bad)).is_err());
        }
    }

    #[tokio::test]
    async fn postgres_provider_reports_durable_offline() {
        // Pins: Postgres is durable even though the lazy pool never connects.
        let pool = PgPool::connect_lazy("postgres://moa:moa@localhost/moa").expect("lazy pool");
        assert!(PostgresKmsProvider::new(pool, ring()).is_durable());
    }

    #[test]
    fn dek_aad_binds_every_context_field_offline() {
        // Pins: every context field participates in DEK-wrap authentication.
        let base = EncryptionContext::new(Uuid::nil(), Uuid::nil(), "rec", "restricted");
        let baseline = dek_wrap_aad(&base);
        for other in [
            EncryptionContext::new(Uuid::from_u128(1), Uuid::nil(), "rec", "restricted"),
            EncryptionContext::new(Uuid::nil(), Uuid::from_u128(1), "rec", "restricted"),
            EncryptionContext::new(Uuid::nil(), Uuid::nil(), "rec2", "restricted"),
            EncryptionContext::new(Uuid::nil(), Uuid::nil(), "rec", "phi"),
        ] {
            assert_ne!(baseline, dek_wrap_aad(&other));
        }
    }
}
