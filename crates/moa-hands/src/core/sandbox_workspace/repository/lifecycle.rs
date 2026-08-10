//! Fenced sandbox-workspace lifecycle transitions and deletion.

use super::*;

impl PostgresWorkspaceRepository {
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
        conn.commit().await?;
        Ok(affected == 1)
    }
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
