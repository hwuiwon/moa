//! Atomic checkpoint creation, publication, recovery, and reads.

use super::*;

impl PostgresWorkspaceRepository {
    /// Atomically verifies and publishes one commit-owned checkpoint and lease state.
    pub async fn publish_checkpoint_commit(
        &self,
        request: PublishCheckpointCommitRequest<'_>,
    ) -> Result<bool> {
        self.publish_checkpoint_commit_with_claim(request, WorkspaceOperationKind::Commit, None)
            .await
    }

    /// Atomically publishes one explicit checkpoint operation and its lease disposition.
    pub async fn publish_workspace_checkpoint(
        &self,
        request: PublishCheckpointCommitRequest<'_>,
    ) -> Result<bool> {
        self.publish_checkpoint_commit_with_claim(request, WorkspaceOperationKind::Checkpoint, None)
            .await
    }

    /// Atomically publishes a recovered checkpoint only while the exact reaper claim is live.
    pub async fn publish_checkpoint_commit_claimed(
        &self,
        request: PublishCheckpointCommitRequest<'_>,
        claimed: &ClaimedWorkspaceOperation,
    ) -> Result<bool> {
        if claimed.operation.operation_id != request.operation_id
            || !matches!(
                claimed.operation.kind,
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint
            )
            || claimed.operation.tenant_id != request.binding.tenant_id
            || claimed.operation.workspace_id != request.binding.workspace_id
            || claimed.operation.provider_account_id != request.binding.provider_account_id
            || u64::try_from(claimed.operation.provider_account_generation).ok()
                != Some(request.binding.provider_account_generation)
            || u64::try_from(claimed.operation.expected_writer_epoch).ok()
                != Some(request.binding.writer_epoch)
            || u64::try_from(claimed.operation.expected_instance_generation).ok()
                != Some(request.binding.instance_generation)
            || u64::try_from(claimed.operation.expected_checkpoint_generation).ok()
                != Some(
                    request
                        .binding
                        .current_revision
                        .as_ref()
                        .map_or(0, |revision| revision.generation),
                )
        {
            return Err(MoaError::ValidationError(
                "claimed checkpoint publication does not match the exact operation fences"
                    .to_string(),
            ));
        }
        self.publish_checkpoint_commit_with_claim(
            request,
            claimed.operation.kind,
            Some(claimed.claim_token),
        )
        .await
    }

    async fn publish_checkpoint_commit_with_claim(
        &self,
        request: PublishCheckpointCommitRequest<'_>,
        operation_kind: WorkspaceOperationKind,
        claim_token: Option<Uuid>,
    ) -> Result<bool> {
        let PublishCheckpointCommitRequest {
            binding,
            operation_id,
            publication,
            post_commit_state,
            lease,
        } = request;
        if !matches!(
            operation_kind,
            WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint
        ) {
            return Err(MoaError::ValidationError(
                "checkpoint publication requires commit or checkpoint operation kind".to_string(),
            ));
        }
        let fence = WorkspaceBindingFence::try_from(binding)?;
        validate_commit_lease_identity(lease, binding)?;
        let publication_fields = CommitPublicationFields::validate(
            binding,
            operation_id,
            publication,
            fence.checkpoint_generation,
        )?;

        let mut conn = self.begin(binding.tenant_id).await?;
        if !checkpoint_capacity_matches(
            conn.as_mut(),
            binding,
            operation_id,
            operation_kind,
            publication_fields.logical_bytes,
        )
        .await?
        {
            conn.rollback().await?;
            return Ok(false);
        }
        let checkpoint_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_checkpoints AS checkpoint
            SET lifecycle_state = 'available', object_reference = $9,
                manifest_digest = $10, logical_bytes = $11, verified_at = now()
            FROM moa.sandbox_workspace_operations AS operation
            WHERE checkpoint.tenant_id = $1 AND checkpoint.workspace_id = $2
              AND checkpoint.checkpoint_id = $3 AND checkpoint.generation = $4
              AND checkpoint.operation_id = $5
              AND checkpoint.source_writer_epoch = $6
              AND checkpoint.source_instance_generation = $7
              AND checkpoint.source_checkpoint_generation = $8
              AND checkpoint.lifecycle_state = 'creating'
              AND checkpoint.object_reference IS NULL
              AND checkpoint.manifest_digest IS NULL
              AND checkpoint.logical_bytes IS NULL
              AND checkpoint.verified_at IS NULL
              AND operation.tenant_id = checkpoint.tenant_id
              AND operation.workspace_id = checkpoint.workspace_id
              AND operation.operation_id = checkpoint.operation_id
              AND operation.operation_kind = $14
              AND operation.expected_writer_epoch = checkpoint.source_writer_epoch
              AND operation.expected_instance_generation = checkpoint.source_instance_generation
              AND operation.expected_checkpoint_generation = checkpoint.source_checkpoint_generation
              AND operation.provider_account_id = $12
              AND operation.provider_account_generation = $13
              AND operation.outcome_class IN ('not_sent', 'unknown')
              AND (
                    ($15::uuid IS NULL AND operation.claim_token IS NULL)
                 OR (operation.claim_token = $15 AND operation.claim_expires_at > now())
              )
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(publication.revision.checkpoint_id)
        .bind(publication_fields.generation)
        .bind(operation_id)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_generation)
        .bind(&publication.storage.resource_id)
        .bind(&publication.manifest_digest)
        .bind(publication_fields.logical_bytes)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(operation_kind.as_str())
        .bind(claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if checkpoint_affected != 1 {
            let replay = commit_is_already_published(
                conn.as_mut(),
                binding,
                lease,
                operation_id,
                operation_kind,
                publication,
                post_commit_state,
                fence,
                publication_fields,
            )
            .await?;
            if replay {
                conn.commit().await?;
                return Ok(true);
            }
            conn.rollback().await?;
            return Ok(false);
        }

        failpoints::hit("post_checkpoint_ready_pre_head_cas").await?;

        let lease_affected = update_post_commit_lease(
            conn.as_mut(),
            binding,
            lease,
            publication.revision.checkpoint_id,
            post_commit_state,
            fence,
        )
        .await?;
        if lease_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        if post_commit_state == WorkspacePostCommitState::ComputeDestroyed {
            let released = sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations
                SET reservation_state = 'released', updated_at = now()
                WHERE tenant_id = $1 AND workspace_id = $2
                  AND provider_account_id = $3 AND provider_account_generation = $4
                  AND hand_provisioning_operation_id = $5
                  AND hand_lease_generation = $6
                  AND expected_writer_epoch = $7
                  AND expected_instance_generation = $8
                  AND resource_dimension = 'active_hands'
                  -- A pending charge is still a charge: releasing compute must
                  -- settle it rather than roll the whole publication back.
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
            if released != 1 {
                conn.rollback().await?;
                return Ok(false);
            }
        }

        let workspace_state = match post_commit_state {
            WorkspacePostCommitState::AttachmentRetained => "active",
            WorkspacePostCommitState::ComputeDestroyed => "ready",
        };
        let workspace_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspaces
            SET current_checkpoint_id = $8, current_checkpoint_generation = $7,
                lifecycle_state = $9, updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2
              AND provider_account_id = $3 AND provider_account_generation = $4
              AND access_fenced_at IS NULL
              AND (
                    ($12::uuid IS NULL AND lifecycle_state = 'committing')
                 OR ($12 IS NOT NULL AND lifecycle_state IN ('committing', 'reconciling'))
              )
              AND writer_epoch = $5 AND instance_generation = $6
              AND current_checkpoint_generation = $10
              AND current_checkpoint_id IS NOT DISTINCT FROM $11
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(publication_fields.generation)
        .bind(publication.revision.checkpoint_id)
        .bind(workspace_state)
        .bind(fence.checkpoint_generation)
        .bind(fence.checkpoint_id)
        .bind(claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if workspace_affected != 1 {
            conn.rollback().await?;
            if !self
                .abandon_checkpoint_after_cas_loss(
                    binding,
                    operation_id,
                    publication.revision.checkpoint_id,
                )
                .await?
            {
                return Err(MoaError::StorageError(
                    "checkpoint CAS loss could not release its exact capacity owner".to_string(),
                ));
            }
            return Ok(false);
        }

        let operation_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'confirmed', confirmed_disposition = 'resource_present',
                claim_token = NULL, claim_expires_at = NULL,
                retry_not_before = NULL, updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2 AND operation_id = $3
              AND operation_kind = $9
              AND provider_account_id = $4 AND provider_account_generation = $5
              AND expected_writer_epoch = $6 AND expected_instance_generation = $7
              AND expected_checkpoint_generation = $8
              AND outcome_class IN ('not_sent', 'unknown')
              AND (
                    ($10::uuid IS NULL AND claim_token IS NULL)
                 OR (claim_token = $10 AND claim_expires_at > now())
              )
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(operation_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_generation)
        .bind(operation_kind.as_str())
        .bind(claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if operation_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }

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
              AND reservation.resource_dimension IN ('checkpoints', 'logical_bytes')
              AND reservation.reservation_state IN ('pending', 'reconciling')
            "#,
        )
        .bind(binding.tenant_id)
        .bind(operation_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        conn.commit().await?;
        Ok(true)
    }

    async fn abandon_checkpoint_after_cas_loss(
        &self,
        binding: &WorkspaceBinding,
        operation_id: WorkspaceOperationId,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<bool> {
        let fence = WorkspaceBindingFence::try_from(binding)?;
        let mut conn = self.begin(binding.tenant_id).await?;
        let checkpoint_affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_checkpoints
            SET lifecycle_state = 'failed'
            WHERE tenant_id = $1 AND workspace_id = $2
              AND checkpoint_id = $3 AND operation_id = $4
              AND source_writer_epoch = $5 AND source_instance_generation = $6
              AND source_checkpoint_generation = $7
              AND lifecycle_state = 'creating'
              AND object_reference IS NULL AND manifest_digest IS NULL
              AND logical_bytes IS NULL AND verified_at IS NULL
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(checkpoint_id)
        .bind(operation_id)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if checkpoint_affected != 1 {
            conn.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_operations
            SET outcome_class = 'unknown', updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2 AND operation_id = $3
              AND provider_account_id = $4 AND provider_account_generation = $5
              AND expected_writer_epoch = $6 AND expected_instance_generation = $7
              AND outcome_class IN ('not_sent', 'unknown')
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(operation_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let released = sqlx::query(
            r#"
            UPDATE moa.sandbox_capacity_reservations
            SET reservation_state = 'released', updated_at = now()
            WHERE tenant_id = $1 AND workspace_id = $2 AND operation_id = $3
              AND provider_account_id = $4 AND provider_account_generation = $5
              AND expected_writer_epoch = $6 AND expected_instance_generation = $7
              AND resource_dimension IN ('checkpoints', 'logical_bytes')
              AND reservation_state IN ('pending', 'reconciling')
            "#,
        )
        .bind(binding.tenant_id)
        .bind(binding.workspace_id)
        .bind(operation_id)
        .bind(binding.provider_account_id)
        .bind(fence.provider_account_generation)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if released == 0 {
            conn.rollback().await?;
            return Ok(false);
        }
        conn.commit().await?;
        Ok(true)
    }

    /// Inserts an immutable `creating` checkpoint before any byte or provider I/O.
    pub async fn create_checkpoint(
        &self,
        request: CreateCheckpointRequest,
    ) -> Result<Option<WorkspaceCheckpoint>> {
        if request.expected_writer_epoch < 0
            || request.expected_instance_generation < 0
            || request.expected_checkpoint_generation < 0
            || (request.expected_checkpoint_generation == 0)
                != request.parent_checkpoint_id.is_none()
        {
            return Err(MoaError::ValidationError(
                "checkpoint intent has invalid generations or parent".to_string(),
            ));
        }
        let generation = request
            .expected_checkpoint_generation
            .checked_add(1)
            .ok_or_else(|| {
                MoaError::ValidationError("checkpoint generation overflow".to_string())
            })?;
        let mut conn = self.begin(request.tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.sandbox_workspace_checkpoints (
                checkpoint_id, tenant_id, workspace_id, generation,
                parent_checkpoint_id, parent_generation, source_writer_epoch,
                source_instance_generation, source_checkpoint_generation,
                operation_id, lifecycle_state
            )
            SELECT $1, operation.tenant_id, operation.workspace_id, $2, $3,
                   CASE WHEN $3::uuid IS NULL THEN NULL ELSE operation.expected_checkpoint_generation END,
                   operation.expected_writer_epoch, operation.expected_instance_generation,
                   operation.expected_checkpoint_generation, operation.operation_id, 'creating'
            FROM moa.sandbox_workspace_operations AS operation
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = operation.tenant_id
             AND workspace.workspace_id = operation.workspace_id
            WHERE operation.tenant_id = $4 AND operation.workspace_id = $5
              AND operation.operation_id = $6
              AND operation.operation_kind IN ('commit', 'checkpoint')
              AND operation.outcome_class = 'not_sent'
              AND operation.expected_writer_epoch = $7
              AND operation.expected_instance_generation = $8
              AND operation.expected_checkpoint_generation = $9
              AND workspace.lifecycle_state = 'committing'
              AND workspace.writer_epoch = operation.expected_writer_epoch
              AND workspace.instance_generation = operation.expected_instance_generation
              AND workspace.current_checkpoint_generation = operation.expected_checkpoint_generation
              AND (
                    (operation.expected_checkpoint_generation = 0 AND $3::uuid IS NULL)
                 OR EXISTS (
                        SELECT 1 FROM moa.sandbox_workspace_checkpoints AS parent
                        WHERE parent.tenant_id = operation.tenant_id
                          AND parent.workspace_id = operation.workspace_id
                          AND parent.checkpoint_id = $3
                          AND parent.generation = operation.expected_checkpoint_generation
                          AND parent.lifecycle_state = 'available'
                    )
              )
            ON CONFLICT DO NOTHING
            RETURNING {CHECKPOINT_COLUMNS}
            "#,
        ))
        .bind(request.checkpoint_id)
        .bind(generation)
        .bind(request.parent_checkpoint_id)
        .bind(request.tenant_id)
        .bind(request.workspace_id)
        .bind(request.operation_id)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .bind(request.expected_checkpoint_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let checkpoint = if let Some(row) = row {
            Some(checkpoint_from_row(&row)?)
        } else {
            let existing = Self::get_checkpoint_for_operation_in_transaction(
                conn.as_mut(),
                request.tenant_id,
                request.workspace_id,
                request.operation_id,
            )
            .await?;
            match existing {
                Some(existing) if checkpoint_matches_create_request(&existing, request) => {
                    Some(existing)
                }
                Some(_) => {
                    conn.rollback().await?;
                    return Err(MoaError::ValidationError(
                        "checkpoint replay conflicts with the persisted operation row".to_string(),
                    ));
                }
                None => None,
            }
        };
        conn.commit().await?;
        Ok(checkpoint)
    }

    /// Loads the checkpoint row owned by one exact commit or checkpoint operation.
    pub async fn get_checkpoint_for_operation(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        operation_id: WorkspaceOperationId,
    ) -> Result<Option<WorkspaceCheckpoint>> {
        let mut conn = self.begin(tenant_id).await?;
        let checkpoint = Self::get_checkpoint_for_operation_in_transaction(
            conn.as_mut(),
            tenant_id,
            workspace_id,
            operation_id,
        )
        .await?;
        conn.commit().await?;
        Ok(checkpoint)
    }

    async fn get_checkpoint_for_operation_in_transaction(
        conn: &mut PgConnection,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        operation_id: WorkspaceOperationId,
    ) -> Result<Option<WorkspaceCheckpoint>> {
        let row = sqlx::query(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM moa.sandbox_workspace_checkpoints WHERE tenant_id = $1 AND workspace_id = $2 AND operation_id = $3"
        ))
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(operation_id)
        .fetch_optional(conn)
        .await
        .map_err(map_sqlx_error)?;
        row.as_ref().map(checkpoint_from_row).transpose()
    }

    /// Loads one exact tenant-owned checkpoint.
    pub async fn get_checkpoint(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<Option<WorkspaceCheckpoint>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM moa.sandbox_workspace_checkpoints WHERE tenant_id = $1 AND workspace_id = $2 AND checkpoint_id = $3"
        ))
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(checkpoint_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let checkpoint = row.as_ref().map(checkpoint_from_row).transpose()?;
        conn.commit().await?;
        Ok(checkpoint)
    }
}

async fn checkpoint_capacity_matches(
    conn: &mut PgConnection,
    binding: &WorkspaceBinding,
    operation_id: WorkspaceOperationId,
    operation_kind: WorkspaceOperationKind,
    logical_bytes: i64,
) -> Result<bool> {
    sqlx::query_scalar(
        r#"
        SELECT
            count(*) FILTER (
                WHERE reservation.resource_dimension = 'checkpoints'
                  AND reservation.quantity = 1
            ) = 1
            AND count(*) FILTER (
                WHERE reservation.resource_dimension = 'logical_bytes'
                  AND reservation.quantity = $8
            ) = CASE WHEN $8 = 0 THEN 0 ELSE 1 END
            AND count(*) = CASE WHEN $8 = 0 THEN 1 ELSE 2 END
        FROM moa.sandbox_capacity_reservations AS reservation
        JOIN moa.sandbox_workspace_operations AS operation
          ON operation.tenant_id = reservation.tenant_id
         AND operation.workspace_id = reservation.workspace_id
         AND operation.operation_id = reservation.operation_id
        WHERE reservation.tenant_id = $1
          AND reservation.workspace_id = $2
          AND reservation.operation_id = $3
          AND reservation.provider_account_id = $4
          AND reservation.provider_account_generation = $5
          AND reservation.expected_writer_epoch = $6
          AND reservation.expected_instance_generation = $7
          AND reservation.resource_dimension IN ('checkpoints', 'logical_bytes')
          AND operation.operation_kind = $9
          AND (
                (
                    operation.outcome_class IN ('not_sent', 'unknown')
                    AND reservation.reservation_state IN ('pending', 'reconciling')
                )
                OR (
                    operation.outcome_class = 'confirmed'
                    AND operation.confirmed_disposition = 'resource_present'
                    AND reservation.reservation_state = 'committed'
                )
          )
        "#,
    )
    .bind(binding.tenant_id)
    .bind(binding.workspace_id)
    .bind(operation_id)
    .bind(binding.provider_account_id)
    .bind(
        i64::try_from(binding.provider_account_generation).map_err(|_| {
            MoaError::ValidationError(
                "workspace provider-account generation overflows Postgres bigint".to_string(),
            )
        })?,
    )
    .bind(i64::try_from(binding.writer_epoch).map_err(|_| {
        MoaError::ValidationError("workspace writer epoch overflows Postgres bigint".to_string())
    })?)
    .bind(i64::try_from(binding.instance_generation).map_err(|_| {
        MoaError::ValidationError(
            "workspace instance generation overflows Postgres bigint".to_string(),
        )
    })?)
    .bind(logical_bytes)
    .bind(operation_kind.as_str())
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)
}
const CHECKPOINT_COLUMNS: &str = "checkpoint_id, tenant_id, workspace_id, generation, \
    parent_checkpoint_id, source_writer_epoch, source_instance_generation, operation_id, \
    lifecycle_state, object_reference, manifest_digest, logical_bytes, \
    verified_at";

#[derive(Debug, Clone, Copy)]
struct CommitPublicationFields {
    generation: i64,
    logical_bytes: i64,
}

impl CommitPublicationFields {
    fn validate(
        binding: &WorkspaceBinding,
        operation_id: WorkspaceOperationId,
        publication: &WorkspaceCheckpointPublication,
        current_generation: i64,
    ) -> Result<Self> {
        let generation = i64::try_from(publication.revision.generation).map_err(|_| {
            MoaError::ValidationError(
                "published checkpoint generation overflows Postgres bigint".to_string(),
            )
        })?;
        let expected_generation = current_generation.checked_add(1).ok_or_else(|| {
            MoaError::ValidationError("checkpoint generation overflow".to_string())
        })?;
        let logical_bytes = i64::try_from(publication.logical_bytes).map_err(|_| {
            MoaError::ValidationError(
                "published checkpoint logical bytes overflow Postgres bigint".to_string(),
            )
        })?;
        if publication.revision.checkpoint_id != WorkspaceCheckpointId(operation_id.0)
            || generation != expected_generation
            || publication.revision.format_version != CHECKPOINT_ARCHIVE_FORMAT_VERSION
            || publication.storage.provider_account_id != binding.provider_account_id
            || publication.storage.provider_account_generation
                != binding.provider_account_generation
            || publication.storage.kind != ProviderStorageKind::PortableCheckpoint
            || publication.storage.resource_id.trim().is_empty()
            || publication.storage.workspace_locator.is_some()
            || publication.manifest_digest.trim().is_empty()
        {
            return Err(MoaError::ValidationError(
                "checkpoint publication does not match its operation, account, format, or payload"
                    .to_string(),
            ));
        }
        Ok(Self {
            generation,
            logical_bytes,
        })
    }
}

async fn update_post_commit_lease(
    conn: &mut PgConnection,
    binding: &WorkspaceBinding,
    lease: &HandLease,
    checkpoint_id: WorkspaceCheckpointId,
    post_commit_state: WorkspacePostCommitState,
    fence: WorkspaceBindingFence,
) -> Result<u64> {
    let query = match post_commit_state {
        WorkspacePostCommitState::AttachmentRetained => {
            r#"
            UPDATE moa.hand_leases
            SET restored_checkpoint_id = $12, updated_at = now()
            WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3
              AND provider = $4 AND generation = $5
              AND provisioning_operation_id = $6 AND status = 'active'
              AND workspace_id = $7 AND workspace_writer_epoch = $8
              AND workspace_instance_generation = $9
              AND restored_checkpoint_id IS NOT DISTINCT FROM $10
              AND handle IS NOT NULL AND $11::text = 'attachment_retained'
            "#
        }
        WorkspacePostCommitState::ComputeDestroyed => {
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
              AND restored_checkpoint_id IS NOT DISTINCT FROM $10
              AND handle IS NOT NULL AND $11::text = 'compute_destroyed'
              AND $12::uuid IS NOT NULL
            "#
        }
    };
    let state = post_commit_state_label(post_commit_state);
    Ok(sqlx::query(query)
        .bind(binding.tenant_id)
        .bind(lease.session_id)
        .bind(&lease.worker_id)
        .bind(&lease.provider)
        .bind(lease.generation)
        .bind(lease.provisioning_operation_id)
        .bind(binding.workspace_id)
        .bind(fence.writer_epoch)
        .bind(fence.instance_generation)
        .bind(fence.checkpoint_id)
        .bind(state)
        .bind(checkpoint_id)
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected())
}

#[allow(clippy::too_many_arguments)]
async fn commit_is_already_published(
    conn: &mut PgConnection,
    binding: &WorkspaceBinding,
    lease: &HandLease,
    operation_id: WorkspaceOperationId,
    operation_kind: WorkspaceOperationKind,
    publication: &WorkspaceCheckpointPublication,
    post_commit_state: WorkspacePostCommitState,
    fence: WorkspaceBindingFence,
    publication_fields: CommitPublicationFields,
) -> Result<bool> {
    let (workspace_state, lease_status, attached_workspace, attached_writer, attached_instance) =
        match post_commit_state {
            WorkspacePostCommitState::AttachmentRetained => (
                "active",
                "active",
                Some(binding.workspace_id),
                Some(fence.writer_epoch),
                Some(fence.instance_generation),
            ),
            WorkspacePostCommitState::ComputeDestroyed => ("ready", "destroyed", None, None, None),
        };
    let restored_checkpoint = matches!(
        post_commit_state,
        WorkspacePostCommitState::AttachmentRetained
    )
    .then_some(publication.revision.checkpoint_id);
    let destroyed = matches!(
        post_commit_state,
        WorkspacePostCommitState::ComputeDestroyed
    );
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.sandbox_workspaces AS workspace
            JOIN moa.sandbox_workspace_checkpoints AS checkpoint
              ON checkpoint.tenant_id = workspace.tenant_id
             AND checkpoint.workspace_id = workspace.workspace_id
             AND checkpoint.checkpoint_id = workspace.current_checkpoint_id
            JOIN moa.sandbox_workspace_operations AS operation
              ON operation.tenant_id = checkpoint.tenant_id
             AND operation.workspace_id = checkpoint.workspace_id
             AND operation.operation_id = checkpoint.operation_id
            JOIN moa.hand_leases AS lease
              ON lease.tenant_id = workspace.tenant_id
            WHERE workspace.tenant_id = $1 AND workspace.workspace_id = $2
              AND workspace.provider_account_id = $3
              AND workspace.provider_account_generation = $4
              AND workspace.writer_epoch = $5 AND workspace.instance_generation = $6
              AND workspace.current_checkpoint_generation = $7
              AND workspace.current_checkpoint_id = $8
              AND workspace.lifecycle_state = $9
              AND checkpoint.generation = $7 AND checkpoint.operation_id = $10
              AND checkpoint.source_writer_epoch = $5
              AND checkpoint.source_instance_generation = $6
              AND checkpoint.source_checkpoint_generation = $11
              AND checkpoint.lifecycle_state = 'available'
              AND checkpoint.object_reference = $12
              AND checkpoint.manifest_digest = $13 AND checkpoint.logical_bytes = $14
              AND checkpoint.verified_at IS NOT NULL
              AND operation.operation_kind = $26
              AND operation.provider_account_id = workspace.provider_account_id
              AND operation.provider_account_generation = workspace.provider_account_generation
              AND operation.outcome_class = 'confirmed'
              AND operation.confirmed_disposition = 'resource_present'
              AND lease.session_id = $15 AND lease.worker_id = $16
              AND lease.provider = $17 AND lease.generation = $18
              AND lease.provisioning_operation_id = $19 AND lease.status = $20
              AND lease.workspace_id IS NOT DISTINCT FROM $21
              AND lease.workspace_writer_epoch IS NOT DISTINCT FROM $22
              AND lease.workspace_instance_generation IS NOT DISTINCT FROM $23
              AND lease.restored_checkpoint_id IS NOT DISTINCT FROM $24
              AND (NOT $25 OR lease.handle IS NULL)
        )
        "#,
    )
    .bind(binding.tenant_id)
    .bind(binding.workspace_id)
    .bind(binding.provider_account_id)
    .bind(fence.provider_account_generation)
    .bind(fence.writer_epoch)
    .bind(fence.instance_generation)
    .bind(publication_fields.generation)
    .bind(publication.revision.checkpoint_id)
    .bind(workspace_state)
    .bind(operation_id)
    .bind(fence.checkpoint_generation)
    .bind(&publication.storage.resource_id)
    .bind(&publication.manifest_digest)
    .bind(publication_fields.logical_bytes)
    .bind(lease.session_id)
    .bind(&lease.worker_id)
    .bind(&lease.provider)
    .bind(lease.generation)
    .bind(lease.provisioning_operation_id)
    .bind(lease_status)
    .bind(attached_workspace)
    .bind(attached_writer)
    .bind(attached_instance)
    .bind(restored_checkpoint)
    .bind(destroyed)
    .bind(operation_kind.as_str())
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)
}

const fn post_commit_state_label(state: WorkspacePostCommitState) -> &'static str {
    match state {
        WorkspacePostCommitState::AttachmentRetained => "attachment_retained",
        WorkspacePostCommitState::ComputeDestroyed => "compute_destroyed",
    }
}

fn validate_commit_lease_identity(lease: &HandLease, binding: &WorkspaceBinding) -> Result<()> {
    if lease.tenant_id != binding.tenant_id
        || lease.provider.trim().is_empty()
        || lease.generation <= 0
    {
        return Err(MoaError::ValidationError(
            "commit lease identity does not match the workspace tenant or generation".to_string(),
        ));
    }
    Ok(())
}

fn checkpoint_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkspaceCheckpoint> {
    Ok(WorkspaceCheckpoint {
        checkpoint_id: row.try_get("checkpoint_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        workspace_id: row.try_get("workspace_id").map_err(map_sqlx_error)?,
        generation: row.try_get("generation").map_err(map_sqlx_error)?,
        parent_checkpoint_id: row
            .try_get("parent_checkpoint_id")
            .map_err(map_sqlx_error)?,
        source_writer_epoch: row.try_get("source_writer_epoch").map_err(map_sqlx_error)?,
        source_instance_generation: row
            .try_get("source_instance_generation")
            .map_err(map_sqlx_error)?,
        operation_id: row.try_get("operation_id").map_err(map_sqlx_error)?,
        state: WorkspaceCheckpointState::from_label(
            &row.try_get::<String, _>("lifecycle_state")
                .map_err(map_sqlx_error)?,
        )?,
        object_reference: row.try_get("object_reference").map_err(map_sqlx_error)?,
        manifest_digest: row.try_get("manifest_digest").map_err(map_sqlx_error)?,
        logical_bytes: row.try_get("logical_bytes").map_err(map_sqlx_error)?,
        verified_at: row.try_get("verified_at").map_err(map_sqlx_error)?,
    })
}

fn checkpoint_matches_create_request(
    checkpoint: &WorkspaceCheckpoint,
    request: CreateCheckpointRequest,
) -> bool {
    checkpoint.checkpoint_id == request.checkpoint_id
        && checkpoint.tenant_id == request.tenant_id
        && checkpoint.workspace_id == request.workspace_id
        && checkpoint.parent_checkpoint_id == request.parent_checkpoint_id
        && checkpoint.operation_id == request.operation_id
        && checkpoint.source_writer_epoch == request.expected_writer_epoch
        && checkpoint.source_instance_generation == request.expected_instance_generation
        && checkpoint.generation == request.expected_checkpoint_generation + 1
}
