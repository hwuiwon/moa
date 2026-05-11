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
        let users: Vec<(Uuid, Uuid, String)> =
            sqlx::query_as("SELECT user_id, tenant_id, sub FROM auth0_user_map LIMIT 1000")
                .fetch_all(&*self.pool)
                .await?;

        for (user_id, tenant_id, sub) in users {
            let groups = match self.reader.groups_for(&sub).await {
                Ok(groups) => groups,
                Err(error) => {
                    tracing::warn!(sub = %sub, error = %error, "groups_for failed; skipping");
                    continue;
                }
            };
            let desired = groups
                .iter()
                .filter_map(|group| parse_group(group))
                .collect::<Vec<_>>();

            let mut tx = self.pool.begin().await?;
            for tuple in &desired {
                enqueue_raw(
                    &mut *tx,
                    TupleOp::Write,
                    &format!("user:{user_id}"),
                    &tuple.relation,
                    &format!("{}:{}", tuple.object_type, tuple.object_id),
                    Some(tenant_id),
                )
                .await?;
            }
            tx.commit().await?;
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
    object_type: String,
    object_id: Uuid,
    relation: String,
}

fn parse_group(group: &str) -> Option<DesiredTuple> {
    let parts: Vec<&str> = group.split(':').collect();
    match parts.as_slice() {
        ["tenant", _tenant, "workspace", workspace, relation] => Some(DesiredTuple {
            object_type: "workspace".to_string(),
            object_id: Uuid::parse_str(workspace).ok()?,
            relation: (*relation).to_string(),
        }),
        ["tenant", tenant, relation] => Some(DesiredTuple {
            object_type: "tenant".to_string(),
            object_id: Uuid::parse_str(tenant).ok()?,
            relation: (*relation).to_string(),
        }),
        _ => None,
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
        assert_eq!(tuple.object_type, "workspace");
        assert_eq!(tuple.object_id, workspace_id);
        assert_eq!(tuple.relation, "admin");
    }
}
