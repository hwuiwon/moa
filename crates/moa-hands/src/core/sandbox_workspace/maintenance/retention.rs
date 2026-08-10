//! Checkpoint-retention maintenance passes and fenced tombstoning.

use super::*;

impl WorkspaceMaintenanceCoordinator {
    /// Claims and processes one transaction-safe checkpoint-retention batch.
    pub async fn run_retention_once(&self) -> Result<WorkspaceRetentionPass> {
        let claimed = self.claim_checkpoints().await?;
        let mut pass = WorkspaceRetentionPass {
            claimed: claimed.len() as u64,
            ..WorkspaceRetentionPass::default()
        };
        for checkpoint in claimed {
            match self.delete_claimed_checkpoint(&checkpoint).await {
                Ok(CheckpointDeleteProgress::Deleted) => pass.deleted += 1,
                Ok(CheckpointDeleteProgress::AwaitingAbsence) => pass.awaiting_absence += 1,
                Err(error) => {
                    self.release_checkpoint_claim(&checkpoint, "checkpoint_delete_failed")
                        .await?;
                    pass.retrying += 1;
                    tracing::warn!(
                        tenant_id = %checkpoint.tenant_id,
                        workspace_id = %checkpoint.workspace_id,
                        checkpoint_id = %checkpoint.checkpoint_id,
                        error_code = "checkpoint_delete_failed",
                        error = %error,
                        "workspace checkpoint retention will retry"
                    );
                }
            }
        }
        Ok(pass)
    }

    /// Reconciles every persisted provider-account generation and persists drift.
    async fn claim_checkpoints(&self) -> Result<Vec<ClaimedCheckpoint>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let claim_token = Uuid::new_v4();
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE protected AS (
                SELECT checkpoint.checkpoint_id, checkpoint.workspace_id,
                       checkpoint.tenant_id, 0::BIGINT AS depth
                FROM moa.sandbox_workspace_checkpoints AS checkpoint
                JOIN moa.sandbox_workspaces AS workspace
                  ON workspace.tenant_id = checkpoint.tenant_id
                 AND workspace.workspace_id = checkpoint.workspace_id
                 AND workspace.current_checkpoint_id = checkpoint.checkpoint_id
                UNION ALL
                SELECT parent.checkpoint_id, parent.workspace_id, parent.tenant_id,
                       protected.depth + 1
                FROM protected
                JOIN moa.sandbox_workspace_checkpoints AS child
                  ON child.tenant_id = protected.tenant_id
                 AND child.workspace_id = protected.workspace_id
                 AND child.checkpoint_id = protected.checkpoint_id
                JOIN moa.sandbox_workspace_checkpoints AS parent
                  ON parent.tenant_id = child.tenant_id
                 AND parent.workspace_id = child.workspace_id
                 AND parent.checkpoint_id = child.parent_checkpoint_id
                WHERE protected.depth < $1
            ), candidates AS (
                SELECT checkpoint.checkpoint_id
                FROM moa.sandbox_workspace_checkpoints AS checkpoint
                WHERE checkpoint.lifecycle_state IN ('available', 'deleting')
                  AND checkpoint.retention_state IN ('retained', 'expired', 'deleting')
                  AND checkpoint.created_at <= now() - make_interval(secs => $2)
                  AND (checkpoint.gc_retry_not_before IS NULL OR checkpoint.gc_retry_not_before <= now())
                  AND (checkpoint.gc_claim_token IS NULL OR checkpoint.gc_claim_expires_at <= now())
                  AND NOT EXISTS (
                      SELECT 1 FROM protected
                      WHERE protected.checkpoint_id = checkpoint.checkpoint_id
                  )
                ORDER BY checkpoint.created_at, checkpoint.checkpoint_id
                LIMIT $3
                FOR UPDATE SKIP LOCKED
            )
            UPDATE moa.sandbox_workspace_checkpoints AS checkpoint
            SET lifecycle_state = 'deleting', retention_state = 'deleting',
                gc_claim_token = $4,
                gc_claim_expires_at = now() + make_interval(secs => $5),
                gc_attempts = gc_attempts + 1,
                deletion_started_at = COALESCE(deletion_started_at, now())
            FROM candidates, moa.sandbox_workspaces AS workspace
            WHERE checkpoint.checkpoint_id = candidates.checkpoint_id
              AND workspace.tenant_id = checkpoint.tenant_id
              AND workspace.workspace_id = checkpoint.workspace_id
            RETURNING checkpoint.checkpoint_id, checkpoint.tenant_id,
                      checkpoint.workspace_id, checkpoint.generation,
                      checkpoint.object_reference, checkpoint.deletion_absence_observation_count,
                      checkpoint.deletion_absence_first_observed_at,
                      checkpoint.deletion_inventory_digest,
                      workspace.provider_account_id,
                      workspace.provider_account_generation,
                      workspace.provider
            "#,
        )
        .bind(i64::from(self.retention.retained_ancestor_count))
        .bind(i64::try_from(self.retention.minimum_age_seconds).map_err(|_| {
            MoaError::ConfigError("checkpoint minimum age overflows Postgres".to_string())
        })?)
        .bind(i64::from(self.retention.gc_batch_size))
        .bind(claim_token)
        .bind(i64::try_from(self.retention.claim_ttl_seconds).map_err(|_| {
            MoaError::ConfigError("checkpoint claim TTL overflows Postgres".to_string())
        })?)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let checkpoints = rows
            .iter()
            .map(|row| claimed_checkpoint_from_row(row, claim_token))
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(checkpoints)
    }

    async fn delete_claimed_checkpoint(
        &self,
        checkpoint: &ClaimedCheckpoint,
    ) -> Result<CheckpointDeleteProgress> {
        self.renew_checkpoint_claim(checkpoint).await?;
        self.checkpoint_store.delete(checkpoint.context).await?;
        self.renew_checkpoint_claim(checkpoint).await?;
        let prior =
            checkpoint
                .first_observed_at
                .map(|first_observed_at| CheckpointEmptyObservation {
                    first_observed_at,
                    inventory_digest: checkpoint.inventory_digest.clone().unwrap_or_default(),
                });
        match self
            .checkpoint_store
            .observe_absence(checkpoint.context, prior.as_ref(), Utc::now())
            .await?
        {
            CheckpointPrefixObservation::Present(_) => {
                self.reset_checkpoint_absence_observation(checkpoint)
                    .await?;
                Err(MoaError::StorageError(
                    "checkpoint objects remain after bounded deletion".to_string(),
                ))
            }
            CheckpointPrefixObservation::EmptyPending(observation) => {
                let mut conn = maintenance_conn(&self.pool).await?;
                let affected = sqlx::query(
                    r#"
                    UPDATE moa.sandbox_workspace_checkpoints
                    SET gc_claim_token = NULL, gc_claim_expires_at = NULL,
                        gc_retry_not_before = $5,
                        deletion_absence_observation_count = 1,
                        deletion_absence_first_observed_at = $6,
                        deletion_absence_last_observed_at = $6,
                        deletion_inventory_digest = $7
                    WHERE checkpoint_id = $1 AND tenant_id = $2 AND workspace_id = $3
                      AND gc_claim_token = $4 AND gc_claim_expires_at > now()
                      AND lifecycle_state = 'deleting'
                    "#,
                )
                .bind(checkpoint.checkpoint_id)
                .bind(checkpoint.tenant_id)
                .bind(checkpoint.workspace_id)
                .bind(checkpoint.claim_token)
                .bind(
                    observation.first_observed_at
                        + chrono::Duration::from_std(
                            self.checkpoint_store.deletion_consistency_window(),
                        )
                        .map_err(|error| MoaError::ConfigError(error.to_string()))?,
                )
                .bind(observation.first_observed_at)
                .bind(observation.inventory_digest)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx)?
                .rows_affected();
                conn.commit().await?;
                if affected != 1 {
                    return Err(MoaError::StorageError(
                        "checkpoint GC claim was lost before first absence observation".to_string(),
                    ));
                }
                Ok(CheckpointDeleteProgress::AwaitingAbsence)
            }
            CheckpointPrefixObservation::Absent(proof) => {
                let mut conn = maintenance_conn(&self.pool).await?;
                let affected = sqlx::query(
                    r#"
                    UPDATE moa.sandbox_workspace_checkpoints
                    SET lifecycle_state = 'deleted', retention_state = 'deleted',
                        object_reference = NULL,
                        gc_claim_token = NULL, gc_claim_expires_at = NULL,
                        gc_retry_not_before = NULL,
                        deletion_absence_observation_count = 2,
                        deletion_absence_first_observed_at = $5,
                        deletion_absence_last_observed_at = $6,
                        deletion_inventory_digest = $7,
                        deleted_at = now()
                    WHERE checkpoint_id = $1 AND tenant_id = $2 AND workspace_id = $3
                      AND gc_claim_token = $4 AND gc_claim_expires_at > now()
                      AND lifecycle_state = 'deleting'
                    "#,
                )
                .bind(checkpoint.checkpoint_id)
                .bind(checkpoint.tenant_id)
                .bind(checkpoint.workspace_id)
                .bind(checkpoint.claim_token)
                .bind(proof.first_observed_at)
                .bind(proof.last_observed_at)
                .bind(proof.inventory_digest)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx)?
                .rows_affected();
                conn.commit().await?;
                if affected != 1 {
                    return Err(MoaError::StorageError(
                        "checkpoint GC claim was lost before tombstone finalization".to_string(),
                    ));
                }
                Ok(CheckpointDeleteProgress::Deleted)
            }
        }
    }

    async fn renew_checkpoint_claim(&self, checkpoint: &ClaimedCheckpoint) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_checkpoints
            SET gc_claim_expires_at = now() + make_interval(secs => $5)
            WHERE checkpoint_id = $1 AND tenant_id = $2 AND workspace_id = $3
              AND gc_claim_token = $4 AND gc_claim_expires_at > now()
              AND lifecycle_state = 'deleting'
            "#,
        )
        .bind(checkpoint.checkpoint_id)
        .bind(checkpoint.tenant_id)
        .bind(checkpoint.workspace_id)
        .bind(checkpoint.claim_token)
        .bind(
            i64::try_from(self.retention.claim_ttl_seconds).map_err(|_| {
                MoaError::ConfigError("checkpoint claim TTL overflows Postgres".to_string())
            })?,
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?
        .rows_affected();
        conn.commit().await?;
        if affected != 1 {
            return Err(MoaError::StorageError(
                "checkpoint GC claim expired before object-store I/O completed".to_string(),
            ));
        }
        Ok(())
    }

    async fn reset_checkpoint_absence_observation(
        &self,
        checkpoint: &ClaimedCheckpoint,
    ) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let affected = sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_checkpoints
            SET deletion_absence_observation_count = 0,
                deletion_absence_first_observed_at = NULL,
                deletion_absence_last_observed_at = NULL,
                deletion_inventory_digest = NULL
            WHERE checkpoint_id = $1 AND tenant_id = $2 AND workspace_id = $3
              AND gc_claim_token = $4 AND gc_claim_expires_at > now()
              AND lifecycle_state = 'deleting'
            "#,
        )
        .bind(checkpoint.checkpoint_id)
        .bind(checkpoint.tenant_id)
        .bind(checkpoint.workspace_id)
        .bind(checkpoint.claim_token)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?
        .rows_affected();
        conn.commit().await?;
        if affected != 1 {
            return Err(MoaError::StorageError(
                "checkpoint GC claim was lost while resetting absence proof".to_string(),
            ));
        }
        Ok(())
    }

    async fn release_checkpoint_claim(
        &self,
        checkpoint: &ClaimedCheckpoint,
        _error_code: &'static str,
    ) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        sqlx::query(
            r#"
            UPDATE moa.sandbox_workspace_checkpoints
            SET gc_claim_token = NULL, gc_claim_expires_at = NULL,
                gc_retry_not_before = now() + make_interval(secs => $5)
            WHERE checkpoint_id = $1 AND tenant_id = $2 AND workspace_id = $3
              AND gc_claim_token = $4 AND gc_claim_expires_at > now()
              AND lifecycle_state = 'deleting'
            "#,
        )
        .bind(checkpoint.checkpoint_id)
        .bind(checkpoint.tenant_id)
        .bind(checkpoint.workspace_id)
        .bind(checkpoint.claim_token)
        .bind(
            i64::try_from(self.retention.retry_backoff_seconds).map_err(|_| {
                MoaError::ConfigError("checkpoint retry backoff overflows Postgres".to_string())
            })?,
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        Ok(())
    }
}
struct ClaimedCheckpoint {
    checkpoint_id: WorkspaceCheckpointId,
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    claim_token: Uuid,
    context: CheckpointStoreContext,
    first_observed_at: Option<DateTime<Utc>>,
    inventory_digest: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum CheckpointDeleteProgress {
    Deleted,
    AwaitingAbsence,
}

fn claimed_checkpoint_from_row(
    row: &sqlx::postgres::PgRow,
    claim_token: Uuid,
) -> Result<ClaimedCheckpoint> {
    let account_generation: i64 = row
        .try_get("provider_account_generation")
        .map_err(map_sqlx)?;
    let count: i32 = row
        .try_get("deletion_absence_observation_count")
        .map_err(map_sqlx)?;
    Ok(ClaimedCheckpoint {
        checkpoint_id: row.try_get("checkpoint_id").map_err(map_sqlx)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx)?,
        workspace_id: row.try_get("workspace_id").map_err(map_sqlx)?,
        claim_token,
        context: CheckpointStoreContext {
            tenant_id: row.try_get("tenant_id").map_err(map_sqlx)?,
            workspace_id: row.try_get("workspace_id").map_err(map_sqlx)?,
            checkpoint_id: row.try_get("checkpoint_id").map_err(map_sqlx)?,
            provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx)?,
            provider_account_generation: u64::try_from(account_generation).map_err(|_| {
                MoaError::StorageError("checkpoint account generation is invalid".to_string())
            })?,
        },
        first_observed_at: (count == 1)
            .then(|| {
                row.try_get("deletion_absence_first_observed_at")
                    .map_err(map_sqlx)
            })
            .transpose()?
            .flatten(),
        inventory_digest: row.try_get("deletion_inventory_digest").map_err(map_sqlx)?,
    })
}
