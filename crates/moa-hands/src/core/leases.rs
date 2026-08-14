//! Durable hand lease storage for sandbox lifecycle recovery.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, error::Result, types::hands::EffectiveSandboxProfile,
    types::hands::HandHandle, types::hands::LifetimeLimit, types::hands::SandboxPolicySources,
    types::hands::SandboxProfile, types::hands::SandboxTier,
    types::identifiers::HandProvisioningOperationId, types::identifiers::SandboxWorkspaceId,
    types::identifiers::SessionId, types::identifiers::TenantId,
    types::identifiers::WorkspaceCheckpointId, types::memory::RlsContext,
};
use moa_db::ScopedConn;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, types::Json};
#[cfg(test)]
use tokio::sync::Mutex;
use uuid::Uuid;

use super::sandbox_workspace::capacity::{
    ActiveHandReaperRelease, release_active_hand_for_reaper_in_transaction,
};

/// Maximum wall-clock time the platform allows one provider create dispatch.
pub(super) const PROVISIONING_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Provider visibility grace after create dispatch can no longer complete.
pub(super) const PROVISIONING_VISIBILITY_GRACE: Duration = Duration::from_secs(30);
/// Separation required between independent empty provider observations.
pub(super) const PROVISIONING_EMPTY_CONFIRMATION: Duration = Duration::from_secs(1);

pub(super) fn provisioning_deadline(
    now: DateTime<Utc>,
    caller_deadline: Option<DateTime<Utc>>,
) -> Result<DateTime<Utc>> {
    let timeout = chrono::Duration::from_std(PROVISIONING_TIMEOUT).map_err(|error| {
        MoaError::StorageError(format!("invalid hand provisioning timeout: {error}"))
    })?;
    let platform_deadline = now.checked_add_signed(timeout).ok_or_else(|| {
        MoaError::StorageError("hand provisioning deadline exceeds timestamp range".to_string())
    })?;
    Ok(caller_deadline.map_or(platform_deadline, |deadline| {
        deadline.min(platform_deadline)
    }))
}

/// Returns the earliest time an operation may be reconciled against a provider.
///
/// Reconciliation waits out the provider visibility grace past the absolute
/// create deadline, so an in-flight create that has not yet become listable is
/// never mistaken for one that left no resource. Postgres expresses the same
/// rule inline in SQL; this is the in-process store's copy of it.
#[cfg(test)]
fn reconciliation_time(provisioning_deadline_at: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let grace = chrono::Duration::from_std(PROVISIONING_VISIBILITY_GRACE).map_err(|error| {
        MoaError::StorageError(format!(
            "invalid hand provisioning visibility grace: {error}"
        ))
    })?;
    provisioning_deadline_at
        .checked_add_signed(grace)
        .ok_or_else(|| {
            MoaError::StorageError(
                "hand provisioning reconciliation time exceeds timestamp range".to_string(),
            )
        })
}

/// Serialized hand handle plus provider-specific metadata needed to reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHandle {
    /// Durable provisioning operation that created this provider handle.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Existing provider handle used by `HandProvider` calls.
    pub handle: HandHandle,
    /// Provider-specific reconnect metadata, such as local bind mount roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
}

impl LeaseHandle {
    /// Creates a durable lease handle without extra provider metadata.
    #[must_use]
    pub fn new(provisioning_operation_id: HandProvisioningOperationId, handle: HandHandle) -> Self {
        Self {
            provisioning_operation_id,
            handle,
            provider_metadata: None,
        }
    }

    /// Creates a durable lease handle with provider-specific metadata.
    #[must_use]
    pub fn with_metadata(
        provisioning_operation_id: HandProvisioningOperationId,
        handle: HandHandle,
        provider_metadata: serde_json::Value,
    ) -> Self {
        Self {
            provisioning_operation_id,
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
    /// Provisioning or cleanup failed and must be reconciled by the reaper.
    Failed,
    /// The durable reaper owns this generation and is destroying it.
    ///
    /// Deliberately unreachable from provisioning: a claimed generation is
    /// finalized as [`HandLeaseStatus::Destroyed`] or released back to
    /// [`HandLeaseStatus::Failed`] for a later retry, never reactivated.
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

/// Exact durable workspace fences carried by one compute-lease generation.
///
/// A provisioning row records the attachment it is trying to hydrate before
/// provider I/O. Activation and renewal compare all four values, so a stale
/// writer, compute instance, or restored revision cannot make a hand routable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLeaseWorkspaceAttachment {
    /// Durable logical workspace attached to the compute lease.
    pub workspace_id: SandboxWorkspaceId,
    /// Exact single-writer epoch claimed for the hand.
    pub workspace_writer_epoch: i64,
    /// Exact provider compute-instance generation claimed for the hand.
    pub workspace_instance_generation: i64,
    /// Verified checkpoint restored into the hand, if the workspace has a head.
    pub restored_checkpoint_id: Option<WorkspaceCheckpointId>,
}

impl HandLeaseWorkspaceAttachment {
    /// Builds and validates one exact workspace attachment fence.
    pub fn new(
        workspace_id: SandboxWorkspaceId,
        workspace_writer_epoch: i64,
        workspace_instance_generation: i64,
        restored_checkpoint_id: Option<WorkspaceCheckpointId>,
    ) -> Result<Self> {
        if workspace_writer_epoch < 0 || workspace_instance_generation < 0 {
            return Err(MoaError::ValidationError(
                "hand lease workspace generations must be non-negative".to_string(),
            ));
        }
        Ok(Self {
            workspace_id,
            workspace_writer_epoch,
            workspace_instance_generation,
            restored_checkpoint_id,
        })
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
    /// Opaque typed-owner scope that owns the hand within the session.
    ///
    /// Runtime sandbox admission supplies a non-empty opaque key derived from a
    /// typed worker or execution-task workspace owner, keeping sibling owners
    /// on distinct leases. Sandbox admission never emits an empty value, and an
    /// empty low-level repository key must not be interpreted as coordinator
    /// ownership.
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
    /// Durable provider-visible identity for this generation's create operation.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Absolute deadline that bounds provider create dispatch and completion.
    pub provisioning_deadline_at: DateTime<Utc>,
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
    /// Earliest time an ambiguous create or failed cleanup may be reaped.
    pub reap_not_before: Option<DateTime<Utc>>,
    /// Exact durable workspace attachment fenced to this compute generation.
    ///
    /// Only terminal rows created before the V58 hard break may omit it.
    /// Provisioning and active rows are rejected at both the database and row
    /// decoder boundaries unless all attachment columns are present.
    pub attachment: Option<HandLeaseWorkspaceAttachment>,
    /// Policy identity this generation was provisioned under.
    ///
    /// `None` represents an incomplete stale row that is immediately
    /// destroyable. A database constraint keeps active and provisioning rows
    /// from ever reaching that state.
    pub policy: Option<HandLeasePolicy>,
}

/// Named inputs for atomically claiming one durable provisioning generation.
pub struct HandLeaseProvisionRequest<'a> {
    /// Session that will own the lease.
    pub session_id: SessionId,
    /// Opaque lease key derived from the typed workspace owner; runtime sandbox
    /// callers always supply a non-empty value.
    pub worker_id: &'a str,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Provider selected for this generation.
    pub provider: &'a str,
    /// Sandbox isolation tier requested from the provider.
    pub tier: SandboxTier,
    /// Exact workspace attachment this provisioning generation must hydrate.
    pub attachment: HandLeaseWorkspaceAttachment,
    /// Fully resolved policy identity and deadlines.
    pub policy: &'a HandLeasePolicy,
    /// Earlier caller deadline that provisioning must not widen.
    pub caller_deadline: Option<DateTime<Utc>>,
}

/// Named inputs for renewing one tenant-scoped active lease generation.
pub struct HandLeaseRenewRequest<'a> {
    /// Tenant that owns the lease.
    pub tenant_id: TenantId,
    /// Session that owns the hand.
    pub session_id: SessionId,
    /// Opaque typed-owner scope within the session.
    pub worker_id: &'a str,
    /// Provider selected for the generation.
    pub provider: &'a str,
    /// Exact compute generation being renewed.
    pub generation: i64,
    /// Exact provisioning operation for that generation.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Exact hydrated workspace attachment being renewed.
    pub attachment: HandLeaseWorkspaceAttachment,
    /// Requested new idle deadline, capped by the immutable hard deadline.
    pub idle_expires_at: DateTime<Utc>,
}

/// Named inputs for activating one exactly hydrated lease generation.
pub struct HandLeaseActivateRequest<'a> {
    /// Tenant that owns the lease.
    pub tenant_id: TenantId,
    /// Session that owns the hand.
    pub session_id: SessionId,
    /// Opaque typed-owner scope within the session.
    pub worker_id: &'a str,
    /// Provider selected for the generation.
    pub provider: &'a str,
    /// Exact compute generation being activated.
    pub generation: i64,
    /// Durable provider handle created by this provisioning operation.
    pub handle: LeaseHandle,
    /// Exact workspace attachment proven hydrated before activation.
    pub attachment: HandLeaseWorkspaceAttachment,
}

/// Stable keyset cursor for one session's live hand leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLeaseSessionCursor {
    /// Opaque typed-owner key of the last lease returned by the prior page.
    pub worker_id: String,
    /// Provider name of the last lease returned by the prior page.
    pub provider: String,
}

/// One bounded page of live hand leases for aggregate session teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandLeaseSessionPage {
    /// Live leases ordered by `(worker_id, provider)`.
    pub leases: Vec<HandLease>,
    /// Cursor to resume after this page, or `None` when the scan is complete.
    pub next_cursor: Option<HandLeaseSessionCursor>,
}

/// Maximum live hand leases returned by one aggregate session page.
pub const HAND_LEASE_SESSION_PAGE_SIZE: usize = 64;
const HAND_LEASE_SESSION_QUERY_LIMIT: i64 = HAND_LEASE_SESSION_PAGE_SIZE as i64 + 1;

/// Store contract for durable hand lease coordination.
///
/// Every foreground method requires the verified tenant in addition to the
/// typed-owner lease key and session. Implementations must install that tenant
/// in the database transaction and repeat it in every predicate; a provider
/// handle or session ID is never an authorization boundary. Fleet-wide cleanup
/// uses the separate maintenance surface in [`super::reaper`].
#[async_trait]
pub trait HandLeaseStore: Send + Sync {
    /// Atomically claims provisioning for a session/typed-owner/provider when no valid active lease exists.
    ///
    /// The claim writes the policy identity, sandbox deadlines, absolute create
    /// deadline, and later reconciliation time, so a generation carries its
    /// complete recovery contract before provider I/O rather than acquiring it
    /// at activation.
    async fn claim_for_provisioning(
        &self,
        request: HandLeaseProvisionRequest<'_>,
    ) -> Result<Option<HandLease>>;

    /// Loads the current lease for a session/typed-owner/provider.
    async fn get(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>>;

    /// Loads one exact owner lease generation by its provider-create identity.
    async fn get_exact_generation(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provisioning_operation_id: HandProvisioningOperationId,
        generation: i64,
    ) -> Result<Option<HandLease>>;

    /// Loads at most two live provider rows for one exact typed owner scope.
    ///
    /// Implementations must use the `(tenant_id, session_id, worker_id)` owner
    /// index prefix and a database-side `LIMIT 2`; the second row detects an
    /// invalid concurrent replacement without materializing owner history.
    async fn list_live_owner_candidates(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<Vec<HandLease>>;

    /// Reports whether any non-destroyed provider row exists for an exact owner.
    async fn has_live_owner(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<bool>;

    /// Lists one bounded keyset page of live leases for a session.
    ///
    /// Rows are ordered by `(worker_id, provider)` and the cursor is exclusive.
    /// Implementations must use the tenant/session index prefix, fetch at most
    /// one lookahead row beyond [`HAND_LEASE_SESSION_PAGE_SIZE`], and omit
    /// already-destroyed history so terminal cleanup stays bounded as a session
    /// ages.
    async fn list_live_session_page(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        cursor: Option<&HandLeaseSessionCursor>,
    ) -> Result<HandLeaseSessionPage>;

    /// Marks a claimed generation active with its durable handle payload.
    ///
    /// Activation carries no policy or hard deadline: both were fixed by the
    /// provisioning claim and are immutable for the life of the generation.
    /// Returns `false` when the exact provisioning generation lost its fence.
    async fn activate(&self, request: HandLeaseActivateRequest<'_>) -> Result<bool>;

    /// Clears the previous generation's handle after the provisioning claimant
    /// has destroyed it, without releasing the generation fence.
    async fn clear_handle_for_provisioning(
        &self,
        tenant_id: TenantId,
        claim: &HandLease,
    ) -> Result<bool>;

    /// Renews the idle deadline of a current active lease if the generation
    /// fence still matches.
    ///
    /// Renewal can only move the idle deadline, never past the hard deadline,
    /// and a lease whose hard deadline has already passed cannot be renewed at
    /// all. That is what keeps a busy sandbox from living forever.
    async fn renew_active(&self, request: HandLeaseRenewRequest<'_>) -> Result<bool>;

    /// Moves one exact generation, status, and handle to a non-reaping status.
    ///
    /// A `Reaping` source or target is rejected because only its destroy-claim
    /// token may finalize or release that ownership fence.
    async fn transition_status(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        status: HandLeaseStatus,
    ) -> Result<bool>;

    /// Claims one exact generation, status, and handle for provider destruction.
    ///
    /// Live `Provisioning` ownership is not preemptible here; only the durable
    /// reaper may take an abandoned provisioning generation under its expiry
    /// policy.
    async fn claim_for_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_ttl: Duration,
    ) -> Result<Option<Uuid>>;

    /// Finalizes a successful destroy against the same exact claim fence.
    async fn finalize_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
    ) -> Result<bool>;

    /// Releases a failed destroy for later reaping without making it active again.
    async fn release_destroy_claim(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
        retry_after: Duration,
    ) -> Result<bool>;
}

/// Postgres-backed hand lease store.
#[derive(Clone)]
pub struct PostgresHandLeaseStore {
    pool: PgPool,
    assume_workspace_maintenance_role: bool,
}

impl PostgresHandLeaseStore {
    /// Creates a Postgres hand lease store from an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: false,
        }
    }

    /// Creates a lease store over the dedicated NOINHERIT maintenance pool.
    #[must_use]
    pub fn new_maintenance(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: true,
        }
    }

    /// Begins one foreground transaction with forced tenant RLS active.
    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        if self.assume_workspace_maintenance_role {
            let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
            sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
            Ok(conn)
        } else {
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await
        }
    }

    /// Loads the unique active lease holding one exact workspace writer fence.
    pub async fn get_for_workspace_reconciliation(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        writer_epoch: i64,
        instance_generation: i64,
    ) -> Result<Option<HandLease>> {
        let mut conn = self.begin(tenant_id).await?;
        let rows = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE tenant_id = $1 AND workspace_id = $2
              AND workspace_writer_epoch = $3 AND workspace_instance_generation = $4
              AND status = 'active' AND handle IS NOT NULL
            ORDER BY updated_at DESC
            LIMIT 2
            "#
        ))
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let leases = rows
            .iter()
            .map(hand_lease_from_row)
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        match leases.as_slice() {
            [] => Ok(None),
            [lease] => Ok(Some(lease.clone())),
            _ => Err(MoaError::StorageError(
                "workspace reconciliation found multiple active writer leases".to_string(),
            )),
        }
    }
}

fn attachment_columns(
    attachment: Option<&HandLeaseWorkspaceAttachment>,
) -> (
    Option<SandboxWorkspaceId>,
    Option<i64>,
    Option<i64>,
    Option<WorkspaceCheckpointId>,
) {
    attachment.map_or((None, None, None, None), |attachment| {
        (
            Some(attachment.workspace_id),
            Some(attachment.workspace_writer_epoch),
            Some(attachment.workspace_instance_generation),
            attachment.restored_checkpoint_id,
        )
    })
}

fn finish_live_session_page(leases: &mut Vec<HandLease>) -> HandLeaseSessionPage {
    let has_more = leases.len() > HAND_LEASE_SESSION_PAGE_SIZE;
    leases.truncate(HAND_LEASE_SESSION_PAGE_SIZE);
    let next_cursor = has_more.then(|| {
        leases.last().map(|lease| HandLeaseSessionCursor {
            worker_id: lease.worker_id.clone(),
            provider: lease.provider.clone(),
        })
    });
    HandLeaseSessionPage {
        leases: std::mem::take(leases),
        next_cursor: next_cursor.flatten(),
    }
}

#[async_trait]
impl HandLeaseStore for PostgresHandLeaseStore {
    async fn claim_for_provisioning(
        &self,
        request: HandLeaseProvisionRequest<'_>,
    ) -> Result<Option<HandLease>> {
        let HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id,
            provider,
            tier,
            attachment,
            policy,
            caller_deadline,
        } = request;
        let now = Utc::now();
        let provisioning_deadline_at = provisioning_deadline(now, caller_deadline)?;
        let provisioning_operation_id = HandProvisioningOperationId::new();
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.hand_leases (
                session_id, worker_id, tenant_id, provider, tier, status, generation,
                provisioning_operation_id, provisioning_deadline_at,
                workspace_id, workspace_writer_epoch, workspace_instance_generation,
                restored_checkpoint_id,
                idle_expires_at, hard_expires_at, profile, profile_hash,
                source_deployment_revision, source_tenant_revision,
                source_agent_revision, source_route_revision, source_origin_revision,
                capability_revision, reap_attempts, reap_not_before
            )
            VALUES ($1, $2, $3, $4, $5, 'provisioning', 1, $6, $7,
                    $19, $20, $21, $22, $8, $9, $10, $11,
                    $12, $13, $14, $15, $16, $17, 0,
                    $7 + make_interval(secs => $18))
            ON CONFLICT (session_id, worker_id, provider) DO UPDATE
            SET tier = EXCLUDED.tier,
                status = 'provisioning',
                generation = moa.hand_leases.generation + 1,
                provisioning_operation_id = EXCLUDED.provisioning_operation_id,
                provisioning_deadline_at = EXCLUDED.provisioning_deadline_at,
                workspace_id = EXCLUDED.workspace_id,
                workspace_writer_epoch = EXCLUDED.workspace_writer_epoch,
                workspace_instance_generation = EXCLUDED.workspace_instance_generation,
                restored_checkpoint_id = EXCLUDED.restored_checkpoint_id,
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
                reap_not_before = EXCLUDED.reap_not_before
            WHERE moa.hand_leases.tenant_id = EXCLUDED.tenant_id
              AND (
                    moa.hand_leases.status IN ('stale', 'destroyed')
                 OR (
                        moa.hand_leases.status = 'active'
                    AND (
                           moa.hand_leases.idle_expires_at <= now()
                        OR moa.hand_leases.hard_expires_at <= now()
                    )
                 )
            )
            RETURNING {LEASE_COLUMNS}
            "#
        ))
        .bind(session_id)
        .bind(worker_id)
        .bind(tenant_id)
        .bind(provider)
        .bind(tier.as_str())
        .bind(provisioning_operation_id)
        .bind(provisioning_deadline_at)
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
        .bind(PROVISIONING_VISIBILITY_GRACE.as_secs_f64())
        .bind(attachment.workspace_id)
        .bind(attachment.workspace_writer_epoch)
        .bind(attachment.workspace_instance_generation)
        .bind(attachment.restored_checkpoint_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let lease = row.map(|row| hand_lease_from_row(&row)).transpose()?;
        conn.commit().await?;
        Ok(lease)
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE session_id = $1 AND worker_id = $2 AND provider = $3
              AND tenant_id = $4
            "#
        ))
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(tenant_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let lease = row.map(|row| hand_lease_from_row(&row)).transpose()?;
        conn.commit().await?;
        Ok(lease)
    }

    async fn get_exact_generation(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provisioning_operation_id: HandProvisioningOperationId,
        generation: i64,
    ) -> Result<Option<HandLease>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3
              AND provisioning_operation_id = $4 AND generation = $5
            "#
        ))
        .bind(tenant_id)
        .bind(session_id)
        .bind(worker_id)
        .bind(provisioning_operation_id)
        .bind(generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let lease = row.map(|row| hand_lease_from_row(&row)).transpose()?;
        conn.commit().await?;
        Ok(lease)
    }

    async fn list_live_owner_candidates(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<Vec<HandLease>> {
        let mut conn = self.begin(tenant_id).await?;
        let rows = sqlx::query(&format!(
            r#"
            SELECT {LEASE_COLUMNS}
            FROM moa.hand_leases
            WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3
              AND status <> 'destroyed'
            ORDER BY provider
            LIMIT 2
            "#
        ))
        .bind(tenant_id)
        .bind(session_id)
        .bind(worker_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let leases = rows
            .iter()
            .map(hand_lease_from_row)
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(leases)
    }

    async fn has_live_owner(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.hand_leases \
             WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3 \
               AND status <> 'destroyed')",
        )
        .bind(tenant_id)
        .bind(session_id)
        .bind(worker_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(exists)
    }

    async fn list_live_session_page(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        cursor: Option<&HandLeaseSessionCursor>,
    ) -> Result<HandLeaseSessionPage> {
        let mut conn = self.begin(tenant_id).await?;
        let rows = match cursor {
            Some(cursor) => {
                sqlx::query(&format!(
                    r#"
                SELECT {LEASE_COLUMNS}
                FROM moa.hand_leases
                WHERE tenant_id = $1 AND session_id = $2
                  AND status <> 'destroyed'
                  AND (worker_id, provider) > ($3, $4)
                ORDER BY worker_id, provider
                LIMIT $5
                "#
                ))
                .bind(tenant_id)
                .bind(session_id)
                .bind(&cursor.worker_id)
                .bind(&cursor.provider)
                .bind(HAND_LEASE_SESSION_QUERY_LIMIT)
                .fetch_all(conn.as_mut())
                .await
            }
            None => {
                sqlx::query(&format!(
                    r#"
                SELECT {LEASE_COLUMNS}
                FROM moa.hand_leases
                WHERE tenant_id = $1 AND session_id = $2
                  AND status <> 'destroyed'
                ORDER BY worker_id, provider
                LIMIT $3
                "#
                ))
                .bind(tenant_id)
                .bind(session_id)
                .bind(HAND_LEASE_SESSION_QUERY_LIMIT)
                .fetch_all(conn.as_mut())
                .await
            }
        }
        .map_err(map_sqlx_error)?;

        let mut leases = rows
            .iter()
            .map(hand_lease_from_row)
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(finish_live_session_page(&mut leases))
    }

    async fn activate(&self, request: HandLeaseActivateRequest<'_>) -> Result<bool> {
        let HandLeaseActivateRequest {
            tenant_id,
            session_id,
            worker_id,
            provider,
            generation,
            handle,
            attachment,
        } = request;
        let provisioning_operation_id = handle.provisioning_operation_id;
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = $6,
                status = 'active',
                updated_at = now(),
                reap_not_before = NULL,
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'provisioning'
              AND handle IS NULL
              AND tenant_id = $7
              AND workspace_id = $8
              AND workspace_writer_epoch = $9
              AND workspace_instance_generation = $10
              AND restored_checkpoint_id IS NOT DISTINCT FROM $11
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .bind(provisioning_operation_id)
        .bind(Json(handle))
        .bind(tenant_id)
        .bind(attachment.workspace_id)
        .bind(attachment.workspace_writer_epoch)
        .bind(attachment.workspace_instance_generation)
        .bind(attachment.restored_checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn clear_handle_for_provisioning(
        &self,
        tenant_id: TenantId,
        claim: &HandLease,
    ) -> Result<bool> {
        let (workspace_id, writer_epoch, instance_generation, checkpoint_id) =
            attachment_columns(claim.attachment.as_ref());
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = NULL, updated_at = now()
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'provisioning'
              AND handle IS NOT DISTINCT FROM $6
              AND tenant_id = $7
              AND workspace_id IS NOT DISTINCT FROM $8
              AND workspace_writer_epoch IS NOT DISTINCT FROM $9
              AND workspace_instance_generation IS NOT DISTINCT FROM $10
              AND restored_checkpoint_id IS NOT DISTINCT FROM $11
            "#,
        )
        .bind(claim.session_id)
        .bind(&claim.worker_id)
        .bind(&claim.provider)
        .bind(claim.generation)
        .bind(claim.provisioning_operation_id)
        .bind(claim.handle.clone().map(Json))
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn renew_active(&self, request: HandLeaseRenewRequest<'_>) -> Result<bool> {
        let HandLeaseRenewRequest {
            tenant_id,
            session_id,
            worker_id,
            provider,
            generation,
            provisioning_operation_id,
            attachment,
            idle_expires_at,
        } = request;
        // `LEAST` is what pins the idle deadline under the hard one: a renewal
        // asking for more than the sandbox's remaining lifetime gets the
        // remaining lifetime, and a lease already past its hard deadline is not
        // matched at all. `hard_expires_at` never appears in `SET`.
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET updated_at = now(),
                idle_expires_at = CASE
                    WHEN hard_expires_at IS NULL THEN $6
                    ELSE LEAST($6, hard_expires_at)
                END
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'active'
              AND (idle_expires_at IS NULL OR idle_expires_at > now())
              AND (hard_expires_at IS NULL OR hard_expires_at > now())
              AND tenant_id = $7
              AND workspace_id = $8
              AND workspace_writer_epoch = $9
              AND workspace_instance_generation = $10
              AND restored_checkpoint_id IS NOT DISTINCT FROM $11
            "#,
        )
        .bind(session_id)
        .bind(worker_id)
        .bind(provider)
        .bind(generation)
        .bind(provisioning_operation_id)
        .bind(idle_expires_at)
        .bind(tenant_id)
        .bind(attachment.workspace_id)
        .bind(attachment.workspace_writer_epoch)
        .bind(attachment.workspace_instance_generation)
        .bind(attachment.restored_checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn transition_status(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        status: HandLeaseStatus,
    ) -> Result<bool> {
        if status == HandLeaseStatus::Reaping || expected.status == HandLeaseStatus::Reaping {
            return Err(MoaError::StorageError(
                "reaping generations may only move through their owned destroy claim".to_string(),
            ));
        }
        let (workspace_id, writer_epoch, instance_generation, checkpoint_id) =
            attachment_columns(expected.attachment.as_ref());
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = CASE WHEN $8 = 'destroyed' THEN NULL ELSE handle END,
                status = $8,
                updated_at = now(),
                reap_not_before = CASE WHEN $8 = 'failed' THEN reap_not_before ELSE NULL END,
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = $6
              AND handle IS NOT DISTINCT FROM $7
              AND tenant_id = $9
              AND workspace_id IS NOT DISTINCT FROM $10
              AND workspace_writer_epoch IS NOT DISTINCT FROM $11
              AND workspace_instance_generation IS NOT DISTINCT FROM $12
              AND restored_checkpoint_id IS NOT DISTINCT FROM $13
            "#,
        )
        .bind(expected.session_id)
        .bind(&expected.worker_id)
        .bind(&expected.provider)
        .bind(expected.generation)
        .bind(expected.provisioning_operation_id)
        .bind(expected.status.as_str())
        .bind(expected.handle.clone().map(Json))
        .bind(status.as_str())
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn claim_for_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_ttl: Duration,
    ) -> Result<Option<Uuid>> {
        if matches!(
            expected.status,
            HandLeaseStatus::Provisioning | HandLeaseStatus::Reaping | HandLeaseStatus::Destroyed
        ) {
            return Ok(None);
        }
        let claim_token = Uuid::new_v4();
        let (workspace_id, writer_epoch, instance_generation, checkpoint_id) =
            attachment_columns(expected.attachment.as_ref());
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'reaping',
                updated_at = now(),
                reap_claim_token = $8,
                reap_claim_expires_at = now() + make_interval(secs => $9)
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = $6
              AND handle IS NOT DISTINCT FROM $7
              AND status <> 'reaping'
              AND (status <> 'failed' OR reap_not_before <= now())
              AND tenant_id = $10
              AND workspace_id IS NOT DISTINCT FROM $11
              AND workspace_writer_epoch IS NOT DISTINCT FROM $12
              AND workspace_instance_generation IS NOT DISTINCT FROM $13
              AND restored_checkpoint_id IS NOT DISTINCT FROM $14
            "#,
        )
        .bind(expected.session_id)
        .bind(&expected.worker_id)
        .bind(&expected.provider)
        .bind(expected.generation)
        .bind(expected.provisioning_operation_id)
        .bind(expected.status.as_str())
        .bind(expected.handle.clone().map(Json))
        .bind(claim_token)
        .bind(claim_ttl.as_secs_f64())
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok((affected == 1).then_some(claim_token))
    }

    async fn finalize_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
    ) -> Result<bool> {
        let (workspace_id, writer_epoch, instance_generation, checkpoint_id) =
            attachment_columns(expected.attachment.as_ref());
        let mut conn = self.begin(tenant_id).await?;
        if expected.attachment.is_some()
            && release_active_hand_for_reaper_in_transaction(
                conn.as_mut(),
                tenant_id,
                expected.provisioning_operation_id,
                expected.generation,
                claim_token,
            )
            .await?
                != ActiveHandReaperRelease::Released
        {
            conn.rollback().await?;
            return Ok(false);
        }
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = NULL,
                status = 'destroyed',
                updated_at = now(),
                reap_attempts = 0,
                reap_not_before = NULL,
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'reaping'
              AND handle IS NOT DISTINCT FROM $6
              AND reap_claim_token = $7
              AND tenant_id = $8
              AND workspace_id IS NOT DISTINCT FROM $9
              AND workspace_writer_epoch IS NOT DISTINCT FROM $10
              AND workspace_instance_generation IS NOT DISTINCT FROM $11
              AND restored_checkpoint_id IS NOT DISTINCT FROM $12
            "#,
        )
        .bind(expected.session_id)
        .bind(&expected.worker_id)
        .bind(&expected.provider)
        .bind(expected.generation)
        .bind(expected.provisioning_operation_id)
        .bind(expected.handle.clone().map(Json))
        .bind(claim_token)
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    async fn release_destroy_claim(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
        retry_after: Duration,
    ) -> Result<bool> {
        let (workspace_id, writer_epoch, instance_generation, checkpoint_id) =
            attachment_columns(expected.attachment.as_ref());
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'failed',
                updated_at = now(),
                reap_attempts = reap_attempts + 1,
                reap_not_before = GREATEST(
                    now() + make_interval(secs => $7),
                    provisioning_deadline_at + make_interval(secs => $9)
                ),
                reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE session_id = $1
              AND worker_id = $2
              AND provider = $3
              AND generation = $4
              AND provisioning_operation_id = $5
              AND status = 'reaping'
              AND handle IS NOT DISTINCT FROM $6
              AND reap_claim_token = $8
              AND tenant_id = $10
              AND workspace_id IS NOT DISTINCT FROM $11
              AND workspace_writer_epoch IS NOT DISTINCT FROM $12
              AND workspace_instance_generation IS NOT DISTINCT FROM $13
              AND restored_checkpoint_id IS NOT DISTINCT FROM $14
            "#,
        )
        .bind(expected.session_id)
        .bind(&expected.worker_id)
        .bind(&expected.provider)
        .bind(expected.generation)
        .bind(expected.provisioning_operation_id)
        .bind(expected.handle.clone().map(Json))
        .bind(retry_after.as_secs_f64())
        .bind(claim_token)
        .bind(PROVISIONING_VISIBILITY_GRACE.as_secs_f64())
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }
}

/// Column list every lease read projects, kept in one place so the row decoder
/// and every query stay in agreement.
pub(super) const LEASE_COLUMNS: &str = "session_id, worker_id, tenant_id, provider, tier, handle, \
     status, generation, provisioning_operation_id, provisioning_deadline_at, workspace_id, \
     workspace_writer_epoch, workspace_instance_generation, restored_checkpoint_id, \
     created_at, updated_at, idle_expires_at, hard_expires_at, reap_not_before, profile, \
     profile_hash, source_deployment_revision, source_tenant_revision, source_agent_revision, \
     source_route_revision, source_origin_revision, capability_revision";

pub(super) fn hand_lease_from_row(row: &sqlx::postgres::PgRow) -> Result<HandLease> {
    let status = HandLeaseStatus::from_str(
        row.try_get::<String, _>("status")
            .map_err(map_sqlx_error)?
            .as_str(),
    )?;
    let attachment = hand_lease_attachment_from_row(row, status)?;
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
        status,
        generation: row.try_get("generation").map_err(map_sqlx_error)?,
        provisioning_operation_id: row
            .try_get("provisioning_operation_id")
            .map_err(map_sqlx_error)?,
        provisioning_deadline_at: row
            .try_get("provisioning_deadline_at")
            .map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
        idle_expires_at: row.try_get("idle_expires_at").map_err(map_sqlx_error)?,
        hard_expires_at: row.try_get("hard_expires_at").map_err(map_sqlx_error)?,
        reap_not_before: row.try_get("reap_not_before").map_err(map_sqlx_error)?,
        attachment,
        policy: hand_lease_policy_from_row(row)?,
    })
}

/// Decodes an all-or-none attachment and rejects non-routable live rows.
fn hand_lease_attachment_from_row(
    row: &sqlx::postgres::PgRow,
    status: HandLeaseStatus,
) -> Result<Option<HandLeaseWorkspaceAttachment>> {
    let workspace_id = row
        .try_get::<Option<SandboxWorkspaceId>, _>("workspace_id")
        .map_err(map_sqlx_error)?;
    let writer_epoch = row
        .try_get::<Option<i64>, _>("workspace_writer_epoch")
        .map_err(map_sqlx_error)?;
    let instance_generation = row
        .try_get::<Option<i64>, _>("workspace_instance_generation")
        .map_err(map_sqlx_error)?;
    let restored_checkpoint_id = row
        .try_get::<Option<WorkspaceCheckpointId>, _>("restored_checkpoint_id")
        .map_err(map_sqlx_error)?;

    match (workspace_id, writer_epoch, instance_generation) {
        (Some(workspace_id), Some(writer_epoch), Some(instance_generation)) => {
            HandLeaseWorkspaceAttachment::new(
                workspace_id,
                writer_epoch,
                instance_generation,
                restored_checkpoint_id,
            )
            .map(Some)
        }
        (None, None, None) if restored_checkpoint_id.is_none() => {
            if matches!(
                status,
                HandLeaseStatus::Provisioning | HandLeaseStatus::Active
            ) {
                return Err(MoaError::StorageError(format!(
                    "{status:?} hand lease is missing its durable workspace attachment"
                )));
            }
            Ok(None)
        }
        _ => Err(MoaError::StorageError(
            "hand lease has a partial durable workspace attachment".to_string(),
        )),
    }
}

/// Decodes the policy identity columns, treating an incomplete identity as stale.
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
    // An absent origin makes the policy identity incomplete, so the destructure
    // below treats the row as stale and never reusable.
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
    leases: Mutex<HashMap<(TenantId, SessionId, String, String), HandLease>>,
    destroy_claims: Mutex<HashMap<(TenantId, SessionId, String, String), Uuid>>,
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
        request: HandLeaseProvisionRequest<'_>,
    ) -> Result<Option<HandLease>> {
        let HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id,
            provider,
            tier,
            attachment,
            policy,
            caller_deadline,
        } = request;
        let mut leases = self.leases.lock().await;
        let key = (
            tenant_id,
            session_id,
            worker_id.to_string(),
            provider.to_string(),
        );
        let now = Utc::now();
        let provisioning_deadline_at = provisioning_deadline(now, caller_deadline)?;
        if leases.values().any(|lease| {
            lease.session_id == session_id
                && lease.worker_id == worker_id
                && lease.provider == provider
                && lease.tenant_id != tenant_id
        }) {
            return Err(MoaError::StorageError(
                "hand lease tenant_id is immutable for an existing session scope".to_string(),
            ));
        }
        if let Some(existing) = leases.get(&key) {
            let active_expired = existing.status == HandLeaseStatus::Active
                && (existing.idle_expires_at.is_some_and(|idle| idle <= now)
                    || existing.hard_expires_at.is_some_and(|hard| hard <= now));
            if !matches!(
                existing.status,
                HandLeaseStatus::Stale | HandLeaseStatus::Destroyed
            ) && !active_expired
            {
                return Ok(None);
            }
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
            provisioning_operation_id: HandProvisioningOperationId::new(),
            provisioning_deadline_at,
            created_at: leases.get(&key).map_or(now, |existing| existing.created_at),
            updated_at: now,
            idle_expires_at: policy.idle_deadline(now),
            hard_expires_at: policy.hard_deadline(now),
            reap_not_before: Some(reconciliation_time(provisioning_deadline_at)?),
            attachment: Some(attachment),
            policy: Some(policy.clone()),
        };
        leases.insert(key, lease.clone());
        Ok(Some(lease))
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>> {
        Ok(self
            .leases
            .lock()
            .await
            .get(&(
                tenant_id,
                session_id,
                worker_id.to_string(),
                provider.to_string(),
            ))
            .cloned())
    }

    async fn get_exact_generation(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
        provisioning_operation_id: HandProvisioningOperationId,
        generation: i64,
    ) -> Result<Option<HandLease>> {
        Ok(self
            .leases
            .lock()
            .await
            .values()
            .find(|lease| {
                lease.tenant_id == tenant_id
                    && lease.session_id == session_id
                    && lease.worker_id == worker_id
                    && lease.provisioning_operation_id == provisioning_operation_id
                    && lease.generation == generation
            })
            .cloned())
    }

    async fn list_live_owner_candidates(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<Vec<HandLease>> {
        let mut leases = self
            .leases
            .lock()
            .await
            .values()
            .filter(|lease| {
                lease.tenant_id == tenant_id
                    && lease.session_id == session_id
                    && lease.worker_id == worker_id
                    && lease.status != HandLeaseStatus::Destroyed
            })
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.provider.cmp(&right.provider));
        leases.truncate(2);
        Ok(leases)
    }

    async fn has_live_owner(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        worker_id: &str,
    ) -> Result<bool> {
        Ok(self.leases.lock().await.values().any(|lease| {
            lease.tenant_id == tenant_id
                && lease.session_id == session_id
                && lease.worker_id == worker_id
                && lease.status != HandLeaseStatus::Destroyed
        }))
    }

    async fn list_live_session_page(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        cursor: Option<&HandLeaseSessionCursor>,
    ) -> Result<HandLeaseSessionPage> {
        let mut leases = self
            .leases
            .lock()
            .await
            .values()
            .filter(|lease| {
                lease.tenant_id == tenant_id
                    && lease.session_id == session_id
                    && lease.status != HandLeaseStatus::Destroyed
                    && cursor.is_none_or(|cursor| {
                        (&lease.worker_id, &lease.provider) > (&cursor.worker_id, &cursor.provider)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| {
            (&left.worker_id, &left.provider).cmp(&(&right.worker_id, &right.provider))
        });
        leases.truncate(HAND_LEASE_SESSION_PAGE_SIZE + 1);
        Ok(finish_live_session_page(&mut leases))
    }

    async fn activate(&self, request: HandLeaseActivateRequest<'_>) -> Result<bool> {
        let HandLeaseActivateRequest {
            tenant_id,
            session_id,
            worker_id,
            provider,
            generation,
            handle,
            attachment,
        } = request;
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(
            tenant_id,
            session_id,
            worker_id.to_string(),
            provider.to_string(),
        )) else {
            return Ok(false);
        };
        if lease.generation != generation
            || lease.provisioning_operation_id != handle.provisioning_operation_id
            || lease.status != HandLeaseStatus::Provisioning
            || lease.handle.is_some()
            || lease.attachment.as_ref() != Some(&attachment)
        {
            return Ok(false);
        }
        lease.handle = Some(handle);
        lease.status = HandLeaseStatus::Active;
        lease.updated_at = Utc::now();
        lease.reap_not_before = None;
        Ok(true)
    }

    async fn clear_handle_for_provisioning(
        &self,
        tenant_id: TenantId,
        claim: &HandLease,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(
            tenant_id,
            claim.session_id,
            claim.worker_id.clone(),
            claim.provider.clone(),
        )) else {
            return Ok(false);
        };
        if lease.generation != claim.generation
            || lease.provisioning_operation_id != claim.provisioning_operation_id
            || lease.status != HandLeaseStatus::Provisioning
            || lease.handle != claim.handle
            || lease.attachment != claim.attachment
        {
            return Ok(false);
        }
        lease.handle = None;
        lease.updated_at = Utc::now();
        Ok(true)
    }

    async fn renew_active(&self, request: HandLeaseRenewRequest<'_>) -> Result<bool> {
        let HandLeaseRenewRequest {
            tenant_id,
            session_id,
            worker_id,
            provider,
            generation,
            provisioning_operation_id,
            attachment,
            idle_expires_at,
        } = request;
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(
            tenant_id,
            session_id,
            worker_id.to_string(),
            provider.to_string(),
        )) else {
            return Ok(false);
        };
        let now = Utc::now();
        if lease.generation != generation
            || lease.provisioning_operation_id != provisioning_operation_id
            || lease.status != HandLeaseStatus::Active
            || lease.attachment.as_ref() != Some(&attachment)
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

    async fn transition_status(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        status: HandLeaseStatus,
    ) -> Result<bool> {
        if status == HandLeaseStatus::Reaping || expected.status == HandLeaseStatus::Reaping {
            return Err(MoaError::StorageError(
                "reaping generations may only move through their owned destroy claim".to_string(),
            ));
        }
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&(
            tenant_id,
            expected.session_id,
            expected.worker_id.clone(),
            expected.provider.clone(),
        )) else {
            return Ok(false);
        };
        if lease.generation != expected.generation
            || lease.provisioning_operation_id != expected.provisioning_operation_id
            || lease.status != expected.status
            || lease.handle != expected.handle
            || lease.attachment != expected.attachment
        {
            return Ok(false);
        }
        if status == HandLeaseStatus::Destroyed {
            lease.handle = None;
        }
        lease.status = status;
        lease.updated_at = Utc::now();
        if status != HandLeaseStatus::Failed {
            lease.reap_not_before = None;
        }
        Ok(true)
    }

    async fn claim_for_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        _claim_ttl: Duration,
    ) -> Result<Option<Uuid>> {
        if matches!(
            expected.status,
            HandLeaseStatus::Provisioning | HandLeaseStatus::Reaping | HandLeaseStatus::Destroyed
        ) || (expected.status == HandLeaseStatus::Failed
            && expected
                .reap_not_before
                .is_some_and(|not_before| not_before > Utc::now()))
        {
            return Ok(None);
        }
        let key = (
            tenant_id,
            expected.session_id,
            expected.worker_id.clone(),
            expected.provider.clone(),
        );
        let mut destroy_claims = self.destroy_claims.lock().await;
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&key) else {
            return Ok(None);
        };
        if lease.generation != expected.generation
            || lease.provisioning_operation_id != expected.provisioning_operation_id
            || lease.status != expected.status
            || lease.handle != expected.handle
            || lease.attachment != expected.attachment
        {
            return Ok(None);
        }
        let claim_token = Uuid::new_v4();
        lease.status = HandLeaseStatus::Reaping;
        lease.updated_at = Utc::now();
        destroy_claims.insert(key, claim_token);
        Ok(Some(claim_token))
    }

    async fn finalize_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
    ) -> Result<bool> {
        let key = (
            tenant_id,
            expected.session_id,
            expected.worker_id.clone(),
            expected.provider.clone(),
        );
        if self.destroy_claims.lock().await.get(&key) != Some(&claim_token) {
            return Ok(false);
        }
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&key) else {
            return Ok(false);
        };
        if lease.generation != expected.generation
            || lease.provisioning_operation_id != expected.provisioning_operation_id
            || lease.status != HandLeaseStatus::Reaping
            || lease.handle != expected.handle
            || lease.attachment != expected.attachment
        {
            return Ok(false);
        }
        lease.handle = None;
        lease.status = HandLeaseStatus::Destroyed;
        lease.updated_at = Utc::now();
        lease.reap_not_before = None;
        drop(leases);
        self.destroy_claims.lock().await.remove(&key);
        Ok(true)
    }

    async fn release_destroy_claim(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: Uuid,
        retry_after: Duration,
    ) -> Result<bool> {
        let key = (
            tenant_id,
            expected.session_id,
            expected.worker_id.clone(),
            expected.provider.clone(),
        );
        if self.destroy_claims.lock().await.get(&key) != Some(&claim_token) {
            return Ok(false);
        }
        let mut leases = self.leases.lock().await;
        let Some(lease) = leases.get_mut(&key) else {
            return Ok(false);
        };
        if lease.generation != expected.generation
            || lease.provisioning_operation_id != expected.provisioning_operation_id
            || lease.status != HandLeaseStatus::Reaping
            || lease.handle != expected.handle
            || lease.attachment != expected.attachment
        {
            return Ok(false);
        }
        let now = Utc::now();
        let backoff = chrono::Duration::from_std(retry_after)
            .ok()
            .and_then(|retry_after| now.checked_add_signed(retry_after))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        // Matches the Postgres store: a failed cleanup never schedules its next
        // attempt inside the provider visibility grace.
        let reconcile_after = reconciliation_time(lease.provisioning_deadline_at)?;
        lease.status = HandLeaseStatus::Failed;
        lease.updated_at = now;
        lease.reap_not_before = Some(backoff.max(reconcile_after));
        drop(leases);
        self.destroy_claims.lock().await.remove(&key);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::test_support::lease_policy;
    use super::*;

    fn provision_request<'a>(
        session_id: SessionId,
        worker_id: &'a str,
        tenant_id: TenantId,
        policy: &'a HandLeasePolicy,
    ) -> HandLeaseProvisionRequest<'a> {
        provision_request_for_provider(session_id, worker_id, tenant_id, policy, "local")
    }

    fn provision_request_for_provider<'a>(
        session_id: SessionId,
        worker_id: &'a str,
        tenant_id: TenantId,
        policy: &'a HandLeasePolicy,
        provider: &'a str,
    ) -> HandLeaseProvisionRequest<'a> {
        HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id,
            provider,
            tier: SandboxTier::Local,
            attachment: HandLeaseWorkspaceAttachment::new(SandboxWorkspaceId::new(), 1, 1, None)
                .expect("test attachment should validate"),
            policy,
            caller_deadline: None,
        }
    }

    #[tokio::test]
    async fn memory_store_fences_concurrent_provision_claims() {
        // Pins: only one router replica can own provisioning for a session/provider generation.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");

        let (left, right) = tokio::join!(
            store.claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy,)),
            store.claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy,))
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
            .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
            .await
            .expect("session claim succeeds")
            .expect("session claim is owned");
        // The same session/provider under a worker scope must still be claimable.
        let worker_claim = store
            .claim_for_provisioning(provision_request(session_id, "sub-x", tenant_id, &policy))
            .await
            .expect("worker claim succeeds")
            .expect("worker claim is owned because it is a distinct scope");

        assert_eq!(session_claim.worker_id, "");
        assert_eq!(worker_claim.worker_id, "sub-x");
        assert!(
            store
                .get(tenant_id, session_id, "", "local")
                .await
                .expect("load session lease")
                .is_some()
        );
        assert!(
            store
                .get(tenant_id, session_id, "sub-x", "local")
                .await
                .expect("load worker lease")
                .is_some()
        );
        let listed = store
            .list_live_session_page(tenant_id, session_id, None)
            .await
            .expect("list leases");
        assert_eq!(
            listed.leases.len(),
            2,
            "both live scopes belong to the session"
        );
        assert_eq!(listed.next_cursor, None);
    }

    #[tokio::test]
    async fn memory_store_live_session_pages_are_bounded_replayable_and_complete() {
        // Pins: terminal cleanup of a session with more than one page of live
        // hands resumes by keyset cursor without materializing destroyed history,
        // skipping a live lease, or changing a replayed page.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-session-page");

        for index in 0..128 {
            let worker_id = format!("destroyed-owner-{index:03}");
            let claim = store
                .claim_for_provisioning(provision_request(
                    session_id, &worker_id, tenant_id, &policy,
                ))
                .await
                .expect("seed destroyed lease")
                .expect("destroyed-history owner should claim its own row");
            assert!(
                store
                    .transition_status(tenant_id, &claim, HandLeaseStatus::Destroyed)
                    .await
                    .expect("mark historical lease destroyed"),
                "seeded historical lease should retain its fence"
            );
        }

        let live_count = HAND_LEASE_SESSION_PAGE_SIZE + 7;
        for index in 0..live_count {
            let worker_id = format!("live-owner-{index:03}");
            store
                .claim_for_provisioning(provision_request(
                    session_id, &worker_id, tenant_id, &policy,
                ))
                .await
                .expect("seed live lease")
                .expect("live owner should claim its own row");
        }

        let first = store
            .list_live_session_page(tenant_id, session_id, None)
            .await
            .expect("load first live session page");
        let replay = store
            .list_live_session_page(tenant_id, session_id, None)
            .await
            .expect("replay first live session page");
        assert_eq!(first, replay, "the same cursor must replay the same page");
        assert_eq!(first.leases.len(), HAND_LEASE_SESSION_PAGE_SIZE);
        let cursor = first
            .next_cursor
            .as_ref()
            .expect("a saturated first page must expose continuation");
        assert_eq!(
            first
                .leases
                .last()
                .map(|lease| (&lease.worker_id, &lease.provider)),
            Some((&cursor.worker_id, &cursor.provider)),
            "continuation must start after the last returned lease"
        );

        let second = store
            .list_live_session_page(tenant_id, session_id, Some(cursor))
            .await
            .expect("load final live session page");
        assert_eq!(second.leases.len(), 7);
        assert_eq!(second.next_cursor, None);

        let keys = first
            .leases
            .iter()
            .chain(&second.leases)
            .map(|lease| (lease.worker_id.as_str(), lease.provider.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), live_count);
        assert!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "keyset pages must be strictly ordered without duplicates"
        );
        assert!(
            keys.iter()
                .all(|(worker_id, _)| worker_id.starts_with("live-owner-")),
            "destroyed session history must not consume a live cleanup page"
        );
    }

    #[tokio::test]
    async fn memory_store_exact_owner_lookup_excludes_unrelated_session_history() {
        // Pins: owner-scoped teardown observes only the exact compensation scope,
        // even when the same session carries a large unrelated lease history.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-owner-lookup");
        for index in 0..128 {
            let worker_id = format!("unrelated-owner-{index}");
            store
                .claim_for_provisioning(provision_request(
                    session_id, &worker_id, tenant_id, &policy,
                ))
                .await
                .expect("seed unrelated lease")
                .expect("unrelated owner should claim its own row");
        }
        let target = "execution_compensation:run-1:compensation-1";
        for provider in ["local", "daytona", "e2b"] {
            store
                .claim_for_provisioning(provision_request_for_provider(
                    session_id, target, tenant_id, &policy, provider,
                ))
                .await
                .expect("seed target lease")
                .expect("target owner should claim its own row");
        }

        let leases = store
            .list_live_owner_candidates(tenant_id, session_id, target)
            .await
            .expect("load exact target owner");
        assert_eq!(
            leases.len(),
            2,
            "the release probe must not materialize every live replacement"
        );
        assert!(leases.iter().all(|lease| lease.worker_id == target));
    }

    #[tokio::test]
    async fn memory_store_reuses_active_generation_until_stale() {
        // Pins: active leases block double-provisioning until they are marked stale.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let claimed = store
            .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        let attachment = claimed
            .attachment
            .clone()
            .expect("provisioning claim carries workspace attachment");
        store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "",
                provider: "local",
                generation: claimed.generation,
                handle: LeaseHandle::new(
                    claimed.provisioning_operation_id,
                    HandHandle::local(PathBuf::from("/tmp/moa-hand")),
                ),
                attachment,
            })
            .await
            .expect("activate lease");

        assert!(
            store
                .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
                .await
                .expect("active claim check")
                .is_none()
        );

        let active = store
            .get(tenant_id, session_id, "", "local")
            .await
            .expect("load active lease")
            .expect("active lease exists");
        assert!(
            store
                .transition_status(tenant_id, &active, HandLeaseStatus::Stale)
                .await
                .expect("mark stale")
        );
        let replacement = store
            .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
            .await
            .expect("replacement claim")
            .expect("stale lease should allow replacement");

        assert_eq!(replacement.generation, claimed.generation + 1);
        assert_ne!(
            replacement.provisioning_operation_id, claimed.provisioning_operation_id,
            "a replacement generation must rotate its durable provider identity"
        );
        assert_eq!(
            replacement.handle,
            Some(LeaseHandle::new(
                claimed.provisioning_operation_id,
                HandHandle::local(PathBuf::from("/tmp/moa-hand")),
            )),
            "stale reclaim must preserve the real handle until a new activation wins"
        );
    }

    #[tokio::test]
    async fn memory_store_provisioning_claim_records_policy_identity_and_caller_deadline() {
        // Pins: a provisioning claim writes no fake handle and carries the exact
        // policy identity and caller-narrowed deadlines from the moment the generation exists.
        let store = MemoryHandLeaseStore::shared();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let caller_deadline = Utc::now() + chrono::Duration::seconds(60);
        let claim = store
            .claim_for_provisioning(HandLeaseProvisionRequest {
                session_id: SessionId::new(),
                worker_id: "",
                tenant_id: TenantId::new(),
                provider: "local",
                tier: SandboxTier::Local,
                attachment: HandLeaseWorkspaceAttachment::new(
                    SandboxWorkspaceId::new(),
                    1,
                    1,
                    None,
                )
                .expect("test attachment should validate"),
                policy: &policy,
                caller_deadline: Some(caller_deadline),
            })
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
        assert_eq!(claim.provisioning_deadline_at, caller_deadline);
        assert!(
            claim.reap_not_before > Some(claim.provisioning_deadline_at),
            "reconciliation must start strictly after create can finish"
        );
    }

    #[tokio::test]
    async fn memory_store_rejects_mismatched_workspace_hydration_fences() {
        // Pins: provider success cannot activate or renew a hand after its
        // workspace writer/instance/revision attachment fence changes.
        let store = MemoryHandLeaseStore::shared();
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let policy = lease_policy(Some(300), Some(3600), "cap-1");
        let claim = store
            .claim_for_provisioning(provision_request(
                session_id, "worker-a", tenant_id, &policy,
            ))
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        let exact = claim
            .attachment
            .clone()
            .expect("provisioning claim carries workspace attachment");
        let stale = HandLeaseWorkspaceAttachment::new(
            exact.workspace_id,
            exact.workspace_writer_epoch + 1,
            exact.workspace_instance_generation,
            exact.restored_checkpoint_id,
        )
        .expect("stale test attachment should validate");
        let handle = LeaseHandle::new(
            claim.provisioning_operation_id,
            HandHandle::local(PathBuf::from("/tmp/moa-hand-fenced")),
        );

        assert!(
            !store
                .activate(HandLeaseActivateRequest {
                    tenant_id,
                    session_id,
                    worker_id: "worker-a",
                    provider: "local",
                    generation: claim.generation,
                    handle: handle.clone(),
                    attachment: stale.clone(),
                })
                .await
                .expect("mismatched activation should not fail storage"),
            "a stale writer epoch must not activate compute"
        );
        let still_provisioning = store
            .get(tenant_id, session_id, "worker-a", "local")
            .await
            .expect("load lease after rejected activation")
            .expect("lease remains present");
        assert_eq!(still_provisioning.status, HandLeaseStatus::Provisioning);
        assert_eq!(still_provisioning.handle, None);

        assert!(
            store
                .activate(HandLeaseActivateRequest {
                    tenant_id,
                    session_id,
                    worker_id: "worker-a",
                    provider: "local",
                    generation: claim.generation,
                    handle,
                    attachment: exact.clone(),
                })
                .await
                .expect("exact activation succeeds")
        );
        assert!(
            !store
                .renew_active(HandLeaseRenewRequest {
                    tenant_id,
                    session_id,
                    worker_id: "worker-a",
                    provider: "local",
                    generation: claim.generation,
                    provisioning_operation_id: claim.provisioning_operation_id,
                    attachment: stale,
                    idle_expires_at: Utc::now() + chrono::Duration::seconds(600),
                })
                .await
                .expect("mismatched renewal should not fail storage"),
            "a stale workspace attachment must not renew an active lease"
        );
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
            .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
            .await
            .expect("claim should succeed")
            .expect("claim should be owned");
        let attachment = claim
            .attachment
            .clone()
            .expect("provisioning claim carries workspace attachment");
        store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "",
                provider: "local",
                generation: claim.generation,
                handle: LeaseHandle::new(
                    claim.provisioning_operation_id,
                    HandHandle::local(PathBuf::from("/tmp/moa-hand")),
                ),
                attachment: attachment.clone(),
            })
            .await
            .expect("activate lease");

        assert!(
            !store
                .renew_active(HandLeaseRenewRequest {
                    tenant_id,
                    session_id,
                    worker_id: "",
                    provider: "local",
                    generation: claim.generation + 1,
                    provisioning_operation_id: claim.provisioning_operation_id,
                    attachment: attachment.clone(),
                    idle_expires_at: renewed_expiry,
                })
                .await
                .expect("wrong generation renewal should not fail storage")
        );
        assert!(
            store
                .renew_active(HandLeaseRenewRequest {
                    tenant_id,
                    session_id,
                    worker_id: "",
                    provider: "local",
                    generation: claim.generation,
                    provisioning_operation_id: claim.provisioning_operation_id,
                    attachment,
                    idle_expires_at: renewed_expiry,
                })
                .await
                .expect("current generation renewal should succeed")
        );
        let renewed = store
            .get(tenant_id, session_id, "", "local")
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
            .claim_for_provisioning(provision_request(session_id, "", tenant_id, &policy))
            .await
            .expect("claim")
            .expect("claim is owned");
        let attachment = claim
            .attachment
            .clone()
            .expect("provisioning claim carries workspace attachment");
        let hard_deadline = claim.hard_expires_at.expect("bounded hard deadline");
        store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "",
                provider: "local",
                generation: claim.generation,
                handle: LeaseHandle::new(
                    claim.provisioning_operation_id,
                    HandHandle::local(PathBuf::from("/tmp/moa-hand")),
                ),
                attachment: attachment.clone(),
            })
            .await
            .expect("activate lease");

        let greedy = Utc::now() + chrono::Duration::hours(24);
        assert!(
            store
                .renew_active(HandLeaseRenewRequest {
                    tenant_id,
                    session_id,
                    worker_id: "",
                    provider: "local",
                    generation: claim.generation,
                    provisioning_operation_id: claim.provisioning_operation_id,
                    attachment,
                    idle_expires_at: greedy,
                })
                .await
                .expect("renewal within the hard lifetime succeeds")
        );

        let renewed = store
            .get(tenant_id, session_id, "", "local")
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
