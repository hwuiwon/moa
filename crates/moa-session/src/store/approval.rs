//! Approval-rule operations for the Postgres session store.

use super::*;

impl PostgresSessionStore {
    /// Lists approval rules visible to the provided workspace.
    pub async fn list_approval_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ApprovalRule>> {
        let approval_rules = self.table_name("approval_rules");
        let rows = sqlx::query(&format!(
            "SELECT id, workspace_id, tool, pattern, action, scope, created_by, created_at \
             FROM {approval_rules} WHERE workspace_id = $1 OR scope = 'global' \
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(approval_rule_from_row).collect()
    }

    /// Creates or updates an approval rule.
    pub async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        let approval_rules = self.table_name("approval_rules");
        sqlx::query(&format!(
            "INSERT INTO {approval_rules} (id, workspace_id, tool, pattern, action, scope, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (workspace_id, tool, pattern) DO UPDATE SET \
                 action = EXCLUDED.action, \
                 scope = EXCLUDED.scope, \
                 created_by = EXCLUDED.created_by, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(rule.id)
        .bind(rule.workspace_id.to_string())
        .bind(rule.tool)
        .bind(rule.pattern)
        .bind(rule.action.as_str())
        .bind(rule.scope.as_str())
        .bind(rule.created_by.to_string())
        .bind(rule.created_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Deletes an approval rule by tool and pattern within a workspace.
    pub async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        let approval_rules = self.table_name("approval_rules");
        sqlx::query(&format!(
            "DELETE FROM {approval_rules} WHERE workspace_id = $1 AND tool = $2 AND pattern = $3"
        ))
        .bind(workspace_id.to_string())
        .bind(tool)
        .bind(pattern)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }
}

#[async_trait]
impl ApprovalRuleStore for PostgresSessionStore {
    /// Lists approval rules visible to a workspace.
    async fn list_approval_rules(&self, workspace_id: &WorkspaceId) -> Result<Vec<ApprovalRule>> {
        self.list_approval_rules(workspace_id).await
    }

    /// Creates or updates an approval rule.
    async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
        self.upsert_approval_rule(rule).await
    }

    /// Deletes an approval rule by tool and pattern.
    async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.delete_approval_rule(workspace_id, tool, pattern).await
    }
}
