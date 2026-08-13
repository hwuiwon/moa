//! Durable provider-operation intent, outcome, and reconciliation ledger.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceOperationId},
        memory::RlsContext,
        sandbox_workspace::{
            WorkspaceConfirmedDisposition, WorkspaceOperationKind, WorkspaceOperationOutcome,
        },
    },
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::failpoints;
use crate::core::leases::map_sqlx_error;

/// Minimum separation between independent empty provider observations.
pub const EMPTY_OBSERVATION_SEPARATION: Duration = Duration::from_secs(1);

/// Durable intent persisted before one provider storage request.
#[derive(Debug, Clone)]
pub struct WorkspaceOperationIntent {
    /// Replay-stable operation identity.
    pub operation_id: WorkspaceOperationId,
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Workspace being changed.
    pub workspace_id: SandboxWorkspaceId,
    /// Provider account receiving the request.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account generation.
    pub provider_account_generation: i64,
    /// External operation kind.
    pub kind: WorkspaceOperationKind,
    /// Canonical request hash used to detect conflicting replay.
    pub request_hash: String,
    /// Required writer fence.
    pub expected_writer_epoch: i64,
    /// Required compute-instance fence.
    pub expected_instance_generation: i64,
    /// Required committed checkpoint generation.
    pub expected_checkpoint_generation: i64,
    /// Absolute provider-call deadline.
    pub deadline_at: DateTime<Utc>,
    /// Earliest safe provider-inventory reconciliation time.
    pub reconcile_not_before: DateTime<Utc>,
}

/// Persisted operation row, including ambiguity and absence evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceOperation {
    /// Durable operation identity.
    pub operation_id: WorkspaceOperationId,
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Workspace being changed.
    pub workspace_id: SandboxWorkspaceId,
    /// Provider account that received the request.
    pub provider_account_id: ProviderAccountId,
    /// Provider-account generation used by the request.
    pub provider_account_generation: i64,
    /// External operation kind.
    pub kind: WorkspaceOperationKind,
    /// Canonical request hash.
    pub request_hash: String,
    /// Expected writer fence.
    pub expected_writer_epoch: i64,
    /// Expected compute-instance fence.
    pub expected_instance_generation: i64,
    /// Expected checkpoint head generation.
    pub expected_checkpoint_generation: i64,
    /// Absolute operation deadline.
    pub deadline_at: DateTime<Utc>,
    /// Earliest safe reconciliation time.
    pub reconcile_not_before: DateTime<Utc>,
    /// Durable external outcome class.
    pub outcome: WorkspaceOperationOutcome,
    /// Verified resource disposition for a confirmed outcome.
    pub confirmed_disposition: Option<WorkspaceConfirmedDisposition>,
    /// Persisted empty observation count, capped at two.
    pub absence_observation_count: i32,
    /// First observation in the current empty proof.
    pub absence_first_observed_at: Option<DateTime<Utc>>,
    /// Last observation in the current empty proof.
    pub absence_last_observed_at: Option<DateTime<Utc>>,
    /// Stable inventory digest shared by the current empty proof.
    pub absence_inventory_digest: Option<String>,
    /// Current reaper claim token.
    pub claim_token: Option<Uuid>,
    /// Current reaper claim expiry.
    pub claim_expires_at: Option<DateTime<Utc>>,
    /// Failed reconciliation attempts.
    pub attempts: i32,
}

/// One operation exclusively claimed by a workspace reaper replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedWorkspaceOperation {
    /// Claimed operation snapshot.
    pub operation: WorkspaceOperation,
    /// Exact owner token required for renew, finalize, or release.
    pub claim_token: Uuid,
}

/// Result of recording one provider inventory observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsenceObservation {
    /// Provider inventory was nonempty or changed, so no absence proof exists.
    Reset,
    /// One empty observation is persisted; a separated second one is required.
    First,
    /// Two separated empty observations with one digest prove absence.
    Proven,
}

/// Postgres-backed workspace-operation ledger.
#[derive(Clone)]
pub struct PostgresWorkspaceOperationRepository {
    pool: PgPool,
    assume_workspace_maintenance_role: bool,
}

impl PostgresWorkspaceOperationRepository {
    /// Creates an operation ledger over the runtime Postgres pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: false,
        }
    }

    /// Creates the process-owned reconciliation ledger over its dedicated pool.
    #[must_use]
    pub const fn new_maintenance(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: true,
        }
    }

    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await
    }

    async fn begin_maintenance(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
        if self.assume_workspace_maintenance_role {
            sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
        } else {
            conn.assume_app_role().await?;
        }
        Ok(conn)
    }

    /// Inserts one intent after verifying every workspace generation fence.
    pub async fn persist_intent(
        &self,
        intent: &WorkspaceOperationIntent,
    ) -> Result<WorkspaceOperation> {
        validate_intent(intent)?;
        let mut conn = self.begin(intent.tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.sandbox_workspace_operations (
                operation_id, tenant_id, workspace_id, provider_account_id,
                provider_account_generation, operation_kind, request_hash,
                expected_writer_epoch, expected_instance_generation,
                expected_checkpoint_generation, deadline_at, reconcile_not_before
            )
            SELECT $1, $2, workspace.workspace_id, $3, $4, $5, $6, $7, $8, $9, $10, $11
            FROM moa.sandbox_workspaces AS workspace
            WHERE workspace.tenant_id = $2 AND workspace.workspace_id = $12
              AND workspace.provider_account_id = $3
              AND workspace.provider_account_generation = $4
              AND workspace.writer_epoch = $7
              AND workspace.instance_generation = $8
              AND workspace.current_checkpoint_generation = $9
              AND workspace.lifecycle_state <> 'deleted'
            ON CONFLICT (operation_id) DO NOTHING
            RETURNING {OPERATION_COLUMNS}
            "#,
        ))
        .bind(intent.operation_id)
        .bind(intent.tenant_id)
        .bind(intent.provider_account_id)
        .bind(intent.provider_account_generation)
        .bind(intent.kind.as_str())
        .bind(&intent.request_hash)
        .bind(intent.expected_writer_epoch)
        .bind(intent.expected_instance_generation)
        .bind(intent.expected_checkpoint_generation)
        .bind(intent.deadline_at)
        .bind(intent.reconcile_not_before)
        .bind(intent.workspace_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let operation = if let Some(row) = row {
            operation_from_row(&row)?
        } else {
            let existing = sqlx::query(&format!(
                "SELECT {OPERATION_COLUMNS} FROM moa.sandbox_workspace_operations WHERE tenant_id = $1 AND operation_id = $2"
            ))
            .bind(intent.tenant_id)
            .bind(intent.operation_id)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
            .as_ref()
            .map(operation_from_row)
            .transpose()?;
            match existing {
                Some(existing) if operation_matches_intent(&existing, intent) => existing,
                Some(_) => {
                    conn.rollback().await?;
                    return Err(MoaError::ValidationError(
                        "workspace operation replay conflicts with the persisted intent"
                            .to_string(),
                    ));
                }
                None => {
                    conn.rollback().await?;
                    return Err(MoaError::StorageError(
                        "workspace operation intent lost its workspace generation fence"
                            .to_string(),
                    ));
                }
            }
        };
        conn.commit().await?;
        Ok(operation)
    }

    /// Loads one tenant-scoped operation.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        operation_id: WorkspaceOperationId,
    ) -> Result<Option<WorkspaceOperation>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            "SELECT {OPERATION_COLUMNS} FROM moa.sandbox_workspace_operations WHERE tenant_id = $1 AND operation_id = $2"
        ))
        .bind(tenant_id)
        .bind(operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let operation = row.as_ref().map(operation_from_row).transpose()?;
        conn.commit().await?;
        Ok(operation)
    }

    /// Renews an expired provider deadline only while an exact commit is provably unsent.
    ///
    /// Once an operation is `unknown` or `confirmed`, its deadline is immutable:
    /// changing it could authorize a duplicate provider effect. The caller must
    /// supply the previously read deadline so concurrent recovery remains a CAS.
    pub async fn renew_not_sent_commit_deadline(
        &self,
        tenant_id: TenantId,
        operation_id: WorkspaceOperationId,
        expected_deadline_at: DateTime<Utc>,
        deadline_at: DateTime<Utc>,
    ) -> Result<bool> {
        if deadline_at <= expected_deadline_at {
            return Err(MoaError::ValidationError(
                "renewed workspace commit deadline must advance".to_string(),
            ));
        }
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET deadline_at = $4, reconcile_not_before = $4 + interval '30 seconds',
                updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND operation_kind = 'commit' AND outcome_class = 'not_sent'
              AND claim_token IS NULL AND deadline_at = $3
            "#,
        )
        .bind(tenant_id)
        .bind(operation_id)
        .bind(expected_deadline_at)
        .bind(deadline_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Fences an exact provider attempt as potentially sent without abandoning its commit CAS.
    ///
    /// This is the last durable write before provider I/O. It changes only a
    /// provably unsent, unclaimed operation to `unknown`. Create and hydration
    /// operations atomically move the exact workspace fence to `reconciling`;
    /// commit keeps `committing` so its synchronous result can still atomically
    /// publish the checkpoint and head. A subsequent error must call
    /// [`Self::mark_unknown`] to move reservations into reconciliation too.
    pub async fn begin_provider_attempt(
        &self,
        tenant_id: TenantId,
        operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'unknown', confirmed_disposition = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND outcome_class = 'not_sent' AND claim_token IS NULL
            RETURNING workspace_id, operation_kind, expected_writer_epoch,
                      expected_instance_generation
            "#,
        )
        .bind(tenant_id)
        .bind(operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if let Some(row) = &row {
            let kind: String = row.try_get("operation_kind").map_err(map_sqlx_error)?;
            if matches!(kind.as_str(), "create" | "attach" | "restore") {
                let workspace_transitioned = sqlx::query(
                    r#"
                    UPDATE moa.sandbox_workspaces
                    SET lifecycle_state = 'reconciling', updated_at = now()
                    WHERE tenant_id = $1 AND workspace_id = $2
                      AND writer_epoch = $3 AND instance_generation = $4
                      AND lifecycle_state = CASE
                            WHEN $5 = 'create' THEN 'creating'
                            ELSE 'restoring'
                          END
                    "#,
                )
                .bind(tenant_id)
                .bind(
                    row.try_get::<SandboxWorkspaceId, _>("workspace_id")
                        .map_err(map_sqlx_error)?,
                )
                .bind(
                    row.try_get::<i64, _>("expected_writer_epoch")
                        .map_err(map_sqlx_error)?,
                )
                .bind(
                    row.try_get::<i64, _>("expected_instance_generation")
                        .map_err(map_sqlx_error)?,
                )
                .bind(&kind)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?
                .rows_affected()
                    == 1;
                if !workspace_transitioned {
                    conn.rollback().await?;
                    return Ok(false);
                }
            }
        }
        conn.commit().await?;
        Ok(row.is_some())
    }

    /// Records that a provider request may have been sent and retains reservations.
    pub async fn mark_unknown(
        &self,
        tenant_id: TenantId,
        operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'unknown', confirmed_disposition = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND outcome_class IN ('not_sent', 'unknown') AND claim_token IS NULL
            RETURNING workspace_id, operation_kind, expected_writer_epoch, expected_instance_generation
            "#,
        )
        .bind(tenant_id)
        .bind(operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if let Some(row) = &row {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations AS reservation
                SET reservation_state = 'reconciling', updated_at = now()
                FROM moa.sandbox_workspace_operations AS operation
                WHERE operation.tenant_id = $1 AND operation.operation_id = $2
                  AND reservation.tenant_id = operation.tenant_id
                  AND reservation.operation_id = operation.operation_id
                  AND reservation.workspace_id = operation.workspace_id
                  AND reservation.provider_account_id = operation.provider_account_id
                  AND reservation.provider_account_generation = operation.provider_account_generation
                  AND reservation.expected_writer_epoch = operation.expected_writer_epoch
                  AND reservation.expected_instance_generation = operation.expected_instance_generation
                  AND reservation.reservation_state = 'pending'
                "#,
            )
            .bind(tenant_id)
            .bind(operation_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            let kind: String = row.try_get("operation_kind").map_err(map_sqlx_error)?;
            if kind != "delete" {
                sqlx::query(
                    r#"
                    UPDATE moa.sandbox_workspaces
                    SET lifecycle_state = 'reconciling', updated_at = now()
                    WHERE tenant_id = $1 AND workspace_id = $2
                      AND writer_epoch = $3 AND instance_generation = $4
                      AND lifecycle_state NOT IN ('deleting', 'deleted')
                    "#,
                )
                .bind(tenant_id)
                .bind(
                    row.try_get::<SandboxWorkspaceId, _>("workspace_id")
                        .map_err(map_sqlx_error)?,
                )
                .bind(
                    row.try_get::<i64, _>("expected_writer_epoch")
                        .map_err(map_sqlx_error)?,
                )
                .bind(
                    row.try_get::<i64, _>("expected_instance_generation")
                        .map_err(map_sqlx_error)?,
                )
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
            }
        }
        conn.commit().await?;
        Ok(row.is_some())
    }

    /// Confirms one synchronous provider result under the exact operation fence.
    ///
    /// The caller invokes this only for the direct result of the provider request
    /// whose `not_sent -> unknown` CAS it just won. Delayed recovery never calls
    /// this method: it uses the claimed reconciliation methods, where absence
    /// requires two separated observations.
    pub async fn confirm_disposition(
        &self,
        tenant_id: TenantId,
        operation_id: WorkspaceOperationId,
        disposition: WorkspaceConfirmedDisposition,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'confirmed', confirmed_disposition = $3,
                claim_token = NULL, claim_expires_at = NULL, retry_not_before = NULL,
                updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND outcome_class = 'unknown'
              AND claim_token IS NULL
            "#,
        )
        .bind(tenant_id)
        .bind(operation_id)
        .bind(disposition.as_str())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 1 {
            let reservation_state = match disposition {
                WorkspaceConfirmedDisposition::ResourcePresent => "committed",
                WorkspaceConfirmedDisposition::ResourceAbsent => "released",
            };
            sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations AS reservation
                SET reservation_state = $3, updated_at = now()
                FROM moa.sandbox_workspace_operations AS operation
                WHERE operation.tenant_id = $1 AND operation.operation_id = $2
                  AND reservation.tenant_id = operation.tenant_id
                  AND reservation.operation_id = operation.operation_id
                  AND reservation.workspace_id = operation.workspace_id
                  AND reservation.provider_account_id = operation.provider_account_id
                  AND reservation.provider_account_generation = operation.provider_account_generation
                  AND reservation.expected_writer_epoch = operation.expected_writer_epoch
                  AND reservation.expected_instance_generation = operation.expected_instance_generation
                  AND reservation.reservation_state IN ('pending', 'reconciling')
                "#,
            )
            .bind(tenant_id)
            .bind(operation_id)
            .bind(reservation_state)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            settle_workspace_lifecycle_after_confirmation(
                conn.as_mut(),
                tenant_id,
                operation_id,
                disposition,
                false,
            )
            .await?;
        }
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Confirms a present resource only while the exact reaper claim is live.
    ///
    /// Capacity becomes committed in the same transaction as the claimed
    /// operation outcome, so another replica cannot release or steal the
    /// reservation between provider proof and durable finalization.
    pub async fn confirm_present_claimed(
        &self,
        claimed: &ClaimedWorkspaceOperation,
    ) -> Result<bool> {
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'confirmed', confirmed_disposition = 'resource_present',
                absence_observation_count = 0,
                absence_first_observed_at = NULL,
                absence_last_observed_at = NULL,
                absence_inventory_digest = NULL,
                claim_token = NULL, claim_expires_at = NULL, retry_not_before = NULL,
                updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND claim_token = $3 AND claim_expires_at > now()
              AND outcome_class IN ('not_sent', 'unknown')
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.operation_id)
        .bind(claimed.claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 1 {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations AS reservation
                SET reservation_state = 'committed', updated_at = now()
                FROM moa.sandbox_workspace_operations AS operation
                WHERE operation.tenant_id = $1 AND operation.operation_id = $2
                  AND reservation.tenant_id = operation.tenant_id
                  AND reservation.operation_id = operation.operation_id
                  AND reservation.workspace_id = operation.workspace_id
                  AND reservation.provider_account_id = operation.provider_account_id
                  AND reservation.provider_account_generation = operation.provider_account_generation
                  AND reservation.expected_writer_epoch = operation.expected_writer_epoch
                  AND reservation.expected_instance_generation = operation.expected_instance_generation
                  AND reservation.reservation_state IN ('pending', 'reconciling')
                "#,
            )
            .bind(claimed.operation.tenant_id)
            .bind(claimed.operation.operation_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            settle_workspace_lifecycle_after_confirmation(
                conn.as_mut(),
                claimed.operation.tenant_id,
                claimed.operation.operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
                true,
            )
            .await?;
        }
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Records one provider inventory observation under the exact reaper claim.
    ///
    /// A nonempty observation or changed digest resets the proof. A second
    /// empty observation only advances the proof after the required separation.
    pub async fn record_inventory_observation(
        &self,
        claimed: &ClaimedWorkspaceOperation,
        inventory_empty: bool,
        inventory_digest: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<AbsenceObservation> {
        if inventory_digest.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "provider inventory digest must not be empty".to_string(),
            ));
        }
        let mut conn = self.begin_maintenance().await?;
        let row = if inventory_empty {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_workspace_operations
                SET absence_observation_count = CASE
                        WHEN absence_inventory_digest IS DISTINCT FROM $4 THEN 1
                        WHEN absence_observation_count = 0 THEN 1
                        WHEN absence_observation_count = 1
                             AND $5 >= absence_first_observed_at + interval '1 second' THEN 2
                        ELSE absence_observation_count
                    END,
                    absence_first_observed_at = CASE
                        WHEN absence_inventory_digest IS DISTINCT FROM $4
                             OR absence_observation_count = 0 THEN $5
                        ELSE absence_first_observed_at
                    END,
                    absence_last_observed_at = CASE
                        WHEN absence_inventory_digest IS DISTINCT FROM $4
                             OR absence_observation_count = 0 THEN $5
                        WHEN absence_observation_count = 1
                             AND $5 >= absence_first_observed_at + interval '1 second' THEN $5
                        ELSE absence_last_observed_at
                    END,
                    absence_inventory_digest = $4,
                    updated_at = now()
                WHERE tenant_id = $1 AND operation_id = $2
                  AND claim_token = $3 AND claim_expires_at > now()
                  AND outcome_class IN ('not_sent', 'unknown')
                RETURNING absence_observation_count
                "#,
            )
            .bind(claimed.operation.tenant_id)
            .bind(claimed.operation.operation_id)
            .bind(claimed.claim_token)
            .bind(inventory_digest)
            .bind(observed_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
        } else {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_workspace_operations
                SET absence_observation_count = 0, absence_first_observed_at = NULL,
                    absence_last_observed_at = NULL, absence_inventory_digest = NULL,
                    updated_at = now()
                WHERE tenant_id = $1 AND operation_id = $2
                  AND claim_token = $3 AND claim_expires_at > now()
                  AND outcome_class IN ('not_sent', 'unknown')
                RETURNING absence_observation_count
                "#,
            )
            .bind(claimed.operation.tenant_id)
            .bind(claimed.operation.operation_id)
            .bind(claimed.claim_token)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
        };
        let count = row
            .as_ref()
            .ok_or_else(|| {
                MoaError::StorageError("workspace reconciliation claim lost".to_string())
            })?
            .try_get::<i32, _>("absence_observation_count")
            .map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(match count {
            0 => AbsenceObservation::Reset,
            1 => AbsenceObservation::First,
            2 => AbsenceObservation::Proven,
            other => {
                return Err(MoaError::StorageError(format!(
                    "invalid persisted absence observation count: {other}"
                )));
            }
        })
    }

    /// Confirms absence only after the database validates a separated pair.
    ///
    /// Reservations are released in the same transaction and only through the
    /// operation's complete workspace/provider/generation fence.
    pub async fn confirm_absent(&self, claimed: &ClaimedWorkspaceOperation) -> Result<bool> {
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'confirmed', confirmed_disposition = 'resource_absent',
                claim_token = NULL, claim_expires_at = NULL, retry_not_before = NULL,
                updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND claim_token = $3 AND claim_expires_at > now()
              AND outcome_class IN ('not_sent', 'unknown')
              AND absence_observation_count = 2
              AND absence_last_observed_at >= absence_first_observed_at + interval '1 second'
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.operation_id)
        .bind(claimed.claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 1 {
            failpoints::hit("post_absence_confirmation_pre_reservation_release").await?;
            sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations AS reservation
                SET reservation_state = 'released', updated_at = now()
                FROM moa.sandbox_workspace_operations AS operation
                WHERE operation.tenant_id = $1 AND operation.operation_id = $2
                  AND reservation.tenant_id = operation.tenant_id
                  AND reservation.operation_id = operation.operation_id
                  AND reservation.workspace_id = operation.workspace_id
                  AND reservation.provider_account_id = operation.provider_account_id
                  AND reservation.provider_account_generation = operation.provider_account_generation
                  AND reservation.expected_writer_epoch = operation.expected_writer_epoch
                  AND reservation.expected_instance_generation = operation.expected_instance_generation
                  AND reservation.reservation_state IN ('pending', 'reconciling')
                "#,
            )
            .bind(claimed.operation.tenant_id)
            .bind(claimed.operation.operation_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            settle_workspace_lifecycle_after_confirmation(
                conn.as_mut(),
                claimed.operation.tenant_id,
                claimed.operation.operation_id,
                WorkspaceConfirmedDisposition::ResourceAbsent,
                true,
            )
            .await?;
        }
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Claims a disjoint bounded batch of operations ready for reconciliation.
    pub async fn claim_reconciliation(
        &self,
        limit: i64,
        claim_ttl: Duration,
    ) -> Result<Vec<ClaimedWorkspaceOperation>> {
        if limit <= 0 || claim_ttl.is_zero() {
            return Err(MoaError::ValidationError(
                "reconciliation limit and claim ttl must be positive".to_string(),
            ));
        }
        let mut conn = self.begin_maintenance().await?;
        let rows = sqlx::query(&format!(
            r#"
            WITH claimable AS (
                SELECT operation_id, tenant_id, workspace_id
                FROM moa.sandbox_workspace_operations
                WHERE outcome_class IN ('not_sent', 'unknown')
                  AND reconcile_not_before <= now()
                  AND (retry_not_before IS NULL OR retry_not_before <= now())
                  AND (claim_token IS NULL OR claim_expires_at <= now())
                ORDER BY reconcile_not_before, created_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE moa.sandbox_workspace_operations AS operation
            SET claim_token = gen_random_uuid(),
                claim_expires_at = now() + make_interval(secs => $2),
                updated_at = now()
            FROM claimable
            WHERE operation.operation_id = claimable.operation_id
              AND operation.tenant_id = claimable.tenant_id
              AND operation.workspace_id = claimable.workspace_id
            RETURNING {QUALIFIED_OPERATION_COLUMNS}
            "#,
        ))
        .bind(limit)
        .bind(claim_ttl.as_secs_f64())
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let claimed = rows
            .iter()
            .map(|row| {
                let operation = operation_from_row(row)?;
                let claim_token = operation.claim_token.ok_or_else(|| {
                    MoaError::StorageError("claimed workspace operation lacks token".to_string())
                })?;
                Ok(ClaimedWorkspaceOperation {
                    operation,
                    claim_token,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(claimed)
    }

    /// Renews one exact, unexpired reaper claim.
    pub async fn renew_claim(
        &self,
        claimed: &ClaimedWorkspaceOperation,
        claim_ttl: Duration,
    ) -> Result<bool> {
        if claim_ttl.is_zero() {
            return Err(MoaError::ValidationError(
                "workspace claim ttl must be positive".to_string(),
            ));
        }
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET claim_expires_at = now() + make_interval(secs => $4), updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2 AND claim_token = $3
              AND claim_expires_at > now() AND outcome_class IN ('not_sent', 'unknown')
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.operation_id)
        .bind(claimed.claim_token)
        .bind(claim_ttl.as_secs_f64())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Releases one failed claim behind a bounded retry delay.
    pub async fn release_claim(
        &self,
        claimed: &ClaimedWorkspaceOperation,
        retry_after: Duration,
        provider_error_code: &str,
    ) -> Result<bool> {
        if retry_after.is_zero() || provider_error_code.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "positive retry delay and safe provider error code are required".to_string(),
            ));
        }
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET attempts = attempts + 1,
                retry_not_before = now() + make_interval(secs => $4),
                provider_error_code = $5,
                claim_token = NULL, claim_expires_at = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2 AND claim_token = $3
              AND outcome_class IN ('not_sent', 'unknown')
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.operation_id)
        .bind(claimed.claim_token)
        .bind(retry_after.as_secs_f64())
        .bind(provider_error_code)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Releases a first-empty claim until an independently separated observation is due.
    pub async fn release_after_first_empty(
        &self,
        claimed: &ClaimedWorkspaceOperation,
    ) -> Result<bool> {
        let mut conn = self.begin_maintenance().await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET retry_not_before = absence_first_observed_at + interval '1 second',
                claim_token = NULL, claim_expires_at = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2 AND claim_token = $3
              AND outcome_class IN ('not_sent', 'unknown')
              AND absence_observation_count = 1
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.operation_id)
        .bind(claimed.claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }
}

async fn settle_workspace_lifecycle_after_confirmation(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    operation_id: WorkspaceOperationId,
    disposition: WorkspaceConfirmedDisposition,
    reconciled: bool,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.sandbox_workspaces AS workspace
        SET lifecycle_state = CASE
                WHEN operation.operation_kind = 'create' AND $3 = 'resource_present'
                    THEN CASE WHEN $4 THEN 'ready' ELSE 'creating' END
                WHEN operation.operation_kind IN ('attach', 'restore')
                     AND $3 = 'resource_present'
                    THEN CASE WHEN $4 THEN 'ready' ELSE 'restoring' END
                WHEN operation.operation_kind IN ('create', 'attach', 'restore')
                     AND $3 = 'resource_absent' THEN 'failed'
                ELSE workspace.lifecycle_state
            END,
            updated_at = now()
        FROM moa.sandbox_workspace_operations AS operation
        WHERE operation.tenant_id = $1 AND operation.operation_id = $2
          AND operation.outcome_class = 'confirmed'
          AND operation.confirmed_disposition = $3
          AND workspace.tenant_id = operation.tenant_id
          AND workspace.workspace_id = operation.workspace_id
          AND workspace.provider_account_id = operation.provider_account_id
          AND workspace.provider_account_generation = operation.provider_account_generation
          AND workspace.writer_epoch = operation.expected_writer_epoch
          AND workspace.instance_generation = operation.expected_instance_generation
          AND workspace.lifecycle_state NOT IN ('deleting', 'deleted')
        "#,
    )
    .bind(tenant_id)
    .bind(operation_id)
    .bind(disposition.as_str())
    .bind(reconciled)
    .execute(conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

const OPERATION_COLUMNS: &str = "operation_id, tenant_id, workspace_id, provider_account_id, \
    provider_account_generation, operation_kind, request_hash, expected_writer_epoch, \
    expected_instance_generation, expected_checkpoint_generation, deadline_at, \
    reconcile_not_before, outcome_class, confirmed_disposition, absence_observation_count, \
    absence_first_observed_at, absence_last_observed_at, absence_inventory_digest, \
    claim_token, claim_expires_at, attempts";

const QUALIFIED_OPERATION_COLUMNS: &str = "operation.operation_id, operation.tenant_id, \
    operation.workspace_id, operation.provider_account_id, \
    operation.provider_account_generation, operation.operation_kind, operation.request_hash, \
    operation.expected_writer_epoch, operation.expected_instance_generation, \
    operation.expected_checkpoint_generation, operation.deadline_at, \
    operation.reconcile_not_before, operation.outcome_class, \
    operation.confirmed_disposition, operation.absence_observation_count, \
    operation.absence_first_observed_at, operation.absence_last_observed_at, \
    operation.absence_inventory_digest, operation.claim_token, operation.claim_expires_at, \
    operation.attempts";

fn validate_intent(intent: &WorkspaceOperationIntent) -> Result<()> {
    if intent.request_hash.trim().is_empty()
        || intent.provider_account_generation <= 0
        || intent.expected_writer_epoch < 0
        || intent.expected_instance_generation < 0
        || intent.expected_checkpoint_generation < 0
        || intent.reconcile_not_before < intent.deadline_at
    {
        return Err(MoaError::ValidationError(
            "workspace operation intent has invalid hash, generations, or deadlines".to_string(),
        ));
    }
    Ok(())
}

fn operation_matches_intent(
    operation: &WorkspaceOperation,
    intent: &WorkspaceOperationIntent,
) -> bool {
    operation.operation_id == intent.operation_id
        && operation.tenant_id == intent.tenant_id
        && operation.workspace_id == intent.workspace_id
        && operation.provider_account_id == intent.provider_account_id
        && operation.provider_account_generation == intent.provider_account_generation
        && operation.kind == intent.kind
        && operation.request_hash == intent.request_hash
        && operation.expected_writer_epoch == intent.expected_writer_epoch
        && operation.expected_instance_generation == intent.expected_instance_generation
        && operation.expected_checkpoint_generation == intent.expected_checkpoint_generation
        && operation.deadline_at.timestamp_micros() == intent.deadline_at.timestamp_micros()
        && operation.reconcile_not_before.timestamp_micros()
            == intent.reconcile_not_before.timestamp_micros()
}

fn operation_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkspaceOperation> {
    let disposition = row
        .try_get::<Option<String>, _>("confirmed_disposition")
        .map_err(map_sqlx_error)?
        .map(|label| WorkspaceConfirmedDisposition::from_label(&label))
        .transpose()?;
    Ok(WorkspaceOperation {
        operation_id: row.try_get("operation_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        workspace_id: row.try_get("workspace_id").map_err(map_sqlx_error)?,
        provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx_error)?,
        provider_account_generation: row
            .try_get("provider_account_generation")
            .map_err(map_sqlx_error)?,
        kind: WorkspaceOperationKind::from_label(
            &row.try_get::<String, _>("operation_kind")
                .map_err(map_sqlx_error)?,
        )?,
        request_hash: row.try_get("request_hash").map_err(map_sqlx_error)?,
        expected_writer_epoch: row
            .try_get("expected_writer_epoch")
            .map_err(map_sqlx_error)?,
        expected_instance_generation: row
            .try_get("expected_instance_generation")
            .map_err(map_sqlx_error)?,
        expected_checkpoint_generation: row
            .try_get("expected_checkpoint_generation")
            .map_err(map_sqlx_error)?,
        deadline_at: row.try_get("deadline_at").map_err(map_sqlx_error)?,
        reconcile_not_before: row
            .try_get("reconcile_not_before")
            .map_err(map_sqlx_error)?,
        outcome: WorkspaceOperationOutcome::from_label(
            &row.try_get::<String, _>("outcome_class")
                .map_err(map_sqlx_error)?,
        )?,
        confirmed_disposition: disposition,
        absence_observation_count: row
            .try_get("absence_observation_count")
            .map_err(map_sqlx_error)?,
        absence_first_observed_at: row
            .try_get("absence_first_observed_at")
            .map_err(map_sqlx_error)?,
        absence_last_observed_at: row
            .try_get("absence_last_observed_at")
            .map_err(map_sqlx_error)?,
        absence_inventory_digest: row
            .try_get("absence_inventory_digest")
            .map_err(map_sqlx_error)?,
        claim_token: row.try_get("claim_token").map_err(map_sqlx_error)?,
        claim_expires_at: row.try_get("claim_expires_at").map_err(map_sqlx_error)?,
        attempts: row.try_get("attempts").map_err(map_sqlx_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::{OPERATION_COLUMNS, QUALIFIED_OPERATION_COLUMNS};

    #[test]
    fn reconciliation_returning_columns_are_all_target_table_qualified_offline() {
        // Pins: UPDATE ... FROM exposes the CTE and target table in one scope;
        // every RETURNING column must name the target alias or Postgres rejects
        // shared names such as operation_id as ambiguous.
        let expected = OPERATION_COLUMNS
            .split(',')
            .map(|column| format!("operation.{}", column.trim()))
            .collect::<Vec<_>>();
        let actual = QUALIFIED_OPERATION_COLUMNS
            .split(',')
            .map(str::trim)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }
}
