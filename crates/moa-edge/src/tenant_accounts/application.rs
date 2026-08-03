//! Tenant-account use cases composed from concrete edge dependencies.

use chrono::{Duration, Utc};
use moa_authz_schema::TupleOp;
use moa_messaging::DeliveryMessage;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::routes::AppState;
use crate::routes::auth_accounts::{UserCredentialRow, UserResponse, load_user_credential_by_id};

use super::repository::{self, AcceptedUserUpdate, InvitedUserUpdate, NewInvitation, NewUser};
use super::{
    AcceptTenantInvitationRequest, ApplicationError, CreateTenantUserRequest, CreatedInvitation,
    InviteTenantUserRequest, TenantInvitationResponse, TenantResponse, TenantSignupRequest,
    TenantUserRole,
};

const INVITATION_TOKEN_TTL_DAYS: i64 = 7;

pub(crate) struct RegistrationResult {
    pub(crate) tenant: TenantResponse,
    pub(crate) credential: UserCredentialRow,
}

pub(crate) async fn register_tenant(
    state: &AppState,
    request: TenantSignupRequest,
    slug: String,
    name: String,
    email: String,
) -> Result<RegistrationResult, ApplicationError> {
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| internal(format!("db begin: {error}")))?;
    let tenant = repository::insert_tenant(
        &mut tx,
        tenant_id,
        user_id,
        &slug,
        &name,
        request.settings.as_ref(),
    )
    .await
    .map_err(|error| internal(format!("create tenant: {error}")))?;
    let user = repository::insert_user(
        &mut tx,
        NewUser {
            id: user_id,
            tenant_id,
            email: &email,
            display_name: request.admin_display_name.as_deref(),
            given_name: request.admin_given_name.as_deref(),
            family_name: request.admin_family_name.as_deref(),
            active: true,
            settings: None,
        },
    )
    .await
    .map_err(|error| internal(format!("create tenant admin user: {error}")))?;
    repository::set_user_password(&mut tx, tenant_id, user_id, &request.admin_password)
        .await
        .map_err(internal)?;
    repository::enqueue_workspace_tuple(&mut tx, tenant_id, TupleOp::Write)
        .await
        .map_err(|error| internal(format!("tenant workspace tuple: {error}")))?;
    repository::enqueue_user_role_tuple(&mut tx, tenant_id, user_id, "admin", TupleOp::Write)
        .await
        .map_err(|error| internal(format!("tenant admin tuple: {error}")))?;
    tx.commit()
        .await
        .map_err(|error| internal(format!("db commit: {error}")))?;

    Ok(RegistrationResult {
        tenant,
        credential: credential_from_user(user),
    })
}

pub(crate) async fn load_tenant(
    state: &AppState,
    tenant_id: Uuid,
) -> Result<Option<TenantResponse>, ApplicationError> {
    repository::load_tenant(&state.pool, tenant_id)
        .await
        .map_err(|error| internal(format!("load tenant: {error}")))
}

pub(crate) async fn patch_tenant(
    state: &AppState,
    tenant_id: Uuid,
    name: Option<String>,
    settings: Option<serde_json::Value>,
) -> Result<bool, ApplicationError> {
    repository::patch_tenant(&state.pool, tenant_id, name, settings)
        .await
        .map(|rows| rows == 1)
        .map_err(|error| internal(format!("patch tenant: {error}")))
}

pub(crate) async fn list_users(
    state: &AppState,
    tenant_id: Uuid,
) -> Result<Vec<UserResponse>, ApplicationError> {
    repository::list_users(&state.pool, tenant_id)
        .await
        .map_err(|error| internal(format!("list users: {error}")))
}

pub(crate) async fn create_user(
    state: &AppState,
    tenant_id: Uuid,
    request: CreateTenantUserRequest,
    email: String,
) -> Result<UserResponse, ApplicationError> {
    let user_id = Uuid::new_v4();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| internal(format!("db begin: {error}")))?;
    let user = repository::insert_user(
        &mut tx,
        NewUser {
            id: user_id,
            tenant_id,
            email: &email,
            display_name: request.display_name.as_deref(),
            given_name: request.given_name.as_deref(),
            family_name: request.family_name.as_deref(),
            active: true,
            settings: request.settings,
        },
    )
    .await
    .map_err(|error| internal(format!("create user: {error}")))?;
    repository::set_user_password(&mut tx, tenant_id, user_id, &request.password)
        .await
        .map_err(internal)?;
    repository::enqueue_user_role_tuple(
        &mut tx,
        tenant_id,
        user_id,
        request.role.relation(),
        TupleOp::Write,
    )
    .await
    .map_err(|error| internal(format!("tenant role tuple: {error}")))?;
    tx.commit()
        .await
        .map_err(|error| internal(format!("db commit: {error}")))?;
    Ok(user)
}

pub(crate) async fn create_invitation(
    state: &AppState,
    tenant_id: Uuid,
    invited_by_user_id: Uuid,
    tenant_name: String,
    request: InviteTenantUserRequest,
    email: String,
) -> Result<CreatedInvitation, ApplicationError> {
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
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| internal(format!("db begin: {error}")))?;
    let existing = repository::load_invited_user(&mut tx, tenant_id, &email)
        .await
        .map_err(|error| internal(format!("load invited user: {error}")))?;
    let user_id = match existing {
        Some((_, true)) => return Err(ApplicationError::Conflict("user already exists")),
        Some((user_id, false)) => {
            repository::update_invited_user(
                &mut tx,
                InvitedUserUpdate {
                    tenant_id,
                    user_id,
                    email: &email,
                    display_name: display_name.as_deref(),
                    given_name: given_name.as_deref(),
                    family_name: family_name.as_deref(),
                    settings: settings.clone(),
                },
            )
            .await
            .map_err(|error| internal(format!("update invited user: {error}")))?;
            user_id
        }
        None => {
            let user_id = Uuid::new_v4();
            repository::insert_user(
                &mut tx,
                NewUser {
                    id: user_id,
                    tenant_id,
                    email: &email,
                    display_name: display_name.as_deref(),
                    given_name: given_name.as_deref(),
                    family_name: family_name.as_deref(),
                    active: false,
                    settings,
                },
            )
            .await
            .map_err(|error| internal(format!("create invited user: {error}")))?;
            user_id
        }
    };
    repository::revoke_invitations(&mut tx, tenant_id, user_id)
        .await
        .map_err(|error| internal(format!("revoke previous invitations: {error}")))?;
    let created_at = repository::insert_invitation(
        &mut tx,
        NewInvitation {
            id: invitation_id,
            tenant_id,
            user_id,
            email: &email,
            role,
            token_hash: &token_hash,
            invited_by_user_id,
            expires_at,
        },
    )
    .await
    .map_err(|error| internal(format!("create invitation: {error}")))?;
    tx.commit()
        .await
        .map_err(|error| internal(format!("db commit: {error}")))?;
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

pub(crate) async fn deliver_invitation(
    state: &AppState,
    invitation: &TenantInvitationResponse,
    tenant_name: &str,
    token: &SecretString,
) -> Result<(), String> {
    // Email transport is deployment-owned, so no tenant partition selects the
    // credential: every tenant's operator mail leaves through the same sender.
    let message = DeliveryMessage::account_invitation_email(
        invitation.tenant_id,
        invitation.user_id,
        invitation.email.clone(),
        tenant_name,
        invitation.role.relation(),
        token.expose_secret(),
        invitation.expires_at,
    );
    let receipt = state
        .delivery
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

pub(crate) async fn accept_invitation(
    state: &AppState,
    request: AcceptTenantInvitationRequest,
    token_hash: String,
) -> Result<UserCredentialRow, ApplicationError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| internal(format!("db begin: {error}")))?;
    let row = repository::consume_invitation(&mut tx, &token_hash)
        .await
        .map_err(|error| internal(format!("consume invitation: {error}")))?;
    let Some((tenant_id, user_id, role)) = row else {
        return Err(ApplicationError::BadRequest(
            "invalid or expired invitation token",
        ));
    };
    let Some(role) = TenantUserRole::from_relation(&role) else {
        return Err(internal("invitation role is invalid"));
    };
    repository::set_user_password(&mut tx, tenant_id, user_id, &request.password)
        .await
        .map_err(internal)?;
    repository::activate_invited_user(
        &mut tx,
        AcceptedUserUpdate {
            tenant_id,
            user_id,
            display_name: request.display_name,
            given_name: request.given_name,
            family_name: request.family_name,
        },
    )
    .await
    .map_err(|error| internal(format!("activate invited user: {error}")))?;
    if role == TenantUserRole::Operator {
        repository::enqueue_user_role_tuple(&mut tx, tenant_id, user_id, "admin", TupleOp::Delete)
            .await
            .map_err(|error| internal(format!("tenant admin tuple delete: {error}")))?;
    }
    repository::enqueue_user_role_tuple(
        &mut tx,
        tenant_id,
        user_id,
        role.relation(),
        TupleOp::Write,
    )
    .await
    .map_err(|error| internal(format!("tenant role tuple: {error}")))?;
    tx.commit()
        .await
        .map_err(|error| internal(format!("db commit: {error}")))?;
    load_user_credential_by_id(&state.pool, tenant_id, user_id)
        .await
        .map_err(ApplicationError::Internal)?
        .ok_or(ApplicationError::NotFound("user not found"))
}

fn credential_from_user(user: UserResponse) -> UserCredentialRow {
    UserCredentialRow {
        id: user.id,
        tenant_id: user.tenant_id,
        email: user.email,
        display_name: user.display_name,
        given_name: user.given_name,
        family_name: user.family_name,
        active: user.active,
        settings: user.settings,
        created_at: user.created_at,
        updated_at: user.updated_at,
        password_hash: String::new(),
    }
}

pub(crate) fn invitation_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn invitation_token() -> String {
    format!(
        "tenant_invite_{}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

fn internal(error: impl Into<String>) -> ApplicationError {
    ApplicationError::Internal(error.into())
}
