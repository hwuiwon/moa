//! Durable hand lease storage for sandbox lifecycle recovery.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, error::Result, types::hands::EffectiveSandboxProfile,
    types::hands::HandHandle, types::hands::LifetimeLimit, types::hands::SandboxPolicySources,
    types::hands::SandboxProfile, types::hands::SandboxTier, types::identifiers::SessionId,
    types::identifiers::TenantId,
};
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
    /// The durable reaper owns this generation and is destroying it.
    ///
    /// Deliberately unreachable from provisioning: a claimed generation is
    /// finalized as [`HandLeaseStatus::Destroyed`] or released back to
    /// [`HandLeaseStatus::Stale`] for a later retry, never reactivated.
    Reaping,
}

impl HandLeaseStatus {
    /// Returns the persisted label for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Destroyed => "destroyed",
            Self::Failed => "failed",
            Self::Reaping => "reaping",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "provisioning" => Ok(Self::Provisioning),
            "active" => Ok(Self::Active),
            "stale" => Ok(Self::Stale),
            "destroyed" => Ok(Self::Destroyed),
            "failed" => Ok(Self::Failed),
            "reaping" => Ok(Self::Reaping),
            other => Err(MoaError::StorageError(format!(
                "unknown hand lease status: {other}"
            ))),
        }
    }
}

/// The exact sandbox policy identity one lease was provisioned under.
///
/// Persisted alongside the handle so recovery can recompute today's policy and
/// compare hashes. Any change to the profile, to any of the five source
/// revisions, or to the provider's capability revision changes `profile_hash`,
/// which is what prevents a sandbox from being reused under a policy it was
/// never admitted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLeasePolicy {
    /// The resolved six-dimension profile.
    pub profile: SandboxProfile,
    /// The `sha256:`-prefixed policy identity hash.
    pub profile_hash: String,
    /// The five contributing policy-layer revisions.
    pub sources: SandboxPolicySources,
    /// The serving provider's capability revision.
    pub capability_revision: String,
}

impl HandLeasePolicy {
    /// Captures the policy identity of one resolved effective profile.
    #[must_use]
    pub fn from_effective(effective: &EffectiveSandboxProfile) -> Self {
        Self {
            profile: effective.profile().clone(),
            profile_hash: effective.profile_hash().to_string(),
            sources: effective.sources().clone(),
            capability_revision: effective.capability_revision().to_string(),
        }
    }

    /// Returns the idle deadline this policy implies, measured from `now`.
    ///
    /// An explicitly `Unbounded` idle timeout has no deadline, which is `None`.
    #[must_use]
    pub fn idle_deadline(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        deadline_from(now, self.profile.idle_timeout)
    }

    /// Returns the immutable hard deadline this policy implies, measured from `now`.
    #[must_use]
    pub fn hard_deadline(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        deadline_from(now, self.profile.max_lifetime)
    }
}

/// Converts a lifetime limit into an absolute deadline, or `None` when unbounded.
fn deadline_from(now: DateTime<Utc>, limit: LifetimeLimit) -> Option<DateTime<Utc>> {
    limit
        .bounded_seconds()
        .and_then(|seconds| i64::try_from(seconds.get()).ok())
        .map(|seconds| now + chrono::Duration::seconds(seconds))
}

/// One durable hand lease row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLease {
    /// Session that owns the hand.
    pub session_id: SessionId,
    /// Worker scope that owns the hand within the session.
    ///
    /// Empty (`""`) denotes the session-level (coordinator) scope, which is the
    /// only scope used today. A non-empty value isolates a worker's sandbox so
    /// the parent and siblings never collapse onto one shared hand.
    pub worker_id: String,
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
    /// Renewable idle deadline; `None` when the profile's idle timeout is
    /// explicitly `Unbounded`.
    pub idle_expires_at: Option<DateTime<Utc>>,
    /// Immutable hard deadline; `None` when the profile's maximum lifetime is
    /// explicitly `Unbounded`. Never advanced after the provisioning claim.
    pub hard_expires_at: Option<DateTime<Utc>>,
    /// Policy identity this generation was provisioned under.
    ///
    /// `None` only for rows written before the policy contract existed, which
    /// V000359 marked stale and immediately destroyable. A database constraint
    /// keeps active and provisioning rows from ever reaching that state.
    pub policy: Option<HandLeasePolicy>,
}

/// Store contract for durable hand lease coordination.
///
/// Every method carries a `worker_id` scope alongside `session_id`. The empty
/// string is the session-level (coordinator) scope used by all callers today; a
/// non-empty value isolates a worker's sandbox. `list_session` intentionally
/// takes no scope so session teardown reclaims every worker scope at once.
#[async_trait]
pub trait HandLeaseStore: Send + Sync {
    /// Atomically claims provisioning for a session/worker/provider when no valid active lease exists.
    ///
    /// The claim writes the policy identity and both deadlines, so a generation
    /// carries its policy from the moment it exists rather than acquiring one
    /// at activation.
    async fn claim_for_provisioning(
        &self,
        session_id: SessionId,
        worker_id: &str,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        policy: &HandLeasePolicy,
    ) -> Result<Option<HandLease>>;

    /// Loads the current lease for a session/worker/provider.
    async fn get(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>>;

    /// Lists all durable leases for a session, across every worker scope.
    async fn list_session(&self, session_id: SessionId) -> Result<Vec<HandLease>>;

    /// Marks a claimed generation active with its durable handle payload.
    ///
    /// Activation carries no policy or hard deadline: both were fixed by the
    /// provisioning claim and are immutable for the life of the generation.
    async fn activate(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
    ) -> Result<()>;

    /// Clears the previous generation's handle after the provisioning claimant
    /// has destroyed it, without releasing the generation fence.
    async fn clear_handle_for_provisioning(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
    ) -> Result<bool>;

    /// Renews the idle deadline of a current active lease if the generation
    /// fence still matches.
    ///
    /// Renewal can only move the idle deadline, never past the hard deadline,
    /// and a lease whose hard deadline has already passed cannot be renewed at
    /// all. That is what keeps a busy sandbox from living forever.
    async fn renew_active(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        idle_expires_at: DateTime<Utc>,
    ) -> Result<bool>;

    /// Marks a claimed generation with a terminal or replaceable status.
    async fn mark_status(
        &self,
        session_id: SessionId,
        worker_id: &str,
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
        worker_id: &str,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        policy: &HandLeasePolicy,
    ) -> Result<Option<HandLease>> {
        let now = Utc::now();
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.hand_leases (
                session_id, worker_id, tenant_id, provider, tier, status, generation,
                idle_expires_at, hard_expires_at, profile, profile_hash,
                source_deployment_revision, source_tenant_revision,
                source_agent_revision, source_route_revision, source_origin_revision,
                capability_revision, reap_attempts, reap_not_before
            )
            VALUES ($1, $2, $3, $4, $5, 'provisioning', 1, $6, $7, $8, $9,
                    $10, $11, $12, $13, $14, $15, 0, NULL)
            ON CONFLICT (session_id, worker_id, provider) DO UPDATE
            SET tenant_id = EXCLUDED.tenant_id,
                tier = EXCLUDED.tier,
                status = 'provisioning',
                generation = moa.hand_leases.generation + 1,
                updated_at = now(),
                idle_expires_at = EXCLUDED.idle_expires_at,
                hard_expires_at = EXCLUDED.hard_expires_at,
                profile = EXCLUDED.profile,
                profile_hash = EXCLUDED.profile_hash,
                source_deployment_revision = EXCLUDED.source_deployment_revision,
                source_tenant_revision = EXCLUDED.source_tenant_revision,
                source_agent_revision = EXCLUDED.source_agent_revision,
                source_route_revision = EXCLUDED.source_route_revision,
                source_origin_revision = EXCLUDED.source_origin_revision,
                capability_revision = EXCLUDED.capability_revision,
                reap_attempts = 0,
                reap_not_before = NULL
            WHERE moa.hand_leases.status IN ('stale', 'destroyed', 'failed')
               OR moa.hand_leases.idle_expires_at <= now()
               OR moa.hand_leases.hard_expires_at <= now()
            RETURNING {LEASE_COLUMNS}
            "#
        ))
        .bind(session_id)
        .bind(worker_id)
        .bind(tenant_id)
        .bind(provider)
        .bind(tier.as_str())
        .bind(policy.idle_deadline(now))
        .bind(policy.hard_deadline(now))
        .bind(Json(&policy.profile))
        .bind(&policy.profile_hash)
        .bind(&policy.sources.deployment)
        .bind(&policy.sources.tenant)
        .bind(&policy.sources.agent)
        .bind(&policy.sources.route)
        .bind(&policy.sources.origin)
        .bind(&policy.capability_revision)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| hand_lease_from_row(&row)).transpose()
    }

    async fn get(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>> {
        let row = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE session_id = $1 AND worker_id = $2 AND provider = $3
            "#
        ))
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| hand_lease_from_row(&row)).transpose()
    }

    async fn list_session(&self, session_id: SessionId) -> Result<Vec<HandLease>> {
        let rows = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE session_id = $1
            ORDER BY worker_id, provider
            "#
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(hand_lease_from_row).collect()
    }

    async fn activate(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
    ) -> Result<()> {
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = $5,
                status = 'active',
                updated_at = now()
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND status = 'provisioning'
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .bind(Json(handle))
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

    async fn clear_handle_for_provisioning(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
    ) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = NULL,
                updated_at = now()
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND status = 'provisioning'
              AND handle IS NOT NULL
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn renew_active(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        idle_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        // `LEAST` is what pins the idle deadline under the hard one: a renewal
        // asking for more than the sandbox's remaining lifetime gets the
        // remaining lifetime, and a lease already past its hard deadline is not
        // matched at all. `hard_expires_at` never appears in `SET`.
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET updated_at = now(),
                idle_expires_at = CASE
                    WHEN hard_expires_at IS NULL THEN $5
                    ELSE LEAST($5, hard_expires_at)
                END
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND status = 'active'
              AND (idle_expires_at IS NULL OR idle_expires_at > now())
              AND (hard_expires_at IS NULL OR hard_expires_at > now())
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .bind(idle_expires_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        Ok(affected == 1)
    }

    async fn mark_status(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        status: HandLeaseStatus,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = CASE WHEN $5 = 'destroyed' THEN NULL ELSE handle END,
                status = $5,
                updated_at = now(),
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .bind(status.as_str())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }
}

/// Column list every lease read projects, kept in one place so the row decoder
/// and every query stay in agreement.
pub(super) const LEASE_COLUMNS: &str = "session_id, worker_id, tenant_id, provider, tier, handle, \
     status, generation, created_at, updated_at, idle_expires_at, hard_expires_at, profile, \
     profile_hash, source_deployment_revision, source_tenant_revision, source_agent_revision, \
     source_route_revision, source_origin_revision, capability_revision";

pub(super) fn hand_lease_from_row(row: &sqlx::postgres::PgRow) -> Result<HandLease> {
    Ok(HandLease {
        session_id: row.try_get("session_id").map_err(map_sqlx_error)?,
        worker_id: row.try_get("worker_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        tier: SandboxTier::from_label(
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
        idle_expires_at: row.try_get("idle_expires_at").map_err(map_sqlx_error)?,
        hard_expires_at: row.try_get("hard_expires_at").map_err(map_sqlx_error)?,
        policy: hand_lease_policy_from_row(row)?,
    })
}

/// Decodes the policy identity columns, which are absent only on the legacy
/// rows V000359 marked stale.
fn hand_lease_policy_from_row(row: &sqlx::postgres::PgRow) -> Result<Option<HandLeasePolicy>> {
    let profile = row
        .try_get::<Option<Json<SandboxProfile>>, _>("profile")
        .map_err(map_sqlx_error)?;
    let profile_hash = row
        .try_get::<Option<String>, _>("profile_hash")
        .map_err(map_sqlx_error)?;
    let deployment = row
        .try_get::<Option<String>, _>("source_deployment_revision")
        .map_err(map_sqlx_error)?;
    let tenant = row
        .try_get::<Option<String>, _>("source_tenant_revision")
        .map_err(map_sqlx_error)?;
    let agent = row
        .try_get::<Option<String>, _>("source_agent_revision")
        .map_err(map_sqlx_error)?;
    let route = row
        .try_get::<Option<String>, _>("source_route_revision")
        .map_err(map_sqlx_error)?;
    // Absent on rows written before V000372 introduced the origin layer. Those
    // rows carry an incomplete policy identity, so the destructure below treats
    // them exactly as V000359's legacy rows: stale, never reusable.
    let origin = row
        .try_get::<Option<String>, _>("source_origin_revision")
        .map_err(map_sqlx_error)?;
    let capability_revision = row
        .try_get::<Option<String>, _>("capability_revision")
        .map_err(map_sqlx_error)?;

    let (
        Some(profile),
        Some(profile_hash),
        Some(deployment),
        Some(tenant),
        Some(agent),
        Some(route),
        Some(origin),
        Some(capability_revision),
    ) = (
        profile,
        profile_hash,
        deployment,
        tenant,
        agent,
        route,
        origin,
        capability_revision,
    )
    else {
        return Ok(None);
    };
    Ok(Some(HandLeasePolicy {
        profile: profile.0,
        profile_hash,
        sources: SandboxPolicySources {
            deployment,
            tenant,
            agent,
            route,
            origin,
        },
        capability_revision,
    }))
}

pub(super) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::num::NonZeroU64;

    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::hands::{
        BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit,
        SandboxPolicySnapshot, SandboxProfile, resolve_effective_sandbox_profile,
    };

    use super::HandLeasePolicy;

    /// Builds a lease policy whose deadlines are the given whole seconds, with
    /// `None` meaning an explicitly unbounded dimension.
    pub(crate) fn lease_policy(
        idle_secs: Option<u64>,
        hard_secs: Option<u64>,
        capability_revision: &str,
    ) -> HandLeasePolicy {
        let limit = |secs: Option<u64>| match secs {
            Some(secs) => LifetimeLimit::Bounded {
                seconds: NonZeroU64::new(secs).expect("nonzero seconds"),
            },
            None => LifetimeLimit::Unbounded,
        };
        let profile = SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::DenyAll,
            limit(idle_secs),
            limit(hard_secs),
        )
        .expect("test profile should validate");
        let snapshot = SandboxPolicySnapshot::new("test-deployment", profile)
            .expect("test snapshot should validate");
        let effective = resolve_effective_sandbox_profile(
            &snapshot,
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
            &SandboxPolicySnapshot::origin(CallOrigin::Production),
            capability_revision,
        )
        .expect("test resolution should succeed");
        HandLeasePolicy::from_effective(&effective)
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemoryHandLeaseStore {
    leases: Mutex<HashMap<(SessionId, String, String), HandLease>>,
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
        worker_id: &str,
        tenant_id: TenantId,
        provider: &str,
        tier: SandboxTier,
        policy: &HandLeasePolicy,
    ) -> Result<Option<HandLease>> {
        let mut leases = self.leases.lock().await;
        let key = (session_id, worker_id.to_string(), provider.to_string());
        let now = Utc::now();
        if let Some(existing) = leases.get(&key)
            && !matches!(
                existing.status,
                HandLeaseStatus::Stale | HandLeaseStatus::Destroyed | HandLeaseStatus::Failed
            )
            && existing.idle_expires_at.is_none_or(|idle| idle > now)
            && existing.hard_expires_at.is_none_or(|hard| hard > now)
        {
            return Ok(None);
        }
        let generation = leases
            .get(&key)
            .map_or(1, |existing| existing.generation + 1);
        let lease = HandLease {
            session_id,
            worker_id: worker_id.to_string(),
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
            idle_expires_at: policy.idle_deadline(now),
            hard_expires_at: policy.hard_deadline(now),
            policy: Some(policy.clone()),
        };
        leases.insert(key, lease.clone());
        Ok(Some(lease))
    }

    async fn get(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>> {
        Ok(self
            .leases
            .lock()
            .await
            .get(&(session_id, worker_id.to_string(), provider.to_string()))
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
        worker_id: &str,
        provider: &str,
        generation: i64,
        handle: LeaseHandle,
    ) -> Result<()> {
        let mut leases = self.leases.lock().await;
        let Some(lease) =
            leases.get_mut(&(session_id, worker_id.to_string(), provider.to_string()))
        else {
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
        Ok(())
    }

    async fn clear_handle_for_provisioning(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().await;
        let Some(lease) =
            leases.get_mut(&(session_id, worker_id.to_string(), provider.to_string()))
        else {
            return Ok(false);
        };
        if lease.generation != generation
            || lease.status != HandLeaseStatus::Provisioning
            || lease.handle.is_none()
        {
            return Ok(false);
        }
        lease.handle = None;
        lease.updated_at = Utc::now();
        Ok(true)
    }

    async fn renew_active(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        idle_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().await;
        let Some(lease) =
            leases.get_mut(&(session_id, worker_id.to_string(), provider.to_string()))
        else {
            return Ok(false);
        };
        let now = Utc::now();
        if lease.generation != generation
            || lease.status != HandLeaseStatus::Active
            || lease.idle_expires_at.is_some_and(|idle| idle <= now)
            || lease.hard_expires_at.is_some_and(|hard| hard <= now)
        {
            return Ok(false);
        }

        lease.updated_at = now;
        // Mirrors the store's `LEAST(...)`: renewal moves the idle deadline and
        // is capped by the immutable hard deadline.
        lease.idle_expires_at = Some(match lease.hard_expires_at {
            Some(hard) => idle_expires_at.min(hard),
            None => idle_expires_at,
        });
        Ok(true)
    }

    async fn mark_status(
        &self,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
        generation: i64,
        status: HandLeaseStatus,
    ) -> Result<()> {
        if let Some(lease) = self.leases.lock().await.get_mut(&(
            session_id,
            worker_id.to_string(),
            provider.to_string(),
        )) && lease.generation == generation
        {
            if status == HandLeaseStatus::Destroyed {
                lease.handle = None;
            }
            lease.status = status;
            lease.updated_at = Utc::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::test_support::lease_policy;
    use super::*;

    #[tokio::test]
    async fn memory_store_fences_concurrent_provision_claims() {
        // Pins: only one router replica can own provisioning for a session/provider generation.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");

        let (left, right) = tokio::join!(
            store.claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy
            ),
            store.claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy
            )
        );

        let claims = [left.expect("left claim"), right.expect("right claim")]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(claims, 1, "only one provisioning claim should win");
    }

    #[tokio::test]
    async fn memory_store_isolates_worker_scope_from_session_scope() {
        // Pins: a worker scope owns a separate lease row from the session scope.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");

        let session_claim = store
            .claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("session claim succeeds")
            .expect("session claim is owned");
        // The same session/provider under a worker scope must still be claimable.
        let worker_claim = store
            .claim_for_provisioning(
                session_id,
                "sub-x",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("worker claim succeeds")
            .expect("worker claim is owned because it is a distinct scope");

        assert_eq!(session_claim.worker_id, "");
        assert_eq!(worker_claim.worker_id, "sub-x");
        assert!(
            store
                .get(session_id, "", "local")
                .await
                .expect("load session lease")
                .is_some()
        );
        assert!(
            store
                .get(session_id, "sub-x", "local")
                .await
                .expect("load worker lease")
                .is_some()
        );
        // list_session reclaims every scope under the session at once.
        let listed = store.list_session(session_id).await.expect("list leases");
        assert_eq!(listed.len(), 2, "both scopes belong to the session");
    }

    #[tokio::test]
    async fn memory_store_reuses_active_generation_until_stale() {
        // Pins: active leases block double-provisioning until they are marked stale.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let claimed = store
            .claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        store
            .activate(
                session_id,
                "",
                "local",
                claimed.generation,
                LeaseHandle::new(HandHandle::local(PathBuf::from("/tmp/moa-hand"))),
            )
            .await
            .expect("activate lease");

        assert!(
            store
                .claim_for_provisioning(
                    session_id,
                    "",
                    tenant_id,
                    "local",
                    SandboxTier::Local,
                    &policy
                )
                .await
                .expect("active claim check")
                .is_none()
        );

        store
            .mark_status(
                session_id,
                "",
                "local",
                claimed.generation,
                HandLeaseStatus::Stale,
            )
            .await
            .expect("mark stale");
        let replacement = store
            .claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
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
    async fn memory_store_provisioning_claim_records_policy_identity() {
        // Pins: a provisioning claim writes no fake handle and carries the exact
        // policy identity and both deadlines from the moment the generation exists.
        let store = MemoryHandLeaseStore::shared();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let claim = store
            .claim_for_provisioning(
                SessionId::new(),
                "",
                TenantId::new(),
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");

        assert_eq!(claim.handle, None);
        assert_eq!(
            claim
                .policy
                .as_ref()
                .map(|policy| policy.profile_hash.clone()),
            Some(policy.profile_hash.clone())
        );
        assert!(claim.idle_expires_at.is_some());
        assert!(claim.hard_expires_at.is_some());
        assert!(claim.idle_expires_at <= claim.hard_expires_at);
    }

    #[tokio::test]
    async fn memory_store_renew_active_is_generation_fenced() {
        // Pins: lease renewal only extends the current active generation.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let renewed_expiry = Utc::now() + chrono::Duration::seconds(600);
        let claim = store
            .claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        store
            .activate(
                session_id,
                "",
                "local",
                claim.generation,
                LeaseHandle::new(HandHandle::local(PathBuf::from("/tmp/moa-hand"))),
            )
            .await
            .expect("activate lease");

        assert!(
            !store
                .renew_active(
                    session_id,
                    "",
                    "local",
                    claim.generation + 1,
                    renewed_expiry
                )
                .await
                .expect("wrong generation renewal should not fail storage")
        );
        assert!(
            store
                .renew_active(session_id, "", "local", claim.generation, renewed_expiry)
                .await
                .expect("current generation renewal should succeed")
        );
        let renewed = store
            .get(session_id, "", "local")
            .await
            .expect("load renewed lease")
            .expect("lease should exist");
        assert_eq!(renewed.idle_expires_at, Some(renewed_expiry));
    }

    #[tokio::test]
    async fn renewal_cannot_push_idle_past_the_hard_deadline() {
        // Pins: an idle renewal asking for more than the sandbox's remaining
        // lifetime is capped at the hard deadline, and the hard deadline itself
        // never moves — a continuously busy sandbox still dies on schedule.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(60), Some(120), "cap-1");
        let claim = store
            .claim_for_provisioning(
                session_id,
                "",
                tenant_id,
                "local",
                SandboxTier::Local,
                &policy,
            )
            .await
            .expect("claim")
            .expect("claim is owned");
        let hard_deadline = claim.hard_expires_at.expect("bounded hard deadline");
        store
            .activate(
                session_id,
                "",
                "local",
                claim.generation,
                LeaseHandle::new(HandHandle::local(PathBuf::from("/tmp/moa-hand"))),
            )
            .await
            .expect("activate lease");

        let greedy = Utc::now() + chrono::Duration::hours(24);
        assert!(
            store
                .renew_active(session_id, "", "local", claim.generation, greedy)
                .await
                .expect("renewal within the hard lifetime succeeds")
        );

        let renewed = store
            .get(session_id, "", "local")
            .await
            .expect("load renewed lease")
            .expect("lease should exist");
        assert_eq!(
            renewed.idle_expires_at,
            Some(hard_deadline),
            "idle renewal is capped at the hard deadline"
        );
        assert_eq!(
            renewed.hard_expires_at,
            Some(hard_deadline),
            "renewal must never move the hard deadline"
        );
        assert_eq!(
            renewed
                .policy
                .as_ref()
                .map(|policy| policy.profile_hash.clone()),
            Some(policy.profile_hash),
            "renewal must never change the policy identity"
        );
    }
}
