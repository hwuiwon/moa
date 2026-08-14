//! Base sandbox-workspace creation and tenant-scoped reads.

use super::*;

impl PostgresWorkspaceRepository {
    /// Resolves the exact healthy-or-unknown provider-account generation selected by rollout policy.
    ///
    /// Admission callers must supply deployment-owned identifiers. This lookup
    /// never falls back to another account or generation when the configured
    /// canary is missing or unavailable.
    pub async fn resolve_provider_account(
        &self,
        provider_account_id: ProviderAccountId,
        generation: u64,
    ) -> Result<Option<WorkspaceProviderAccount>> {
        let generation = i64::try_from(generation).map_err(|_| {
            MoaError::ValidationError(
                "provider-account generation exceeds the supported range".to_string(),
            )
        })?;
        let row = sqlx::query(
            r#"
            SELECT provider_account_id, generation, provider
            FROM moa.sandbox_provider_accounts
            WHERE provider_account_id = $1
              AND generation = $2
              AND health IN ('healthy', 'unknown')
            "#,
        )
        .bind(provider_account_id)
        .bind(generation)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        row.map(|row| {
            Ok(WorkspaceProviderAccount {
                provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx_error)?,
                generation: row.try_get("generation").map_err(map_sqlx_error)?,
                provider: row.try_get("provider").map_err(map_sqlx_error)?,
            })
        })
        .transpose()
    }

    /// Persists workspace ownership and initial state before any provider create.
    pub async fn create(&self, request: &CreateWorkspaceRequest) -> Result<SandboxWorkspace> {
        let mut conn = self.begin(request.tenant_id).await?;
        let workspace = Self::create_in_transaction(conn.as_mut(), request).await?;
        conn.commit().await?;
        Ok(workspace)
    }

    /// Creates one workspace using the caller's already-scoped transaction.
    pub async fn create_in_transaction(
        conn: &mut PgConnection,
        request: &CreateWorkspaceRequest,
    ) -> Result<SandboxWorkspace> {
        if request.provider.trim().is_empty() || request.provider_account_generation <= 0 {
            return Err(MoaError::ValidationError(
                "workspace provider and positive provider-account generation are required"
                    .to_string(),
            ));
        }
        let (scope_kind, session_id, worker_id, run_id, task_id) = scope_columns(&request.scope)?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.sandbox_workspaces (
                workspace_id, tenant_id, scope_kind, scope_session_id, scope_worker_id,
                scope_run_id, scope_task_id, provider, provider_account_id,
                provider_account_generation, durability_class, lifecycle_state,
                retention_deadline_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'creating', $12)
            RETURNING {WORKSPACE_COLUMNS}
            "#,
        ))
        .bind(request.workspace_id)
        .bind(request.tenant_id)
        .bind(scope_kind)
        .bind(session_id)
        .bind(worker_id)
        .bind(run_id)
        .bind(task_id)
        .bind(&request.provider)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.durability_class.as_str())
        .bind(request.retention_deadline_at)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        sqlx::query_scalar::<_, Uuid>(
            "SELECT moa.reserve_sandbox_workspace_capacity($1, $2, $3, $4, 0)",
        )
        .bind(request.tenant_id)
        .bind(request.workspace_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        workspace_from_row(&row)
    }

    /// Loads one exact tenant-owned workspace.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<Option<SandboxWorkspace>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM moa.sandbox_workspaces WHERE tenant_id = $1 AND workspace_id = $2"
        ))
        .bind(tenant_id)
        .bind(workspace_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let workspace = row.as_ref().map(workspace_from_row).transpose()?;
        conn.commit().await?;
        Ok(workspace)
    }

    /// Resolves the one live workspace owned by an exact typed execution scope.
    pub async fn get_by_scope(
        &self,
        tenant_id: TenantId,
        scope: &SandboxWorkspaceScope,
    ) -> Result<Option<SandboxWorkspace>> {
        let (scope_kind, session_id, worker_id, run_id, task_id) = scope_columns(scope)?;
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {WORKSPACE_COLUMNS}
            FROM moa.sandbox_workspaces
            WHERE tenant_id = $1 AND scope_kind = $2
              AND scope_session_id IS NOT DISTINCT FROM $3
              AND scope_worker_id IS NOT DISTINCT FROM $4
              AND scope_run_id IS NOT DISTINCT FROM $5
              AND scope_task_id IS NOT DISTINCT FROM $6
              AND lifecycle_state <> 'deleted'
            "#,
        ))
        .bind(tenant_id)
        .bind(scope_kind)
        .bind(session_id)
        .bind(worker_id)
        .bind(run_id)
        .bind(task_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let workspace = row.as_ref().map(workspace_from_row).transpose()?;
        conn.commit().await?;
        Ok(workspace)
    }

    /// Lists tenant rows whose logical IDs were authorized by OpenFGA first.
    pub async fn list_authorized(
        &self,
        tenant_id: TenantId,
        workspace_ids: &[SandboxWorkspaceId],
    ) -> Result<Vec<SandboxWorkspace>> {
        if workspace_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.begin(tenant_id).await?;
        let rows = sqlx::query(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM moa.sandbox_workspaces \
             WHERE tenant_id = $1 AND workspace_id = ANY($2) \
               AND access_fenced_at IS NULL AND lifecycle_state NOT IN ('deleting', 'deleted') \
             ORDER BY created_at, workspace_id"
        ))
        .bind(tenant_id)
        .bind(workspace_ids)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let workspaces = rows
            .iter()
            .map(workspace_from_row)
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(workspaces)
    }
}
