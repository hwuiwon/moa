//! Concrete Postgres persistence for tenant accounts.

use chrono::{DateTime, Utc};
use moa_auth_providers::hash_password;
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::routes::auth_accounts::UserResponse;

use super::{TenantResponse, TenantUserRole};

pub(crate) async fn set_user_password(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    password: &str,
) -> Result<(), String> {
    let password = password.to_string();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(hash)) => hash,
        Ok(Err(error)) => return Err(format!("password hash: {error}")),
        Err(error) => return Err(format!("password hash task: {error}")),
    };
    sqlx::query(
        r#"
        INSERT INTO local_user_credentials
            (user_id, tenant_id, password_hash, password_set_at, password_reset_required)
        VALUES ($1, $2, $3, NOW(), FALSE)
        ON CONFLICT (user_id)
        DO UPDATE SET
            tenant_id = EXCLUDED.tenant_id,
            password_hash = EXCLUDED.password_hash,
            password_set_at = NOW(),
            password_reset_required = FALSE,
            updated_at = NOW()
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(hash)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("set password: {error}"))?;
    Ok(())
}

pub(crate) async fn insert_tenant(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    slug: &str,
    name: &str,
    settings: Option<&Value>,
) -> Result<TenantResponse, sqlx::Error> {
    let (id, slug, name, status, settings, created_at, updated_at) = sqlx::query_as(
        r#"
        INSERT INTO tenants (id, slug, name, settings, created_by_user_id)
        VALUES ($1, $2, $3, COALESCE($4, '{}'::jsonb), $5)
        RETURNING id, slug, name, status, settings, created_at, updated_at
        "#,
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(name)
    .bind(settings)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(TenantResponse {
        id,
        slug,
        name,
        status,
        settings,
        created_at,
        updated_at,
    })
}

pub(crate) struct NewUser<'a> {
    pub(crate) id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) email: &'a str,
    pub(crate) display_name: Option<&'a str>,
    pub(crate) given_name: Option<&'a str>,
    pub(crate) family_name: Option<&'a str>,
    pub(crate) active: bool,
    pub(crate) settings: Option<Value>,
}

pub(crate) async fn insert_user(
    tx: &mut Transaction<'_, Postgres>,
    user: NewUser<'_>,
) -> Result<UserResponse, sqlx::Error> {
    let (
        id,
        tenant_id,
        email,
        display_name,
        given_name,
        family_name,
        active,
        settings,
        created_at,
        updated_at,
    ) = sqlx::query_as(
        r#"
        INSERT INTO users
            (id, tenant_id, email, given_name, family_name, display_name, active, settings)
        VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, '{}'::jsonb))
        RETURNING id, tenant_id, email, display_name, given_name, family_name,
                  active, settings, created_at, updated_at
        "#,
    )
    .bind(user.id)
    .bind(user.tenant_id)
    .bind(user.email)
    .bind(user.given_name)
    .bind(user.family_name)
    .bind(user.display_name)
    .bind(user.active)
    .bind(user.settings)
    .fetch_one(&mut **tx)
    .await?;
    Ok(UserResponse {
        id,
        tenant_id,
        email,
        display_name,
        given_name,
        family_name,
        active,
        settings,
        created_at,
        updated_at,
    })
}

pub(crate) async fn load_tenant(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<Option<TenantResponse>, sqlx::Error> {
    let row = sqlx::query_as(
        r#"
        SELECT id, slug, name, status, settings, created_at, updated_at
        FROM tenants
        WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(id, slug, name, status, settings, created_at, updated_at)| TenantResponse {
            id,
            slug,
            name,
            status,
            settings,
            created_at,
            updated_at,
        },
    ))
}

pub(crate) async fn patch_tenant(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    name: Option<String>,
    settings: Option<Value>,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE tenants
        SET name = COALESCE($2, name),
            settings = COALESCE($3, settings),
            updated_at = NOW()
        WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(tenant_id)
    .bind(name)
    .bind(settings)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
}

pub(crate) async fn list_users(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<Vec<UserResponse>, sqlx::Error> {
    let rows = sqlx::query_as(
        r#"
        SELECT id, tenant_id, email, display_name, given_name, family_name,
               active, settings, created_at, updated_at
        FROM users
        WHERE tenant_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                tenant_id,
                email,
                display_name,
                given_name,
                family_name,
                active,
                settings,
                created_at,
                updated_at,
            )| UserResponse {
                id,
                tenant_id,
                email,
                display_name,
                given_name,
                family_name,
                active,
                settings,
                created_at,
                updated_at,
            },
        )
        .collect())
}

pub(crate) async fn load_invited_user(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    email: &str,
) -> Result<Option<(Uuid, bool)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT id, active
        FROM users
        WHERE tenant_id = $1 AND lower(email) = lower($2)
        FOR UPDATE
        "#,
    )
    .bind(tenant_id)
    .bind(email)
    .fetch_optional(&mut **tx)
    .await
}

pub(crate) struct InvitedUserUpdate<'a> {
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) email: &'a str,
    pub(crate) display_name: Option<&'a str>,
    pub(crate) given_name: Option<&'a str>,
    pub(crate) family_name: Option<&'a str>,
    pub(crate) settings: Option<Value>,
}

pub(crate) async fn update_invited_user(
    tx: &mut Transaction<'_, Postgres>,
    user: InvitedUserUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE users
        SET email = $3,
            display_name = COALESCE($4, display_name),
            given_name = COALESCE($5, given_name),
            family_name = COALESCE($6, family_name),
            settings = COALESCE($7, settings),
            updated_at = NOW(),
            version = version + 1
        WHERE tenant_id = $1 AND id = $2
        "#,
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .bind(user.email)
    .bind(user.display_name)
    .bind(user.given_name)
    .bind(user.family_name)
    .bind(user.settings)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn revoke_invitations(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE tenant_user_invitations
        SET revoked_at = NOW()
        WHERE tenant_id = $1
          AND user_id = $2
          AND accepted_at IS NULL
          AND revoked_at IS NULL
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) struct NewInvitation<'a> {
    pub(crate) id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) email: &'a str,
    pub(crate) role: TenantUserRole,
    pub(crate) token_hash: &'a str,
    pub(crate) invited_by_user_id: Uuid,
    pub(crate) expires_at: DateTime<Utc>,
}

pub(crate) async fn insert_invitation(
    tx: &mut Transaction<'_, Postgres>,
    invitation: NewInvitation<'_>,
) -> Result<DateTime<Utc>, sqlx::Error> {
    let (created_at,) = sqlx::query_as(
        r#"
        INSERT INTO tenant_user_invitations
            (id, tenant_id, user_id, email, role, token_hash, invited_by_user_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING created_at
        "#,
    )
    .bind(invitation.id)
    .bind(invitation.tenant_id)
    .bind(invitation.user_id)
    .bind(invitation.email)
    .bind(invitation.role.relation())
    .bind(invitation.token_hash)
    .bind(invitation.invited_by_user_id)
    .bind(invitation.expires_at)
    .fetch_one(&mut **tx)
    .await?;
    Ok(created_at)
}

pub(crate) async fn consume_invitation(
    tx: &mut Transaction<'_, Postgres>,
    token_hash: &str,
) -> Result<Option<(Uuid, Uuid, String)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        UPDATE tenant_user_invitations
        SET accepted_at = NOW()
        WHERE token_hash = $1
          AND accepted_at IS NULL
          AND revoked_at IS NULL
          AND expires_at > NOW()
        RETURNING tenant_id, user_id, role
        "#,
    )
    .bind(token_hash)
    .fetch_optional(&mut **tx)
    .await
}

pub(crate) struct AcceptedUserUpdate {
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
}

pub(crate) async fn activate_invited_user(
    tx: &mut Transaction<'_, Postgres>,
    user: AcceptedUserUpdate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE users
        SET active = TRUE,
            deactivated_at = NULL,
            display_name = COALESCE($3, display_name),
            given_name = COALESCE($4, given_name),
            family_name = COALESCE($5, family_name),
            updated_at = NOW(),
            version = version + 1
        WHERE tenant_id = $1 AND id = $2
        "#,
    )
    .bind(user.tenant_id)
    .bind(user.user_id)
    .bind(user.display_name)
    .bind(user.given_name)
    .bind(user.family_name)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn enqueue_workspace_tuple(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    op: TupleOp,
) -> Result<(), moa_authz::AuthzError> {
    enqueue_raw(
        &mut **tx,
        op,
        &format!("workspace:{}", moa_core::WORKSPACE_ID),
        "workspace",
        &format!("tenant:{tenant_id}"),
        Some(tenant_id),
    )
    .await
}

pub(crate) async fn enqueue_user_role_tuple(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    relation: &str,
    op: TupleOp,
) -> Result<(), moa_authz::AuthzError> {
    enqueue_raw(
        &mut **tx,
        op,
        &format!("operator:{user_id}"),
        relation,
        &format!("tenant:{tenant_id}"),
        Some(tenant_id),
    )
    .await
}
