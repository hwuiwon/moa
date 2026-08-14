//! Fenced sandbox-workspace lifecycle transitions and deletion.

use super::*;
use crate::core::sandbox_workspace::capacity::release_workspace_in_transaction;

impl PostgresWorkspaceRepository {
    /// Persists the exact task-attempt release intent before checkpoint or destroy I/O.
    ///
    /// While this row is pending, the migration guard prevents the task attempt
    /// generation from advancing. That closes the validation-to-provider-I/O race
    /// without holding a database transaction open across an external call.
    pub async fn begin_task_execution_hand_release(
        &self,
        intent: TaskHandReleaseIntent<'_>,
    ) -> Result<(Uuid, Uuid, chrono::DateTime<chrono::Utc>)> {
        let TaskHandReleaseIntent {
            receipt_id,
            contact_id,
            run_id,
            task_id,
            logical_generation,
            attempt_generation,
            deadline_at,
            recovery_claim_expires_at,
            workspace,
            lease,
        } = intent;
        let active_identity = workspace.state == SandboxWorkspaceState::Active
            && lease.status == HandLeaseStatus::Active
            && lease.handle.is_some()
            && lease.attachment.as_ref().map(|attachment| {
                (
                    attachment.workspace_id,
                    attachment.workspace_writer_epoch,
                    attachment.workspace_instance_generation,
                )
            }) == Some((
                workspace.workspace_id,
                workspace.writer_epoch,
                workspace.instance_generation,
            ));
        let released_identity = workspace.state == SandboxWorkspaceState::Ready
            && lease.status == HandLeaseStatus::Destroyed
            && lease.handle.is_none();
        if workspace.scope != (SandboxWorkspaceScope::ExecutionTask { run_id, task_id })
            || !(active_identity || released_identity)
        {
            return Err(MoaError::ValidationError(
                "task hand release intent does not match the active workspace and lease"
                    .to_string(),
            ));
        }
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let claim_token = Uuid::now_v7();
        let mut conn = self
            .begin_with_contact(workspace.tenant_id, contact_id)
            .await?;
        let row = sqlx::query(
            r#"
            INSERT INTO moa.sandbox_execution_hand_release_receipts (
                receipt_id, tenant_id, run_uid, owner_kind, task_id,
                logical_generation, attempt_generation,
                workspace_id, writer_epoch, instance_generation,
                hand_provisioning_operation_id, hand_lease_generation,
                receipt_state, claim_token, claim_expires_at,
                requested_at, deadline_at
            )
            SELECT $1, $2, $3, 'task', $4, $5, $6, $7, $8, $9, $10, $11,
                   'pending', $13, $14, now(), $12
            FROM moa.execution_task AS task
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = task.tenant_id AND workspace.workspace_id = $7
            JOIN moa.hand_leases AS lease
              ON lease.tenant_id = task.tenant_id
             AND lease.provisioning_operation_id = $10 AND lease.generation = $11
            JOIN moa.sandbox_capacity_reservations AS capacity
              ON capacity.tenant_id = task.tenant_id
             AND capacity.workspace_id = workspace.workspace_id
             AND capacity.hand_provisioning_operation_id = lease.provisioning_operation_id
             AND capacity.hand_lease_generation = lease.generation
             AND capacity.resource_dimension = 'active_hands'
            WHERE task.tenant_id = $2 AND task.run_uid = $3 AND task.task_id = $4
              AND task.generation = $5 AND task.attempt_generation = $6
              AND task.attempt_state = 'cancelling'
              AND workspace.writer_epoch = $8 AND workspace.instance_generation = $9
              AND workspace.access_fenced_at IS NULL
              AND capacity.expected_writer_epoch = $8
              AND capacity.expected_instance_generation = $9
              AND (
                    (
                        workspace.lifecycle_state = 'active'
                        AND lease.status = 'active' AND lease.handle IS NOT NULL
                        AND lease.workspace_id = workspace.workspace_id
                        AND lease.workspace_writer_epoch = workspace.writer_epoch
                        AND lease.workspace_instance_generation = workspace.instance_generation
                        AND capacity.reservation_state = 'committed'
                    )
                    OR
                    (
                        workspace.lifecycle_state = 'ready'
                        AND lease.status = 'destroyed' AND lease.handle IS NULL
                        AND capacity.reservation_state = 'released'
                        AND EXISTS (
                            SELECT 1
                            FROM moa.sandbox_execution_hand_release_receipts AS pending
                            WHERE pending.tenant_id = $2 AND pending.run_uid = $3
                              AND pending.task_id = $4 AND pending.logical_generation = $5
                              AND pending.attempt_generation = $6
                              AND pending.receipt_id = $1 AND pending.workspace_id = $7
                              AND pending.writer_epoch = $8 AND pending.instance_generation = $9
                              AND pending.hand_provisioning_operation_id = $10
                              AND pending.hand_lease_generation = $11
                              AND pending.receipt_state = 'pending'
                        )
                    )
              )
            ON CONFLICT (tenant_id, run_uid, task_id, logical_generation, attempt_generation)
                WHERE owner_kind = 'task'
            DO UPDATE SET claim_token = $13, claim_expires_at = $14,
                          updated_at = now()
            WHERE sandbox_execution_hand_release_receipts.receipt_state = 'pending'
              AND sandbox_execution_hand_release_receipts.claim_expires_at <= now()
              AND sandbox_execution_hand_release_receipts.receipt_id = $1
              AND sandbox_execution_hand_release_receipts.logical_generation = $5
              AND sandbox_execution_hand_release_receipts.attempt_generation = $6
              AND sandbox_execution_hand_release_receipts.workspace_id = $7
              AND sandbox_execution_hand_release_receipts.writer_epoch = $8
              AND sandbox_execution_hand_release_receipts.instance_generation = $9
              AND sandbox_execution_hand_release_receipts.hand_provisioning_operation_id = $10
              AND sandbox_execution_hand_release_receipts.hand_lease_generation = $11
            RETURNING receipt_id, claim_token, requested_at
            "#,
        )
        .bind(receipt_id)
        .bind(workspace.tenant_id)
        .bind(run_id)
        .bind(task_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(workspace.workspace_id)
        .bind(workspace.writer_epoch)
        .bind(workspace.instance_generation)
        .bind(lease.provisioning_operation_id)
        .bind(lease.generation)
        .bind(deadline_at)
        .bind(claim_token)
        .bind(recovery_claim_expires_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let row = row.ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
            operation_id: format!(
                "execution-task-hand-release:{run_id}:{task_id}:{attempt_generation}"
            ),
        })?;
        let persisted_id = row.try_get("receipt_id").map_err(map_sqlx_error)?;
        let persisted_claim = row.try_get("claim_token").map_err(map_sqlx_error)?;
        let requested_at = row.try_get("requested_at").map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok((persisted_id, persisted_claim, requested_at))
    }

    /// Loads the verified release receipt for one exact execution-task attempt.
    pub async fn get_task_execution_hand_release_receipt(
        &self,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        run_id: ExecutionRunScopeId,
        task_id: ExecutionTaskScopeId,
        logical_generation: u64,
        attempt_generation: u64,
    ) -> Result<Option<ExecutionHandReleaseReceipt>> {
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let mut conn = self.begin_with_contact(tenant_id, contact_id).await?;
        let row = sqlx::query(
            r#"
            SELECT receipt_id, tenant_id, run_uid, owner_kind, task_id, compensation_id,
                   logical_generation, attempt_generation,
                   workspace_id, writer_epoch, instance_generation,
                   hand_provisioning_operation_id, hand_lease_generation,
                   checkpoint_id, checkpoint_generation,
                   checkpoint_manifest_digest, checkpoint_logical_bytes,
                   requested_at, released_at
            FROM moa.sandbox_execution_hand_release_receipts
            WHERE tenant_id = $1 AND run_uid = $2 AND task_id = $3
              AND owner_kind = 'task' AND logical_generation = $4
              AND attempt_generation = $5
              AND receipt_state = 'released'
              AND destroy_outcome = 'verified_absent'
            "#,
        )
        .bind(tenant_id)
        .bind(run_id)
        .bind(task_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let receipt = row
            .as_ref()
            .map(execution_hand_release_receipt_from_row)
            .transpose()?;
        conn.commit().await?;
        Ok(receipt)
    }

    /// Persists a verified-absence receipt for a cancelling task with no live hand.
    ///
    /// A ready workspace left by an earlier attempt is permitted only when its
    /// active-hand capacity is already released. Any live lease, active workspace,
    /// or unreleased active-hand reservation rejects the absence proof.
    pub async fn record_absent_task_execution_hand_release_receipt(
        &self,
        intent: AbsentTaskHandReleaseIntent,
    ) -> Result<ExecutionHandReleaseReceipt> {
        let logical_generation = i64::try_from(intent.logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(intent.attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let mut conn = self
            .begin_with_contact(intent.tenant_id, intent.contact_id)
            .await?;
        sqlx::query(
            r#"
            WITH locked_task AS MATERIALIZED (
                SELECT task.tenant_id, task.run_uid, task.task_id,
                       task.generation, task.attempt_generation, run.session_id
                FROM moa.execution_task AS task
                JOIN moa.execution_run AS run
                  ON run.tenant_id = task.tenant_id AND run.run_uid = task.run_uid
                WHERE task.tenant_id = $2 AND task.run_uid = $3 AND task.task_id = $4
                  AND task.generation = $5 AND task.attempt_generation = $6
                  AND task.attempt_state = 'cancelling'
                FOR UPDATE OF task
            )
            INSERT INTO moa.sandbox_execution_hand_release_receipts (
                receipt_id, tenant_id, run_uid, owner_kind, task_id,
                logical_generation, attempt_generation, receipt_state,
                destroy_outcome, requested_at, deadline_at, released_at
            )
            SELECT $1, task.tenant_id, task.run_uid, 'task', task.task_id,
                   task.generation, task.attempt_generation, 'released',
                   'verified_absent', $7, $7, $7
            FROM locked_task AS task
            WHERE NOT EXISTS (
                    SELECT 1
                    FROM moa.hand_leases AS lease
                    WHERE lease.tenant_id = task.tenant_id
                      AND lease.session_id = task.session_id
                      AND lease.worker_id =
                          'execution:' || task.run_uid::text || ':' || task.task_id::text
                      AND lease.status <> 'destroyed'
              )
              AND NOT EXISTS (
                    SELECT 1
                    FROM moa.sandbox_workspaces AS workspace
                    WHERE workspace.tenant_id = task.tenant_id
                      AND workspace.scope_kind = 'execution_task'
                      AND workspace.scope_run_id = task.run_uid
                      AND workspace.scope_task_id = task.task_id
                      AND workspace.lifecycle_state <> 'deleted'
                      AND (
                            workspace.lifecycle_state <> 'ready'
                            OR EXISTS (
                                SELECT 1
                                FROM moa.sandbox_capacity_reservations AS capacity
                                WHERE capacity.tenant_id = workspace.tenant_id
                                  AND capacity.workspace_id = workspace.workspace_id
                                  AND capacity.resource_dimension = 'active_hands'
                                  AND capacity.reservation_state <> 'released'
                            )
                      )
              )
            ON CONFLICT (tenant_id, run_uid, task_id, logical_generation, attempt_generation)
                WHERE owner_kind = 'task'
            DO NOTHING
            "#,
        )
        .bind(intent.receipt_id)
        .bind(intent.tenant_id)
        .bind(intent.run_id)
        .bind(intent.task_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(intent.verified_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let row = sqlx::query(
            r#"
            SELECT receipt_id, tenant_id, run_uid, owner_kind, task_id, compensation_id,
                   logical_generation, attempt_generation,
                   workspace_id, writer_epoch, instance_generation,
                   hand_provisioning_operation_id, hand_lease_generation,
                   checkpoint_id, checkpoint_generation,
                   checkpoint_manifest_digest, checkpoint_logical_bytes,
                   requested_at, released_at
            FROM moa.sandbox_execution_hand_release_receipts
            WHERE receipt_id = $1 AND tenant_id = $2 AND run_uid = $3
              AND owner_kind = 'task' AND task_id = $4
              AND logical_generation = $5 AND attempt_generation = $6
              AND receipt_state = 'released' AND destroy_outcome = 'verified_absent'
              AND workspace_id IS NULL AND hand_provisioning_operation_id IS NULL
            "#,
        )
        .bind(intent.receipt_id)
        .bind(intent.tenant_id)
        .bind(intent.run_id)
        .bind(intent.task_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
            operation_id: format!(
                "execution-task-hand-absence:{}:{}:{}",
                intent.run_id, intent.task_id, intent.attempt_generation
            ),
        })?;
        let receipt = execution_hand_release_receipt_from_row(&row)?;
        conn.commit().await?;
        Ok(receipt)
    }

    /// Persists a compensation release intent after proving the exact attempt is cancelling.
    pub async fn begin_compensation_execution_hand_release(
        &self,
        intent: CompensationHandReleaseIntent<'_>,
    ) -> Result<(Uuid, Uuid, chrono::DateTime<chrono::Utc>)> {
        let CompensationHandReleaseIntent {
            receipt_id,
            tenant_id,
            contact_id,
            session_id,
            run_id,
            compensation_id,
            logical_generation,
            attempt_generation,
            hand_scope,
            lease,
            deadline_at,
            recovery_claim_expires_at,
        } = intent;
        if lease.is_some_and(|lease| {
            lease.tenant_id != tenant_id
                || lease.session_id != session_id
                || lease.worker_id != hand_scope
                || lease.status == HandLeaseStatus::Destroyed
        }) {
            return Err(MoaError::ValidationError(
                "compensation hand release lease does not match its exact scope".to_string(),
            ));
        }
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let claim_token = Uuid::now_v7();
        let mut conn = self.begin_with_contact(tenant_id, contact_id).await?;
        let row = sqlx::query(
            r#"
            INSERT INTO moa.sandbox_execution_hand_release_receipts (
                receipt_id, tenant_id, run_uid, owner_kind, compensation_id,
                logical_generation, attempt_generation,
                hand_provisioning_operation_id, hand_lease_generation, receipt_state,
                claim_token, claim_expires_at, requested_at, deadline_at
            )
            SELECT $1, $2, $3, 'compensation', $4, $5, $6, $11, $12, 'pending',
                   $10, $13, now(), $9
            FROM moa.execution_compensation AS compensation
            WHERE compensation.tenant_id = $2 AND compensation.run_uid = $3
              AND compensation.compensation_id = $4 AND compensation.generation = $5
              AND compensation.attempt_generation = $6
              AND compensation.attempt_state = 'cancelling'
              AND (($11::uuid IS NULL AND $12::bigint IS NULL AND NOT EXISTS (
                      SELECT 1 FROM moa.hand_leases AS lease
                      WHERE lease.tenant_id = $2 AND lease.session_id = $7
                        AND lease.worker_id = $8 AND lease.status <> 'destroyed'
                   )) OR ($11::uuid IS NOT NULL AND $12::bigint IS NOT NULL AND EXISTS (
                      SELECT 1 FROM moa.hand_leases AS lease
                      WHERE lease.tenant_id = $2 AND lease.session_id = $7
                        AND lease.worker_id = $8
                        AND lease.provisioning_operation_id = $11
                        AND lease.generation = $12 AND lease.status <> 'destroyed'
                   )))
            ON CONFLICT (
                tenant_id, run_uid, compensation_id, logical_generation, attempt_generation
            ) WHERE owner_kind = 'compensation'
            DO UPDATE SET claim_token = $10, claim_expires_at = $13,
                          updated_at = now()
            WHERE sandbox_execution_hand_release_receipts.receipt_state = 'pending'
              AND sandbox_execution_hand_release_receipts.claim_expires_at <= now()
              AND sandbox_execution_hand_release_receipts.receipt_id = $1
              AND sandbox_execution_hand_release_receipts.hand_provisioning_operation_id
                    IS NOT DISTINCT FROM $11
              AND sandbox_execution_hand_release_receipts.hand_lease_generation
                    IS NOT DISTINCT FROM $12
            RETURNING receipt_id, claim_token, requested_at
            "#,
        )
        .bind(receipt_id)
        .bind(tenant_id)
        .bind(run_id)
        .bind(compensation_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(session_id)
        .bind(hand_scope)
        .bind(deadline_at)
        .bind(claim_token)
        .bind(lease.map(|lease| lease.provisioning_operation_id.0))
        .bind(lease.map(|lease| lease.generation))
        .bind(recovery_claim_expires_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
            operation_id: format!(
                "execution-compensation-hand-release:{run_id}:{compensation_id}:{logical_generation}:{attempt_generation}"
            ),
        })?;
        let persisted_id = row.try_get("receipt_id").map_err(map_sqlx_error)?;
        let persisted_claim = row.try_get("claim_token").map_err(map_sqlx_error)?;
        let requested_at = row.try_get("requested_at").map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok((persisted_id, persisted_claim, requested_at))
    }

    /// Loads verified absence proof for one exact compensation attempt.
    pub async fn get_compensation_execution_hand_release_receipt(
        &self,
        tenant_id: TenantId,
        run_id: ExecutionRunScopeId,
        compensation_id: ExecutionCompensationScopeId,
        logical_generation: u64,
        attempt_generation: u64,
    ) -> Result<Option<ExecutionHandReleaseReceipt>> {
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(
            r#"
            SELECT receipt_id, tenant_id, run_uid, owner_kind, task_id, compensation_id,
                   logical_generation, attempt_generation, workspace_id, writer_epoch,
                   instance_generation, hand_provisioning_operation_id,
                   hand_lease_generation, checkpoint_id, checkpoint_generation,
                   checkpoint_manifest_digest, checkpoint_logical_bytes,
                   requested_at, released_at
            FROM moa.sandbox_execution_hand_release_receipts
            WHERE tenant_id = $1 AND run_uid = $2 AND owner_kind = 'compensation'
              AND compensation_id = $3 AND logical_generation = $4
              AND attempt_generation = $5 AND receipt_state = 'released'
              AND destroy_outcome = 'verified_absent'
            "#,
        )
        .bind(tenant_id)
        .bind(run_id)
        .bind(compensation_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let receipt = row
            .as_ref()
            .map(execution_hand_release_receipt_from_row)
            .transpose()?;
        conn.commit().await?;
        Ok(receipt)
    }

    /// Reclaims one expired pending compensation release without rediscovering its lease.
    ///
    /// The immutable provider deadline continues to prohibit new provider I/O.
    /// This short database claim exists only to verify already-absent compute and
    /// finalize the persisted receipt using its original provisioning identity.
    pub async fn claim_pending_compensation_execution_hand_release(
        &self,
        intent: CompensationHandReleaseClaimIntent,
    ) -> Result<Option<CompensationHandReleaseClaim>> {
        let CompensationHandReleaseClaimIntent {
            tenant_id,
            contact_id,
            run_id,
            compensation_id,
            logical_generation,
            attempt_generation,
            recovery_claim_expires_at,
        } = intent;
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let claim_token = Uuid::now_v7();
        let mut conn = self.begin_with_contact(tenant_id, contact_id).await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.sandbox_execution_hand_release_receipts AS receipt
            SET claim_token = $6, claim_expires_at = $7, updated_at = now()
            FROM moa.execution_compensation AS compensation
            WHERE receipt.tenant_id = $1 AND receipt.run_uid = $2
              AND receipt.owner_kind = 'compensation'
              AND receipt.compensation_id = $3
              AND receipt.logical_generation = $4
              AND receipt.attempt_generation = $5
              AND receipt.receipt_state = 'pending'
              AND receipt.claim_expires_at <= now() AND $7 > now()
              AND compensation.tenant_id = $1 AND compensation.run_uid = $2
              AND compensation.compensation_id = $3 AND compensation.generation = $4
              AND compensation.attempt_generation = $5
              AND compensation.attempt_state = 'cancelling'
            RETURNING receipt.receipt_id, receipt.claim_token, receipt.requested_at,
                      receipt.hand_provisioning_operation_id,
                      receipt.hand_lease_generation
            "#,
        )
        .bind(tenant_id)
        .bind(run_id)
        .bind(compensation_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(claim_token)
        .bind(recovery_claim_expires_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let claim = row
            .as_ref()
            .map(|row| {
                let operation_id: Option<Uuid> = row
                    .try_get("hand_provisioning_operation_id")
                    .map_err(map_sqlx_error)?;
                let generation: Option<i64> = row
                    .try_get("hand_lease_generation")
                    .map_err(map_sqlx_error)?;
                if operation_id.is_some() != generation.is_some() {
                    return Err(MoaError::StorageError(
                        "pending compensation release has a partial hand identity".to_string(),
                    ));
                }
                Ok(CompensationHandReleaseClaim {
                    receipt_id: row.try_get("receipt_id").map_err(map_sqlx_error)?,
                    claim_token: row.try_get("claim_token").map_err(map_sqlx_error)?,
                    requested_at: row.try_get("requested_at").map_err(map_sqlx_error)?,
                    hand_provisioning_operation_id: operation_id.map(HandProvisioningOperationId),
                    hand_lease_generation: generation,
                })
            })
            .transpose()?;
        conn.commit().await?;
        Ok(claim)
    }

    /// Finalizes compensation release only while exact cancelling ownership persists.
    pub async fn record_compensation_execution_hand_release_receipt(
        &self,
        receipt: &ExecutionHandReleaseReceipt,
        session_id: SessionId,
        hand_scope: &str,
        claim_token: Uuid,
        contact_id: Option<ContactId>,
    ) -> Result<ExecutionHandReleaseReceipt> {
        let (compensation_id, logical_generation) = match receipt.owner {
            ExecutionHandReleaseOwner::Compensation {
                compensation_id,
                logical_generation,
            } => (compensation_id, logical_generation),
            ExecutionHandReleaseOwner::Task { .. } => {
                return Err(MoaError::ValidationError(
                    "compensation hand release receipt has a task owner".to_string(),
                ));
            }
        };
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(receipt.attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "compensation attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let hand_operation_id = receipt
            .hand_provisioning_operation_id
            .map(|operation_id| operation_id.0);
        let hand_generation = receipt
            .hand_lease_generation
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                MoaError::ValidationError(
                    "compensation hand generation overflows Postgres bigint".to_string(),
                )
            })?;
        if hand_operation_id.is_some() != hand_generation.is_some() {
            return Err(MoaError::ValidationError(
                "compensation hand release identity must be wholly present or absent".to_string(),
            ));
        }
        let mut conn = self
            .begin_with_contact(receipt.tenant_id, contact_id)
            .await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.sandbox_execution_hand_release_receipts AS receipt
            SET receipt_state = 'released', destroy_outcome = 'verified_absent',
                claim_token = NULL, claim_expires_at = NULL,
                released_at = $9, updated_at = now()
            FROM moa.execution_compensation AS compensation
            WHERE receipt.receipt_id = $1 AND receipt.tenant_id = $2
              AND receipt.run_uid = $3 AND receipt.owner_kind = 'compensation'
              AND receipt.compensation_id = $4 AND receipt.logical_generation = $5
              AND receipt.attempt_generation = $6 AND receipt.receipt_state = 'pending'
              AND receipt.claim_token = $10 AND receipt.claim_expires_at > now()
              AND receipt.requested_at = $8
              AND compensation.tenant_id = $2 AND compensation.run_uid = $3
              AND compensation.compensation_id = $4 AND compensation.generation = $5
              AND compensation.attempt_generation = $6
              AND compensation.attempt_state = 'cancelling'
              AND NOT EXISTS (
                  SELECT 1 FROM moa.hand_leases AS lease
                  WHERE lease.tenant_id = $2 AND lease.session_id = $7
                    AND lease.worker_id = $11 AND lease.status <> 'destroyed'
              )
              AND (($12::uuid IS NULL AND $13::bigint IS NULL
                    AND receipt.hand_provisioning_operation_id IS NULL
                    AND receipt.hand_lease_generation IS NULL)
                OR ($12::uuid IS NOT NULL AND $13::bigint IS NOT NULL
                    AND receipt.hand_provisioning_operation_id = $12
                    AND receipt.hand_lease_generation = $13
                    AND EXISTS (
                        SELECT 1 FROM moa.hand_leases AS released_lease
                        WHERE released_lease.tenant_id = $2
                          AND released_lease.session_id = $7
                          AND released_lease.worker_id = $11
                          AND released_lease.provisioning_operation_id = $12
                          AND released_lease.generation = $13
                          AND released_lease.status = 'destroyed'
                          AND released_lease.handle IS NULL
                    )))
            RETURNING receipt.receipt_id, receipt.tenant_id, receipt.run_uid,
                      receipt.owner_kind, receipt.task_id, receipt.compensation_id,
                      receipt.logical_generation, receipt.attempt_generation,
                      receipt.workspace_id, receipt.writer_epoch, receipt.instance_generation,
                      receipt.hand_provisioning_operation_id, receipt.hand_lease_generation,
                      receipt.checkpoint_id, receipt.checkpoint_generation,
                      receipt.checkpoint_manifest_digest, receipt.checkpoint_logical_bytes,
                      receipt.requested_at, receipt.released_at
            "#,
        )
        .bind(receipt.receipt_id)
        .bind(receipt.tenant_id)
        .bind(receipt.run_id)
        .bind(compensation_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(session_id)
        .bind(receipt.requested_at)
        .bind(receipt.released_at)
        .bind(claim_token)
        .bind(hand_scope)
        .bind(hand_operation_id)
        .bind(hand_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
            operation_id: format!(
                "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{attempt_generation}",
                receipt.run_id
            ),
        })?;
        let persisted = execution_hand_release_receipt_from_row(&row)?;
        conn.commit().await?;
        Ok(persisted)
    }

    /// Persists a release receipt only after every exact durable fence proves release.
    ///
    /// A racing retry receives the original receipt. A conflicting identity for the
    /// same task attempt is rejected rather than silently replacing recovery truth.
    pub async fn record_task_execution_hand_release_receipt(
        &self,
        receipt: &ExecutionHandReleaseReceipt,
        claim_token: Uuid,
        contact_id: Option<ContactId>,
    ) -> Result<ExecutionHandReleaseReceipt> {
        let (task_id, logical_generation) = match receipt.owner {
            ExecutionHandReleaseOwner::Task {
                task_id,
                logical_generation,
            } => (task_id, logical_generation),
            ExecutionHandReleaseOwner::Compensation { .. } => {
                return Err(MoaError::ValidationError(
                    "task hand release receipt has a compensation owner".to_string(),
                ));
            }
        };
        let workspace_id = receipt.workspace_id.ok_or_else(|| {
            MoaError::ValidationError("task hand release receipt has no workspace".to_string())
        })?;
        let writer_epoch = receipt.writer_epoch.ok_or_else(|| {
            MoaError::ValidationError("task hand release receipt has no writer epoch".to_string())
        })?;
        let instance_generation = receipt.instance_generation.ok_or_else(|| {
            MoaError::ValidationError(
                "task hand release receipt has no instance generation".to_string(),
            )
        })?;
        let hand_provisioning_operation_id =
            receipt.hand_provisioning_operation_id.ok_or_else(|| {
                MoaError::ValidationError(
                    "task hand release receipt has no provisioning identity".to_string(),
                )
            })?;
        let hand_lease_generation = receipt.hand_lease_generation.ok_or_else(|| {
            MoaError::ValidationError(
                "task hand release receipt has no hand lease generation".to_string(),
            )
        })?;
        let checkpoint_id = receipt.checkpoint_id.ok_or_else(|| {
            MoaError::ValidationError("task hand release receipt has no checkpoint".to_string())
        })?;
        let checkpoint_generation = receipt.checkpoint_generation.ok_or_else(|| {
            MoaError::ValidationError(
                "task hand release receipt has no checkpoint generation".to_string(),
            )
        })?;
        let checkpoint_manifest_digest =
            receipt
                .checkpoint_manifest_digest
                .as_deref()
                .ok_or_else(|| {
                    MoaError::ValidationError(
                        "task hand release receipt has no checkpoint digest".to_string(),
                    )
                })?;
        let checkpoint_logical_bytes = receipt.checkpoint_logical_bytes.ok_or_else(|| {
            MoaError::ValidationError(
                "task hand release receipt has no checkpoint byte count".to_string(),
            )
        })?;
        let logical_generation = i64::try_from(logical_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task logical generation overflows Postgres bigint".to_string(),
            )
        })?;
        let attempt_generation = i64::try_from(receipt.attempt_generation).map_err(|_| {
            MoaError::ValidationError(
                "execution task attempt generation overflows Postgres bigint".to_string(),
            )
        })?;
        let writer_epoch = i64::try_from(writer_epoch).map_err(|_| {
            MoaError::ValidationError(
                "workspace writer epoch overflows Postgres bigint".to_string(),
            )
        })?;
        let instance_generation = i64::try_from(instance_generation).map_err(|_| {
            MoaError::ValidationError(
                "workspace instance generation overflows Postgres bigint".to_string(),
            )
        })?;
        let hand_lease_generation = i64::try_from(hand_lease_generation).map_err(|_| {
            MoaError::ValidationError("hand lease generation overflows Postgres bigint".to_string())
        })?;
        let checkpoint_generation = i64::try_from(checkpoint_generation).map_err(|_| {
            MoaError::ValidationError("checkpoint generation overflows Postgres bigint".to_string())
        })?;
        let checkpoint_logical_bytes = i64::try_from(checkpoint_logical_bytes).map_err(|_| {
            MoaError::ValidationError(
                "checkpoint logical bytes overflow Postgres bigint".to_string(),
            )
        })?;
        let mut conn = self
            .begin_with_contact(receipt.tenant_id, contact_id)
            .await?;
        let row = sqlx::query(
            r#"
            UPDATE moa.sandbox_execution_hand_release_receipts AS receipt
            SET checkpoint_id = $12, checkpoint_generation = $13,
                checkpoint_manifest_digest = $14, checkpoint_logical_bytes = $15,
                receipt_state = 'released', destroy_outcome = 'verified_absent',
                claim_token = NULL, claim_expires_at = NULL,
                released_at = $17, updated_at = now()
            FROM moa.execution_task AS task
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = task.tenant_id
             AND workspace.workspace_id = $7
            JOIN moa.hand_leases AS lease
              ON lease.tenant_id = task.tenant_id
             AND lease.provisioning_operation_id = $10
             AND lease.generation = $11
            JOIN moa.sandbox_workspace_checkpoints AS checkpoint
              ON checkpoint.tenant_id = task.tenant_id
             AND checkpoint.workspace_id = workspace.workspace_id
             AND checkpoint.checkpoint_id = $12
            JOIN moa.sandbox_capacity_reservations AS capacity
              ON capacity.tenant_id = task.tenant_id
             AND capacity.workspace_id = workspace.workspace_id
             AND capacity.hand_provisioning_operation_id = lease.provisioning_operation_id
             AND capacity.hand_lease_generation = lease.generation
             AND capacity.expected_writer_epoch = $8
             AND capacity.expected_instance_generation = $9
             AND capacity.resource_dimension = 'active_hands'
            WHERE receipt.receipt_id = $1 AND receipt.tenant_id = $2
              AND receipt.run_uid = $3 AND receipt.owner_kind = 'task'
              AND receipt.task_id = $4 AND receipt.logical_generation = $5
              AND receipt.attempt_generation = $6
              AND receipt.workspace_id = $7
              AND receipt.writer_epoch = $8 AND receipt.instance_generation = $9
              AND receipt.hand_provisioning_operation_id = $10
              AND receipt.hand_lease_generation = $11
              AND receipt.receipt_state = 'pending'
              AND receipt.claim_token = $18 AND receipt.claim_expires_at > now()
              AND receipt.requested_at = $16
              AND task.tenant_id = $2 AND task.run_uid = $3 AND task.task_id = $4
              AND task.generation = $5 AND task.attempt_generation = $6
              AND task.attempt_state = 'cancelling'
              AND workspace.writer_epoch = $8 AND workspace.instance_generation = $9
              AND workspace.lifecycle_state = 'ready'
              AND workspace.current_checkpoint_id = $12
              AND workspace.current_checkpoint_generation = $13
              AND lease.status = 'destroyed' AND lease.handle IS NULL
              AND lease.workspace_id IS NULL
              AND checkpoint.lifecycle_state = 'available'
              AND checkpoint.generation = $13
              AND checkpoint.manifest_digest = $14
              AND checkpoint.logical_bytes = $15
              AND capacity.reservation_state = 'released'
            RETURNING receipt.receipt_id, receipt.tenant_id, receipt.run_uid,
                      receipt.owner_kind, receipt.task_id, receipt.compensation_id,
                      receipt.logical_generation, receipt.attempt_generation, receipt.workspace_id,
                      receipt.writer_epoch, receipt.instance_generation,
                      receipt.hand_provisioning_operation_id, receipt.hand_lease_generation,
                      receipt.checkpoint_id, receipt.checkpoint_generation,
                      receipt.checkpoint_manifest_digest, receipt.checkpoint_logical_bytes,
                      receipt.requested_at, receipt.released_at
            "#,
        )
        .bind(receipt.receipt_id)
        .bind(receipt.tenant_id)
        .bind(receipt.run_id)
        .bind(task_id)
        .bind(logical_generation)
        .bind(attempt_generation)
        .bind(workspace_id)
        .bind(writer_epoch)
        .bind(instance_generation)
        .bind(hand_provisioning_operation_id)
        .bind(hand_lease_generation)
        .bind(checkpoint_id)
        .bind(checkpoint_generation)
        .bind(checkpoint_manifest_digest)
        .bind(checkpoint_logical_bytes)
        .bind(receipt.requested_at)
        .bind(receipt.released_at)
        .bind(claim_token)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let row = if let Some(row) = row {
            row
        } else {
            sqlx::query(
                r#"
                SELECT receipt_id, tenant_id, run_uid, owner_kind, task_id, compensation_id,
                       logical_generation, attempt_generation,
                       workspace_id, writer_epoch, instance_generation,
                       hand_provisioning_operation_id, hand_lease_generation,
                       checkpoint_id, checkpoint_generation,
                       checkpoint_manifest_digest, checkpoint_logical_bytes,
                       requested_at, released_at
                FROM moa.sandbox_execution_hand_release_receipts
                WHERE receipt_id = $1 AND tenant_id = $2 AND run_uid = $3
                  AND owner_kind = 'task' AND task_id = $4 AND logical_generation = $5
                  AND attempt_generation = $6 AND workspace_id = $7
                  AND writer_epoch = $8 AND instance_generation = $9
                  AND hand_provisioning_operation_id = $10 AND hand_lease_generation = $11
                  AND checkpoint_id = $12 AND checkpoint_generation = $13
                  AND checkpoint_manifest_digest = $14 AND checkpoint_logical_bytes = $15
                  AND receipt_state = 'released' AND destroy_outcome = 'verified_absent'
                  AND requested_at = $16
                "#,
            )
            .bind(receipt.receipt_id)
            .bind(receipt.tenant_id)
            .bind(receipt.run_id)
            .bind(task_id)
            .bind(logical_generation)
            .bind(attempt_generation)
            .bind(workspace_id)
            .bind(writer_epoch)
            .bind(instance_generation)
            .bind(hand_provisioning_operation_id)
            .bind(hand_lease_generation)
            .bind(checkpoint_id)
            .bind(checkpoint_generation)
            .bind(checkpoint_manifest_digest)
            .bind(checkpoint_logical_bytes)
            .bind(receipt.requested_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!(
                    "execution-task-hand-release:{}:{}:{}",
                    receipt.run_id, task_id, receipt.attempt_generation
                ),
            })?
        };
        let persisted = execution_hand_release_receipt_from_row(&row)?;
        conn.commit().await?;
        Ok(persisted)
    }

    /// Finalizes verified compute destruction after a checkpoint was already committed.
    ///
    /// This is the recovery seam for an ambiguous checkpoint attempt that was later
    /// reconciled with its attachment retained. Execution tasks and conversational
    /// workers share this exact atomic boundary. Provider destruction happens before
    /// this call; the lease, capacity charge, and workspace state then advance under
    /// the exact hand and workspace generations in one transaction.
    pub async fn finalize_checkpointed_hand_destroy(
        &self,
        binding: &WorkspaceBinding,
        lease: &HandLease,
    ) -> Result<bool> {
        let fence = WorkspaceBindingFence::try_from(binding)?;
        let checkpoint_id = fence.checkpoint_id.ok_or_else(|| {
            MoaError::ValidationError(
                "task hand release requires a verified checkpoint head".to_string(),
            )
        })?;
        if lease.status != HandLeaseStatus::Active
            || lease.attachment
                != Some(crate::core::leases::HandLeaseWorkspaceAttachment::new(
                    binding.workspace_id,
                    fence.writer_epoch,
                    fence.instance_generation,
                    Some(checkpoint_id),
                )?)
            || lease.handle.is_none()
        {
            return Err(MoaError::ValidationError(
                "task hand release lease does not match the committed workspace head".to_string(),
            ));
        }
        let mut conn = self.begin(binding.tenant_id).await?;
        let lease_affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET status = 'destroyed', handle = NULL, workspace_id = NULL,
                workspace_writer_epoch = NULL, workspace_instance_generation = NULL,
                restored_checkpoint_id = NULL, reap_not_before = NULL,
                reap_claim_token = NULL, reap_claim_expires_at = NULL, updated_at = now()
            WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3
              AND provider = $4 AND generation = $5
              AND provisioning_operation_id = $6 AND status = 'active'
              AND workspace_id = $7 AND workspace_writer_epoch = $8
              AND workspace_instance_generation = $9
              AND restored_checkpoint_id = $10 AND handle IS NOT NULL
            "#,
        )
        .bind(binding.tenant_id)
        .bind(lease.session_id)
        .bind(&lease.worker_id)
        .bind(&lease.provider)
        .bind(lease.generation)
        .bind(lease.provisioning_operation_id)
        .bind(binding.workspace_id)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if lease_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        let capacity_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_capacity_reservations
            SET reservation_state = 'released', updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND provider_account_id = $3 AND provider_account_generation = $4
              AND hand_provisioning_operation_id = $5
              AND hand_lease_generation = $6
              AND expected_writer_epoch = $7 AND expected_instance_generation = $8
              AND resource_dimension = 'active_hands'
              -- A pending charge is still a charge: destroying the yielded hand
              -- must settle it rather than roll the whole release back.
              AND reservation_state IN ('pending', 'committed')
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(lease.provisioning_operation_id)
        .bind(lease.generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if capacity_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        let workspace_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspaces
            SET lifecycle_state = 'ready', updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND provider_account_id = $3 AND provider_account_generation = $4
              AND writer_epoch = $5 AND instance_generation = $6
              AND current_checkpoint_generation = $7 AND current_checkpoint_id = $8
              AND lifecycle_state = 'active' AND access_fenced_at IS NULL
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_generation)
        .bind(checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if workspace_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        conn.commit().await?;
        Ok(true)
    }

    /// Applies one documented lifecycle transition under writer and instance fences.
    pub async fn transition(&self, transition: WorkspaceTransition) -> Result<bool> {
        if !allowed_transition(transition.from, transition.to) {
            return Err(MoaError::ValidationError(format!(
                "invalid workspace transition {} -> {}",
                transition.from.as_str(),
                transition.to.as_str()
            )));
        }
        let mut conn = self.begin(transition.tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspaces
            SET lifecycle_state = $3, updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND lifecycle_state = $4
              AND writer_epoch = $5 AND instance_generation = $6
              AND lifecycle_state NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(transition.tenant_id)
        .bind(transition.workspace_id)
        .bind(transition.to.as_str())
        .bind(transition.from.as_str())
        .bind(transition.writer_epoch)
        .bind(transition.instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Claims the sole writable attachment by atomically advancing both fences.
    pub async fn claim_writer(
        &self,
        claim: WorkspaceWriterClaim,
    ) -> Result<Option<SandboxWorkspace>> {
        if claim.expected_state != SandboxWorkspaceState::Ready {
            return Err(MoaError::ValidationError(
                "a writer may only be claimed from ready".to_string(),
            ));
        }
        let mut conn = self.begin(claim.tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.sandbox_workspaces
            SET lifecycle_state = 'restoring', writer_epoch = writer_epoch + 1,
                instance_generation = instance_generation + 1, updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND lifecycle_state = $3
              AND writer_epoch = $4 AND instance_generation = $5
              AND access_fenced_at IS NULL
            RETURNING {WORKSPACE_COLUMNS}
            "#,
        ))
        .bind(claim.tenant_id)
        .bind(claim.workspace_id)
        .bind(claim.expected_state.as_str())
        .bind(claim.expected_writer_epoch)
        .bind(claim.expected_instance_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let workspace = row.as_ref().map(workspace_from_row).transpose()?;
        conn.commit().await?;
        Ok(workspace)
    }

    /// Atomically makes an exactly hydrated provisioning lease and workspace routable.
    pub async fn activate_hydrated(
        &self,
        request: ActivateHydratedWorkspaceRequest<'_>,
    ) -> Result<bool> {
        let ActivateHydratedWorkspaceRequest {
            binding,
            lease,
            handle,
        } = request;
        let fence = WorkspaceBindingFence::try_from(binding)?;
        validate_lease_for_binding(lease, binding, HandLeaseStatus::Provisioning)?;
        if handle.provisioning_operation_id != lease.provisioning_operation_id {
            return Err(MoaError::ValidationError(
                "hydrated hand handle does not match the claimed lease operation".to_string(),
            ));
        }

        let mut conn = self.begin(binding.tenant_id).await?;
        let lease_affected = sqlx::query(
            r#"
            UPDATE moa.hand_leases
            SET handle = $8, status = 'active', updated_at = now(),
                reap_not_before = NULL, reap_claim_token = NULL,
                reap_claim_expires_at = NULL
            WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3
              AND provider = $4 AND generation = $5
              AND provisioning_operation_id = $6
              AND provisioning_operation_id = $7
              AND status = 'provisioning' AND handle IS NULL
              AND workspace_id = $9 AND workspace_writer_epoch = $10
              AND workspace_instance_generation = $11
              AND restored_checkpoint_id IS NOT DISTINCT FROM $12
            "#,
        )
        .bind(binding.tenant_id)
        .bind(lease.session_id)
        .bind(&lease.worker_id)
        .bind(&lease.provider)
        .bind(lease.generation)
        .bind(lease.provisioning_operation_id)
        .bind(handle.provisioning_operation_id)
        .bind(Json(handle))
        .bind(binding.workspace_id)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if lease_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }

        let workspace_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspaces
            SET lifecycle_state = 'active', updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND provider_account_id = $3 AND provider_account_generation = $4
              AND lifecycle_state = 'restoring' AND access_fenced_at IS NULL
              AND writer_epoch = $5 AND instance_generation = $6
              AND current_checkpoint_generation = $7
              AND current_checkpoint_id IS NOT DISTINCT FROM $8
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_generation)
        .bind(fence.checkpoint_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if workspace_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        conn.commit().await?;
        Ok(true)
    }

    /// Fences caller access and begins workspace deletion under exact generations.
    pub async fn fence_for_deletion(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        expected_writer_epoch: i64,
        expected_instance_generation: i64,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let workspace = Self::fence_for_deletion_in_transaction(
            conn.as_mut(),
            tenant_id,
            workspace_id,
            expected_writer_epoch,
            expected_instance_generation,
        )
        .await?;
        conn.commit().await?;
        Ok(workspace.is_some())
    }

    /// Fences deletion and marks every desired grant absent in one transaction.
    pub async fn fence_for_deletion_with_grants_in_transaction(
        conn: &mut PgConnection,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        expected_writer_epoch: i64,
        expected_instance_generation: i64,
    ) -> Result<Option<(SandboxWorkspace, Vec<WorkspaceGrant>)>> {
        let Some(workspace) = Self::fence_for_deletion_in_transaction(
            conn,
            tenant_id,
            workspace_id,
            expected_writer_epoch,
            expected_instance_generation,
        )
        .await?
        else {
            return Ok(None);
        };
        let grants = Self::load_grants_in_transaction(conn, tenant_id, workspace_id).await?;
        Self::reconcile_grants_in_transaction(
            conn,
            tenant_id,
            workspace_id,
            workspace.delete_generation,
            "absent",
            &grants,
        )
        .await?;
        Ok(Some((workspace, grants)))
    }

    /// Fences deletion using the caller's already-scoped transaction.
    pub async fn fence_for_deletion_in_transaction(
        conn: &mut PgConnection,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        expected_writer_epoch: i64,
        expected_instance_generation: i64,
    ) -> Result<Option<SandboxWorkspace>> {
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.sandbox_workspaces
            SET lifecycle_state = 'deleting', access_fenced_at = now(),
                delete_generation = delete_generation + 1, updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND lifecycle_state IN ('ready', 'active', 'failed')
              AND writer_epoch = $3 AND instance_generation = $4
              AND access_fenced_at IS NULL
            RETURNING {WORKSPACE_COLUMNS}
            "#,
        ))
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(expected_writer_epoch)
        .bind(expected_instance_generation)
        .fetch_optional(conn)
        .await
        .map_err(map_sqlx_error)?;
        row.as_ref().map(workspace_from_row).transpose()
    }

    /// Marks a deleting workspace deleted only after its delete operation proves absence.
    pub async fn finalize_deleted(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        delete_generation: i64,
        operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspaces AS workspace
            SET lifecycle_state = 'deleted', updated_at = now()
            FROM moa.sandbox_workspace_operations AS operation
            WHERE workspace.tenant_id = $1 AND workspace.workspace_id = $2
              AND workspace.lifecycle_state = 'deleting'
              AND workspace.delete_generation = $3
              AND operation.operation_id = $4
              AND operation.tenant_id = workspace.tenant_id
              AND operation.workspace_id = workspace.workspace_id
              AND operation.operation_kind = 'delete'
              AND operation.outcome_class = 'confirmed'
              AND operation.confirmed_disposition = 'resource_absent'
              AND operation.absence_observation_count = 2
              AND operation.absence_last_observed_at >= operation.absence_first_observed_at + interval '1 second'
            "#,
        )
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(delete_generation)
        .bind(operation_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 1
            && !release_workspace_in_transaction(
                conn.as_mut(),
                tenant_id,
                workspace_id,
                delete_generation,
            )
            .await?
        {
            conn.rollback().await?;
            return Ok(false);
        }
        conn.commit().await?;
        Ok(affected == 1)
    }
}

fn execution_hand_release_receipt_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExecutionHandReleaseReceipt> {
    let owner_kind: String = row.try_get("owner_kind").map_err(map_sqlx_error)?;
    let logical_generation: i64 = row.try_get("logical_generation").map_err(map_sqlx_error)?;
    let logical_generation = u64::try_from(logical_generation).map_err(|_| {
        MoaError::StorageError("release receipt logical generation is not positive".to_string())
    })?;
    let owner = match owner_kind.as_str() {
        "task" => ExecutionHandReleaseOwner::Task {
            task_id: row.try_get("task_id").map_err(map_sqlx_error)?,
            logical_generation,
        },
        "compensation" => ExecutionHandReleaseOwner::Compensation {
            compensation_id: row.try_get("compensation_id").map_err(map_sqlx_error)?,
            logical_generation,
        },
        other => {
            return Err(MoaError::StorageError(format!(
                "unknown execution hand release owner kind {other}"
            )));
        }
    };
    let attempt_generation: i64 = row.try_get("attempt_generation").map_err(map_sqlx_error)?;
    let writer_epoch: Option<i64> = row.try_get("writer_epoch").map_err(map_sqlx_error)?;
    let instance_generation: Option<i64> =
        row.try_get("instance_generation").map_err(map_sqlx_error)?;
    let hand_lease_generation: Option<i64> = row
        .try_get("hand_lease_generation")
        .map_err(map_sqlx_error)?;
    let checkpoint_generation: Option<i64> = row
        .try_get("checkpoint_generation")
        .map_err(map_sqlx_error)?;
    let checkpoint_logical_bytes: Option<i64> = row
        .try_get("checkpoint_logical_bytes")
        .map_err(map_sqlx_error)?;
    let hand_provisioning_operation_id: Option<Uuid> = row
        .try_get("hand_provisioning_operation_id")
        .map_err(map_sqlx_error)?;
    Ok(ExecutionHandReleaseReceipt {
        receipt_id: row.try_get("receipt_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        run_id: row.try_get("run_uid").map_err(map_sqlx_error)?,
        owner,
        attempt_generation: u64::try_from(attempt_generation).map_err(|_| {
            MoaError::StorageError("release receipt attempt generation is not positive".to_string())
        })?,
        workspace_id: row.try_get("workspace_id").map_err(map_sqlx_error)?,
        writer_epoch: writer_epoch.map(u64::try_from).transpose().map_err(|_| {
            MoaError::StorageError("release receipt writer epoch is negative".to_string())
        })?,
        instance_generation: instance_generation
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                MoaError::StorageError(
                    "release receipt instance generation is negative".to_string(),
                )
            })?,
        hand_provisioning_operation_id: hand_provisioning_operation_id
            .map(HandProvisioningOperationId),
        hand_lease_generation: hand_lease_generation
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                MoaError::StorageError(
                    "release receipt hand lease generation is not positive".to_string(),
                )
            })?,
        checkpoint_id: row.try_get("checkpoint_id").map_err(map_sqlx_error)?,
        checkpoint_generation: checkpoint_generation
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                MoaError::StorageError(
                    "release receipt checkpoint generation is not positive".to_string(),
                )
            })?,
        checkpoint_manifest_digest: row
            .try_get("checkpoint_manifest_digest")
            .map_err(map_sqlx_error)?,
        checkpoint_logical_bytes: checkpoint_logical_bytes
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                MoaError::StorageError(
                    "release receipt checkpoint logical bytes are negative".to_string(),
                )
            })?,
        requested_at: row.try_get("requested_at").map_err(map_sqlx_error)?,
        released_at: row.try_get("released_at").map_err(map_sqlx_error)?,
    })
}

fn allowed_transition(from: SandboxWorkspaceState, to: SandboxWorkspaceState) -> bool {
    use SandboxWorkspaceState::{
        Active, Committing, Creating, Deleting, Failed, Quiescing, Ready, Reconciling, Restoring,
    };
    matches!(
        (from, to),
        (Creating, Ready)
            | (Ready, Restoring)
            | (Active, Quiescing)
            | (Quiescing, Committing)
            | (Committing, Active)
            | (Committing, Ready)
            | (Committing, Reconciling)
            | (Reconciling, Active)
            | (Reconciling, Failed)
            | (Restoring, Active)
            | (Restoring, Reconciling)
            | (Ready | Active | Failed, Deleting)
    )
}

#[cfg(test)]
mod tests {
    use moa_core::types::sandbox_workspace::SandboxWorkspaceState;

    use super::allowed_transition;

    #[test]
    fn state_machine_rejects_direct_active_to_deleted_offline() {
        // Pins: external cleanup and a separated absence proof cannot be skipped.
        assert!(!allowed_transition(
            SandboxWorkspaceState::Active,
            SandboxWorkspaceState::Deleted
        ));
        assert!(allowed_transition(
            SandboxWorkspaceState::Active,
            SandboxWorkspaceState::Quiescing
        ));
    }
}
