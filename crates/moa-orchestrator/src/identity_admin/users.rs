//! User provisioning persistence and deactivation cascade logic.

use chrono::{DateTime, Utc};
use moa_auth_providers::api_keys;
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use moa_ocsf::ActorInput;
use sqlx::{Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::services::scim::{ScimResponseError, map_db};

/// SCIM user list filter understood by the user repository.
#[derive(Debug)]
pub(crate) enum UserFilter {
    /// Match a user's primary email or SCIM userName.
    Email(String),
    /// Match a user's external identity-provider id.
    ExternalId(String),
}

/// User row data used by SCIM response assembly.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct UserRow {
    /// User UUID.
    pub(crate) id: Uuid,
    /// SCIM external id.
    pub(crate) external_id: Option<String>,
    /// Primary email.
    pub(crate) email: String,
    /// Given name.
    pub(crate) given_name: Option<String>,
    /// Family name.
    pub(crate) family_name: Option<String>,
    /// Display name.
    pub(crate) display_name: Option<String>,
    /// Active flag.
    pub(crate) active: bool,
    /// Creation timestamp.
    pub(crate) created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub(crate) updated_at: DateTime<Utc>,
    /// Optimistic version used for SCIM ETags.
    pub(crate) version: i64,
}

/// User data accepted by SCIM create and replace operations.
#[derive(Debug, Clone)]
pub(crate) struct UserWrite {
    /// Primary email.
    pub(crate) email: String,
    /// SCIM external id.
    pub(crate) external_id: Option<String>,
    /// Given name.
    pub(crate) given_name: Option<String>,
    /// Family name.
    pub(crate) family_name: Option<String>,
    /// Display name.
    pub(crate) display_name: Option<String>,
    /// Desired active flag.
    pub(crate) active: bool,
}

/// Partial user field mutation accepted by SCIM PATCH.
#[derive(Debug, Clone)]
pub(crate) struct UserPatch {
    /// Optional primary email replacement.
    pub(crate) email: Option<String>,
    /// Optional display name replacement.
    pub(crate) display_name: Option<String>,
    /// Optional given name replacement.
    pub(crate) given_name: Option<String>,
    /// Optional family name replacement.
    pub(crate) family_name: Option<String>,
    /// Optional active flag replacement.
    pub(crate) active: Option<bool>,
}

/// Summary of a deactivation cascade.
#[derive(Debug, Clone, Default)]
pub struct CascadeSummary {
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

/// Fetch one page of users for a tenant.
pub(crate) async fn fetch_users_page(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    filter: Option<&UserFilter>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<UserRow>), sqlx::Error> {
    let offset = start.saturating_sub(1);
    match filter {
        None => {
            let total = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(pool)
                .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
                WHERE tenant_id = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(tenant_id)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            Ok((total, rows))
        }
        Some(UserFilter::Email(email)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM users WHERE tenant_id = $1 AND lower(email) = lower($2)",
            )
            .bind(tenant_id)
            .bind(email)
            .fetch_one(pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
                WHERE tenant_id = $1 AND lower(email) = lower($2)
                ORDER BY created_at DESC
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(email)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            Ok((total, rows))
        }
        Some(UserFilter::ExternalId(external_id)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM users WHERE tenant_id = $1 AND external_id = $2",
            )
            .bind(tenant_id)
            .bind(external_id)
            .fetch_one(pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
                WHERE tenant_id = $1 AND external_id = $2
                ORDER BY created_at DESC
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(external_id)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            Ok((total, rows))
        }
    }
}

/// Fetch one user by id within a tenant.
pub(crate) async fn fetch_user_by_id(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<Option<UserRow>, ScimResponseError> {
    sqlx::query_as(
        r#"
        SELECT id, external_id, email, given_name, family_name, display_name,
               active, created_at, updated_at, version
        FROM users
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(map_db)
}

/// Create a SCIM user and matching authorization/audit records atomically.
pub(crate) async fn create_user(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    actor: ActorInput,
    user: UserWrite,
) -> Result<Uuid, ScimResponseError> {
    let user_id = Uuid::new_v4();
    let mut tx = pool.begin().await.map_err(map_db)?;

    sqlx::query(
        r#"
        INSERT INTO users
            (id, tenant_id, email, external_id, given_name, family_name, display_name, active)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(&user.email)
    .bind(user.external_id.as_deref())
    .bind(user.given_name.as_deref())
    .bind(user.family_name.as_deref())
    .bind(user.display_name.as_deref())
    .bind(user.active)
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    moa_ocsf::emit_scim_user_created_tx(&mut tx, tenant_id, actor, user_id)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    Ok(user_id)
}

/// Replace a SCIM user and preserve deactivation cascade semantics.
pub(crate) async fn replace_user(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
    actor: ActorInput,
    user: UserWrite,
) -> Result<(), ScimResponseError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, tenant_id, user_id).await?;

    if user.active {
        sqlx::query(
            r#"
            UPDATE users
            SET email = $3,
                external_id = $4,
                given_name = $5,
                family_name = $6,
                display_name = $7,
                active = true,
                deactivated_at = NULL,
                updated_at = NOW(),
                version = version + 1
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(&user.email)
        .bind(user.external_id.as_deref())
        .bind(user.given_name.as_deref())
        .bind(user.family_name.as_deref())
        .bind(user.display_name.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
    } else {
        cascade_deactivate_user(&mut tx, tenant_id, user_id, actor.clone())
            .await
            .map_err(map_cascade)?;
        apply_user_mutation(
            &mut tx,
            tenant_id,
            user_id,
            UserPatch {
                email: Some(user.email),
                display_name: user.display_name,
                given_name: user.given_name,
                family_name: user.family_name,
                active: None,
            },
        )
        .await?;
    }

    moa_ocsf::emit_scim_user_updated_tx(&mut tx, tenant_id, actor, user_id)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)
}

/// Patch a SCIM user and preserve deactivation cascade semantics.
pub(crate) async fn patch_user(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
    actor: ActorInput,
    mutation: UserPatch,
) -> Result<(), ScimResponseError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, tenant_id, user_id).await?;

    match mutation.active {
        Some(false) => {
            cascade_deactivate_user(&mut tx, tenant_id, user_id, actor.clone())
                .await
                .map_err(map_cascade)?;
        }
        Some(true) => {
            sqlx::query(
                r#"
                UPDATE users
                SET active = true,
                    deactivated_at = NULL,
                    updated_at = NOW(),
                    version = version + 1
                WHERE id = $1 AND tenant_id = $2
                "#,
            )
            .bind(user_id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(map_db)?;
        }
        None => {}
    }
    apply_user_mutation(&mut tx, tenant_id, user_id, mutation).await?;

    moa_ocsf::emit_scim_user_updated_tx(&mut tx, tenant_id, actor, user_id)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)
}

/// Delete a SCIM user and all local access records in one transaction.
pub(crate) async fn delete_user(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, tenant_id, user_id).await?;
    cascade_deactivate_user(&mut tx, tenant_id, user_id, actor.clone())
        .await
        .map_err(map_cascade)?;
    sqlx::query("DELETE FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    moa_ocsf::emit_scim_user_deleted_tx(&mut tx, tenant_id, actor, user_id)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)
}

/// Deactivate a user and tear down all local access in one transaction.
pub async fn cascade_deactivate_user(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    actor: ActorInput,
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
    let mut summary = CascadeSummary::default();

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

    revoke_user_api_keys(tx, tenant_id, user_id, actor.clone())
        .await
        .map(|count| {
            summary.api_keys_revoked = count;
        })?;
    enqueue_direct_user_tuple_deletes(tx, tenant_id, user_id).await?;
    summary.agents_orphaned = orphan_user_agents(tx, tenant_id, user_id).await?;
    delete_group_memberships(tx, user_id).await?;

    moa_ocsf::emit_user_deactivated_tx(tx, tenant_id, actor, user_id)
        .await
        .map_err(|error| sqlx::Error::Protocol(format!("audit user deactivate: {error}")))?;

    Ok(summary)
}

async fn ensure_user_exists(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), ScimResponseError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND tenant_id = $2)")
            .bind(user_id)
            .bind(tenant_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_db)?;
    if exists {
        Ok(())
    } else {
        Err(ScimResponseError::not_found("user not found"))
    }
}

async fn apply_user_mutation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    mutation: UserPatch,
) -> Result<(), ScimResponseError> {
    if mutation.email.is_none()
        && mutation.display_name.is_none()
        && mutation.given_name.is_none()
        && mutation.family_name.is_none()
    {
        return Ok(());
    }
    sqlx::query(
        r#"
        UPDATE users
        SET email = COALESCE($3, email),
            display_name = COALESCE($4, display_name),
            given_name = COALESCE($5, given_name),
            family_name = COALESCE($6, family_name),
            updated_at = NOW(),
            version = version + 1
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(mutation.email)
    .bind(mutation.display_name)
    .bind(mutation.given_name)
    .bind(mutation.family_name)
    .execute(&mut **tx)
    .await
    .map_err(map_db)?;
    Ok(())
}

async fn revoke_user_api_keys(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    actor: ActorInput,
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
        for relation in ["admin", "operator"] {
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
        moa_ocsf::emit_api_key_revoked_tx(
            tx,
            tenant_id,
            actor.clone(),
            *key_id,
            Some("deactivation_cascade"),
        )
        .await
        .map_err(|error| sqlx::Error::Protocol(format!("audit api key revoke: {error}")))?;
    }

    Ok(rows.len())
}

async fn enqueue_direct_user_tuple_deletes(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), CascadeError> {
    let user_wire = format!("user:{user_id}");

    for relation in ["admin", "operator"] {
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

fn map_cascade(error: CascadeError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM deactivation cascade failed");
    ScimResponseError::internal("deactivation cascade failed")
}

fn map_audit(error: moa_ocsf::EmitError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM security audit failed");
    ScimResponseError::internal("security audit failed")
}
