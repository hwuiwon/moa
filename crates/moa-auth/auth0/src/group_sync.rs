//! Additive OIDC group-to-FGA tuple synchronization.
//!
//! Group names use the convention documented in
//! `docs/operations/oidc-group-mapping.md`. P1.7 only enqueues write tuples;
//! full delete reconciliation belongs with SCIM deactivation in P1.9.

use async_trait::async_trait;
use moa_authz::outbox::enqueue_raw;
use moa_authz_schema::TupleOp;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::time::interval;
use uuid::Uuid;

const GROUP_SYNC_BATCH_SIZE: i64 = 1000;

/// Reads group membership names for one external identity provider subject.
#[async_trait]
pub trait IdpGroupReader: Send + Sync {
    /// Return all group names this external subject currently belongs to.
    async fn groups_for(&self, sub: &str) -> Result<Vec<String>, GroupSyncError>;
}

/// Stub Auth0 Management API group reader for deployments without credentials.
pub struct Auth0GroupReader;

#[async_trait]
impl IdpGroupReader for Auth0GroupReader {
    async fn groups_for(&self, _sub: &str) -> Result<Vec<String>, GroupSyncError> {
        Err(GroupSyncError::NotConfigured(
            "Auth0 Management API group reader is not configured".to_string(),
        ))
    }
}

/// Background task that maps IdP group names into FGA tuple writes.
pub struct OidcGroupSync {
    pool: Arc<PgPool>,
    reader: Arc<dyn IdpGroupReader>,
    sync_interval: Duration,
}

impl OidcGroupSync {
    /// Create a group sync task with the default 60 second interval.
    #[must_use]
    pub fn new(pool: Arc<PgPool>, reader: Arc<dyn IdpGroupReader>) -> Self {
        Self {
            pool,
            reader,
            sync_interval: Duration::from_secs(60),
        }
    }

    /// Spawn the background sync loop.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = interval(self.sync_interval);
            loop {
                tick.tick().await;
                if let Err(error) = self.sync_once().await {
                    tracing::error!(error = %error, "group sync failed");
                }
            }
        })
    }

    async fn sync_once(&self) -> Result<(), GroupSyncError> {
        let workspace_tenant_column = workspace_tenant_column(&self.pool).await?;
        if workspace_tenant_column == WorkspaceTenantColumn::Missing {
            tracing::warn!(
                "cannot validate OIDC workspace groups because workspaces.tenant_id is absent"
            );
        }

        let mut cursor = None;
        loop {
            let users = auth0_user_batch(&self.pool, cursor.as_ref()).await?;
            if users.is_empty() {
                break;
            }

            for (user_id, tenant_id, sub) in &users {
                let groups = match self.reader.groups_for(sub).await {
                    Ok(groups) => groups,
                    Err(error) => {
                        tracing::warn!(sub = %sub, error = %error, "groups_for failed; skipping");
                        continue;
                    }
                };
                let mut desired = Vec::new();
                for group in &groups {
                    match validate_group_for_user(
                        &self.pool,
                        group,
                        *tenant_id,
                        workspace_tenant_column,
                    )
                    .await
                    {
                        Ok(Some(tuple)) => desired.push(tuple),
                        Ok(None) => {
                            tracing::debug!(group = %group, user_id = %user_id, "discarded unmappable OIDC group");
                        }
                        Err(error) => {
                            tracing::warn!(
                                group = %group,
                                user_id = %user_id,
                                error = %error,
                                "failed to validate OIDC group; skipping"
                            );
                        }
                    }
                }

                let mut tx = self.pool.begin().await?;
                for tuple in &desired {
                    enqueue_raw(
                        &mut *tx,
                        TupleOp::Write,
                        &format!("user:{user_id}"),
                        &tuple.relation,
                        &format!("{}:{}", tuple.object_type, tuple.object_id),
                        Some(*tenant_id),
                    )
                    .await?;
                }
                tx.commit().await?;
            }

            cursor = users
                .last()
                .map(|(_, tenant_id, sub)| (sub.clone(), *tenant_id));
            if users.len() < GROUP_SYNC_BATCH_SIZE as usize {
                break;
            }
        }

        Ok(())
    }
}

/// Group synchronization failures.
#[derive(Debug, Error)]
pub enum GroupSyncError {
    /// No IdP group reader has been configured.
    #[error("group reader not configured: {0}")]
    NotConfigured(String),
    /// Database access failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Enqueuing an FGA tuple failed.
    #[error("authz outbox error: {0}")]
    Outbox(#[from] moa_authz::AuthzError),
}

struct DesiredTuple {
    tenant_id: Uuid,
    object_type: String,
    object_id: Uuid,
    relation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTenantColumn {
    Missing,
    Text,
    Uuid,
}

async fn auth0_user_batch(
    pool: &PgPool,
    cursor: Option<&(String, Uuid)>,
) -> Result<Vec<(Uuid, Uuid, String)>, GroupSyncError> {
    match cursor {
        Some((last_sub, last_tenant_id)) => sqlx::query_as(
            r#"
            SELECT user_id, tenant_id, sub
            FROM auth0_user_map
            WHERE (sub, tenant_id) > ($1, $2)
            ORDER BY sub ASC, tenant_id ASC
            LIMIT $3
            "#,
        )
        .bind(last_sub)
        .bind(last_tenant_id)
        .bind(GROUP_SYNC_BATCH_SIZE)
        .fetch_all(pool)
        .await
        .map_err(GroupSyncError::from),
        None => sqlx::query_as(
            r#"
            SELECT user_id, tenant_id, sub
            FROM auth0_user_map
            ORDER BY sub ASC, tenant_id ASC
            LIMIT $1
            "#,
        )
        .bind(GROUP_SYNC_BATCH_SIZE)
        .fetch_all(pool)
        .await
        .map_err(GroupSyncError::from),
    }
}

fn parse_group(group: &str) -> Option<DesiredTuple> {
    let parts: Vec<&str> = group.split(':').collect();
    match parts.as_slice() {
        ["tenant", tenant, "workspace", workspace, relation] => Some(DesiredTuple {
            tenant_id: Uuid::parse_str(tenant).ok()?,
            object_type: "workspace".to_string(),
            object_id: Uuid::parse_str(workspace).ok()?,
            relation: (*relation).to_string(),
        }),
        ["tenant", tenant, relation] => Some(DesiredTuple {
            tenant_id: Uuid::parse_str(tenant).ok()?,
            object_type: "tenant".to_string(),
            object_id: Uuid::parse_str(tenant).ok()?,
            relation: (*relation).to_string(),
        }),
        _ => None,
    }
}

async fn validate_group_for_user(
    pool: &PgPool,
    group: &str,
    user_tenant_id: Uuid,
    workspace_tenant_column: WorkspaceTenantColumn,
) -> Result<Option<DesiredTuple>, GroupSyncError> {
    let Some(tuple) = prevalidate_group_for_user(group, user_tenant_id) else {
        return Ok(None);
    };
    if tuple.object_type == "workspace"
        && !workspace_belongs_to_tenant(
            pool,
            tuple.object_id,
            tuple.tenant_id,
            workspace_tenant_column,
        )
        .await?
    {
        return Ok(None);
    }

    Ok(Some(tuple))
}

fn prevalidate_group_for_user(group: &str, user_tenant_id: Uuid) -> Option<DesiredTuple> {
    let tuple = parse_group(group)?;
    if tuple.tenant_id != user_tenant_id {
        return None;
    }
    if !allowed_group_relation(&tuple.object_type, &tuple.relation) {
        return None;
    }
    Some(tuple)
}

fn allowed_group_relation(object_type: &str, relation: &str) -> bool {
    matches!(
        (object_type, relation),
        ("tenant", "admin" | "billing_admin" | "member")
            | ("workspace", "admin" | "editor" | "member")
    )
}

async fn workspace_tenant_column(pool: &PgPool) -> Result<WorkspaceTenantColumn, GroupSyncError> {
    let data_type = sqlx::query_scalar::<_, Option<String>>(
        r#"
        SELECT data_type
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'workspaces'
          AND column_name = 'tenant_id'
        LIMIT 1
        "#,
    )
    .fetch_one(pool)
    .await?;

    Ok(match data_type.as_deref() {
        Some("uuid") => WorkspaceTenantColumn::Uuid,
        Some(_) => WorkspaceTenantColumn::Text,
        None => WorkspaceTenantColumn::Missing,
    })
}

async fn workspace_belongs_to_tenant(
    pool: &PgPool,
    workspace_id: Uuid,
    tenant_id: Uuid,
    tenant_column: WorkspaceTenantColumn,
) -> Result<bool, GroupSyncError> {
    match tenant_column {
        WorkspaceTenantColumn::Missing => Ok(false),
        WorkspaceTenantColumn::Uuid => sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM workspaces
                WHERE id = $1
                  AND tenant_id = $2
            )
            "#,
        )
        .bind(workspace_id.to_string())
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .map_err(GroupSyncError::from),
        WorkspaceTenantColumn::Text => sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM workspaces
                WHERE id = $1
                  AND tenant_id = $2
            )
            "#,
        )
        .bind(workspace_id.to_string())
        .bind(tenant_id.to_string())
        .fetch_one(pool)
        .await
        .map_err(GroupSyncError::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_group_maps_workspace_relation() {
        let workspace_id = Uuid::from_u128(7);
        let tuple = parse_group(&format!(
            "tenant:{}:workspace:{workspace_id}:admin",
            Uuid::from_u128(1)
        ))
        .expect("group should parse");
        assert_eq!(tuple.tenant_id, Uuid::from_u128(1));
        assert_eq!(tuple.object_type, "workspace");
        assert_eq!(tuple.object_id, workspace_id);
        assert_eq!(tuple.relation, "admin");
    }

    #[test]
    fn relation_allowlist_rejects_unknown_group_relations() {
        assert!(allowed_group_relation("tenant", "admin"));
        assert!(allowed_group_relation("workspace", "editor"));
        assert!(!allowed_group_relation("tenant", "owner"));
        assert!(!allowed_group_relation("workspace", "tenant"));
        assert!(!allowed_group_relation("session", "participant"));
    }

    #[test]
    fn parsed_group_carries_tenant_for_cross_tenant_validation() {
        let tenant_id = Uuid::from_u128(1);
        let other_tenant_id = Uuid::from_u128(2);
        let tuple = parse_group(&format!("tenant:{other_tenant_id}:admin"))
            .expect("tenant group should parse");

        assert_ne!(tuple.tenant_id, tenant_id);
        assert_eq!(tuple.object_id, other_tenant_id);
        assert_eq!(tuple.relation, "admin");
        assert!(
            prevalidate_group_for_user(&format!("tenant:{other_tenant_id}:admin"), tenant_id)
                .is_none(),
            "cross-tenant OIDC groups must not produce FGA tuples"
        );
    }

    #[test]
    fn prevalidation_rejects_unknown_relation_before_fga_write() {
        let tenant_id = Uuid::from_u128(1);

        assert!(
            prevalidate_group_for_user(&format!("tenant:{tenant_id}:owner"), tenant_id).is_none()
        );
        assert!(
            prevalidate_group_for_user(
                &format!("tenant:{tenant_id}:workspace:{}:tenant", Uuid::from_u128(9)),
                tenant_id,
            )
            .is_none()
        );
    }
}
