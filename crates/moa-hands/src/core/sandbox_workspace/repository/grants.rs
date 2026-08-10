//! Workspace grant-ledger creation, reconciliation, and reads.

use super::*;

impl PostgresWorkspaceRepository {
    /// Creates one workspace and its exact desired grant ledger atomically.
    pub async fn create_with_grants_in_transaction(
        conn: &mut PgConnection,
        request: &CreateWorkspaceRequest,
        grants: &[WorkspaceGrant],
    ) -> Result<SandboxWorkspace> {
        let workspace = Self::create_in_transaction(conn, request).await?;
        Self::reconcile_grants_in_transaction(
            conn,
            request.tenant_id,
            request.workspace_id,
            workspace.delete_generation,
            "present",
            grants,
        )
        .await?;
        Ok(workspace)
    }

    /// Reconciles desired grants and inverse tuple intents in one transaction.
    pub(super) async fn reconcile_grants_in_transaction(
        conn: &mut PgConnection,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        delete_generation: i64,
        desired_state: &'static str,
        grants: &[WorkspaceGrant],
    ) -> Result<()> {
        for grant in grants {
            sqlx::query(
                r#"
                INSERT INTO moa.sandbox_workspace_grants (
                    grant_id, tenant_id, workspace_id, subject_type, subject_id,
                    subject_relation, object_type, object_id, relation,
                    desired_state, tuple_generation, outbox_state,
                    workspace_delete_generation
                ) VALUES ($1, $2, $3, $4, $5, $6, 'sandbox_workspace', $3, $7, $8, 1, 'pending', $9)
                ON CONFLICT (
                    tenant_id, workspace_id, subject_type, subject_id,
                    subject_relation, object_type, object_id, relation
                ) DO UPDATE SET
                    desired_state = EXCLUDED.desired_state,
                    tuple_generation = moa.sandbox_workspace_grants.tuple_generation + 1,
                    outbox_state = 'pending',
                    workspace_delete_generation = EXCLUDED.workspace_delete_generation,
                    updated_at = now()
                WHERE moa.sandbox_workspace_grants.desired_state IS DISTINCT FROM EXCLUDED.desired_state
                   OR moa.sandbox_workspace_grants.outbox_state = 'dead_letter'
                "#,
            )
            .bind(grant.grant_id)
            .bind(tenant_id)
            .bind(workspace_id)
            .bind(grant.subject_type.as_str())
            .bind(grant.subject_id)
            .bind(grant.subject_relation.as_deref())
            .bind(grant.relation.as_str())
            .bind(desired_state)
            .bind(delete_generation)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }

    /// Loads the exact grant ledger for one workspace in the current transaction.
    pub(super) async fn load_grants_in_transaction(
        conn: &mut PgConnection,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<Vec<WorkspaceGrant>> {
        let rows = sqlx::query(
            r#"
            SELECT grant_id, subject_type, subject_id, subject_relation, relation
            FROM moa.sandbox_workspace_grants
            WHERE tenant_id = $1 AND workspace_id = $2
            ORDER BY grant_id
            "#,
        )
        .bind(tenant_id)
        .bind(workspace_id)
        .fetch_all(conn)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(grant_from_row).collect()
    }
}
fn grant_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkspaceGrant> {
    Ok(WorkspaceGrant {
        grant_id: row.try_get("grant_id").map_err(map_sqlx_error)?,
        subject_type: WorkspaceGrantSubjectType::from_label(
            &row.try_get::<String, _>("subject_type")
                .map_err(map_sqlx_error)?,
        )?,
        subject_id: row.try_get("subject_id").map_err(map_sqlx_error)?,
        subject_relation: row.try_get("subject_relation").map_err(map_sqlx_error)?,
        relation: WorkspaceGrantRelation::from_label(
            &row.try_get::<String, _>("relation")
                .map_err(map_sqlx_error)?,
        )?,
    })
}
