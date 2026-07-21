//! High-level OCSF emission helpers.
//!
//! The `emit_*` helpers synchronously sign and insert a `security_events` row and
//! are intentionally fail-closed: if the audit write fails, callers can roll back
//! the state mutation that would otherwise be missing an audit trail. The
//! `spawn_*` helpers instead hand the event to the background batch writer
//! ([`crate::init_background_audit`]); they never block or fail the caller and are
//! used on hot request paths where an audit write must not gate the response.

use crate::classes::{
    AccountChangeEvent, Actor, AuthenticationEvent, AuthorizationEvent, DataAccess,
    DataAccessEvent, EntityManagementEvent, Metadata, NetworkEndpoint, Product, Resource,
    SCHEMA_VERSION, Session, User,
};
use crate::enums::{
    account_activity, authn_activity, authn_status, authz_activity, authz_status, category_uid,
    class_uid, datastore_activity, entity_activity, severity_id,
};
use crate::signing;
use chrono::{DateTime, Utc};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::context::WorkingContext;
use moa_core::types::identifiers::TenantId;
use moa_core::types::session::SessionMeta;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

/// Audit-event emission failures.
#[derive(Debug, Error)]
pub enum EmitError {
    /// Caller supplied inconsistent or unstable audit identity.
    #[error("invalid input: {0}")]
    InvalidInput(String),
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

/// Inputs describing one memory-retrieval data access for transparency audit.
///
/// Built at the retrieval boundary where the caller identity, scope, and exact
/// returned node IDs are known, then handed to [`emit_data_access`] (durable).
/// It carries no node content or names, so the resulting audit event stays safe
/// to ship and query.
#[derive(Debug, Clone)]
pub struct MemoryDataAccess {
    identity: Identity,
    session_uid: String,
    retrieval_operation_id: String,
    node_uids: Vec<Uuid>,
    scope_uid: String,
    scope_tier: String,
    storage_partition: String,
    source_tiers: Vec<String>,
    turn_uid: Option<String>,
    agent_uid: Option<String>,
}

/// Operation-specific fields for one memory data-access event.
#[derive(Debug, Clone)]
pub struct MemoryDataAccessDetails {
    /// Replay-stable logical operation identifier supplied by the caller.
    pub retrieval_operation_id: String,
    /// Exact graph node UIDs returned by the retrieval.
    pub node_uids: Vec<Uuid>,
    /// Stable scope UID used as the OCSF resource UID and queryable target.
    pub scope_uid: String,
    /// Memory scope tier read: `tenant` or `contact`.
    pub scope_tier: String,
    /// Source tiers touched by the retrieval, e.g. `tenant_knowledge`, `user_memory`.
    pub source_tiers: Vec<String>,
    /// Turn that triggered the retrieval, when available.
    pub turn_uid: Option<String>,
}

impl MemoryDataAccess {
    /// Builds canonical access metadata from a durable session and authenticated caller.
    #[must_use]
    pub fn from_session(
        identity: &Identity,
        session: &SessionMeta,
        details: MemoryDataAccessDetails,
    ) -> Self {
        Self::from_parts(
            identity.clone(),
            session.id.0,
            session.tenant_id,
            session
                .agent_context
                .as_ref()
                .and_then(|agent| agent.agent_id),
            details,
        )
    }

    /// Builds canonical access metadata from a compiled working context.
    pub fn from_working_context(
        ctx: &WorkingContext,
        details: MemoryDataAccessDetails,
    ) -> Result<Self, EmitError> {
        let identity = ctx.caller_identity.clone().ok_or_else(|| {
            EmitError::InvalidInput(
                "memory retrieval requires exact authenticated actor provenance".to_string(),
            )
        })?;
        Ok(Self::from_parts(
            identity,
            ctx.session_id.0,
            ctx.tenant_id,
            ctx.agent_context.as_ref().and_then(|agent| agent.agent_id),
            details,
        ))
    }

    fn from_parts(
        identity: Identity,
        session_id: Uuid,
        tenant_id: TenantId,
        agent_id: Option<Uuid>,
        mut details: MemoryDataAccessDetails,
    ) -> Self {
        let mut source_tiers = Vec::with_capacity(details.source_tiers.len());
        for source_tier in details.source_tiers.drain(..) {
            if !source_tiers.contains(&source_tier) {
                source_tiers.push(source_tier);
            }
        }
        Self {
            identity,
            session_uid: format!("session:{session_id}"),
            retrieval_operation_id: details.retrieval_operation_id,
            node_uids: details.node_uids,
            scope_uid: details.scope_uid,
            scope_tier: details.scope_tier,
            storage_partition: tenant_id.to_string(),
            source_tiers,
            turn_uid: details.turn_uid,
            agent_uid: agent_id.map(|agent_id| format!("agent:{agent_id}")),
        }
    }
}

impl ActorInput {
    /// Build an actor from an authenticated MOA identity.
    #[must_use]
    pub fn from_identity(identity: &Identity) -> Self {
        let type_id = match identity.identity_type {
            IdentityType::Operator => 1,
            IdentityType::Contact => 2,
            IdentityType::Agent => 3,
            IdentityType::Service => 4,
        };
        Self {
            uid: format!("{}:{}", identity.identity_type.as_str(), identity.id),
            type_id,
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
}

/// Build an authentication success event.
fn authn_success_event(
    identity: &Identity,
    auth_protocol: &str,
    src_ip: Option<&str>,
) -> AuthenticationEvent {
    AuthenticationEvent {
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
        actor: actor_from_input(ActorInput::from_identity(identity)),
        auth_protocol: auth_protocol.to_string(),
        src_endpoint: src_ip.map(network_endpoint),
    }
}

/// Build an authentication failure event.
fn authn_failure_event(
    actor_uid: Option<&str>,
    auth_protocol: &str,
    src_ip: Option<&str>,
    reason: &str,
) -> AuthenticationEvent {
    AuthenticationEvent {
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
    }
}

/// Emit an authentication success event synchronously, failing closed.
pub async fn emit_authn_success(
    pool: &PgPool,
    tenant_id: Uuid,
    identity: &Identity,
    auth_protocol: &str,
    src_ip: Option<&str>,
) -> Result<Uuid, EmitError> {
    let event = authn_success_event(identity, auth_protocol, src_ip);
    insert_pool(pool, tenant_id, &event, None).await
}

/// Enqueue an authentication success event on the background audit writer.
///
/// This never blocks the caller and never fails: if the queue is saturated or
/// uninitialized the event is dropped and counted. Use this on hot request
/// paths where an audit write must not gate the response.
pub fn spawn_authn_success(
    tenant_id: Uuid,
    identity: &Identity,
    auth_protocol: &str,
    src_ip: Option<&str>,
) {
    let event = authn_success_event(identity, auth_protocol, src_ip);
    enqueue_event(tenant_id, &event, None);
}

/// Emit an authentication failure event synchronously, failing closed.
pub async fn emit_authn_failure(
    pool: &PgPool,
    tenant_id: Uuid,
    actor_uid: Option<&str>,
    auth_protocol: &str,
    src_ip: Option<&str>,
    reason: &str,
) -> Result<Uuid, EmitError> {
    let event = authn_failure_event(actor_uid, auth_protocol, src_ip, reason);
    insert_pool(pool, tenant_id, &event, None).await
}

/// Enqueue an authentication failure event on the background audit writer.
///
/// Non-blocking and non-fatal like [`spawn_authn_success`]. This is used for
/// unauthenticated and rejected credentials so a flood of bad requests cannot
/// amplify into synchronous signed inserts on the request path.
pub fn spawn_authn_failure(
    tenant_id: Uuid,
    actor_uid: Option<&str>,
    auth_protocol: &str,
    src_ip: Option<&str>,
    reason: &str,
) {
    let event = authn_failure_event(actor_uid, auth_protocol, src_ip, reason);
    enqueue_event(tenant_id, &event, None);
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

/// Enqueue an authorization decision event on the background audit writer.
///
/// Non-blocking and non-fatal: denial audits (and allow audits, when enabled)
/// are recorded off the request path so an authorization check never fails or
/// stalls because of an audit write.
pub fn spawn_authz_decision(
    tenant_id: TenantId,
    identity: &Identity,
    object_uid: &str,
    object_type: &str,
    relation: &str,
    allowed: bool,
) {
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
    enqueue_event(tenant_id.0, &event, Some(object_uid.to_string()));
}

/// Emit a memory data-access event synchronously, failing closed.
///
/// Records one retrieval operation as a summary access event on the durable
/// path. Callers must await this before consuming retrieved data.
pub async fn emit_data_access(
    pool: &PgPool,
    tenant_id: TenantId,
    access: MemoryDataAccess,
) -> Result<Uuid, EmitError> {
    let mut access = access;
    if access.retrieval_operation_id.trim().is_empty() {
        return Err(EmitError::InvalidInput(
            "retrieval operation id must not be empty".to_string(),
        ));
    }
    if access.identity.tenant_id != tenant_id {
        return Err(EmitError::InvalidInput(
            "retrieval identity tenant does not match event tenant".to_string(),
        ));
    }
    access.node_uids.sort_unstable();
    access.node_uids.dedup();
    let event = data_access_event(&access);
    let value = serde_json::to_value(&event)?;
    let mut tx = pool.begin().await?;
    let (signing_key_id, signature_hex, event_jcs) =
        signing::sign_tx(&mut tx, tenant_id.0, &value).await?;
    let columns = EventColumns::from_value(&value);
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO security_events
            (id, tenant_id, class_uid, activity_id, category_uid, severity_id,
             type_uid, actor_user_uid, actor_session_uid, target_resource_uid,
             event_jcs, signature_hex, signing_key_id, occurred_at,
             retrieval_operation_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (tenant_id, retrieval_operation_id)
            WHERE retrieval_operation_id IS NOT NULL
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(columns.id)
    .bind(tenant_id.0)
    .bind(columns.class_uid)
    .bind(columns.activity_id)
    .bind(columns.category_uid)
    .bind(columns.severity_id)
    .bind(columns.type_uid)
    .bind(columns.actor_user_uid)
    .bind(columns.actor_session_uid)
    .bind(&access.scope_uid)
    .bind(&event_jcs)
    .bind(signature_hex)
    .bind(signing_key_id)
    .bind(columns.occurred_at)
    .bind(&access.retrieval_operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match inserted {
        Some(id) => id,
        None => sqlx::query_scalar(
            "SELECT id FROM security_events WHERE tenant_id = $1 AND retrieval_operation_id = $2",
        )
        .bind(tenant_id.0)
        .bind(&access.retrieval_operation_id)
        .fetch_one(&mut *tx)
        .await?,
    };
    tx.commit().await?;
    Ok(id)
}

/// Build a Datastore Activity (Read) event from a memory data-access summary.
fn data_access_event(access: &MemoryDataAccess) -> DataAccessEvent {
    let actor = ActorInput::from_identity(&access.identity);
    DataAccessEvent {
        class_uid: class_uid::DATASTORE_ACTIVITY,
        class_name: "Datastore Activity".to_string(),
        category_uid: category_uid::APPLICATION_ACTIVITY,
        category_name: "Application Activity".to_string(),
        type_uid: type_uid(class_uid::DATASTORE_ACTIVITY, datastore_activity::READ),
        activity_id: datastore_activity::READ,
        activity_name: "Read".to_string(),
        severity_id: severity_id::INFORMATIONAL,
        severity: "Informational".to_string(),
        time: Utc::now(),
        metadata: metadata(),
        actor: Actor {
            user: User {
                uid: actor.uid,
                name: None,
                email_addr: None,
                type_id: actor.type_id,
            },
            session: Some(Session {
                uid: access.session_uid.clone(),
                created_time: None,
            }),
        },
        resource: Resource {
            uid: access.scope_uid.clone(),
            name: None,
            resource_type: "memory_graph".to_string(),
        },
        access: DataAccess {
            retrieval_operation_id: access.retrieval_operation_id.clone(),
            node_uids: access.node_uids.iter().map(Uuid::to_string).collect(),
            scope_tier: access.scope_tier.clone(),
            storage_partition: access.storage_partition.clone(),
            source_tiers: access.source_tiers.clone(),
            records_returned: u32::try_from(access.node_uids.len()).unwrap_or(u32::MAX),
            turn_uid: access.turn_uid.clone(),
            agent_uid: access.agent_uid.clone(),
            api_key_uid: access.identity.api_key_id.map(|id| format!("api_key:{id}")),
            acting_on_behalf_of_uid: access
                .identity
                .acting_on_behalf_of
                .map(|id| format!("principal:{id}")),
        },
    }
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
    let columns = EventColumns::from_value(&value);
    let id = columns.id;

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
    .bind(columns.class_uid)
    .bind(columns.activity_id)
    .bind(columns.category_uid)
    .bind(columns.severity_id)
    .bind(columns.type_uid)
    .bind(columns.actor_user_uid)
    .bind(columns.actor_session_uid)
    .bind(target_resource_uid)
    .bind(&event_jcs)
    .bind(signature_hex)
    .bind(signing_key_id)
    .bind(columns.occurred_at)
    .execute(&mut **tx)
    .await?;

    Ok(id)
}

/// Non-signature `security_events` columns derived from a serialized event.
///
/// Shared by the synchronous [`insert_tx`] path and the background batch writer
/// so both persist identical column values for a given event.
pub(crate) struct EventColumns {
    pub(crate) id: Uuid,
    pub(crate) class_uid: i32,
    pub(crate) activity_id: i32,
    pub(crate) category_uid: i32,
    pub(crate) severity_id: i32,
    pub(crate) type_uid: i64,
    pub(crate) actor_user_uid: Option<String>,
    pub(crate) actor_session_uid: Option<String>,
    pub(crate) occurred_at: DateTime<Utc>,
}

impl EventColumns {
    pub(crate) fn from_value(value: &Value) -> Self {
        Self {
            id: Uuid::now_v7(),
            class_uid: json_i32(value, "class_uid"),
            activity_id: json_i32(value, "activity_id"),
            category_uid: json_i32(value, "category_uid"),
            severity_id: json_i32(value, "severity_id"),
            type_uid: value.get("type_uid").and_then(Value::as_i64).unwrap_or(0),
            actor_user_uid: value
                .pointer("/actor/user/uid")
                .and_then(Value::as_str)
                .map(str::to_string),
            actor_session_uid: value
                .pointer("/actor/session/uid")
                .and_then(Value::as_str)
                .map(str::to_string),
            occurred_at: occurred_at(value),
        }
    }
}

/// Serialize an event and hand it to the background audit writer.
///
/// Serialization failures are logged and dropped rather than surfaced: the
/// spawn variants are used only where an audit write must never fail a request.
fn enqueue_event<E: serde::Serialize>(
    tenant_id: Uuid,
    event: &E,
    target_resource_uid: Option<String>,
) {
    match serde_json::to_value(event) {
        Ok(value) => crate::audit_sink::enqueue(tenant_id, value, target_resource_uid),
        Err(error) => {
            tracing::warn!(error = %error, "failed to serialize security audit event; dropping");
        }
    }
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
