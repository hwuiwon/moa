//! Action-policy rule operations for the Postgres session store.

use moa_core::{ActionRuleScope, StoragePartitionId, TenantId, UserId};

use super::*;

impl PostgresSessionStore {
    /// Lists action-policy rules visible to one tenant user and tool.
    pub async fn list_action_policy_rules_for_tool(
        &self,
        tenant_id: &TenantId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        let action_policy_rules = self.table_name("action_policy_rules");
        let rows = sqlx::query(&format!(
            "SELECT id, tenant_id, storage_partition_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at \
             FROM {action_policy_rules} \
             WHERE tenant_id = $1 \
               AND ((scope = 'tenant' AND user_id IS NULL) OR (scope = 'contact' AND user_id = $2)) \
               AND tool = $3 \
             ORDER BY CASE WHEN scope = 'contact' THEN 0 ELSE 1 END, created_at ASC"
        ))
        .bind(tenant_id.0)
        .bind(user_id.to_string())
        .bind(tool)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(action_policy_rule_from_row).collect()
    }

    /// Creates or updates an action-policy rule.
    pub async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        moa_security::validate_action_policy_rule(&rule)?;

        let action_policy_rules = self.table_name("action_policy_rules");
        let storage_partition_id = stored_storage_partition_id_for_rule(&rule);
        let tenant_id = stored_tenant_id_for_rule(&rule);
        let user_id = stored_user_id_for_rule(&rule);
        sqlx::query(&format!(
            "INSERT INTO {action_policy_rules} (id, tenant_id, storage_partition_id, user_id, tool, pattern, effect, scope, reason, created_by, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (storage_partition_id, tool, pattern, (COALESCE(user_id, ''))) DO UPDATE SET \
                 tenant_id = EXCLUDED.tenant_id, \
                 effect = EXCLUDED.effect, \
                 scope = EXCLUDED.scope, \
                 reason = EXCLUDED.reason, \
                 created_by = EXCLUDED.created_by, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(rule.id)
        .bind(tenant_id)
        .bind(storage_partition_id.to_string())
        .bind(user_id.as_deref())
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

    /// Deletes an action-policy rule by tool and pattern within a tenant.
    pub async fn delete_action_policy_rule(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&UserId>,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        let action_policy_rules = self.table_name("action_policy_rules");
        sqlx::query(&format!(
            "DELETE FROM {action_policy_rules} \
             WHERE tenant_id = $1 \
               AND ((scope = 'tenant' AND user_id IS NULL AND $2::text IS NULL) \
                    OR (scope = 'contact' AND user_id = $2)) \
               AND tool = $3 \
               AND pattern = $4"
        ))
        .bind(tenant_id.0)
        .bind(user_id.map(ToString::to_string))
        .bind(tool)
        .bind(pattern)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }
}

fn stored_storage_partition_id_for_rule(rule: &ActionPolicyRule) -> StoragePartitionId {
    match rule.scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
            StoragePartitionId::for_tenant(tenant_id)
        }
    }
}

fn stored_tenant_id_for_rule(rule: &ActionPolicyRule) -> uuid::Uuid {
    match rule.scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
            tenant_id.0
        }
    }
}

fn stored_user_id_for_rule(rule: &ActionPolicyRule) -> Option<String> {
    match rule.scope {
        ActionRuleScope::Tenant { .. } => None,
        ActionRuleScope::Contact { contact_id, .. } => Some(contact_id.to_string()),
    }
}

#[async_trait]
impl ActionPolicyRuleStore for PostgresSessionStore {
    /// Lists action-policy rules visible to one tenant user and tool.
    async fn list_action_policy_rules_for_tool(
        &self,
        tenant_id: &TenantId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        self.list_action_policy_rules_for_tool(tenant_id, user_id, tool)
            .await
    }

    /// Creates or updates an action-policy rule.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()> {
        self.upsert_action_policy_rule(rule).await
    }

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&UserId>,
        tool: &str,
        pattern: &str,
    ) -> Result<()> {
        self.delete_action_policy_rule(tenant_id, user_id, tool, pattern)
            .await
    }
}
