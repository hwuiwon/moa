//! Durable hand lease storage for sandbox lifecycle recovery.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{HandHandle, MoaError, Result, SandboxTier, SessionId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, types::Json};
#[cfg(test)]
use tokio::sync::Mutex;

/// Serialized hand handle plus provider-specific metadata needed to reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHandle {
    /// Existing provider handle used by `HandProvider` calls.
    pub handle: HandHandle,
    /// Provider-specific reconnect metadata, such as local bind mount roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
}

impl LeaseHandle {
    /// Creates a durable lease handle without extra provider metadata.
    #[must_use]
    pub fn new(handle: HandHandle) -> Self {
        Self {
            handle,
            provider_metadata: None,
        }
    }

    /// Creates a durable lease handle with provider-specific metadata.
    #[must_use]
    pub fn with_metadata(handle: HandHandle, provider_metadata: serde_json::Value) -> Self {
        Self {
            handle,
            provider_metadata: Some(provider_metadata),
        }
    }
}

/// Durable lifecycle state for a hand lease row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandLeaseStatus {
    /// A replica owns provisioning for this generation.
    Provisioning,
    /// The persisted handle is valid for reuse.
    Active,
    /// The persisted handle could not be resumed and should be replaced.
    Stale,
    /// The hand has been destroyed.
    Destroyed,
    /// Provisioning failed for the current generation.
    Failed,
}

impl HandLeaseStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Destroyed => "destroyed",
            Self::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "provisioning" => Ok(Self::Provisioning),
            "active" => Ok(Self::Active),
            "stale" => Ok(Self::Stale),
            "destroyed" => Ok(Self::Destroyed),
            "failed" => Ok(Self::Failed),
            other => Err(MoaError::StorageError(format!(
                "unknown hand lease status: {other}"
            ))),
        }
    }
}

/// One durable hand lease row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLease {
    /// Session that owns the hand.
    pub session_id: SessionId,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Provider name, such as `local`, `daytona`, or `e2b`.
    pub provider: String,
    /// Requested sandbox isolation tier.
    pub tier: SandboxTier,
    /// Serialized hand handle and provider metadata when the row has an active sandbox.
    pub handle: Option<LeaseHandle>,
    /// Current lease lifecycle state.
    pub status: HandLeaseStatus,
    /// Monotonic fencing generation.
    pub generation: i64,
    /// Row creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Row update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Lease expiry timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Store contract for durable hand lease coordination.
#[async_trait]
pub trait HandLeaseStore: Send + Sync {
    /// Atomically claims provisioning for a session/provider when no valid active lease exists.
    async fn claim_for_provisioning(
        &self,
        session_id: SessionId,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<HandLease>>;

    /// Loads the current lease for a session/provider.
    async fn get(&self, session_id: SessionId, provider: &str) -> Result<Option<HandLease>>;

    /// Lists all durable leases for a session.
    async fn list_session(&self, session_id: SessionId) -> Result<Vec<HandLease>>;

    /// Marks a claimed generation active with its durable handle payload.
    async fn activate(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
        expires_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Renews a current active lease if the generation fence still matches.
    async fn renew_active(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        expires_at: DateTime<Utc>,
    ) -> Result<bool>;

    /// Marks a claimed generation with a terminal or replaceable status.
    async fn mark_status(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        status: HandLeaseStatus,
    ) -> Result<()>;
}

/// Postgres-backed hand lease store.
#[derive(Clone)]
pub struct PostgresHandLeaseStore {
    pool: PgPool,
}

impl PostgresHandLeaseStore {
    /// Creates a Postgres hand lease store from an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HandLeaseStore for PostgresHandLeaseStore {
    async fn claim_for_provisioning(
        &self,
        session_id: SessionId,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<HandLease>> {
        let row = sqlx::query(
            r#"
            INSERT INTO moa.hand_leases (
                session_id, tenant_id, provider, tier, status, generation, expires_at
            )
            VALUES ($1, $2, $3, $4, 'provisioning', 1, $5)
            ON CONFLICT (session_id, provider) DO UPDATE
            SET tenant_id = EXCLUDED.tenant_id,
                tier = EXCLUDED.tier,
                status = 'provisioning',
                generation = moa.hand_leases.generation + 1,
                updated_at = now(),
                expires_at = EXCLUDED.expires_at
            WHERE moa.hand_leases.status IN ('stale', 'destroyed', 'failed')
               OR moa.hand_leases.expires_at <= now()
            RETURNING session_id, tenant_id, provider, tier, handle, status,
                      generation, created_at, updated_at, expires_at
            "#,
        )
        .bind(session_id)
        .bind(tenant_id)
        .bind(provider)
        .bind(tier_label(&tier))
        .bind(expires_at)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| hand_lease_from_row(&row)).transpose()
    }

    async fn get(&self, session_id: SessionId, provider: &str) -> Result<Option<HandLease>> {
        let row = sqlx::query(
            r#"
            SELECT session_id, tenant_id, provider, tier, handle, status,
                   generation, created_at, updated_at, expires_at
            FROM moa.hand_leases
            WHERE session_id = $1 AND provider = $2
            "#,
        )
        .bind(session_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| hand_lease_from_row(&row)).transpose()
    }

    async fn list_session(&self, session_id: SessionId) -> Result<Vec<HandLease>> {
        let rows = sqlx::query(
            r#"
            SELECT session_id, tenant_id, provider, tier, handle, status,
                   generation, created_at, updated_at, expires_at
            FROM moa.hand_leases
            WHERE session_id = $1
            ORDER BY provider
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(hand_lease_from_row).collect()
    }

    async fn activate(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = $4,
                status = 'active',
                updated_at = now(),
                expires_at = $5
            WHERE session_id = $1
              AND provider = $2
              AND generation = $3
              AND status = 'provisioning'
            "#,
        )
        .bind(session_id)
        .bind(provider)
        .bind(generation)
        .bind(Json(handle))
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 1 {
            Ok(())
        } else {
            Err(MoaError::StorageError(format!(
                "hand lease activation lost generation fence for session {session_id} provider {provider}"
            )))
        }
    }

    async fn renew_active(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET updated_at = now(),
                expires_at = $4
            WHERE session_id = $1
              AND provider = $2
              AND generation = $3
              AND status = 'active'
              AND expires_at > now()
            "#,
        )
        .bind(session_id)
        .bind(provider)
        .bind(generation)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        Ok(affected == 1)
    }

    async fn mark_status(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        status: HandLeaseStatus,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = $4,
                updated_at = now()
            WHERE session_id = $1
              AND provider = $2
              AND generation = $3
            "#,
        )
        .bind(session_id)
        .bind(provider)
        .bind(generation)
        .bind(status.as_str())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }
}

fn hand_lease_from_row(row: &sqlx::postgres::PgRow) -> Result<HandLease> {
    Ok(HandLease {
        session_id: row.try_get("session_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        tier: tier_from_label(
            row.try_get::<String, _>("tier")
                .map_err(map_sqlx_error)?
                .as_str(),
        )?,
        handle: row
            .try_get::<Option<Json<LeaseHandle>>, _>("handle")
            .map_err(map_sqlx_error)?
            .map(|handle| handle.0),
        status: HandLeaseStatus::from_str(
            row.try_get::<String, _>("status")
                .map_err(map_sqlx_error)?
                .as_str(),
        )?,
        generation: row.try_get("generation").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
        expires_at: row.try_get("expires_at").map_err(map_sqlx_error)?,
    })
}

/// Returns the persisted label for a sandbox tier.
pub fn tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

fn tier_from_label(value: &str) -> Result<SandboxTier> {
    match value {
        "none" => Ok(SandboxTier::None),
        "container" => Ok(SandboxTier::Container),
        "microvm" => Ok(SandboxTier::MicroVM),
        "local" => Ok(SandboxTier::Local),
        other => Err(MoaError::StorageError(format!(
            "unknown hand lease tier: {other}"
        ))),
    }
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryHandLeaseStore {
    leases: Mutex<HashMap<(SessionId, String), HandLease>>,
}

#[cfg(test)]
impl MemoryHandLeaseStore {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[cfg(test)]
#[async_trait]
impl HandLeaseStore for MemoryHandLeaseStore {
    async fn claim_for_provisioning(
        &self,
        session_id: SessionId,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<HandLease>> {
        let mut leases = self.leases.lock().await;
        let key = (session_id, provider.to_string());
        let now = Utc::now();
        if let Some(existing) = leases.get(&key)
            && existing.status != HandLeaseStatus::Stale
            && existing.status != HandLeaseStatus::Destroyed
            && existing.status != HandLeaseStatus::Failed
            && existing.expires_at > now
        {
            return Ok(None);
        }
        let generation = leases
            .get(&key)
            .map_or(1, |existing| existing.generation + 1);
        let lease = HandLease {
            session_id,
            tenant_id,
            provider: provider.to_string(),
            tier,
            handle: leases
                .get(&key)
                .and_then(|existing| existing.handle.clone()),
            status: HandLeaseStatus::Provisioning,
            generation,
            created_at: leases.get(&key).map_or(now, |existing| existing.created_at),
            updated_at: now,
            expires_at,
        };
        leases.insert(key, lease.clone());
        Ok(Some(lease))
    }

    async fn get(&self, session_id: SessionId, provider: &str) -> Result<Option<HandLease>> {
        Ok(self
            .leases
            .lock()
            .await
            .get(&(session_id, provider.to_string()))
            .cloned())
    }

    async fn list_session(&self, session_id: SessionId) -> Result<Vec<HandLease>> {
        let mut leases = self
            .leases
            .lock()
            .await
            .values()
            .filter(|lease| lease.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.provider.cmp(&right.provider));
        Ok(leases)
    }

    async fn activate(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(session_id, provider.to_string())) else {
            return Err(MoaError::StorageError("missing hand lease".to_string()));
        };
        if lease.generation != generation || lease.status != HandLeaseStatus::Provisioning {
            return Err(MoaError::StorageError(
                "hand lease activation lost generation fence".to_string(),
            ));
        }
        lease.handle = Some(handle);
        lease.status = HandLeaseStatus::Active;
        lease.updated_at = Utc::now();
        lease.expires_at = expires_at;
        Ok(())
    }

    async fn renew_active(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(session_id, provider.to_string())) else {
            return Ok(false);
        };
        if lease.generation != generation
            || lease.status != HandLeaseStatus::Active
            || lease.expires_at <= Utc::now()
        {
            return Ok(false);
        }

        lease.updated_at = Utc::now();
        lease.expires_at = expires_at;
        Ok(true)
    }

    async fn mark_status(
        &self,
        session_id: SessionId,
        provider: &str,
        generation: i64,
        status: HandLeaseStatus,
    ) -> Result<()> {
        if let Some(lease) = self
            .leases
            .lock()
            .await
            .get_mut(&(session_id, provider.to_string()))
            && lease.generation == generation
        {
            lease.status = status;
            lease.updated_at = Utc::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[tokio::test]
    async fn memory_store_fences_concurrent_provision_claims() {
        // Pins: only one router replica can own provisioning for a session/provider generation.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let expires_at = Utc::now() + chrono::Duration::minutes(5);

        let (left, right) = tokio::join!(
            store.claim_for_provisioning(
                session_id,
                tenant_id,
                "local",
                SandboxTier::Local,
                expires_at
            ),
            store.claim_for_provisioning(
                session_id,
                tenant_id,
                "local",
                SandboxTier::Local,
                expires_at
            )
        );

        let claims = [left.expect("left claim"), right.expect("right claim")]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(claims, 1, "only one provisioning claim should win");
    }

    #[tokio::test]
    async fn memory_store_reuses_active_generation_until_stale() {
        // Pins: active leases block double-provisioning until they are marked stale.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let expires_at = Utc::now() + chrono::Duration::minutes(5);
        let claimed = store
            .claim_for_provisioning(
                session_id,
                tenant_id,
                "local",
                SandboxTier::Local,
                expires_at,
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        store
            .activate(
                session_id,
                "local",
                claimed.generation,
                LeaseHandle::new(HandHandle::local(PathBuf::from("/tmp/moa-hand"))),
                expires_at,
            )
            .await
            .expect("activate lease");

        assert!(
            store
                .claim_for_provisioning(
                    session_id,
                    tenant_id,
                    "local",
                    SandboxTier::Local,
                    expires_at
                )
                .await
                .expect("active claim check")
                .is_none()
        );

        store
            .mark_status(
                session_id,
                "local",
                claimed.generation,
                HandLeaseStatus::Stale,
            )
            .await
            .expect("mark stale");
        let replacement = store
            .claim_for_provisioning(
                session_id,
                tenant_id,
                "local",
                SandboxTier::Local,
                expires_at,
            )
            .await
            .expect("replacement claim")
            .expect("stale lease should allow replacement");

        assert_eq!(replacement.generation, claimed.generation + 1);
        assert_eq!(
            replacement.handle,
            Some(LeaseHandle::new(HandHandle::local(PathBuf::from(
                "/tmp/moa-hand"
            )))),
            "stale reclaim must preserve the real handle until a new activation wins"
        );
    }

    #[tokio::test]
    async fn memory_store_provisioning_claim_has_no_placeholder_handle() {
        // Pins: provisioning claims do not write an empty fake handle over durable state.
        let store = MemoryHandLeaseStore::shared();
        let claim = store
            .claim_for_provisioning(
                SessionId::new(),
                TenantId::new(),
                "local",
                SandboxTier::Local,
                Utc::now() + chrono::Duration::minutes(5),
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");

        assert_eq!(claim.handle, None);
    }

    #[tokio::test]
    async fn memory_store_renew_active_is_generation_fenced() {
        // Pins: lease renewal only extends the current active generation.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let first_expiry = Utc::now() + chrono::Duration::minutes(5);
        let renewed_expiry = Utc::now() + chrono::Duration::minutes(10);
        let claim = store
            .claim_for_provisioning(
                session_id,
                tenant_id,
                "local",
                SandboxTier::Local,
                first_expiry,
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        store
            .activate(
                session_id,
                "local",
                claim.generation,
                LeaseHandle::new(HandHandle::local(PathBuf::from("/tmp/moa-hand"))),
                first_expiry,
            )
            .await
            .expect("activate lease");

        assert!(
            !store
                .renew_active(session_id, "local", claim.generation + 1, renewed_expiry)
                .await
                .expect("wrong generation renewal should not fail storage")
        );
        assert!(
            store
                .renew_active(session_id, "local", claim.generation, renewed_expiry)
                .await
                .expect("current generation renewal should succeed")
        );
        let renewed = store
            .get(session_id, "local")
            .await
            .expect("load renewed lease")
            .expect("lease should exist");
        assert_eq!(renewed.expires_at, renewed_expiry);
    }
}
