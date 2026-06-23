//! High-level OCSF emission helpers.
//!
//! Every helper synchronously signs and inserts a `security_events` row. This
//! is intentionally fail-closed: if the audit write fails, callers should roll
//! back the state mutation that would otherwise be missing an audit trail.

use crate::classes::{
    AccountChangeEvent, Actor, AuthenticationEvent, AuthorizationEvent, EntityManagementEvent,
    Metadata, NetworkEndpoint, Product, Resource, SCHEMA_VERSION, Session, User,
};
use crate::enums::{
    account_activity, authn_activity, authn_status, authz_activity, authz_status, category_uid,
    class_uid, entity_activity, severity_id,
};
use crate::signing;
use chrono::{DateTime, Utc};
use moa_core::TenantId;
use moa_core::traits::{Identity, IdentityType};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

/// Audit-event emission failures.
#[derive(Debug, Error)]
pub enum EmitError {
    /// Signing or key lookup failed.
    #[error("signing: {0}")]
    Signing(#[from] signing::SigningError),
    /// Event serialization failed.
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Database insertion failed.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

/// Actor information for audit sites that do not already have an `Identity`.
#[derive(Debug, Clone)]
pub struct ActorInput {
    /// OCSF actor UID.
    pub uid: String,
    /// OCSF actor user type id.
    pub type_id: i32,
}

struct EntityEventInput<'a> {
    activity_id: i32,
    activity_name: &'a str,
    entity_uid: &'a str,
    entity_type: &'a str,
    comment: Option<&'a str>,
}

impl ActorInput {
    /// Build an actor from an authenticated MOA identity.
    #[must_use]
    pub fn from_identity(identity: &Identity) -> Self {
        let prefix = match identity.identity_type {
            IdentityType::User => "user",
            IdentityType::Contact => "contact",
            IdentityType::Agent => "agent",
            IdentityType::Service => "service",
        };
        Self {
            uid: format!("{prefix}:{}", identity.id),
            type_id: type_id_for_identity(identity.identity_type),
        }
    }

    /// Build a human user actor.
    #[must_use]
    pub fn user(user_id: Uuid) -> Self {
        Self {
            uid: format!("user:{user_id}"),
            type_id: 1,
        }
    }

    /// Build an agent actor.
    #[must_use]
    pub fn agent(agent_id: Uuid) -> Self {
        Self {
            uid: format!("agent:{agent_id}"),
            type_id: 2,
        }
    }

    /// Build a service actor.
    #[must_use]
    pub fn service(service_id: Uuid) -> Self {
        Self {
            uid: format!("service:{service_id}"),
            type_id: 3,
        }
    }

    /// Build an unknown actor.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            uid: "unknown".to_string(),
            type_id: 0,
        }
    }
}

/// Emit an authentication success event.
pub async fn emit_authn_success(
    pool: &PgPool,
    tenant_id: Uuid,
    identity: &Identity,
    auth_protocol: &str,
    src_ip: Option<&str>,
) -> Result<Uuid, EmitError> {
    let actor = ActorInput::from_identity(identity);
    let event = AuthenticationEvent {
        class_uid: class_uid::AUTHENTICATION,
        class_name: "Authentication".to_string(),
        category_uid: category_uid::IAM,
        category_name: "Identity & Access Management".to_string(),
        type_uid: type_uid(class_uid::AUTHENTICATION, authn_activity::LOGON),
        activity_id: authn_activity::LOGON,
        activity_name: "Logon".to_string(),
        severity_id: severity_id::INFORMATIONAL,
        severity: "Informational".to_string(),
        status_id: authn_status::SUCCESS,
        status: "Success".to_string(),
        time: Utc::now(),
        metadata: metadata(),
        actor: actor_from_input(actor),
        auth_protocol: auth_protocol.to_string(),
        src_endpoint: src_ip.map(network_endpoint),
    };
    insert_pool(pool, tenant_id, &event, None).await
}

/// Emit an authentication failure event.
pub async fn emit_authn_failure(
    pool: &PgPool,
    tenant_id: Uuid,
    actor_uid: Option<&str>,
    auth_protocol: &str,
    src_ip: Option<&str>,
    reason: &str,
) -> Result<Uuid, EmitError> {
    let event = AuthenticationEvent {
        class_uid: class_uid::AUTHENTICATION,
        class_name: "Authentication".to_string(),
        category_uid: category_uid::IAM,
        category_name: "Identity & Access Management".to_string(),
        type_uid: type_uid(
            class_uid::AUTHENTICATION,
            authn_activity::CREDENTIAL_VALIDATION,
        ),
        activity_id: authn_activity::CREDENTIAL_VALIDATION,
        activity_name: "Credential Validation".to_string(),
        severity_id: severity_id::LOW,
        severity: "Low".to_string(),
        status_id: authn_status::FAILURE,
        status: format!("Failure: {reason}"),
        time: Utc::now(),
        metadata: metadata(),
        actor: actor_from_input(ActorInput {
            uid: actor_uid.unwrap_or("unknown").to_string(),
            type_id: 0,
        }),
        auth_protocol: auth_protocol.to_string(),
        src_endpoint: src_ip.map(network_endpoint),
    };
    insert_pool(pool, tenant_id, &event, None).await
}

/// Emit an authorization decision event.
pub async fn emit_authz_decision(
    pool: &PgPool,
    tenant_id: TenantId,
    identity: &Identity,
    object_uid: &str,
    object_type: &str,
    relation: &str,
    allowed: bool,
) -> Result<Uuid, EmitError> {
    let event = authorization_event(
        ActorInput::from_identity(identity),
        object_uid,
        object_type,
        relation,
        allowed,
        if allowed {
            authz_activity::GRANT_PRIVILEGES
        } else {
            authz_activity::OTHER
        },
    );
    insert_pool(pool, tenant_id.0, &event, Some(object_uid)).await
}

/// Emit an API-key creation event.
pub async fn emit_api_key_created(
    pool: &PgPool,
    tenant_id: Uuid,
    identity: &Identity,
    api_key_id: Uuid,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = emit_api_key_created_tx(&mut tx, tenant_id, identity, api_key_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Emit an API-key creation event in an existing transaction.
pub async fn emit_api_key_created_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    identity: &Identity,
    api_key_id: Uuid,
) -> Result<Uuid, EmitError> {
    let entity_uid = format!("api_key:{api_key_id}");
    emit_entity_tx(
        tx,
        tenant_id,
        ActorInput::from_identity(identity),
        EntityEventInput {
            activity_id: entity_activity::CREATE,
            activity_name: "Create",
            entity_uid: &entity_uid,
            entity_type: "api_key",
            comment: None,
        },
    )
    .await
}

/// Emit an API-key revocation event.
pub async fn emit_api_key_revoked(
    pool: &PgPool,
    tenant_id: Uuid,
    actor: ActorInput,
    api_key_id: Uuid,
    reason: Option<&str>,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = emit_api_key_revoked_tx(&mut tx, tenant_id, actor, api_key_id, reason).await?;
    tx.commit().await?;
    Ok(id)
}

/// Emit an API-key revocation event in an existing transaction.
pub async fn emit_api_key_revoked_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    api_key_id: Uuid,
    reason: Option<&str>,
) -> Result<Uuid, EmitError> {
    let entity_uid = format!("api_key:{api_key_id}");
    emit_entity_tx(
        tx,
        tenant_id,
        actor,
        EntityEventInput {
            activity_id: entity_activity::DELETE,
            activity_name: "Delete",
            entity_uid: &entity_uid,
            entity_type: "api_key",
            comment: reason,
        },
    )
    .await
}

/// Emit an agent registration event.
pub async fn emit_agent_registered(
    pool: &PgPool,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = emit_agent_registered_tx(&mut tx, tenant_id, identity, agent_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Emit an agent registration event in an existing transaction.
pub async fn emit_agent_registered_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        ActorInput::from_identity(identity),
        account_activity::CREATE,
        "Create",
        &format!("agent:{agent_id}"),
        2,
    )
    .await
}

/// Emit an agent deactivation event.
pub async fn emit_agent_deactivated(
    pool: &PgPool,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = emit_agent_deactivated_tx(&mut tx, tenant_id, identity, agent_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Emit an agent deactivation event in an existing transaction.
pub async fn emit_agent_deactivated_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        ActorInput::from_identity(identity),
        account_activity::DISABLE,
        "Disable",
        &format!("agent:{agent_id}"),
        2,
    )
    .await
}

/// Emit a delegation grant event.
pub async fn emit_delegation_granted_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    let object_uid = format!("agent:{agent_id}");
    let event = authorization_event(
        ActorInput::from_identity(identity),
        &object_uid,
        "agent",
        &format!("can_act_as user:{user_id}"),
        true,
        authz_activity::GRANT_PRIVILEGES,
    );
    insert_tx(tx, tenant_id, &event, Some(&object_uid)).await
}

/// Emit a delegation revocation event.
pub async fn emit_delegation_revoked_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    identity: &Identity,
    agent_id: Uuid,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    let object_uid = format!("agent:{agent_id}");
    let event = authorization_event(
        ActorInput::from_identity(identity),
        &object_uid,
        "agent",
        &format!("can_act_as user:{user_id}"),
        true,
        authz_activity::REVOKE_PRIVILEGES,
    );
    insert_tx(tx, tenant_id, &event, Some(&object_uid)).await
}

/// Emit a SCIM user-creation event.
pub async fn emit_scim_user_created_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        actor,
        account_activity::CREATE,
        "Create",
        &format!("user:{user_id}"),
        1,
    )
    .await
}

/// Emit a SCIM user-update event in an existing transaction.
pub async fn emit_scim_user_updated_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        actor,
        account_activity::OTHER,
        "Other",
        &format!("user:{user_id}"),
        1,
    )
    .await
}

/// Emit a user-creation event.
pub async fn emit_user_created(
    pool: &PgPool,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = emit_scim_user_created_tx(&mut tx, tenant_id, actor, user_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Emit a user-deactivation event in an existing transaction.
pub async fn emit_user_deactivated_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        actor,
        account_activity::DISABLE,
        "Disable",
        &format!("user:{user_id}"),
        1,
    )
    .await
}

/// Emit a SCIM user-deactivation event in an existing transaction.
pub async fn emit_scim_user_deactivated_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_user_deactivated_tx(tx, tenant_id, actor, user_id).await
}

/// Emit a SCIM user-delete event in an existing transaction.
pub async fn emit_scim_user_deleted_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_account_tx(
        tx,
        tenant_id,
        actor,
        account_activity::DELETE,
        "Delete",
        &format!("user:{user_id}"),
        1,
    )
    .await
}

/// Emit a SCIM group creation event in an existing transaction.
pub async fn emit_scim_group_created_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_group_entity_tx(
        tx,
        tenant_id,
        actor,
        group_id,
        entity_activity::CREATE,
        "Create",
    )
    .await
}

/// Emit a SCIM group update event in an existing transaction.
pub async fn emit_scim_group_updated_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_group_entity_tx(
        tx,
        tenant_id,
        actor,
        group_id,
        entity_activity::UPDATE,
        "Update",
    )
    .await
}

/// Emit a SCIM group delete event in an existing transaction.
pub async fn emit_scim_group_deleted_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
) -> Result<Uuid, EmitError> {
    emit_group_entity_tx(
        tx,
        tenant_id,
        actor,
        group_id,
        entity_activity::DELETE,
        "Delete",
    )
    .await
}

/// Emit a group membership grant event in an existing transaction.
pub async fn emit_group_membership_added_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    let object_uid = format!("scim_group:{group_id}");
    let event = authorization_event(
        actor,
        &object_uid,
        "scim_group",
        &format!("member user:{user_id}"),
        true,
        authz_activity::GRANT_PRIVILEGES,
    );
    insert_tx(tx, tenant_id, &event, Some(&object_uid)).await
}

/// Emit a group membership revoke event in an existing transaction.
pub async fn emit_group_membership_removed_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
    user_id: Uuid,
) -> Result<Uuid, EmitError> {
    let object_uid = format!("scim_group:{group_id}");
    let event = authorization_event(
        actor,
        &object_uid,
        "scim_group",
        &format!("member user:{user_id}"),
        true,
        authz_activity::REVOKE_PRIVILEGES,
    );
    insert_tx(tx, tenant_id, &event, Some(&object_uid)).await
}

/// Emit an approval decision event in an existing transaction.
pub async fn emit_approval_decided_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    approval_id: Uuid,
    approved: bool,
) -> Result<Uuid, EmitError> {
    let object_uid = format!("approval:{approval_id}");
    let event = authorization_event(
        actor,
        &object_uid,
        "approval",
        if approved { "approve" } else { "deny" },
        approved,
        if approved {
            authz_activity::GRANT_PRIVILEGES
        } else {
            authz_activity::REVOKE_PRIVILEGES
        },
    );
    insert_tx(tx, tenant_id, &event, Some(&object_uid)).await
}

async fn emit_entity_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    input: EntityEventInput<'_>,
) -> Result<Uuid, EmitError> {
    let event = EntityManagementEvent {
        class_uid: class_uid::ENTITY_MANAGEMENT,
        class_name: "Entity Management".to_string(),
        category_uid: category_uid::IAM,
        category_name: "Identity & Access Management".to_string(),
        type_uid: type_uid(class_uid::ENTITY_MANAGEMENT, input.activity_id),
        activity_id: input.activity_id,
        activity_name: input.activity_name.to_string(),
        severity_id: severity_id::INFORMATIONAL,
        severity: "Informational".to_string(),
        time: Utc::now(),
        metadata: metadata(),
        actor: actor_from_input(actor),
        entity: Resource {
            uid: input.entity_uid.to_string(),
            name: None,
            resource_type: input.entity_type.to_string(),
        },
        comment: input.comment.map(str::to_string),
    };
    insert_tx(tx, tenant_id, &event, Some(input.entity_uid)).await
}

async fn emit_account_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    activity_id: i32,
    activity_name: &str,
    user_uid: &str,
    user_type_id: i32,
) -> Result<Uuid, EmitError> {
    let severity_id = if activity_id == account_activity::DISABLE {
        severity_id::MEDIUM
    } else {
        severity_id::INFORMATIONAL
    };
    let event = AccountChangeEvent {
        class_uid: class_uid::ACCOUNT_CHANGE,
        class_name: "Account Change".to_string(),
        category_uid: category_uid::IAM,
        category_name: "Identity & Access Management".to_string(),
        type_uid: type_uid(class_uid::ACCOUNT_CHANGE, activity_id),
        activity_id,
        activity_name: activity_name.to_string(),
        severity_id,
        severity: severity_label(severity_id).to_string(),
        time: Utc::now(),
        metadata: metadata(),
        actor: actor_from_input(actor),
        user: User {
            uid: user_uid.to_string(),
            name: None,
            email_addr: None,
            type_id: user_type_id,
        },
    };
    insert_tx(tx, tenant_id, &event, Some(user_uid)).await
}

async fn emit_group_entity_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    group_id: Uuid,
    activity_id: i32,
    activity_name: &str,
) -> Result<Uuid, EmitError> {
    let entity_uid = format!("scim_group:{group_id}");
    emit_entity_tx(
        tx,
        tenant_id,
        actor,
        EntityEventInput {
            activity_id,
            activity_name,
            entity_uid: &entity_uid,
            entity_type: "scim_group",
            comment: None,
        },
    )
    .await
}

async fn insert_pool<E: serde::Serialize>(
    pool: &PgPool,
    tenant_id: Uuid,
    event: &E,
    target_resource_uid: Option<&str>,
) -> Result<Uuid, EmitError> {
    let mut tx = pool.begin().await?;
    let id = insert_tx(&mut tx, tenant_id, event, target_resource_uid).await?;
    tx.commit().await?;
    Ok(id)
}

async fn insert_tx<E: serde::Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    event: &E,
    target_resource_uid: Option<&str>,
) -> Result<Uuid, EmitError> {
    let value = serde_json::to_value(event)?;
    let (signing_key_id, signature_hex, event_jcs) =
        signing::sign_tx(tx, tenant_id, &value).await?;
    let id = Uuid::now_v7();
    let class_uid = json_i32(&value, "class_uid");
    let activity_id = json_i32(&value, "activity_id");
    let category_uid = json_i32(&value, "category_uid");
    let severity_id = json_i32(&value, "severity_id");
    let type_uid = value.get("type_uid").and_then(Value::as_i64).unwrap_or(0);
    let actor_user_uid = value
        .pointer("/actor/user/uid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let actor_session_uid = value
        .pointer("/actor/session/uid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let occurred_at = occurred_at(&value);

    sqlx::query(
        r#"
        INSERT INTO security_events
            (id, tenant_id, class_uid, activity_id, category_uid, severity_id,
             type_uid, actor_user_uid, actor_session_uid, target_resource_uid,
             event_jcs, signature_hex, signing_key_id, occurred_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
    )
    .bind(id)
    .bind(tenant_id)
    .bind(class_uid)
    .bind(activity_id)
    .bind(category_uid)
    .bind(severity_id)
    .bind(type_uid)
    .bind(actor_user_uid)
    .bind(actor_session_uid)
    .bind(target_resource_uid)
    .bind(&event_jcs)
    .bind(signature_hex)
    .bind(signing_key_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await?;

    Ok(id)
}

fn authorization_event(
    actor: ActorInput,
    object_uid: &str,
    object_type: &str,
    relation: &str,
    allowed: bool,
    activity_id: i32,
) -> AuthorizationEvent {
    AuthorizationEvent {
        class_uid: class_uid::AUTHORIZATION,
        class_name: "Authorization".to_string(),
        category_uid: category_uid::IAM,
        category_name: "Identity & Access Management".to_string(),
        type_uid: type_uid(class_uid::AUTHORIZATION, activity_id),
        activity_id,
        activity_name: match activity_id {
            authz_activity::GRANT_PRIVILEGES => "Grant Privileges",
            authz_activity::REVOKE_PRIVILEGES => "Revoke Privileges",
            _ => "Other",
        }
        .to_string(),
        severity_id: if allowed {
            severity_id::INFORMATIONAL
        } else {
            severity_id::LOW
        },
        severity: if allowed { "Informational" } else { "Low" }.to_string(),
        status_id: if allowed {
            authz_status::ALLOWED
        } else {
            authz_status::DENIED
        },
        status: if allowed { "Allowed" } else { "Denied" }.to_string(),
        time: Utc::now(),
        metadata: metadata(),
        actor: actor_from_input(actor),
        resource: Resource {
            uid: object_uid.to_string(),
            name: None,
            resource_type: object_type.to_string(),
        },
        privileges: vec![relation.to_string()],
    }
}

fn actor_from_input(actor: ActorInput) -> Actor {
    Actor {
        user: User {
            uid: actor.uid,
            name: None,
            email_addr: None,
            type_id: actor.type_id,
        },
        session: None::<Session>,
    }
}

fn metadata() -> Metadata {
    Metadata {
        version: SCHEMA_VERSION.to_string(),
        product: Product {
            name: "MOA".to_string(),
            vendor_name: std::env::var("MOA_VENDOR_NAME").unwrap_or_else(|_| "MOA".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

fn type_id_for_identity(identity_type: IdentityType) -> i32 {
    match identity_type {
        IdentityType::User => 1,
        IdentityType::Contact => 2,
        IdentityType::Agent => 3,
        IdentityType::Service => 4,
    }
}

fn network_endpoint(ip: &str) -> NetworkEndpoint {
    NetworkEndpoint {
        ip: ip.to_string(),
        port: None,
    }
}

fn type_uid(class_uid: i32, activity_id: i32) -> i64 {
    i64::from(class_uid * 100 + activity_id)
}

fn json_i32(value: &Value, key: &str) -> i32 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0) as i32
}

fn occurred_at(value: &Value) -> DateTime<Utc> {
    value
        .get("time")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn severity_label(severity_id: i32) -> &'static str {
    match severity_id {
        severity_id::LOW => "Low",
        severity_id::MEDIUM => "Medium",
        severity_id::HIGH => "High",
        severity_id::CRITICAL => "Critical",
        severity_id::FATAL => "Fatal",
        _ => "Informational",
    }
}
