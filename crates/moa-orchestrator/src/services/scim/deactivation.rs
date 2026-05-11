//! Atomic user deactivation cascade for SCIM deprovisioning.

use moa_auth_providers::api_keys;
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use sqlx::{Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

/// Summary of a deactivation cascade.
#[derive(Debug, Clone, Default)]
pub struct CascadeSummary {
    /// Whether this call changed an active user to inactive.
    pub deactivated: bool,
    /// Number of sessions cancelled.
    pub sessions_cancelled: u64,
    /// Number of API keys revoked.
    pub api_keys_revoked: usize,
    /// Number of agent operator edges orphaned.
    pub agents_orphaned: usize,
}

/// Cascade failures.
#[derive(Debug, Error)]
pub enum CascadeError {
    /// Database failure.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    /// Authorization outbox failure.
    #[error("authz outbox: {0}")]
    Authz(#[from] moa_authz::AuthzError),
    /// API-key revocation failure.
    #[error("api key revocation: {0}")]
    ApiKey(#[from] api_keys::ApiKeyError),
}

/// Deactivate a user and tear down all local access in one transaction.
pub async fn cascade_deactivate_user(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<CascadeSummary, CascadeError> {
    let result = sqlx::query(
        r#"
        UPDATE users
        SET active = false,
            deactivated_at = COALESCE(deactivated_at, NOW()),
            updated_at = NOW(),
            version = version + 1
        WHERE id = $1 AND tenant_id = $2 AND active = true
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;

    if result.rows_affected() == 0 {
        return Ok(CascadeSummary::default());
    }

    let user_wire = format!("user:{user_id}");
    let mut summary = CascadeSummary {
        deactivated: true,
        ..CascadeSummary::default()
    };

    let active_sessions: Vec<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT id
        FROM sessions
        WHERE user_id = $1
          AND status NOT IN ('cancelled', 'completed', 'failed')
        "#,
    )
    .bind(user_id.to_string())
    .fetch_all(&mut **tx)
    .await?;

    let session_update = sqlx::query(
        r#"
        UPDATE sessions
        SET status = 'cancelled',
            updated_at = NOW(),
            completed_at = COALESCE(completed_at, NOW())
        WHERE user_id = $1
          AND status NOT IN ('cancelled', 'completed', 'failed')
        "#,
    )
    .bind(user_id.to_string())
    .execute(&mut **tx)
    .await?;
    summary.sessions_cancelled = session_update.rows_affected();

    for (session_id,) in active_sessions {
        for relation in ["owner", "participant"] {
            enqueue_raw(
                &mut **tx,
                TupleOp::Delete,
                &user_wire,
                relation,
                &format!("session:{session_id}"),
                Some(tenant_id),
            )
            .await?;
        }
    }

    revoke_user_api_keys(tx, tenant_id, user_id)
        .await
        .map(|count| {
            summary.api_keys_revoked = count;
        })?;
    enqueue_direct_user_tuple_deletes(tx, tenant_id, user_id).await?;
    summary.agents_orphaned = orphan_user_agents(tx, tenant_id, user_id).await?;
    delete_group_memberships(tx, user_id).await?;

    // TODO P1.10: emit OCSF iam.user.disable with `summary`.

    Ok(summary)
}

async fn revoke_user_api_keys(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<usize, CascadeError> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM api_keys WHERE owner_user_id = $1 AND revoked_at IS NULL")
            .bind(user_id)
            .fetch_all(&mut **tx)
            .await?;

    for (key_id,) in &rows {
        api_keys::revoke(&mut **tx, *key_id, "deactivation_cascade", None).await?;
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &format!("user:{user_id}"),
            "owner",
            &format!("api_key:{key_id}"),
            Some(tenant_id),
        )
        .await?;
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &format!("tenant:{tenant_id}"),
            "tenant",
            &format!("api_key:{key_id}"),
            Some(tenant_id),
        )
        .await?;
        for relation in ["member", "admin", "scim_admin"] {
            enqueue_raw(
                &mut **tx,
                TupleOp::Delete,
                &format!("api_key:{key_id}"),
                relation,
                &format!("tenant:{tenant_id}"),
                Some(tenant_id),
            )
            .await?;
        }
    }

    Ok(rows.len())
}

async fn enqueue_direct_user_tuple_deletes(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), CascadeError> {
    let user_wire = format!("user:{user_id}");

    for relation in ["member", "admin", "billing_admin"] {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &user_wire,
            relation,
            &format!("tenant:{tenant_id}"),
            Some(tenant_id),
        )
        .await?;
    }

    if table_exists(tx, "workspaces").await? {
        let workspaces: Vec<(String,)> = sqlx::query_as("SELECT id::TEXT FROM workspaces")
            .fetch_all(&mut **tx)
            .await?;
        for (workspace_id,) in workspaces {
            for relation in ["member", "editor", "admin"] {
                enqueue_raw(
                    &mut **tx,
                    TupleOp::Delete,
                    &user_wire,
                    relation,
                    &format!("workspace:{workspace_id}"),
                    Some(tenant_id),
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn orphan_user_agents(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<usize, CascadeError> {
    if !table_exists(tx, "agents").await? {
        return Ok(0);
    }
    let agents: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM agents WHERE operator_user_id = $1")
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await?;
    for (agent_id,) in &agents {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &format!("user:{user_id}"),
            "operator",
            &format!("agent:{agent_id}"),
            Some(tenant_id),
        )
        .await?;
        sqlx::query("UPDATE agents SET operator_user_id = NULL WHERE id = $1")
            .bind(agent_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(agents.len())
}

async fn delete_group_memberships(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(), CascadeError> {
    sqlx::query("DELETE FROM scim_group_members WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;

    if table_exists(tx, "oidc_user_groups").await? {
        sqlx::query("DELETE FROM oidc_user_groups WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn table_exists(
    tx: &mut Transaction<'_, Postgres>,
    table_name: &str,
) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("public.{table_name}"))
        .fetch_one(&mut **tx)
        .await?;
    Ok(exists)
}
