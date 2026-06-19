//! Action-policy rule operations for the Postgres session store.

use moa_core::{ActionRuleScope, UserId, WorkspaceId};
use moa_security::GLOBAL_ACTION_POLICY_WORKSPACE_ID;

use super::*;

impl PostgresSessionStore {
    /// Lists action-policy rules visible to one workspace user and tool.
    pub async fn list_action_policy_rules_for_tool(
        &self,
        workspace_id: &WorkspaceId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        let action_policy_rules = self.table_name("action_policy_rules");
        let rows = sqlx::query(&format!(
            "SELECT id, workspace_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at \
             FROM {action_policy_rules} \
             WHERE (workspace_id = $1 OR (workspace_id = $2 AND scope = 'global')) \
               AND (user_id IS NULL OR user_id = $3) \
               AND tool = $4 \
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.to_string())
        .bind(GLOBAL_ACTION_POLICY_WORKSPACE_ID)
        .bind(user_id.to_string())
        .bind(tool)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(action_policy_rule_from_row).collect()
    }

    /// Creates or updates an action-policy rule.
    pub async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        let action_policy_rules = self.table_name("action_policy_rules");
        let workspace_id = stored_workspace_id_for_rule(&rule);
        sqlx::query(&format!(
            "INSERT INTO {action_policy_rules} (id, workspace_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (workspace_id, tool, pattern, (COALESCE(user_id, ''))) DO UPDATE SET \
                 effect = EXCLUDED.effect, \
                 scope = EXCLUDED.scope, \
                 reason = EXCLUDED.reason, \
                 created_by = EXCLUDED.created_by, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(rule.id)
        .bind(workspace_id.to_string())
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
        user_id: Option<&UserId>,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        let action_policy_rules = self.table_name("action_policy_rules");
        sqlx::query(&format!(
            "DELETE FROM {action_policy_rules} \
             WHERE workspace_id = $1 \
               AND ((user_id IS NULL AND $2::text IS NULL) OR user_id = $2) \
               AND tool = $3 \
               AND pattern = $4"
        ))
        .bind(workspace_id.to_string())
        .bind(user_id.map(ToString::to_string))
        .bind(tool)
        .bind(pattern)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }
}

fn stored_workspace_id_for_rule(rule: &ActionPolicyRule) -> WorkspaceId {
    if matches!(rule.scope, ActionRuleScope::Global) {
        WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID)
    } else {
        rule.workspace_id.clone()
    }
}

#[async_trait]
impl ActionPolicyRuleStore for PostgresSessionStore {
    /// Lists action-policy rules visible to one workspace user and tool.
    async fn list_action_policy_rules_for_tool(
        &self,
        workspace_id: &WorkspaceId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        self.list_action_policy_rules_for_tool(workspace_id, user_id, tool)
            .await
    }

    /// Creates or updates an action-policy rule.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        self.upsert_action_policy_rule(rule).await
    }

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        workspace_id: &WorkspaceId,
        user_id: Option<&UserId>,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.delete_action_policy_rule(workspace_id, user_id, tool, pattern)
            .await
    }
}
