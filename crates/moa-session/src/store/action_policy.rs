//! Action-policy rule operations for the Postgres session store.

use super::*;

impl PostgresSessionStore {
    /// Lists action-policy rules visible to the provided workspace.
    pub async fn list_action_policy_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ActionPolicyRule>> {
        let action_policy_rules = self.table_name("action_policy_rules");
        let rows = sqlx::query(&format!(
            "SELECT id, workspace_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at \
             FROM {action_policy_rules} WHERE workspace_id = $1 OR scope = 'global' \
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(action_policy_rule_from_row).collect()
    }

    /// Creates or updates an action-policy rule.
    pub async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        let action_policy_rules = self.table_name("action_policy_rules");
        sqlx::query(&format!(
            "INSERT INTO {action_policy_rules} (id, workspace_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (workspace_id, tool, pattern) DO UPDATE SET \
                 user_id = EXCLUDED.user_id, \
                 effect = EXCLUDED.effect, \
                 scope = EXCLUDED.scope, \
                 reason = EXCLUDED.reason, \
                 created_by = EXCLUDED.created_by, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(rule.id)
        .bind(rule.workspace_id.to_string())
        .bind(rule.user_id.as_ref().map(ToString::to_string))
        .bind(rule.tool)
        .bind(rule.pattern)
        .bind(rule.effect.as_str())
        .bind(rule.scope.as_str())
        .bind(rule.reason)
        .bind(rule.created_by.to_string())
        .bind(rule.created_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Deletes an action-policy rule by tool and pattern within a workspace.
    pub async fn delete_action_policy_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        let action_policy_rules = self.table_name("action_policy_rules");
        sqlx::query(&format!(
            "DELETE FROM {action_policy_rules} WHERE workspace_id = $1 AND tool = $2 AND pattern = $3"
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
impl ActionPolicyRuleStore for PostgresSessionStore {
    /// Lists action-policy rules visible to a workspace.
    async fn list_action_policy_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ActionPolicyRule>> {
        self.list_action_policy_rules(workspace_id).await
    }

    /// Creates or updates an action-policy rule.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        self.upsert_action_policy_rule(rule).await
    }

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.delete_action_policy_rule(workspace_id, tool, pattern)
            .await
    }
}
