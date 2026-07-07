//! Tenant signup, tenant settings, tenant users, and tenant deletion routes.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use moa_authz::enqueue_raw;
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::{StoragePartitionId, TenantId};
use moa_messaging::{DeliveryMessage, ProviderDeliverySink};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::auth_accounts::{
    UserCredentialRow, UserResponse, internal_error, issue_login_session,
    load_user_credential_by_id, normalize_email, set_user_password_in_tx, validate_password_policy,
    validate_settings,
};
use super::{
    AppState, attach_set_cookie, authenticate_direct_request, parse_json_body, require_direct_authz,
};

const INVITATION_TOKEN_TTL_DAYS: i64 = 7;

/// Public tenant signup request.
#[derive(Debug, Deserialize)]
pub struct TenantSignupRequest {
    /// Tenant display name.
    pub name: String,
    /// URL-safe tenant slug.
    pub slug: String,
    /// First tenant-admin email.
    pub admin_email: String,
    /// First tenant-admin password.
    pub admin_password: String,
    /// First tenant-admin display name.
    pub admin_display_name: Option<String>,
    /// First tenant-admin given name.
    pub admin_given_name: Option<String>,
    /// First tenant-admin family name.
    pub admin_family_name: Option<String>,
    /// Initial tenant settings.
    pub settings: Option<Value>,
}

/// Tenant settings mutation.
#[derive(Debug, Deserialize)]
pub struct PatchTenantRequest {
    /// Tenant display name.
    pub name: Option<String>,
    /// Tenant settings object.
    pub settings: Option<Value>,
}

/// Tenant-admin user creation request.
#[derive(Debug, Deserialize)]
pub struct CreateTenantUserRequest {
    /// User email address.
    pub email: String,
    /// User initial password.
    pub password: String,
    /// User role in this tenant.
    pub role: TenantUserRole,
    /// User display name.
    pub display_name: Option<String>,
    /// User given name.
    pub given_name: Option<String>,
    /// User family name.
    pub family_name: Option<String>,
    /// User settings object.
    pub settings: Option<Value>,
}

/// Tenant-admin invitation request.
#[derive(Debug, Deserialize)]
pub struct InviteTenantUserRequest {
    /// User email address.
    pub email: String,
    /// User role in this tenant.
    pub role: TenantUserRole,
    /// User display name.
    pub display_name: Option<String>,
    /// User given name.
    pub given_name: Option<String>,
    /// User family name.
    pub family_name: Option<String>,
    /// User settings object.
    pub settings: Option<Value>,
}

/// Tenant invitation acceptance request.
#[derive(Debug, Deserialize)]
pub struct AcceptTenantInvitationRequest {
    /// One-time invitation token delivered out of band.
    pub token: String,
    /// New plaintext password.
    pub password: String,
    /// Optional display name override.
    pub display_name: Option<String>,
    /// Optional given name override.
    pub given_name: Option<String>,
    /// Optional family name override.
    pub family_name: Option<String>,
}

/// Tenant user role that can be assigned through tenant-scoped account endpoints.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TenantUserRole {
    /// Tenant administrator.
    Admin,
    /// Tenant operator.
    Operator,
}

impl TenantUserRole {
    fn relation(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
        }
    }

    fn from_relation(relation: &str) -> Option<Self> {
        match relation {
            "admin" => Some(Self::Admin),
            "operator" => Some(Self::Operator),
            _ => None,
        }
    }
}

/// Tenant account response.
#[derive(Debug, Serialize)]
pub struct TenantResponse {
    /// Tenant UUID.
    pub id: Uuid,
    /// Tenant slug.
    pub slug: String,
    /// Tenant display name.
    pub name: String,
    /// Tenant status.
    pub status: String,
    /// Tenant settings.
    pub settings: Value,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Update time.
    pub updated_at: DateTime<Utc>,
}

/// Tenant account delete request.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteTenantRequest {
    /// Optional slug confirmation.
    pub confirm_slug: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeletedTenantResponse {
    deleted: bool,
    tenant_id: Uuid,
}

#[derive(Debug, Serialize)]
struct TenantInvitationResponse {
    id: Uuid,
    tenant_id: Uuid,
    user_id: Uuid,
    email: String,
    role: TenantUserRole,
    expires_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    delivery_sent: bool,
}

struct CreatedInvitation {
    response: TenantInvitationResponse,
    tenant_name: String,
    token: SecretString,
}

/// Create a new tenant and its first tenant admin.
#[tracing::instrument(skip(state, body))]
// SAFETY: Public signup creates a new tenant boundary and grants only tenant-admin access for that new tenant.
pub async fn signup(State(state): State<AppState>, body: Bytes) -> Response {
    let request: TenantSignupRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let slug = match normalize_slug(&request.slug) {
        Ok(slug) => slug,
        Err(response) => return response,
    };
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "tenant name is required").into_response();
    }
    if let Err(response) = validate_password_policy(&request.admin_password) {
        return response;
    }
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.admin_email);
    if !looks_like_email(&email) {
        return (
            StatusCode::BAD_REQUEST,
            "admin_email must be an email address",
        )
            .into_response();
    }

    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };

    let tenant_row: TenantResponse = match sqlx::query_as(
        r#"
        INSERT INTO tenants (id, slug, name, settings, created_by_user_id)
        VALUES ($1, $2, $3, COALESCE($4, '{}'::jsonb), $5)
        RETURNING id, slug, name, status, settings, created_at, updated_at
        "#,
    )
    .bind(tenant_id)
    .bind(&slug)
    .bind(&name)
    .bind(request.settings.clone())
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    {
        Ok((id, slug, name, status, settings, created_at, updated_at)) => TenantResponse {
            id,
            slug,
            name,
            status,
            settings,
            created_at,
            updated_at,
        },
        Err(error) => return internal_error(format!("create tenant: {error}")),
    };

    let user_row: UserResponse = match sqlx::query_as(
        r#"
        INSERT INTO users
            (id, tenant_id, email, given_name, family_name, display_name, active, settings)
        VALUES ($1, $2, $3, $4, $5, $6, TRUE, '{}'::jsonb)
        RETURNING id, tenant_id, email, display_name, given_name, family_name,
                  active, settings, created_at, updated_at
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(&email)
    .bind(request.admin_given_name.as_deref())
    .bind(request.admin_family_name.as_deref())
    .bind(request.admin_display_name.as_deref())
    .fetch_one(&mut *tx)
    .await
    {
        Ok((
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
        )) => UserResponse {
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
        Err(error) => return internal_error(format!("create tenant admin user: {error}")),
    };
    if let Err(response) =
        set_user_password_in_tx(&mut tx, tenant_id, user_id, &request.admin_password).await
    {
        return response;
    }
    if let Err(error) = enqueue_workspace_tuple(&mut tx, tenant_id, TupleOp::Write).await {
        return internal_error(format!("tenant workspace tuple: {error}"));
    }
    if let Err(error) =
        enqueue_user_role_tuple(&mut tx, tenant_id, user_id, "admin", TupleOp::Write).await
    {
        return internal_error(format!("tenant admin tuple: {error}"));
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }

    let credential = UserCredentialRow {
        id: user_row.id,
        tenant_id: user_row.tenant_id,
        email: user_row.email,
        display_name: user_row.display_name,
        given_name: user_row.given_name,
        family_name: user_row.family_name,
        active: user_row.active,
        settings: user_row.settings,
        created_at: user_row.created_at,
        updated_at: user_row.updated_at,
        password_hash: String::new(),
    };
    match issue_login_session(&state, credential, true).await {
        Ok(session) => {
            let response = (
                StatusCode::CREATED,
                Json(serde_json::json!({ "tenant": tenant_row, "session": session.body })),
            )
                .into_response();
            attach_set_cookie(response, session.set_cookie)
        }
        Err(response) => response,
    }
}

/// Return the authenticated caller's tenant account.
#[tracing::instrument(skip(state, headers))]
pub async fn get_tenant(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match load_tenant(&state.pool, identity.tenant_id.0).await {
        Ok(Some(tenant)) => Json(tenant).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => internal_error(error),
    }
}

/// Patch tenant account settings.
#[tracing::instrument(skip(state, headers, body))]
pub async fn patch_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let request: PatchTenantRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let result = sqlx::query(
        r#"
        UPDATE tenants
        SET name = COALESCE($2, name),
            settings = COALESCE($3, settings),
            updated_at = NOW()
        WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(identity.tenant_id.0)
    .bind(request.name.map(|name| name.trim().to_string()))
    .bind(request.settings)
    .execute(state.pool.as_ref())
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => get_tenant(State(state), headers).await,
        Ok(_) => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => internal_error(format!("patch tenant: {error}")),
    }
}

/// List tenant users.
#[tracing::instrument(skip(state, headers))]
pub async fn list_users(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant/users").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let rows: Result<Vec<UserResponse>, sqlx::Error> = sqlx::query_as(
        r#"
        SELECT id, tenant_id, email, display_name, given_name, family_name,
               active, settings, created_at, updated_at
        FROM users
        WHERE tenant_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(identity.tenant_id.0)
    .fetch_all(state.pool.as_ref())
    .await
    .map(|rows| {
        rows.into_iter()
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
            .collect()
    });
    match rows {
        Ok(users) => Json(serde_json::json!({ "users": users })).into_response(),
        Err(error) => internal_error(format!("list users: {error}")),
    }
}

/// Create a tenant admin or operator.
#[tracing::instrument(skip(state, headers, body))]
pub async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant/users").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let request: CreateTenantUserRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.password) {
        return response;
    }
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.email);
    if !looks_like_email(&email) {
        return (StatusCode::BAD_REQUEST, "email must be an email address").into_response();
    }
    let user_id = Uuid::new_v4();
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };
    let user: UserResponse = match sqlx::query_as(
        r#"
        INSERT INTO users
            (id, tenant_id, email, given_name, family_name, display_name, active, settings)
        VALUES ($1, $2, $3, $4, $5, $6, TRUE, COALESCE($7, '{}'::jsonb))
        RETURNING id, tenant_id, email, display_name, given_name, family_name,
                  active, settings, created_at, updated_at
        "#,
    )
    .bind(user_id)
    .bind(identity.tenant_id.0)
    .bind(&email)
    .bind(request.given_name.as_deref())
    .bind(request.family_name.as_deref())
    .bind(request.display_name.as_deref())
    .bind(request.settings)
    .fetch_one(&mut *tx)
    .await
    {
        Ok((
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
        )) => UserResponse {
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
        Err(error) => return internal_error(format!("create user: {error}")),
    };
    if let Err(response) =
        set_user_password_in_tx(&mut tx, identity.tenant_id.0, user_id, &request.password).await
    {
        return response;
    }
    if let Err(error) = enqueue_user_role_tuple(
        &mut tx,
        identity.tenant_id.0,
        user_id,
        request.role.relation(),
        TupleOp::Write,
    )
    .await
    {
        return internal_error(format!("tenant role tuple: {error}"));
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }
    (StatusCode::CREATED, Json(user)).into_response()
}

/// Invite a tenant admin or operator to set up their own account.
#[tracing::instrument(skip(state, headers, body))]
pub async fn invite_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/tenant/invitations").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let request: InviteTenantUserRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.email);
    if !looks_like_email(&email) {
        return (StatusCode::BAD_REQUEST, "email must be an email address").into_response();
    }
    let tenant = match load_tenant(&state.pool, identity.tenant_id.0).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => return internal_error(error),
    };
    let invitation = match create_tenant_invitation(
        &state.pool,
        identity.tenant_id.0,
        identity.id,
        tenant.name,
        request,
        email,
    )
    .await
    {
        Ok(invitation) => invitation,
        Err(response) => return response,
    };
    let mut response = invitation.response;
    response.delivery_sent = match deliver_tenant_invitation(
        &state,
        &response,
        &invitation.tenant_name,
        &invitation.token,
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant_id = %response.tenant_id,
                user_id = %response.user_id,
                invitation_id = %response.id,
                "tenant invitation delivery failed"
            );
            false
        }
    };
    (StatusCode::CREATED, Json(response)).into_response()
}

/// Accept a tenant invitation, set the user's password, and sign in.
#[tracing::instrument(skip(state, body))]
// SAFETY: Invitation acceptance is authorized by consuming a short-lived one-time bearer token for one tenant user.
pub async fn accept_invitation(State(state): State<AppState>, body: Bytes) -> Response {
    let request: AcceptTenantInvitationRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.password) {
        return response;
    }
    let token = request.token.trim();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "invitation token is required").into_response();
    }
    let token_hash = invitation_token_hash(token);
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };
    let row: Option<(Uuid, Uuid, String)> = match sqlx::query_as(
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
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(row) => row,
        Err(error) => return internal_error(format!("consume invitation: {error}")),
    };
    let Some((tenant_id, user_id, role)) = row else {
        return (
            StatusCode::BAD_REQUEST,
            "invalid or expired invitation token",
        )
            .into_response();
    };
    let Some(role) = TenantUserRole::from_relation(&role) else {
        return internal_error("invitation role is invalid");
    };
    if let Err(response) =
        set_user_password_in_tx(&mut tx, tenant_id, user_id, &request.password).await
    {
        return response;
    }
    if let Err(error) = sqlx::query(
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
    .bind(tenant_id)
    .bind(user_id)
    .bind(request.display_name)
    .bind(request.given_name)
    .bind(request.family_name)
    .execute(&mut *tx)
    .await
    {
        return internal_error(format!("activate invited user: {error}"));
    }
    if role == TenantUserRole::Operator
        && let Err(error) =
            enqueue_user_role_tuple(&mut tx, tenant_id, user_id, "admin", TupleOp::Delete).await
    {
        return internal_error(format!("tenant admin tuple delete: {error}"));
    }
    if let Err(error) =
        enqueue_user_role_tuple(&mut tx, tenant_id, user_id, role.relation(), TupleOp::Write).await
    {
        return internal_error(format!("tenant role tuple: {error}"));
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }
    let credential = match load_user_credential_by_id(&state.pool, tenant_id, user_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::NOT_FOUND, "user not found").into_response(),
        Err(error) => return internal_error(error),
    };
    match issue_login_session(&state, credential, true).await {
        Ok(session) => session.into_response(),
        Err(response) => response,
    }
}

/// Delete this tenant account and tenant-owned data.
#[tracing::instrument(skip(state, headers, body))]
pub async fn delete_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let tenant = match load_tenant(&state.pool, identity.tenant_id.0).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => return internal_error(error),
    };
    let request = if body.is_empty() {
        DeleteTenantRequest::default()
    } else {
        match parse_json_body::<DeleteTenantRequest>(&body) {
            Ok(request) => request,
            Err(response) => return response,
        }
    };
    if request
        .confirm_slug
        .as_deref()
        .is_some_and(|confirm_slug| confirm_slug != tenant.slug)
    {
        return (
            StatusCode::BAD_REQUEST,
            "confirm_slug does not match tenant",
        )
            .into_response();
    }
    match purge_tenant_account(&state.pool, tenant.id).await {
        Ok(()) => Json(DeletedTenantResponse {
            deleted: true,
            tenant_id: tenant.id,
        })
        .into_response(),
        Err(error) => internal_error(error),
    }
}

async fn load_tenant(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<Option<TenantResponse>, String> {
    sqlx::query_as(
        r#"
        SELECT id, slug, name, status, settings, created_at, updated_at
        FROM tenants
        WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(id, slug, name, status, settings, created_at, updated_at)| TenantResponse {
                id,
                slug,
                name,
                status,
                settings,
                created_at,
                updated_at,
            },
        )
    })
    .map_err(|error| format!("load tenant: {error}"))
}

async fn create_tenant_invitation(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    invited_by_user_id: Uuid,
    tenant_name: String,
    request: InviteTenantUserRequest,
    email: String,
) -> Result<CreatedInvitation, Response> {
    let InviteTenantUserRequest {
        role,
        display_name,
        given_name,
        family_name,
        settings,
        ..
    } = request;
    let token = invitation_token();
    let token_hash = invitation_token_hash(&token);
    let expires_at = Utc::now() + Duration::days(INVITATION_TOKEN_TTL_DAYS);
    let invitation_id = Uuid::new_v4();
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| internal_error(format!("db begin: {error}")))?;

    let existing: Option<(Uuid, bool)> = sqlx::query_as(
        r#"
        SELECT id, active
        FROM users
        WHERE tenant_id = $1 AND lower(email) = lower($2)
        FOR UPDATE
        "#,
    )
    .bind(tenant_id)
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| internal_error(format!("load invited user: {error}")))?;

    let user_id = match existing {
        Some((_, true)) => {
            return Err((StatusCode::CONFLICT, "user already exists").into_response());
        }
        Some((user_id, false)) => {
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
            .bind(tenant_id)
            .bind(user_id)
            .bind(&email)
            .bind(display_name.as_deref())
            .bind(given_name.as_deref())
            .bind(family_name.as_deref())
            .bind(settings.clone())
            .execute(&mut *tx)
            .await
            .map_err(|error| internal_error(format!("update invited user: {error}")))?;
            user_id
        }
        None => {
            let user_id = Uuid::new_v4();
            sqlx::query(
                r#"
                INSERT INTO users
                    (id, tenant_id, email, given_name, family_name, display_name, active, settings)
                VALUES ($1, $2, $3, $4, $5, $6, FALSE, COALESCE($7, '{}'::jsonb))
                "#,
            )
            .bind(user_id)
            .bind(tenant_id)
            .bind(&email)
            .bind(given_name.as_deref())
            .bind(family_name.as_deref())
            .bind(display_name.as_deref())
            .bind(settings)
            .execute(&mut *tx)
            .await
            .map_err(|error| internal_error(format!("create invited user: {error}")))?;
            user_id
        }
    };

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
    .execute(&mut *tx)
    .await
    .map_err(|error| internal_error(format!("revoke previous invitations: {error}")))?;

    let (created_at,): (DateTime<Utc>,) = sqlx::query_as(
        r#"
        INSERT INTO tenant_user_invitations
            (id, tenant_id, user_id, email, role, token_hash, invited_by_user_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING created_at
        "#,
    )
    .bind(invitation_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(&email)
    .bind(role.relation())
    .bind(token_hash)
    .bind(invited_by_user_id)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| internal_error(format!("create invitation: {error}")))?;

    tx.commit()
        .await
        .map_err(|error| internal_error(format!("db commit: {error}")))?;

    Ok(CreatedInvitation {
        response: TenantInvitationResponse {
            id: invitation_id,
            tenant_id,
            user_id,
            email,
            role,
            expires_at,
            created_at,
            delivery_sent: false,
        },
        tenant_name,
        token: SecretString::from(token),
    })
}

async fn deliver_tenant_invitation(
    state: &AppState,
    invitation: &TenantInvitationResponse,
    tenant_name: &str,
    token: &SecretString,
) -> Result<(), String> {
    let scope = StoragePartitionId::for_tenant(TenantId::from(invitation.tenant_id));
    let sink = ProviderDeliverySink::from_env(scope.as_str(), &state.config.messaging)
        .await
        .map_err(|error| format!("build delivery sink: {error}"))?;
    let message = DeliveryMessage::account_invitation_email(
        invitation.tenant_id,
        invitation.user_id,
        invitation.email.clone(),
        tenant_name,
        invitation.role.relation(),
        token.expose_secret(),
        invitation.expires_at,
    );
    let receipt = sink
        .deliver(message)
        .await
        .map_err(|error| format!("deliver invitation email: {error}"))?;
    tracing::info!(
        tenant_id = %invitation.tenant_id,
        user_id = %invitation.user_id,
        invitation_id = %invitation.id,
        delivery_channel = receipt.channel.as_str(),
        provider = %receipt.provider,
        provider_message_id = ?receipt.provider_message_id,
        provider_status = ?receipt.provider_status,
        "tenant invitation token delivered"
    );
    Ok(())
}

async fn purge_tenant_account(pool: &sqlx::PgPool, tenant_id: Uuid) -> Result<(), String> {
    let storage_partition_id = format!("tenant:{tenant_id}");
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("db begin: {error}"))?;
    let user_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load tenant users: {error}"))?;
    let api_key_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM api_keys WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load tenant api keys: {error}"))?;
    let session_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM sessions WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load tenant sessions: {error}"))?;
    let contact_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM contacts WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| format!("load tenant contacts: {error}"))?;

    enqueue_workspace_tuple(&mut tx, tenant_id, TupleOp::Delete)
        .await
        .map_err(|error| format!("tenant workspace delete tuple: {error}"))?;
    for user_id in &user_ids {
        for relation in ["admin", "operator"] {
            enqueue_user_role_tuple(&mut tx, tenant_id, *user_id, relation, TupleOp::Delete)
                .await
                .map_err(|error| format!("tenant user tuple delete: {error}"))?;
        }
    }
    for key_id in &api_key_ids {
        enqueue_api_key_tuples(&mut tx, tenant_id, *key_id).await?;
    }
    for session_id in &session_ids {
        enqueue_raw(
            &mut *tx,
            TupleOp::Delete,
            &format!("tenant:{tenant_id}"),
            "tenant",
            &format!("session:{session_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("session tenant tuple delete: {error}"))?;
        for user_id in &user_ids {
            for relation in ["owner", "participant"] {
                enqueue_raw(
                    &mut *tx,
                    TupleOp::Delete,
                    &format!("operator:{user_id}"),
                    relation,
                    &format!("session:{session_id}"),
                    Some(tenant_id),
                )
                .await
                .map_err(|error| format!("session user tuple delete: {error}"))?;
            }
        }
        for contact_id in &contact_ids {
            enqueue_raw(
                &mut *tx,
                TupleOp::Delete,
                &format!("contact:{contact_id}"),
                "contact",
                &format!("session:{session_id}"),
                Some(tenant_id),
            )
            .await
            .map_err(|error| format!("session contact tuple delete: {error}"))?;
        }
    }

    delete_tenant_rows(&mut tx, tenant_id, &storage_partition_id).await?;
    sqlx::query(
        r#"
        INSERT INTO tenants (id, slug, name, status, deleted_at)
        VALUES ($1, $2, $3, 'deleted', NOW())
        ON CONFLICT (id) DO UPDATE
        SET status = 'deleted',
            deleted_at = NOW(),
            updated_at = NOW()
        "#,
    )
    .bind(tenant_id)
    .bind(format!("deleted-{tenant_id}"))
    .bind("deleted tenant")
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("mark tenant deleted: {error}"))?;
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("delete tenant row: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("db commit: {error}"))
}

async fn delete_tenant_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    storage_partition_id: &str,
) -> Result<(), String> {
    let tenant_deletes = [
        "DELETE FROM moa.knowledge_object_ingestion_claims WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_group_memberships WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_groups WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_chunks WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_blocks WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_document_versions WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_provider_events WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_ingestion_steps WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_objects WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_sync_runs WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_connections WHERE tenant_id = $1",
        "DELETE FROM security_events WHERE tenant_id = $1",
        "DELETE FROM tenant_audit_destinations WHERE tenant_id = $1",
        "DELETE FROM tenant_signing_keys WHERE tenant_id = $1",
        "DELETE FROM tenant_action_reviews WHERE tenant_id = $1",
        "DELETE FROM action_policy_rules WHERE tenant_id = $1",
        "DELETE FROM builtin_pending_approvals WHERE tenant_id = $1",
        "DELETE FROM auth0_ciba_approvals WHERE tenant_id = $1",
        "DELETE FROM moa.hand_leases WHERE tenant_id = $1",
        "DELETE FROM session_agent_context WHERE tenant_id = $1",
        "DELETE FROM session_attachments WHERE tenant_id = $1",
        "DELETE FROM session_blobs WHERE tenant_id = $1",
        "DELETE FROM session_channel_bindings WHERE tenant_id = $1",
        "DELETE FROM contact_verification_challenges WHERE tenant_id = $1",
        "DELETE FROM contact_token_grants WHERE tenant_id = $1",
        "DELETE FROM contact_channel_accounts WHERE tenant_id = $1",
        "DELETE FROM contact_points WHERE tenant_id = $1",
        "DELETE FROM contacts WHERE tenant_id = $1",
        "DELETE FROM tenant_user_invitations WHERE tenant_id = $1",
        "DELETE FROM password_reset_tokens WHERE tenant_id = $1",
        "DELETE FROM user_session_tokens WHERE tenant_id = $1",
        "DELETE FROM local_user_credentials WHERE tenant_id = $1",
        "DELETE FROM auth0_user_map WHERE tenant_id = $1",
        "DELETE FROM linked_connections WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        "DELETE FROM scim_group_members WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        "DELETE FROM scim_groups WHERE tenant_id = $1",
        "DELETE FROM agents WHERE tenant_id = $1",
        "DELETE FROM api_key_revocations WHERE api_key_id IN (SELECT id FROM api_keys WHERE tenant_id = $1)",
        "DELETE FROM api_keys WHERE tenant_id = $1",
        "DELETE FROM users WHERE tenant_id = $1",
    ];
    for statement in tenant_deletes {
        sqlx::query(statement)
            .bind(tenant_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }

    let storage_deletes = [
        "DELETE FROM moa.agent_deployment WHERE storage_partition_id = $1",
        "DELETE FROM moa.agent_installation WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_trial WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run_artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run WHERE storage_partition_id = $1",
        "DELETE FROM analytics.score_run WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_node_run WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_run WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_file WHERE storage_partition_id = $1",
        "UPDATE moa.artifact SET latest_revision_uid = NULL WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact WHERE storage_partition_id = $1",
        "DELETE FROM learning_candidates WHERE storage_partition_id = $1",
        "DELETE FROM experience_attributions WHERE storage_partition_id = $1",
        "DELETE FROM experience_records WHERE storage_partition_id = $1",
        "DELETE FROM learning_log WHERE storage_partition_id = $1",
        "DELETE FROM task_segments WHERE storage_partition_id = $1",
        "DELETE FROM analytics.turn_lineage WHERE storage_partition_id = $1",
        "DELETE FROM analytics.scores WHERE storage_partition_id = $1",
        "DELETE FROM analytics.audit_roots WHERE storage_partition_id = $1",
        "DELETE FROM analytics.compliance_storage_partition_state WHERE storage_partition_id = $1",
        "DELETE FROM analytics.compliance_tenants WHERE storage_partition_id = $1",
        "DELETE FROM pii_vault.plaintext_side WHERE storage_partition_id = $1",
        "DELETE FROM pii_vault.subject_keys WHERE first_storage_partition_id = $1",
        "DELETE FROM moa.retrieval_lineage WHERE storage_partition_id = $1",
        "DELETE FROM moa.memory_digests WHERE storage_partition_id = $1",
        "DELETE FROM moa.ingest_dlq WHERE storage_partition_id = $1",
        "DELETE FROM moa.ingest_dedup WHERE storage_partition_id = $1",
        "DELETE FROM moa.embeddings WHERE storage_partition_id = $1",
        "DELETE FROM moa.graph_changelog WHERE storage_partition_id = $1",
        "DELETE FROM moa.edge_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.node_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    ];
    for statement in storage_deletes {
        sqlx::query(statement)
            .bind(storage_partition_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }

    let session_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM sessions WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load remaining sessions: {error}"))?;
    if !session_ids.is_empty() {
        sqlx::query("DELETE FROM session_event_dedupe WHERE session_id = ANY($1)")
            .bind(&session_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("delete session dedupe: {error}"))?;
    }
    sqlx::query("UPDATE sessions SET active_channel_binding_id = NULL WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("clear active channel binding: {error}"))?;
    for statement in [
        "DELETE FROM context_snapshots WHERE tenant_id = $1",
        "DELETE FROM pending_signals WHERE tenant_id = $1",
        "DELETE FROM events WHERE tenant_id = $1",
        "DELETE FROM sessions WHERE tenant_id = $1",
    ] {
        sqlx::query(statement)
            .bind(tenant_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }
    Ok(())
}

async fn enqueue_workspace_tuple(
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

async fn enqueue_user_role_tuple(
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

async fn enqueue_api_key_tuples(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    key_id: Uuid,
) -> Result<(), String> {
    for (user, relation, object) in [
        (
            format!("tenant:{tenant_id}"),
            "tenant".to_string(),
            format!("api_key:{key_id}"),
        ),
        (
            format!("api_key:{key_id}"),
            "admin".to_string(),
            format!("tenant:{tenant_id}"),
        ),
        (
            format!("api_key:{key_id}"),
            "operator".to_string(),
            format!("tenant:{tenant_id}"),
        ),
    ] {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &user,
            &relation,
            &object,
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("api key tuple delete: {error}"))?;
    }
    Ok(())
}

fn normalize_slug(slug: &str) -> Result<String, Response> {
    let slug = slug.trim().to_ascii_lowercase();
    let valid_len = (3..=63).contains(&slug.len());
    let valid_chars = slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    let valid_edges = !slug.starts_with('-') && !slug.ends_with('-');
    if valid_len && valid_chars && valid_edges {
        Ok(slug)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "slug must be 3-63 lowercase letters, digits, or hyphens",
        )
            .into_response())
    }
}

fn invitation_token() -> String {
    format!(
        "tenant_invite_{}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn invitation_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn looks_like_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn normalize_slug_accepts_dashboard_safe_slugs() {
        // Pins: tenant signup canonicalizes slugs before persistence.
        assert_eq!(
            normalize_slug("Acme-Team").expect("slug should normalize"),
            "acme-team"
        );
    }

    #[test]
    fn normalize_slug_rejects_path_like_values() {
        // Pins: tenant slugs cannot contain path separators or leading/trailing hyphens.
        for slug in ["../acme", "-acme", "acme-", "a"] {
            let response = normalize_slug(slug).expect_err("slug should be rejected");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn tenant_signup_email_validation_requires_domain_dot() {
        // Pins: tenant signup rejects obvious non-email login IDs before credential creation.
        assert!(looks_like_email("admin@example.com"));
        assert!(!looks_like_email("admin"));
        assert!(!looks_like_email("admin@example"));
    }

    #[test]
    fn tenant_user_roles_round_trip_openfga_relations() {
        // Pins: invitation role strings are exactly the tenant relations written to OpenFGA.
        assert_eq!(TenantUserRole::Admin.relation(), "admin");
        assert_eq!(TenantUserRole::Operator.relation(), "operator");
        assert_eq!(
            TenantUserRole::from_relation("admin"),
            Some(TenantUserRole::Admin)
        );
        assert_eq!(
            TenantUserRole::from_relation("operator"),
            Some(TenantUserRole::Operator)
        );
        assert_eq!(TenantUserRole::from_relation("workspace_admin"), None);
    }

    #[test]
    fn invitation_token_hash_is_not_the_raw_token() {
        // Pins: invitation tokens are stored as a deterministic digest, never as the bearer value.
        let token = "tenant_invite_example";
        let digest = invitation_token_hash(token);

        assert_ne!(digest, token);
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, invitation_token_hash(token));
    }
}
