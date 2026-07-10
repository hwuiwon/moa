//! First-party dashboard login, password, and user-profile routes.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use moa_auth_providers::user_sessions;
use moa_auth_providers::{NewUserSessionToken, hash_password, verify_password};
use moa_authz::fga_subject;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{Identity, IdentityType};
use moa_core::{StoragePartitionId, TenantId};
use moa_messaging::{DeliveryMessage, ProviderDeliverySink};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    AppState, attach_set_cookie, authenticate_direct_request, clear_session_cookie_header,
    parse_json_body, require_direct_authz, session_cookie_header, user_session_token_from_headers,
};

const PASSWORD_MIN_CHARS: usize = 12;
const PASSWORD_MAX_CHARS: usize = 1024;
const RESET_TOKEN_TTL_MINUTES: i64 = 30;

/// Public login request.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// User email address.
    pub email: String,
    /// User plaintext password.
    pub password: String,
    /// Optional tenant ID when the email exists in multiple tenants.
    pub tenant_id: Option<Uuid>,
    /// Optional tenant slug when the email exists in multiple tenants.
    pub tenant_slug: Option<String>,
    /// Whether to issue a long-lived browser session.
    #[serde(default = "default_remember_me")]
    pub remember_me: bool,
}

/// Password reset request DTO.
#[derive(Debug, Deserialize)]
pub struct PasswordResetRequest {
    /// User email address.
    pub email: String,
    /// Optional tenant ID when the email exists in multiple tenants.
    pub tenant_id: Option<Uuid>,
    /// Optional tenant slug when the email exists in multiple tenants.
    pub tenant_slug: Option<String>,
}

/// Password reset token consumption DTO.
#[derive(Debug, Deserialize)]
pub struct ResetPasswordRequest {
    /// One-time reset token delivered out of band.
    pub token: String,
    /// New plaintext password.
    pub new_password: String,
}

/// Tenant-admin password set DTO.
#[derive(Debug, Deserialize)]
pub struct SetPasswordRequest {
    /// New plaintext password.
    pub new_password: String,
}

/// Authenticated password change DTO.
#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    /// Current plaintext password.
    pub current_password: String,
    /// New plaintext password.
    pub new_password: String,
    /// Whether to revoke the caller's other login sessions.
    #[serde(default)]
    pub revoke_other_sessions: bool,
}

/// Self-profile mutation DTO.
#[derive(Debug, Deserialize)]
pub struct PatchMeRequest {
    /// Display name.
    pub display_name: Option<String>,
    /// Given name.
    pub given_name: Option<String>,
    /// Family name.
    pub family_name: Option<String>,
    /// User-owned settings blob.
    pub settings: Option<Value>,
}

/// User JSON returned by account endpoints.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UserResponse {
    /// User UUID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// Email address.
    pub email: String,
    /// Display name.
    pub display_name: Option<String>,
    /// Given name.
    pub given_name: Option<String>,
    /// Family name.
    pub family_name: Option<String>,
    /// Whether the user is active.
    pub active: bool,
    /// User settings.
    pub settings: Value,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Update time.
    pub updated_at: DateTime<Utc>,
}

/// Resolved caller role summary.
#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct RoleSummary {
    /// Caller is a workspace-level super admin.
    pub workspace_admin: bool,
    /// Caller can administer the current tenant.
    pub tenant_admin: bool,
    /// Caller can operate the current tenant.
    pub tenant_operator: bool,
}

/// Login or signup response.
#[derive(Debug, Serialize)]
pub(super) struct AuthSessionResponse {
    /// Token expiration.
    pub expires_at: DateTime<Utc>,
    /// Authenticated user.
    pub user: UserResponse,
    /// Role summary in the token's home tenant.
    pub roles: RoleSummary,
}

#[derive(Debug, Serialize)]
struct AcceptedResponse {
    accepted: bool,
}

#[derive(Debug, Clone)]
pub(super) struct UserCredentialRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub active: bool,
    pub settings: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub password_hash: String,
}

type UserCredentialRecord = (
    Uuid,
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Value,
    DateTime<Utc>,
    DateTime<Utc>,
    String,
);

/// Issued browser session and response body.
pub(super) struct IssuedAuthSession {
    /// Response body returned to the browser.
    pub body: AuthSessionResponse,
    /// HttpOnly cookie value that carries the session token.
    pub set_cookie: HeaderValue,
}

impl IssuedAuthSession {
    /// Convert the session body into a response that sets the browser session cookie.
    pub(super) fn into_response(self) -> Response {
        attach_set_cookie(Json(self.body).into_response(), self.set_cookie)
    }
}

#[derive(Clone)]
struct CreatedPasswordResetToken {
    token: SecretString,
    expires_at: DateTime<Utc>,
}

/// Sign in a workspace admin, tenant admin, or tenant operator with email/password.
#[tracing::instrument(skip(state, body))]
// SAFETY: Public login reads credential rows only to verify the presented password and returns a token for the matched user.
pub async fn login(State(state): State<AppState>, body: Bytes) -> Response {
    let request: LoginRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let email = normalize_email(&request.email);
    let row = match load_login_user(
        &state.pool,
        &email,
        request.tenant_id,
        request.tenant_slug.as_deref(),
    )
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return invalid_login(),
        Err(error) => return internal_error(error),
    };
    let password = request.password;
    let hash = row.password_hash.clone();
    let verified =
        match tokio::task::spawn_blocking(move || verify_password(&password, &hash)).await {
            Ok(Ok(verified)) => verified,
            Ok(Err(error)) => return internal_error(format!("password verify: {error}")),
            Err(error) => return internal_error(format!("password verify task: {error}")),
        };
    if !verified {
        return invalid_login();
    }
    match issue_login_session(&state, row, request.remember_me).await {
        Ok(session) => session.into_response(),
        Err(response) => response,
    }
}

/// Log out the current browser session by revoking the presented session token and clearing the cookie.
#[tracing::instrument(skip(state, headers))]
// SAFETY: Logout only revokes the opaque session token presented by the caller and always clears the browser cookie.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = user_session_token_from_headers(&headers) {
        match user_sessions::revoke_presented(state.pool.as_ref(), &token, "logout").await {
            Ok(resolved) => {
                tracing::info!(
                    tenant_id = %resolved.tenant_id,
                    user_id = %resolved.user_id,
                    token_id = %resolved.id,
                    "user session token revoked on logout"
                );
            }
            Err(error) => {
                tracing::info!(error = %error, "logout received a session token that was not revoked");
            }
        }
    }
    attach_set_cookie(
        Json(AcceptedResponse { accepted: true }).into_response(),
        clear_session_cookie_header(),
    )
}

/// Request a password reset token for a local user.
#[tracing::instrument(skip(state, body))]
// SAFETY: Public reset request intentionally returns the same response for found and missing users to avoid account enumeration.
pub async fn request_password_reset(State(state): State<AppState>, body: Bytes) -> Response {
    let request: PasswordResetRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let email = normalize_email(&request.email);
    let row = match load_login_user(
        &state.pool,
        &email,
        request.tenant_id,
        request.tenant_slug.as_deref(),
    )
    .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(error = %error, "password reset lookup failed");
            None
        }
    };
    if let Some(row) = row {
        match create_password_reset_token(&state.pool, row.tenant_id, row.id).await {
            Ok(reset_token) => {
                if let Err(error) = deliver_password_reset_token(&state, &row, &reset_token).await {
                    tracing::warn!(
                        error = %error,
                        tenant_id = %row.tenant_id,
                        user_id = %row.id,
                        "password reset delivery failed"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, user_id = %row.id, "password reset token create failed");
            }
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(AcceptedResponse { accepted: true }),
    )
        .into_response()
}

/// Set a new password using a one-time reset token.
#[tracing::instrument(skip(state, body))]
// SAFETY: Reset token verification binds this unauthenticated mutation to one user and consumes the token before the password change commits.
pub async fn reset_password(State(state): State<AppState>, body: Bytes) -> Response {
    let request: ResetPasswordRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.new_password) {
        return response;
    }
    let token_hash = reset_token_hash(&request.token);
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };
    let row: Option<(Uuid, Uuid)> = match sqlx::query_as(
        r#"
        UPDATE password_reset_tokens
        SET used_at = NOW()
        WHERE token_hash = $1
          AND used_at IS NULL
          AND expires_at > NOW()
        RETURNING tenant_id, user_id
        "#,
    )
    .bind(token_hash)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(row) => row,
        Err(error) => return internal_error(format!("password reset consume: {error}")),
    };
    let Some((tenant_id, user_id)) = row else {
        return (StatusCode::BAD_REQUEST, "invalid or expired reset token").into_response();
    };
    if let Err(response) =
        set_user_password_in_tx(&mut tx, tenant_id, user_id, &request.new_password).await
    {
        return response;
    }
    if let Err(error) = revoke_user_sessions_in_tx(&mut tx, tenant_id, user_id, None).await {
        return internal_error(format!("revoke sessions: {error}"));
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }
    Json(AcceptedResponse { accepted: true }).into_response()
}

/// Change the authenticated user's password.
#[tracing::instrument(skip(state, headers, body))]
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/auth/password").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if identity.identity_type != IdentityType::Operator {
        return (StatusCode::FORBIDDEN, "only users can change password").into_response();
    }
    let request: ChangePasswordRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.new_password) {
        return response;
    }
    let row = match load_user_credential_by_id(&state.pool, identity.tenant_id.0, identity.id).await
    {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::NOT_FOUND, "user not found").into_response(),
        Err(error) => return internal_error(error),
    };
    let current_password = request.current_password;
    let current_hash = row.password_hash;
    let verified = match tokio::task::spawn_blocking(move || {
        verify_password(&current_password, &current_hash)
    })
    .await
    {
        Ok(Ok(verified)) => verified,
        Ok(Err(error)) => return internal_error(format!("password verify: {error}")),
        Err(error) => return internal_error(format!("password verify task: {error}")),
    };
    if !verified {
        return (StatusCode::UNAUTHORIZED, "invalid current password").into_response();
    }
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };
    if let Err(response) = set_user_password_in_tx(
        &mut tx,
        identity.tenant_id.0,
        identity.id,
        &request.new_password,
    )
    .await
    {
        return response;
    }
    if request.revoke_other_sessions {
        let except_token_id = current_user_session_token_id(&state, &headers, &identity).await;
        if let Err(error) =
            revoke_user_sessions_in_tx(&mut tx, identity.tenant_id.0, identity.id, except_token_id)
                .await
        {
            return internal_error(format!("revoke sessions: {error}"));
        }
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }
    Json(AcceptedResponse { accepted: true }).into_response()
}

/// Return the authenticated user's profile.
#[tracing::instrument(skip(state, headers))]
// SAFETY: The route reads only the authenticated user's own user row.
pub async fn get_me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/users/me").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if identity.identity_type != IdentityType::Operator {
        return (StatusCode::FORBIDDEN, "only users have profiles").into_response();
    }
    let user = match load_user_response(&state.pool, identity.tenant_id.0, identity.id).await {
        Ok(Some(user)) => user,
        Ok(None) => return (StatusCode::NOT_FOUND, "user not found").into_response(),
        Err(error) => return internal_error(error),
    };
    let roles = role_summary(&state, &identity).await;
    Json(serde_json::json!({ "user": user, "roles": roles })).into_response()
}

/// Patch the authenticated user's profile.
#[tracing::instrument(skip(state, headers, body))]
// SAFETY: The route mutates only the authenticated user's own user row.
pub async fn patch_me(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/users/me").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if identity.identity_type != IdentityType::Operator {
        return (StatusCode::FORBIDDEN, "only users have profiles").into_response();
    }
    let request: PatchMeRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE users
        SET display_name = COALESCE($3, display_name),
            given_name = COALESCE($4, given_name),
            family_name = COALESCE($5, family_name),
            settings = COALESCE($6, settings),
            updated_at = NOW(),
            version = version + 1
        WHERE tenant_id = $1 AND id = $2
        "#,
    )
    .bind(identity.tenant_id.0)
    .bind(identity.id)
    .bind(request.display_name)
    .bind(request.given_name)
    .bind(request.family_name)
    .bind(request.settings)
    .execute(state.pool.as_ref())
    .await
    {
        return internal_error(format!("patch user: {error}"));
    }
    let user = match load_user_response(&state.pool, identity.tenant_id.0, identity.id).await {
        Ok(Some(user)) => user,
        Ok(None) => return (StatusCode::NOT_FOUND, "user not found").into_response(),
        Err(error) => return internal_error(error),
    };
    Json(user).into_response()
}

/// Set a tenant user's password inside the caller's transaction.
pub(super) async fn set_user_password_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    password: &str,
) -> Result<(), Response> {
    let password = password.to_string();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(hash)) => hash,
        Ok(Err(error)) => return Err(internal_error(format!("password hash: {error}"))),
        Err(error) => return Err(internal_error(format!("password hash task: {error}"))),
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
    .map_err(|error| internal_error(format!("set password: {error}")))?;
    Ok(())
}

/// Load a JSON user response by tenant and ID.
pub(super) async fn load_user_response(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<Option<UserResponse>, String> {
    sqlx::query_as(
        r#"
        SELECT id, tenant_id, email, display_name, given_name, family_name,
               active, settings, created_at, updated_at
        FROM users
        WHERE tenant_id = $1 AND id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
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
    })
    .map_err(|error| format!("load user: {error}"))
}

/// Return a normalized email value.
#[must_use]
pub(super) fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

/// Validate a new password against MOA's local password policy.
pub(super) fn validate_password_policy(password: &str) -> Result<(), Response> {
    let chars = password.chars().count();
    if !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&chars) {
        return Err((
            StatusCode::BAD_REQUEST,
            "password must be between 12 and 1024 characters",
        )
            .into_response());
    }
    Ok(())
}

/// Validate a user settings JSON object.
pub(super) fn validate_settings(settings: Option<&Value>) -> Result<(), Response> {
    if settings.is_some_and(|value| !value.is_object()) {
        return Err((StatusCode::BAD_REQUEST, "settings must be a JSON object").into_response());
    }
    Ok(())
}

/// Return the caller role summary for the current tenant.
pub(super) async fn role_summary(state: &AppState, identity: &Identity) -> RoleSummary {
    let Some(fga) = state.fga.as_ref() else {
        return RoleSummary::default();
    };
    let subject = fga_subject(identity);
    let tenant_object = format!("tenant:{}", identity.tenant_id.0);
    let workspace_object = format!("workspace:{}", moa_core::WORKSPACE_ID);
    // Resolve all three roles in one batched OpenFGA request instead of three
    // sequential round trips. A failed batch defaults every role to false,
    // matching the previous per-check `unwrap_or(false)` fail-safe.
    let checks = [
        (subject.clone(), "admin".to_string(), workspace_object),
        (subject.clone(), "admin".to_string(), tenant_object.clone()),
        (subject, "operator".to_string(), tenant_object),
    ];
    let results = fga.batch_check(&checks).await.unwrap_or_default();
    RoleSummary {
        workspace_admin: results.first().copied().unwrap_or(false),
        tenant_admin: results.get(1).copied().unwrap_or(false),
        tenant_operator: results.get(2).copied().unwrap_or(false),
    }
}

/// Issue a local user session for an already-authenticated user row.
pub(super) async fn issue_login_session(
    state: &AppState,
    row: UserCredentialRow,
    remember_me: bool,
) -> Result<IssuedAuthSession, Response> {
    let expires_at = Utc::now()
        + if remember_me {
            Duration::days(30)
        } else {
            Duration::hours(12)
        };
    let issued = user_sessions::create(
        state.pool.as_ref(),
        NewUserSessionToken {
            tenant_id: row.tenant_id,
            user_id: row.id,
            expires_at,
        },
    )
    .await
    .map_err(|error| internal_error(format!("create session token: {error}")))?;
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: row.id,
        tenant_id: moa_core::TenantId::from(row.tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let roles = role_summary(state, &identity).await;
    let body = AuthSessionResponse {
        expires_at: issued.expires_at,
        user: UserResponse {
            id: row.id,
            tenant_id: row.tenant_id,
            email: row.email,
            display_name: row.display_name,
            given_name: row.given_name,
            family_name: row.family_name,
            active: row.active,
            settings: row.settings,
            created_at: row.created_at,
            updated_at: row.updated_at,
        },
        roles,
    };
    let set_cookie = session_cookie_header(issued.token.expose_secret(), issued.expires_at)
        .map_err(internal_error)?;
    Ok(IssuedAuthSession { body, set_cookie })
}

async fn current_user_session_token_id(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
) -> Option<Uuid> {
    let token = user_session_token_from_headers(headers)?;
    match user_sessions::validate(state.pool.as_ref(), &token).await {
        Ok(resolved)
            if resolved.tenant_id == identity.tenant_id.0 && resolved.user_id == identity.id =>
        {
            Some(resolved.id)
        }
        Ok(resolved) => {
            tracing::warn!(
                tenant_id = %identity.tenant_id.0,
                user_id = %identity.id,
                token_tenant_id = %resolved.tenant_id,
                token_user_id = %resolved.user_id,
                "presented user session token does not match authenticated identity"
            );
            None
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to resolve current user session token");
            None
        }
    }
}

/// Shared tenant-admin endpoint for setting another user's password.
#[tracing::instrument(skip(state, headers, body))]
pub async fn set_user_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/tenant/users/{user_id}/password")
            .await
        {
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
    let request: SetPasswordRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.new_password) {
        return response;
    }
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(format!("db begin: {error}")),
    };
    if let Err(response) = set_user_password_in_tx(
        &mut tx,
        identity.tenant_id.0,
        user_id,
        &request.new_password,
    )
    .await
    {
        return response;
    }
    if let Err(error) =
        revoke_user_sessions_in_tx(&mut tx, identity.tenant_id.0, user_id, None).await
    {
        return internal_error(format!("revoke sessions: {error}"));
    }
    if let Err(error) = tx.commit().await {
        return internal_error(format!("db commit: {error}"));
    }
    Json(AcceptedResponse { accepted: true }).into_response()
}

async fn load_login_user(
    pool: &sqlx::PgPool,
    email: &str,
    tenant_id: Option<Uuid>,
    tenant_slug: Option<&str>,
) -> Result<Option<UserCredentialRow>, String> {
    let rows = if let Some(tenant_id) = tenant_id {
        query_login_users(pool, email, Some(tenant_id), None).await?
    } else if let Some(slug) = tenant_slug {
        query_login_users(pool, email, None, Some(slug)).await?
    } else {
        query_login_users(pool, email, None, None).await?
    };
    if rows.len() == 1 {
        Ok(rows.into_iter().next())
    } else {
        Ok(None)
    }
}

pub(super) async fn load_user_credential_by_id(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<Option<UserCredentialRow>, String> {
    query_user_credential(pool, Some(user_id), None, Some(tenant_id), None)
        .await
        .map(|mut rows| rows.pop())
}

async fn query_login_users(
    pool: &sqlx::PgPool,
    email: &str,
    tenant_id: Option<Uuid>,
    tenant_slug: Option<&str>,
) -> Result<Vec<UserCredentialRow>, String> {
    query_user_credential(pool, None, Some(email), tenant_id, tenant_slug).await
}

async fn query_user_credential(
    pool: &sqlx::PgPool,
    user_id: Option<Uuid>,
    email: Option<&str>,
    tenant_id: Option<Uuid>,
    tenant_slug: Option<&str>,
) -> Result<Vec<UserCredentialRow>, String> {
    let rows: Vec<UserCredentialRecord> = sqlx::query_as(
        r#"
        SELECT u.id, u.tenant_id, u.email, u.display_name, u.given_name,
               u.family_name, u.active, u.settings, u.created_at, u.updated_at,
               c.password_hash
        FROM users u
        JOIN local_user_credentials c
          ON c.user_id = u.id
         AND c.tenant_id = u.tenant_id
        LEFT JOIN tenants t
          ON t.id = u.tenant_id
        WHERE u.active = TRUE
          AND ($1::UUID IS NULL OR u.id = $1)
          AND ($2::TEXT IS NULL OR lower(u.email) = lower($2))
          AND ($3::UUID IS NULL OR u.tenant_id = $3)
          AND ($4::TEXT IS NULL OR lower(t.slug) = lower($4))
          AND COALESCE(t.status, 'active') = 'active'
        ORDER BY u.created_at ASC
        LIMIT 2
        "#,
    )
    .bind(user_id)
    .bind(email)
    .bind(tenant_id)
    .bind(tenant_slug)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load credential user: {error}"))?;
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
                password_hash,
            )| UserCredentialRow {
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
                password_hash,
            },
        )
        .collect())
}

async fn create_password_reset_token(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<CreatedPasswordResetToken, sqlx::Error> {
    let token = reset_token();
    let token_hash = reset_token_hash(&token);
    let expires_at = Utc::now() + Duration::minutes(RESET_TOKEN_TTL_MINUTES);
    sqlx::query(
        r#"
        INSERT INTO password_reset_tokens
            (tenant_id, user_id, token_hash, expires_at)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .execute(pool)
    .await
    .map(|_| CreatedPasswordResetToken {
        token: SecretString::from(token),
        expires_at,
    })
}

async fn deliver_password_reset_token(
    state: &AppState,
    row: &UserCredentialRow,
    reset_token: &CreatedPasswordResetToken,
) -> Result<(), String> {
    let scope = StoragePartitionId::for_tenant(TenantId::from(row.tenant_id));
    let sink = ProviderDeliverySink::from_env(scope.as_str(), &state.config.messaging)
        .await
        .map_err(|error| format!("build delivery sink: {error}"))?;
    let message = DeliveryMessage::password_reset_email(
        row.tenant_id,
        row.id,
        row.email.clone(),
        reset_token.token.expose_secret(),
        reset_token.expires_at,
    );
    let receipt = sink
        .deliver(message)
        .await
        .map_err(|error| format!("deliver reset email: {error}"))?;
    tracing::info!(
        tenant_id = %row.tenant_id,
        user_id = %row.id,
        delivery_channel = receipt.channel.as_str(),
        provider = %receipt.provider,
        provider_message_id = ?receipt.provider_message_id,
        provider_status = ?receipt.provider_status,
        "password reset token delivered"
    );
    Ok(())
}

async fn revoke_user_sessions_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    except_token_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE user_session_tokens
        SET revoked_at = COALESCE(revoked_at, NOW()),
            revoked_reason = COALESCE(revoked_reason, 'password_changed')
        WHERE tenant_id = $1
          AND user_id = $2
          AND revoked_at IS NULL
          AND ($3::UUID IS NULL OR id <> $3)
        "#,
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(except_token_id)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

fn reset_token() -> String {
    format!(
        "password_reset_{}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn reset_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn default_remember_me() -> bool {
    true
}

fn invalid_login() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid email or password").into_response()
}

pub(super) fn internal_error(error: impl std::fmt::Display) -> Response {
    tracing::error!(error = %error, "account route failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "account operation failed",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn password_policy_rejects_short_passwords() {
        // Pins: local account passwords must meet the minimum length enforced by account endpoints.
        let response =
            validate_password_policy("too-short").expect_err("short password should be rejected");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn password_policy_accepts_long_memorable_passwords() {
        // Pins: the policy accepts long passphrases instead of enforcing arbitrary character classes.
        validate_password_policy("correct horse battery staple")
            .expect("long passphrase should be accepted");
    }

    #[test]
    fn reset_token_hash_is_not_the_raw_token() {
        // Pins: password reset tokens are stored as a deterministic digest, never as the bearer value.
        let token = "password_reset_example";
        let digest = reset_token_hash(token);

        assert_ne!(digest, token);
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, reset_token_hash(token));
    }
}
