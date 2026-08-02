//! High-level OCSF emission helpers.
//!
//! The `emit_*` helpers synchronously sign and insert a `security_events` row and
//! are intentionally fail-closed: if the audit write fails, callers can roll back
//! the state mutation that would otherwise be missing an audit trail. The
//! `spawn_*` helpers instead hand the event to the background batch writer
//! ([`crate::init_background_audit`]); they never block or fail the caller and are
//! used on hot request paths where an audit write must not gate the response.

use crate::audit_sink::{SignedRow, insert_rows};
use crate::classes::{
    AccountChangeEvent, Actor, AuthenticationEvent, AuthorizationEvent, DataAccess,
    DataAccessEvent, DetectionFindingEvent, EntityManagementEvent, FindingInfo, Metadata,
    NetworkEndpoint, Product, PromptInjectionCircuit, Resource, SCHEMA_VERSION, Session, User,
};
use crate::enums::{
    account_activity, authn_activity, authn_status, authz_activity, authz_status, category_uid,
    class_uid, datastore_activity, detection_activity, entity_activity, severity_id,
};
use crate::signing;
use chrono::{DateTime, Utc};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::context::WorkingContext;
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_core::types::security::{
    InjectionSignal, SecurityCircuitStage, SecurityCircuitTransition, TransitionKeyInput,
    transition_key,
};
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
    /// A stored event with the same deterministic identity does not match this one.
    #[error("replay conflict: {0}")]
    ReplayConflict(String),
}

/// Actor information for audit sites that do not already have an `Identity`.
#[derive(Debug, Clone)]
pub struct ActorInput {
    /// OCSF actor UID.
    pub uid: String,
    /// OCSF actor user type id.
    pub type_id: i32,
}

/// One security-audit change produced by a SCIM group transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScimGroupAuditChange {
    /// A group row was created.
    Created {
        /// Group identifier.
        group_id: Uuid,
    },
    /// A group's durable metadata changed.
    Updated {
        /// Group identifier.
        group_id: Uuid,
    },
    /// A group row was deleted.
    Deleted {
        /// Group identifier.
        group_id: Uuid,
    },
    /// A membership row was inserted.
    MembershipAdded {
        /// Group identifier.
        group_id: Uuid,
        /// Added user identifier.
        user_id: Uuid,
    },
    /// A membership row was deleted.
    MembershipRemoved {
        /// Group identifier.
        group_id: Uuid,
        /// Removed user identifier.
        user_id: Uuid,
    },
    /// An authorization tuple was granted without changing membership.
    PrivilegeGranted {
        /// Group identifier whose mapping changed.
        group_id: Uuid,
        /// Retained member receiving the privilege.
        user_id: Uuid,
        /// Exact OpenFGA relation granted.
        relation: String,
        /// Exact OpenFGA object wire identifier granted.
        object: String,
    },
    /// An authorization tuple was revoked without changing membership.
    PrivilegeRevoked {
        /// Group identifier whose mapping changed.
        group_id: Uuid,
        /// Retained member losing the privilege.
        user_id: Uuid,
        /// Exact OpenFGA relation revoked.
        relation: String,
        /// Exact OpenFGA object wire identifier revoked.
        object: String,
    },
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
/// This never blocks the caller and never fails: if the queue is saturated the
/// event is dropped and counted. Use this on hot request paths where an audit
/// write must not gate the response. Taking the emitter by reference is what
/// makes the writer's ownership visible here rather than resolved from a global
/// that may never have been installed.
pub fn spawn_authn_success(
    emitter: &crate::AuditEmitter,
    tenant_id: Uuid,
    identity: &Identity,
    auth_protocol: &str,
    src_ip: Option<&str>,
) {
    let event = authn_success_event(identity, auth_protocol, src_ip);
    enqueue_event(emitter, tenant_id, &event, None);
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
    emitter: &crate::AuditEmitter,
    tenant_id: Uuid,
    actor_uid: Option<&str>,
    auth_protocol: &str,
    src_ip: Option<&str>,
    reason: &str,
) {
    let event = authn_failure_event(actor_uid, auth_protocol, src_ip, reason);
    enqueue_event(emitter, tenant_id, &event, None);
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
    emitter: &crate::AuditEmitter,
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
    enqueue_event(emitter, tenant_id.0, &event, Some(object_uid.to_string()));
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
    let signing::SignedPayload {
        signing_key_id,
        signature_hex,
        event_jcs,
    } = signing::sign_tx(&mut tx, tenant_id.0, &value).await?;
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

/// Emit a bounded batch of SCIM group changes in the caller's transaction.
///
/// The whole batch uses one active tenant signing key and one array/`UNNEST`
/// insert. Empty input is a true no-op: it does not look up or create a key.
pub async fn emit_scim_group_changes_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    actor: ActorInput,
    changes: &[ScimGroupAuditChange],
) -> Result<Vec<Uuid>, EmitError> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let (values, target_resource_uids): (Vec<_>, Vec<_>) = changes
        .iter()
        .map(|change| scim_group_event(actor.clone(), change))
        .collect::<Result<Vec<_>, EmitError>>()?
        .into_iter()
        .unzip();
    let signed = signing::sign_batch_tx(tx, tenant_id, &values).await?;
    if signed.len() != values.len() {
        return Err(EmitError::InvalidInput(
            "SCIM group signing cardinality mismatch".to_string(),
        ));
    }
    let rows: Vec<_> = values
        .into_iter()
        .zip(target_resource_uids)
        .zip(signed)
        .map(|((value, target_resource_uid), signed)| SignedRow {
            columns: EventColumns::from_value(&value),
            tenant_id,
            target_resource_uid: Some(target_resource_uid),
            event_jcs: signed.event_jcs,
            signature_hex: signed.signature_hex,
            signing_key_id: signed.signing_key_id,
        })
        .collect();
    let ids = rows.iter().map(|row| row.columns.id).collect();
    insert_rows(&mut **tx, &rows).await?;
    Ok(ids)
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
    let target_resource_uid = input.entity_uid.to_string();
    let event = entity_event(actor, input);
    insert_tx(tx, tenant_id, &event, Some(&target_resource_uid)).await
}

fn entity_event(actor: ActorInput, input: EntityEventInput<'_>) -> EntityManagementEvent {
    EntityManagementEvent {
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
    }
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

fn scim_group_event(
    actor: ActorInput,
    change: &ScimGroupAuditChange,
) -> Result<(Value, String), EmitError> {
    let (event, target_resource_uid) = match change {
        ScimGroupAuditChange::Created { group_id } => {
            scim_group_entity_event(actor, *group_id, entity_activity::CREATE, "Create")?
        }
        ScimGroupAuditChange::Updated { group_id } => {
            scim_group_entity_event(actor, *group_id, entity_activity::UPDATE, "Update")?
        }
        ScimGroupAuditChange::Deleted { group_id } => {
            scim_group_entity_event(actor, *group_id, entity_activity::DELETE, "Delete")?
        }
        ScimGroupAuditChange::MembershipAdded { group_id, user_id } => scim_group_membership_event(
            actor,
            *group_id,
            *user_id,
            authz_activity::GRANT_PRIVILEGES,
        )?,
        ScimGroupAuditChange::MembershipRemoved { group_id, user_id } => {
            scim_group_membership_event(
                actor,
                *group_id,
                *user_id,
                authz_activity::REVOKE_PRIVILEGES,
            )?
        }
        ScimGroupAuditChange::PrivilegeGranted {
            group_id,
            user_id,
            relation,
            object,
        } => scim_group_privilege_event(
            actor,
            *group_id,
            *user_id,
            relation,
            object,
            authz_activity::GRANT_PRIVILEGES,
        )?,
        ScimGroupAuditChange::PrivilegeRevoked {
            group_id,
            user_id,
            relation,
            object,
        } => scim_group_privilege_event(
            actor,
            *group_id,
            *user_id,
            relation,
            object,
            authz_activity::REVOKE_PRIVILEGES,
        )?,
    };
    Ok((event, target_resource_uid))
}

fn scim_group_entity_event(
    actor: ActorInput,
    group_id: Uuid,
    activity_id: i32,
    activity_name: &str,
) -> Result<(Value, String), EmitError> {
    let target = format!("scim_group:{group_id}");
    let event = entity_event(
        actor,
        EntityEventInput {
            activity_id,
            activity_name,
            entity_uid: &target,
            entity_type: "scim_group",
            comment: None,
        },
    );
    Ok((serde_json::to_value(event)?, target))
}

fn scim_group_membership_event(
    actor: ActorInput,
    group_id: Uuid,
    user_id: Uuid,
    activity_id: i32,
) -> Result<(Value, String), EmitError> {
    let target = format!("scim_group:{group_id}");
    let event = authorization_event(
        actor,
        &target,
        "scim_group",
        &format!("member user:{user_id}"),
        true,
        activity_id,
    );
    Ok((serde_json::to_value(event)?, target))
}

fn scim_group_privilege_event(
    actor: ActorInput,
    group_id: Uuid,
    user_id: Uuid,
    relation: &str,
    object: &str,
    activity_id: i32,
) -> Result<(Value, String), EmitError> {
    let resource_type = object
        .split_once(':')
        .map_or("authorization_object", |pair| pair.0);
    let mut event = authorization_event(actor, object, resource_type, relation, true, activity_id);
    event.privileges.extend([
        format!("subject:operator:{user_id}"),
        format!("source:scim_group:{group_id}"),
    ]);
    Ok((serde_json::to_value(event)?, object.to_string()))
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
    let signing::SignedPayload {
        signing_key_id,
        signature_hex,
        event_jcs,
    } = signing::sign_tx(tx, tenant_id, &value).await?;
    let columns = EventColumns::from_value(&value);
    let id = columns.id;
    insert_rows(
        &mut **tx,
        &[SignedRow {
            columns,
            tenant_id,
            target_resource_uid: target_resource_uid.map(str::to_string),
            event_jcs,
            signature_hex,
            signing_key_id,
        }],
    )
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
    emitter: &crate::AuditEmitter,
    tenant_id: Uuid,
    event: &E,
    target_resource_uid: Option<String>,
) {
    match serde_json::to_value(event) {
        Ok(value) => emitter.enqueue(crate::audit_sink::QueuedAudit {
            tenant_id,
            value,
            target_resource_uid,
        }),
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

/// Inputs for one prompt-injection circuit finding.
///
/// The owner supplies both the identity and the occurrence time. Neither is
/// generated here: a replayed owner must reproduce a byte-identical event, and
/// anything read from the clock or a fresh UUID inside this function would make
/// the second attempt look like a different finding.
#[derive(Debug, Clone)]
pub struct PromptInjectionFinding {
    /// Session that owns the transition.
    pub session_id: Uuid,
    /// Exact transition the owner applied.
    pub transition: SecurityCircuitTransition,
    /// Stable detector signals behind the triggering assessment.
    pub signals: Vec<InjectionSignal>,
    /// Timestamp the owner journaled before applying the transition.
    pub occurred_at: DateTime<Utc>,
}

/// Outcome of persisting one prompt-injection finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingWrite {
    /// This call inserted the finding.
    Inserted,
    /// The finding already existed and matched byte for byte.
    ReplayMatched,
}

/// Emits one signed OCSF Detection Finding for a circuit transition.
///
/// Identity is UUIDv5 over the transition key, so a crash-and-replay writes the
/// same primary key rather than a second row. On a primary-key conflict this
/// does not silently succeed: it loads the existing row and requires the same
/// tenant, occurrence timestamp, and canonical JCS payload, and verifies the
/// stored signature using **the key the row was signed with** — looked up by the
/// row's own `signing_key_id`, not the tenant's currently active key — so a
/// rotation between the original write and the replay cannot turn a genuine
/// match into a spurious drift error. Any real difference is a replay conflict,
/// which means two different transitions collided on one identity and must be
/// surfaced rather than absorbed.
pub async fn emit_prompt_injection_finding(
    pool: &PgPool,
    tenant_id: TenantId,
    finding: PromptInjectionFinding,
) -> Result<(Uuid, FindingWrite), EmitError> {
    let expected_key = transition_key(TransitionKeyInput {
        session_id: SessionId(finding.session_id),
        owner: &finding.transition.owner,
        capability: &finding.transition.capability,
        tool_call_id: finding.transition.tool_call_id,
        prior_stage: finding.transition.prior_stage,
        reached_stage: finding.transition.reached_stage,
    });
    if finding.transition.key != expected_key {
        return Err(EmitError::InvalidInput(
            "prompt-injection transition key does not match its coordinates".to_string(),
        ));
    }
    let event_id = finding.transition.event_uuid();
    let event = detection_finding_event(&finding);
    let value = serde_json::to_value(&event)?;
    let columns = EventColumns::from_value(&value);

    let mut tx = pool.begin().await?;
    let signing::SignedPayload {
        signing_key_id,
        signature_hex,
        event_jcs,
    } = signing::sign_tx(&mut tx, tenant_id.0, &value).await?;
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO security_events
            (id, tenant_id, class_uid, activity_id, category_uid, severity_id,
             type_uid, actor_user_uid, actor_session_uid, target_resource_uid,
             event_jcs, signature_hex, signing_key_id, occurred_at,
             retrieval_operation_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, NULL)
        ON CONFLICT (id) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(event_id)
    .bind(tenant_id.0)
    .bind(columns.class_uid)
    .bind(columns.activity_id)
    .bind(columns.category_uid)
    .bind(columns.severity_id)
    .bind(columns.type_uid)
    .bind(Option::<String>::None)
    .bind(Some(finding.session_id.to_string()))
    .bind(finding.transition.capability.render())
    .bind(&event_jcs)
    .bind(&signature_hex)
    .bind(signing_key_id)
    .bind(finding.occurred_at)
    .fetch_optional(&mut *tx)
    .await?;

    if inserted.is_some() {
        tx.commit().await?;
        return Ok((event_id, FindingWrite::Inserted));
    }

    let existing: (Uuid, DateTime<Utc>, Vec<u8>, String, Uuid) = sqlx::query_as(
        "SELECT tenant_id, occurred_at, event_jcs, signature_hex, signing_key_id \
         FROM security_events WHERE id = $1",
    )
    .bind(event_id)
    .fetch_one(&mut *tx)
    .await?;

    let (stored_tenant, stored_occurred_at, stored_jcs, stored_signature, stored_key_id) = existing;
    // Verified on the transaction we already hold. The owner is blocked
    // synchronously on this call, so acquiring a second pool connection here
    // would double the pool footprint of one logical write and, under
    // saturation, could deadlock against this very transaction.
    let signature_matches =
        signing::verify_tx(&mut tx, stored_key_id, &stored_jcs, &stored_signature).await?;
    tx.commit().await?;

    if stored_tenant != tenant_id.0 {
        return Err(EmitError::ReplayConflict(
            "an existing finding with this identity belongs to another tenant".to_string(),
        ));
    }
    if stored_occurred_at != finding.occurred_at {
        return Err(EmitError::ReplayConflict(
            "an existing finding with this identity has a different occurrence time".to_string(),
        ));
    }
    if stored_jcs != event_jcs {
        return Err(EmitError::ReplayConflict(
            "an existing finding with this identity has a different canonical payload".to_string(),
        ));
    }
    // Resolved by the row's own `signing_key_id`. Using the tenant's currently
    // active key would report drift for every finding written before the most
    // recent rotation.
    if !signature_matches {
        return Err(EmitError::ReplayConflict(
            "an existing finding with this identity failed signature verification".to_string(),
        ));
    }
    Ok((event_id, FindingWrite::ReplayMatched))
}

/// Builds the Detection Finding for one circuit transition.
fn detection_finding_event(finding: &PromptInjectionFinding) -> DetectionFindingEvent {
    let transition = &finding.transition;
    let severity_id = finding_severity_id(transition.reached_stage);
    DetectionFindingEvent {
        class_uid: class_uid::DETECTION_FINDING,
        class_name: "Detection Finding".to_string(),
        category_uid: category_uid::FINDINGS,
        category_name: "Findings".to_string(),
        type_uid: type_uid(class_uid::DETECTION_FINDING, detection_activity::CREATE),
        activity_id: detection_activity::CREATE,
        activity_name: "Create".to_string(),
        severity_id,
        severity: severity_label(severity_id).to_string(),
        time: finding.occurred_at,
        metadata: metadata(),
        session: Session {
            uid: finding.session_id.to_string(),
            created_time: None,
        },
        resource: Resource {
            uid: transition.capability.render(),
            name: None,
            resource_type: "tool_capability".to_string(),
        },
        finding_info: FindingInfo {
            uid: transition.key.clone(),
            title: PROMPT_INJECTION_FINDING_TITLE.to_string(),
            desc: PROMPT_INJECTION_FINDING_DESC.to_string(),
            analytic: transition.detector_revision.clone(),
        },
        circuit: PromptInjectionCircuit {
            owner_kind: transition.owner.kind().to_string(),
            owner_generation: transition.owner.generation(),
            capability: transition.capability.render(),
            tool_call_uid: transition.tool_call_id.0.to_string(),
            assessment_class: transition.class.as_str().to_string(),
            detector_revision: transition.detector_revision.clone(),
            prior_stage: transition.prior_stage.as_str().to_string(),
            reached_stage: transition.reached_stage.as_str().to_string(),
            prior_score: transition.prior_score,
            reached_score: transition.reached_score,
            signals: finding
                .signals
                .iter()
                .map(|signal| signal.as_str().to_string())
                .collect(),
        },
    }
}

/// Fixed safe finding title. Never derived from output.
const PROMPT_INJECTION_FINDING_TITLE: &str = "Prompt-injection security circuit transition";

/// Fixed safe finding description. Never derived from output.
const PROMPT_INJECTION_FINDING_DESC: &str = "A tool output was classified as a prompt-injection or restricted-material result and \
     advanced the owning agent's security circuit to a new stage.";

/// Maps a reached stage to its deterministic OCSF severity.
///
/// Deterministic rather than configurable so the same transition always produces
/// the same signed bytes on replay.
const fn finding_severity_id(stage: SecurityCircuitStage) -> i32 {
    match stage {
        SecurityCircuitStage::Clear => severity_id::INFORMATIONAL,
        SecurityCircuitStage::Warned => severity_id::LOW,
        SecurityCircuitStage::Disabled => severity_id::MEDIUM,
        SecurityCircuitStage::SuspendedForInput => severity_id::HIGH,
        SecurityCircuitStage::Halted => severity_id::CRITICAL,
    }
}
