//! Tenant-scoped persistence for connector connections, bindings, and invocations.

use async_trait::async_trait;
use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::types::action_policy::ActionPolicyEffect;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_db::ScopedConn;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use std::num::NonZeroU64;
use std::str::FromStr;
use uuid::Uuid;

use crate::catalog::InstalledConnectorCatalogSource;
use crate::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth,
    ConnectionOrigin, ConnectionStatus, ConnectorConnection, ConnectorInvocationId,
    ConnectorInvocationRecord, ConnectorInvocationState, ConnectorInvocationTerminal,
    InstalledActionBinding, InstalledActionBindingId, ManagedParentClaim, ManagedParentDefinition,
    ManagedParentDeleteOutcome, ManagedParentPreservationReason, OperationContractHash,
};
use crate::service::{
    ManagedParentActivationRequest, ManagedParentClaimRequest, ManagedParentDeleteRequest,
};
use crate::{Error, Result};

/// Values required to create one tenant-owned connector connection.
#[derive(Clone, Debug)]
pub struct NewConnectorConnection {
    /// Replay-stable identity selected by the caller.
    pub connection_id: ConnectorConnectionId,
    /// Owning tenant and RLS boundary.
    pub tenant_id: TenantId,
    /// Human-readable connection label.
    pub display_name: String,
    /// Exact artifact revision or built-in version installed by this connection.
    pub definition_ref: ConnectionDefinitionRef,
    /// Secret-free runtime configuration.
    pub non_secret_config: Value,
    /// Identity that initiated creation, when the caller is a durable identity.
    pub created_by_identity_id: Option<Uuid>,
    /// Operator that directly owns the OpenFGA resource.
    pub owner_identity_id: Uuid,
}

/// Atomic activation write after the application service compiles and verifies a definition.
#[derive(Clone, Debug)]
pub struct ConnectionActivation {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Connection being activated.
    pub connection_id: ConnectorConnectionId,
    /// Generation observed before credential verification and contract compilation.
    pub expected_generation: ConnectionGeneration,
    /// Immutable bindings for the next connection generation.
    pub bindings: Vec<InstalledActionBinding>,
}

/// Closed same-tenant subject eligible for a direct connector `Use` relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorUseSubject {
    /// Tenant operator identity stored in `public.users`.
    Operator {
        /// Existing operator UUID.
        id: Uuid,
    },
    /// Tenant agent identity stored in `public.agents`.
    Agent {
        /// Existing agent UUID.
        id: Uuid,
    },
    /// Tenant contact identity stored in `public.contacts`.
    Contact {
        /// Existing contact UUID.
        id: Uuid,
    },
}

impl ConnectorUseSubject {
    const fn id(self) -> Uuid {
        match self {
            Self::Operator { id } | Self::Agent { id } | Self::Contact { id } => id,
        }
    }

    const fn kind(self) -> &'static str {
        match self {
            Self::Operator { .. } => "operator",
            Self::Agent { .. } => "agent",
            Self::Contact { .. } => "contact",
        }
    }

    const fn user_type(self) -> UserType {
        match self {
            Self::Operator { .. } => UserType::Operator,
            Self::Agent { .. } => UserType::Agent,
            Self::Contact { .. } => UserType::Contact,
        }
    }

    fn tuple(self, connection_id: ConnectorConnectionId) -> TupleKey {
        TupleKey::new(
            self.user_type(),
            self.id(),
            Relation::Use,
            ObjectType::ConnectorConnection,
            connection_id.0,
        )
    }
}

/// Request to change one direct connector `Use` relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionUseRequest {
    /// Tenant whose RLS scope owns both the connection and subject.
    pub tenant_id: TenantId,
    /// Connection receiving or losing the direct relationship.
    pub connection_id: ConnectorConnectionId,
    /// Closed same-tenant subject.
    pub subject: ConnectorUseSubject,
}

/// Request to reserve one replay-stable connector action invocation.
#[derive(Clone, Debug)]
pub struct InvocationReservationRequest {
    /// Replay-stable invocation row identity.
    pub invocation_id: ConnectorInvocationId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Installed connection being called.
    pub connection_id: ConnectorConnectionId,
    /// Exact installed binding being called.
    pub binding_id: InstalledActionBindingId,
    /// Generation whose immutable binding admitted the call.
    pub connection_generation: ConnectionGeneration,
    /// Stable tool-call identity used as the replay key.
    pub tool_call_id: String,
    /// Canonical request hash; reuse with a different hash is a conflict.
    pub request_hash: OperationContractHash,
    /// Optional upstream idempotency value approved by the operation contract.
    pub upstream_idempotency_key: Option<String>,
}

/// Outcome of reserving a tool-call replay key.
#[derive(Clone, Debug, PartialEq)]
pub enum InvocationReservation {
    /// This call inserted the row and is the only caller allowed to transmit it.
    Reserved(ConnectorInvocationRecord),
    /// The same request already reached a terminal state; return that exact record.
    Replay(ConnectorInvocationRecord),
    /// The same request is reserved or transmitting, so redispatch is unsafe.
    InFlight(ConnectorInvocationRecord),
}

/// Persistence contract for tenant connector lifecycle and replay state.
#[async_trait]
pub trait ConnectionRepository: Send + Sync {
    /// Creates a pending connection and its tenant/owner authorization intents atomically.
    async fn create(&self, request: NewConnectorConnection) -> Result<ConnectorConnection>;

    /// Loads one tenant-scoped connection.
    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>>;

    /// Lists tenant connections in deterministic connection-ID order.
    async fn list(&self, tenant_id: TenantId) -> Result<Vec<ConnectorConnection>>;

    /// Atomically claims or creates one exact code-owned managed parent.
    async fn claim_managed_parent(
        &self,
        _request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim> {
        Err(Error::ManagedParentRepositoryUnavailable)
    }

    /// Loads one exact installed binding under its tenant and connection identity.
    async fn load_binding(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
    ) -> Result<Option<InstalledActionBinding>>;

    /// Applies one valid lifecycle transition under generation compare-and-swap.
    async fn transition(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        target: ConnectionStatus,
    ) -> Result<ConnectorConnection>;

    /// Updates health independently from lifecycle under generation compare-and-swap.
    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> Result<ConnectorConnection>;

    /// Advances the credential fence after a credential write commits.
    async fn advance_credential_generation(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection>;

    /// Atomically replaces catalog-visible bindings and activates a connection.
    async fn activate(&self, request: ConnectionActivation) -> Result<ConnectorConnection>;

    /// Activates an exact knowledge-only managed parent without changing generation.
    async fn activate_managed_knowledge_parent(
        &self,
        _request: ManagedParentActivationRequest,
    ) -> Result<ConnectorConnection> {
        Err(Error::ManagedParentRepositoryUnavailable)
    }

    /// Deletes an exact claim-created managed parent only when no capability depends on it.
    async fn delete_managed_parent_if_unused(
        &self,
        _request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome> {
        Err(Error::ManagedParentRepositoryUnavailable)
    }

    /// Reserves a replay key before any external request is sent.
    async fn reserve_invocation(
        &self,
        request: InvocationReservationRequest,
    ) -> Result<InvocationReservation>;

    /// Loads one exact invocation for post-journal completion-ticket validation.
    async fn load_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<Option<ConnectorInvocationRecord>>;

    /// Transfers a reserved invocation to transport exactly once before sending.
    async fn mark_transmitting(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<ConnectorInvocationRecord>;

    /// Moves a reserved or transmitting invocation to one allowed terminal state exactly once.
    async fn finish_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
        terminal: ConnectorInvocationTerminal,
    ) -> Result<ConnectorInvocationRecord>;
}

/// Persistence contract for direct connector `Use` relationship desired state.
#[async_trait]
pub trait ConnectionUseGrantRepository: Send + Sync {
    /// Grants one direct relationship after validating an active same-tenant subject.
    async fn grant_use(&self, request: ConnectionUseRequest) -> Result<()>;

    /// Revokes one direct relationship after validating the subject remains same-tenant.
    async fn revoke_use(&self, request: ConnectionUseRequest) -> Result<()>;
}

/// Postgres implementation using tenant RLS and the transactional OpenFGA outbox.
///
/// The pool role must be the repository owner role granted membership in
/// `moa_app`: every connector-table operation assumes `moa_app`, while create
/// and delete restore the owning role only after their connector writes so the
/// intentionally owner-only outbox intent can commit in the same transaction.
#[derive(Clone)]
pub struct PostgresConnectionRepository {
    pool: PgPool,
}

impl PostgresConnectionRepository {
    /// Creates a repository from the owning pool used by MOA runtime composition.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin(
            &self.pool,
            &moa_core::types::memory::RlsContext::tenant(tenant_id),
        )
        .await?;
        conn.assume_app_role().await?;
        Ok(conn)
    }
}

#[derive(FromRow)]
struct ConnectionRow {
    connection_uid: Uuid,
    tenant_id: Uuid,
    display_name: String,
    artifact_uid: Option<Uuid>,
    revision_uid: Option<Uuid>,
    built_in_key: Option<String>,
    built_in_version: Option<i64>,
    non_secret_config: Value,
    config_generation: i64,
    lifecycle_status: String,
    health_status: String,
    health_reason: Option<String>,
    created_by_identity_id: Option<Uuid>,
    owner_identity_id: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow)]
struct BindingRow {
    binding_uid: Uuid,
    tenant_id: Uuid,
    connection_uid: Uuid,
    action_id: String,
    connection_generation: i64,
    compiled_contract: Value,
    contract_hash: String,
    governed_contract_revision: String,
    minimum_effect: String,
    enabled: bool,
}

#[derive(FromRow)]
struct InvocationRow {
    invocation_uid: Uuid,
    tenant_id: Uuid,
    connection_uid: Uuid,
    binding_uid: Uuid,
    connection_generation: i64,
    tool_call_id: String,
    request_hash: String,
    upstream_idempotency_key: Option<String>,
    state: String,
    error_metadata: Option<Value>,
    output_metadata: Option<Value>,
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow)]
struct ConnectionUseGrantRow {
    subject_kind: String,
    subject_id: Uuid,
}

#[derive(FromRow)]
struct ManagedParentClaimRow {
    request_hash: String,
    connection_uid: Uuid,
    parent_created_by_claim: bool,
}

const CONNECTION_COLUMNS: &str = "connection_uid, tenant_id, display_name, artifact_uid, revision_uid, built_in_key, \
     built_in_version, non_secret_config, config_generation, lifecycle_status, health_status, \
     health_reason, created_by_identity_id, owner_identity_id, created_at, updated_at";

const INVOCATION_COLUMNS: &str = "invocation_uid, tenant_id, connection_uid, binding_uid, connection_generation, \
     tool_call_id, request_hash, upstream_idempotency_key, state, error_metadata, \
     output_metadata, started_at, completed_at, updated_at";

#[async_trait]
impl ConnectionRepository for PostgresConnectionRepository {
    async fn create(&self, mut request: NewConnectorConnection) -> Result<ConnectorConnection> {
        validate_new_connection(&mut request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let definition = definition_columns(&request.definition_ref)?;
        let query = format!(
            "INSERT INTO moa.connector_connections (connection_uid, tenant_id, display_name, \
             artifact_uid, revision_uid, built_in_key, built_in_version, non_secret_config, \
             created_by_identity_id, owner_identity_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.connection_id.0)
            .bind(request.tenant_id.0)
            .bind(&request.display_name)
            .bind(definition.artifact_uid)
            .bind(definition.revision_uid)
            .bind(definition.built_in_key)
            .bind(definition.built_in_version)
            .bind(&request.non_secret_config)
            .bind(request.created_by_identity_id)
            .bind(request.owner_identity_id)
            .fetch_one(conn.as_mut())
            .await?;

        // The connector row is now fully written under forced tenant RLS. The
        // shared authz outbox is intentionally owner-only, so restore the pool's
        // owning role without leaving this transaction; no connector table is
        // accessed after this boundary.
        assume_owner_role(&mut conn).await?;

        enqueue_raw(
            conn.as_mut(),
            TupleOp::Write,
            &format!("tenant:{}", request.tenant_id),
            "tenant",
            &format!("connector_connection:{}", request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        let owner = TupleKey::new(
            UserType::Operator,
            request.owner_identity_id,
            Relation::Owner,
            ObjectType::ConnectorConnection,
            request.connection_id.0,
        );
        enqueue(
            conn.as_mut(),
            TupleOp::Write,
            &owner,
            Some(request.tenant_id.0),
        )
        .await?;

        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 AND connection_uid = $2"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        conn.commit().await?;
        row.map(connection_from_row).transpose()
    }

    async fn list(&self, tenant_id: TenantId) -> Result<Vec<ConnectorConnection>> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 ORDER BY connection_uid"
        );
        let rows = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .fetch_all(conn.as_mut())
            .await?;
        let connections = rows
            .into_iter()
            .map(connection_from_row)
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(connections)
    }

    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim> {
        validate_managed_parent_claim_request(&request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        if let Some(claim) =
            load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id).await?
        {
            validate_managed_parent_claim_identity(&request, &claim)?;
            let parent =
                lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
            validate_managed_parent_compatibility(&request, &parent, true)?;
            conn.commit().await?;
            return Ok(ManagedParentClaim {
                connection: parent,
                parent_created_by_claim: claim.parent_created_by_claim,
            });
        }

        let existing =
            lock_connection_optional(&mut conn, request.tenant_id, request.connection_id).await?;
        let (parent, parent_created_by_claim) = match existing {
            Some(parent) => {
                validate_managed_parent_compatibility(&request, &parent, false)?;
                (parent, false)
            }
            None => {
                let owner_identity_id =
                    request
                        .owner_identity_id
                        .ok_or(Error::ManagedParentOwnerRequired {
                            connection_id: request.connection_id,
                        })?;
                insert_managed_parent(&mut conn, &request, owner_identity_id).await?
            }
        };

        let inserted_claim: Option<ManagedParentClaimRow> = sqlx::query_as(
            "INSERT INTO moa.connector_managed_parent_claims (tenant_id, operation_id, \
             request_hash, connection_uid, parent_created_by_claim) VALUES ($1,$2,$3,$4,$5) \
             ON CONFLICT (tenant_id, operation_id) DO NOTHING \
             RETURNING request_hash, connection_uid, parent_created_by_claim",
        )
        .bind(request.tenant_id.0)
        .bind(&request.operation_id)
        .bind(&request.request_hash)
        .bind(request.connection_id.0)
        .bind(parent_created_by_claim)
        .fetch_optional(conn.as_mut())
        .await?;

        let durable_claim = match inserted_claim {
            Some(claim) => claim,
            None => load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id)
                .await?
                .ok_or(Error::ManagedParentClaimConflict {
                    connection_id: request.connection_id,
                })?,
        };
        validate_managed_parent_claim_identity(&request, &durable_claim)?;

        if parent_created_by_claim {
            let owner_identity_id =
                request
                    .owner_identity_id
                    .ok_or(Error::ManagedParentOwnerRequired {
                        connection_id: request.connection_id,
                    })?;
            assume_owner_role(&mut conn).await?;
            enqueue_connection_authz_create(
                &mut conn,
                request.tenant_id,
                request.connection_id,
                owner_identity_id,
            )
            .await?;
        }
        conn.commit().await?;
        Ok(ManagedParentClaim {
            connection: parent,
            parent_created_by_claim: durable_claim.parent_created_by_claim,
        })
    }

    async fn load_binding(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
    ) -> Result<Option<InstalledActionBinding>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query_as::<_, BindingRow>(
            "SELECT binding_uid, tenant_id, connection_uid, action_id, connection_generation, \
             compiled_contract, contract_hash, governed_contract_revision, minimum_effect, enabled \
             FROM moa.connector_action_bindings WHERE tenant_id = $1 AND connection_uid = $2 \
             AND binding_uid = $3",
        )
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .bind(binding_id.0)
        .fetch_optional(conn.as_mut())
        .await?;
        conn.commit().await?;
        row.map(binding_from_row).transpose()
    }

    async fn transition(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        target: ConnectionStatus,
    ) -> Result<ConnectorConnection> {
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        if target == ConnectionStatus::Active && current.status == ConnectionStatus::PendingAuth {
            return Err(Error::InvalidContract {
                message: "initial connection activation must compile bindings and verify credential slots"
                    .to_string(),
            });
        }
        current.status.transition(target)?;
        if target == ConnectionStatus::Active
            && !has_enabled_current_binding(&mut conn, tenant_id, connection_id, current.generation)
                .await?
        {
            return Err(Error::InvalidContract {
                message: "connection has no enabled binding for its current credential generation"
                    .to_string(),
            });
        }
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = $3, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $4 \
             AND lifecycle_status = $5 RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(target.as_str())
            .bind(generation_i64(expected_generation)?)
            .bind(current.status.as_str())
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            })?;

        if matches!(
            target,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted
        ) {
            sqlx::query(
                "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
            )
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .execute(conn.as_mut())
            .await?;
        }
        if target == ConnectionStatus::Deleted {
            let grants = take_connection_use_grants(&mut conn, tenant_id, connection_id).await?;
            // All connector reads and writes completed under `moa_app`. The
            // exact inverse intents must share this transaction, while the
            // outbox deliberately remains unavailable to the application role.
            assume_owner_role(&mut conn).await?;
            enqueue_connection_authz_delete(
                &mut conn,
                tenant_id,
                connection_id,
                current.owner_identity_id,
                &grants,
            )
            .await?;
        }
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> Result<ConnectorConnection> {
        validate_health_reason(reason.as_deref())?;
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        let query = format!(
            "UPDATE moa.connector_connections SET health_status = $3, health_reason = $4, \
             updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $5 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(health.as_str())
            .bind(reason)
            .bind(generation_i64(expected_generation)?)
            .fetch_optional(conn.as_mut())
            .await?;
        let row = match row {
            Some(row) => row,
            None => {
                return Err(Error::GenerationConflict {
                    expected: expected_generation,
                    actual: current.generation,
                });
            }
        };
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn advance_credential_generation(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        let next_generation = expected_generation.next()?;
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        let target = match current.status {
            ConnectionStatus::Active => ConnectionStatus::Suspended,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended => current.status,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted => {
                return Err(Error::InvalidContract {
                    message:
                        "credential generation cannot advance while connection teardown is active"
                            .to_string(),
                });
            }
        };

        sqlx::query(
            "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
        )
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .execute(conn.as_mut())
        .await?;
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = $3, config_generation = $4, \
             updated_at = NOW() WHERE tenant_id = $1 AND connection_uid = $2 \
             AND config_generation = $5 AND lifecycle_status = $6 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(target.as_str())
            .bind(generation_i64(next_generation)?)
            .bind(generation_i64(expected_generation)?)
            .bind(current.status.as_str())
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            })?;
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn activate(&self, request: ConnectionActivation) -> Result<ConnectorConnection> {
        let next = request.expected_generation.next()?;
        validate_activation_bindings(&request, next)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        check_generation(current.generation, request.expected_generation)?;
        if !matches!(
            current.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended
        ) {
            return Err(Error::InvalidContract {
                message: "compiled connector activation requires pending_auth or suspended"
                    .to_string(),
            });
        }
        current.status.transition(ConnectionStatus::Active)?;

        sqlx::query(
            "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .execute(conn.as_mut())
        .await?;
        for binding in &request.bindings {
            insert_binding(&mut conn, binding).await?;
        }
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'active', \
             config_generation = $3, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $4 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(generation_i64(next)?)
            .bind(generation_i64(request.expected_generation)?)
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: request.expected_generation,
                actual: current.generation,
            })?;
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> Result<ConnectorConnection> {
        let mut conn = self.begin(request.tenant_id).await?;
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        check_generation(current.generation, request.expected_generation)?;
        validate_managed_parent_definition(
            request.connection_id,
            &current.definition,
            request.definition,
        )?;
        if has_any_action_binding(&mut conn, request.tenant_id, request.connection_id).await? {
            return Err(Error::ManagedParentActionDependents {
                connection_id: request.connection_id,
            });
        }
        if current.status == ConnectionStatus::Active {
            conn.commit().await?;
            return Ok(current);
        }
        if !matches!(
            current.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended
        ) {
            return Err(Error::InvalidTransition {
                from: current.status,
                to: ConnectionStatus::Active,
            });
        }
        current.status.transition(ConnectionStatus::Active)?;
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'active', updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $3 \
             AND lifecycle_status = $4 RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(generation_i64(request.expected_generation)?)
            .bind(current.status.as_str())
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: request.expected_generation,
                actual: current.generation,
            })?;
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome> {
        validate_managed_parent_claim_identity_fields(
            request.connection_id,
            &request.operation_id,
            &request.request_hash,
        )?;
        let mut conn = self.begin(request.tenant_id).await?;
        let claim = load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id)
            .await?
            .ok_or(Error::ManagedParentClaimConflict {
                connection_id: request.connection_id,
            })?;
        if claim.connection_uid != request.connection_id.0
            || claim.request_hash != request.request_hash
        {
            return Err(Error::ManagedParentClaimConflict {
                connection_id: request.connection_id,
            });
        }
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        validate_any_closed_managed_parent(request.connection_id, &current.definition)?;
        if !claim.parent_created_by_claim {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::Preserved {
                connection: current,
                reason: ManagedParentPreservationReason::PreExisting,
            });
        }
        if current.status == ConnectionStatus::Deleted {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::AlreadyDeleted(current));
        }
        if has_managed_parent_dependents(
            &mut conn,
            request.tenant_id,
            request.connection_id,
            &request.operation_id,
        )
        .await?
        {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::Preserved {
                connection: current,
                reason: ManagedParentPreservationReason::DependentCapability,
            });
        }

        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'deleted', updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND lifecycle_status <> 'deleted' \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .fetch_one(conn.as_mut())
            .await?;
        let deleted = connection_from_row(row)?;
        assume_owner_role(&mut conn).await?;
        enqueue_connection_authz_delete(
            &mut conn,
            request.tenant_id,
            request.connection_id,
            current.owner_identity_id,
            &[],
        )
        .await?;
        conn.commit().await?;
        Ok(ManagedParentDeleteOutcome::Deleted(deleted))
    }

    async fn reserve_invocation(
        &self,
        request: InvocationReservationRequest,
    ) -> Result<InvocationReservation> {
        validate_invocation_request(&request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let query = format!(
            "INSERT INTO moa.connector_action_invocations (invocation_uid, tenant_id, \
             connection_uid, binding_uid, connection_generation, tool_call_id, request_hash, \
             upstream_idempotency_key) \
             SELECT $1,$2,$3,$4,$5,$6,$7,$8 \
             FROM moa.connector_action_bindings AS binding \
             JOIN moa.connector_connections AS connection \
               ON connection.connection_uid = binding.connection_uid \
              AND connection.tenant_id = binding.tenant_id \
              AND connection.config_generation = binding.connection_generation \
             WHERE binding.tenant_id = $2 AND binding.connection_uid = $3 \
               AND binding.binding_uid = $4 AND binding.connection_generation = $5 \
               AND binding.enabled AND connection.lifecycle_status = 'active' \
             ON CONFLICT (tenant_id, tool_call_id) DO NOTHING RETURNING {INVOCATION_COLUMNS}"
        );
        let inserted = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(request.invocation_id.0)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(request.binding_id.0)
            .bind(generation_i64(request.connection_generation)?)
            .bind(&request.tool_call_id)
            .bind(request.request_hash.to_string())
            .bind(&request.upstream_idempotency_key)
            .fetch_optional(conn.as_mut())
            .await?;
        let reservation = if let Some(row) = inserted {
            InvocationReservation::Reserved(invocation_from_row(row)?)
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND tool_call_id = $2"
            );
            let existing = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(request.tenant_id.0)
                .bind(&request.tool_call_id)
                .fetch_optional(conn.as_mut())
                .await?
                .ok_or_else(|| Error::CatalogInvariant {
                    message:
                        "connector invocation binding is not active, enabled, and current-generation"
                            .to_string(),
                })
                .and_then(invocation_from_row)?;
            if existing.request_hash != request.request_hash
                || existing.connection_id != request.connection_id
                || existing.binding_id != request.binding_id
                || existing.connection_generation != request.connection_generation
                || existing.upstream_idempotency_key != request.upstream_idempotency_key
            {
                return Err(Error::InvocationConflict {
                    tool_call_id: request.tool_call_id,
                });
            }
            if existing.state.is_terminal() {
                InvocationReservation::Replay(existing)
            } else {
                InvocationReservation::InFlight(existing)
            }
        };
        conn.commit().await?;
        Ok(reservation)
    }

    async fn load_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<Option<ConnectorInvocationRecord>> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
             WHERE tenant_id = $1 AND invocation_uid = $2"
        );
        let row = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        conn.commit().await?;
        row.map(invocation_from_row).transpose()
    }

    async fn mark_transmitting(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<ConnectorInvocationRecord> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "UPDATE moa.connector_action_invocations SET state = 'transmitting', updated_at = NOW() \
             WHERE tenant_id = $1 AND invocation_uid = $2 AND state = 'reserved' \
             AND EXISTS ( \
                 SELECT 1 FROM moa.connector_action_bindings AS binding \
                 JOIN moa.connector_connections AS connection \
                   ON connection.connection_uid = binding.connection_uid \
                  AND connection.tenant_id = binding.tenant_id \
                  AND connection.config_generation = binding.connection_generation \
                 WHERE binding.binding_uid = moa.connector_action_invocations.binding_uid \
                   AND binding.tenant_id = moa.connector_action_invocations.tenant_id \
                   AND binding.connection_uid = moa.connector_action_invocations.connection_uid \
                   AND binding.connection_generation = moa.connector_action_invocations.connection_generation \
                   AND binding.enabled AND connection.lifecycle_status = 'active' \
             ) \
             RETURNING {INVOCATION_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        let result = if let Some(row) = updated {
            invocation_from_row(row)?
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND invocation_uid = $2"
            );
            let existing = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(tenant_id.0)
                .bind(invocation_id.0)
                .fetch_optional(conn.as_mut())
                .await?
                .map(invocation_from_row)
                .transpose()?
                .ok_or(Error::InvocationStateConflict {
                    invocation_id,
                    from: ConnectorInvocationState::Reserved,
                    to: ConnectorInvocationState::Transmitting,
                })?;
            return Err(Error::InvocationStateConflict {
                invocation_id,
                from: existing.state,
                to: ConnectorInvocationState::Transmitting,
            });
        };
        conn.commit().await?;
        Ok(result)
    }

    async fn finish_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
        terminal: ConnectorInvocationTerminal,
    ) -> Result<ConnectorInvocationRecord> {
        validate_terminal_metadata(&terminal)?;
        let target = terminal.state();
        let source = if target == ConnectorInvocationState::FailedBeforeSend {
            ConnectorInvocationState::Reserved
        } else {
            ConnectorInvocationState::Transmitting
        };
        source.transition(invocation_id, target)?;
        let (error_metadata, output_metadata) = terminal_metadata(&terminal);
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "UPDATE moa.connector_action_invocations SET state = $3, error_metadata = $4, \
             output_metadata = $5, completed_at = NOW(), updated_at = NOW() \
             WHERE tenant_id = $1 AND invocation_uid = $2 AND state = $6 \
             RETURNING {INVOCATION_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .bind(target.as_str())
            .bind(error_metadata)
            .bind(output_metadata)
            .bind(source.as_str())
            .fetch_optional(conn.as_mut())
            .await?;
        let result = if let Some(row) = updated {
            invocation_from_row(row)?
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND invocation_uid = $2"
            );
            let row = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(tenant_id.0)
                .bind(invocation_id.0)
                .fetch_optional(conn.as_mut())
                .await?
                .ok_or(Error::InvocationStateConflict {
                    invocation_id,
                    from: source,
                    to: target,
                })?;
            let existing = invocation_from_row(row)?;
            if terminal_matches(&existing, &terminal) {
                existing
            } else {
                return Err(Error::InvocationStateConflict {
                    invocation_id,
                    from: existing.state,
                    to: target,
                });
            }
        };
        conn.commit().await?;
        Ok(result)
    }
}

#[async_trait]
impl ConnectionUseGrantRepository for PostgresConnectionRepository {
    async fn grant_use(&self, request: ConnectionUseRequest) -> Result<()> {
        let mut conn = self.begin(request.tenant_id).await?;
        let connection =
            lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        if matches!(
            connection.status,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted
        ) {
            return Err(Error::UseGrantConnectionUnavailable {
                connection_id: request.connection_id,
                status: connection.status,
            });
        }
        validate_use_subject(&mut conn, request.tenant_id, request.subject, true).await?;
        sqlx::query(
            "INSERT INTO moa.connector_connection_use_grants \
             (tenant_id, connection_uid, subject_kind, subject_id) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, connection_uid, subject_kind, subject_id) DO NOTHING",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .bind(request.subject.kind())
        .bind(request.subject.id())
        .execute(conn.as_mut())
        .await?;

        assume_owner_role(&mut conn).await?;
        enqueue(
            conn.as_mut(),
            TupleOp::Write,
            &request.subject.tuple(request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        conn.commit().await?;
        Ok(())
    }

    async fn revoke_use(&self, request: ConnectionUseRequest) -> Result<()> {
        let mut conn = self.begin(request.tenant_id).await?;
        lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        validate_use_subject(&mut conn, request.tenant_id, request.subject, false).await?;
        sqlx::query(
            "DELETE FROM moa.connector_connection_use_grants \
             WHERE tenant_id = $1 AND connection_uid = $2 \
               AND subject_kind = $3 AND subject_id = $4",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .bind(request.subject.kind())
        .bind(request.subject.id())
        .execute(conn.as_mut())
        .await?;

        assume_owner_role(&mut conn).await?;
        enqueue(
            conn.as_mut(),
            TupleOp::Delete,
            &request.subject.tuple(request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        conn.commit().await?;
        Ok(())
    }
}

fn validate_new_connection(request: &mut NewConnectorConnection) -> Result<()> {
    if request.display_name.trim().is_empty() || request.display_name.trim() != request.display_name
    {
        return Err(Error::InvalidContract {
            message: "connector display name must be non-empty and trimmed".to_string(),
        });
    }
    if !request.non_secret_config.is_object() {
        return Err(Error::InvalidContract {
            message: "connector non-secret config must be a JSON object".to_string(),
        });
    }
    if let Some(origin) = request.non_secret_config.get_mut("origin") {
        let raw = origin.as_str().ok_or(Error::InvalidConnectionOrigin {
            reason: "origin configuration must be a string",
        })?;
        let canonical = ConnectionOrigin::parse(raw)?;
        *origin = Value::String(canonical.to_string());
    }
    Ok(())
}

fn validate_managed_parent_claim_request(request: &ManagedParentClaimRequest) -> Result<()> {
    validate_managed_parent_claim_identity_fields(
        request.connection_id,
        &request.operation_id,
        &request.request_hash,
    )?;
    if request.display_name.trim().is_empty()
        || request.display_name.trim() != request.display_name
        || request.display_name.len() > 512
    {
        return Err(Error::InvalidContract {
            message:
                "managed parent display name must be non-empty, trimmed, and at most 512 bytes"
                    .to_string(),
        });
    }
    for (field, value) in [
        ("provider_config_key", request.provider_config_key.as_str()),
        (
            "provider_connection_id",
            request.provider_connection_id.as_str(),
        ),
        ("connector", request.connector.as_str()),
    ] {
        if value.trim().is_empty() || value.trim() != value || value.len() > 1_024 {
            return Err(Error::InvalidContract {
                message: format!(
                    "managed parent {field} must be non-empty, trimmed, and at most 1024 bytes"
                ),
            });
        }
    }
    Ok(())
}

fn validate_managed_parent_claim_identity_fields(
    _connection_id: ConnectorConnectionId,
    operation_id: &str,
    request_hash: &str,
) -> Result<()> {
    if operation_id.trim().is_empty()
        || operation_id.trim() != operation_id
        || operation_id.len() > 512
    {
        return Err(Error::InvalidContract {
            message:
                "managed parent operation id must be non-empty, trimmed, and at most 512 bytes"
                    .to_string(),
        });
    }
    if request_hash.len() != 64
        || !request_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidContract {
            message: "managed parent request hash must be 64 lowercase hexadecimal characters"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_managed_parent_claim_identity(
    request: &ManagedParentClaimRequest,
    claim: &ManagedParentClaimRow,
) -> Result<()> {
    if claim.connection_uid != request.connection_id.0 || claim.request_hash != request.request_hash
    {
        return Err(Error::ManagedParentClaimConflict {
            connection_id: request.connection_id,
        });
    }
    Ok(())
}

fn validate_managed_parent_definition(
    connection_id: ConnectorConnectionId,
    actual: &ConnectionDefinitionRef,
    expected: ManagedParentDefinition,
) -> Result<()> {
    if *actual == expected.definition_ref() {
        Ok(())
    } else {
        Err(Error::ManagedParentMismatch {
            connection_id,
            field: "definition",
        })
    }
}

fn validate_any_closed_managed_parent(
    connection_id: ConnectorConnectionId,
    actual: &ConnectionDefinitionRef,
) -> Result<()> {
    if [
        ManagedParentDefinition::KnowledgeNangoV1,
        ManagedParentDefinition::KnowledgeMergeV1,
    ]
    .into_iter()
    .any(|definition| actual == &definition.definition_ref())
    {
        Ok(())
    } else {
        Err(Error::ManagedParentMismatch {
            connection_id,
            field: "definition",
        })
    }
}

fn validate_managed_parent_compatibility(
    request: &ManagedParentClaimRequest,
    connection: &ConnectorConnection,
    exact_replay: bool,
) -> Result<()> {
    validate_managed_parent_definition(
        request.connection_id,
        &connection.definition,
        request.definition,
    )?;
    let lifecycle_compatible = if exact_replay {
        matches!(
            connection.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Active | ConnectionStatus::Suspended
        )
    } else {
        connection.status == ConnectionStatus::Active
    };
    if !lifecycle_compatible {
        return Err(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "lifecycle_status",
        });
    }
    let config = connection
        .non_secret_config
        .as_object()
        .ok_or(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "non_secret_config",
        })?;
    let expected = [
        ("provider_config_key", request.provider_config_key.as_str()),
        (
            "provider_connection_id",
            request.provider_connection_id.as_str(),
        ),
        ("connector", request.connector.as_str()),
    ];
    for (field, value) in expected {
        if config.get(field).and_then(Value::as_str) != Some(value) {
            return Err(Error::ManagedParentMismatch {
                connection_id: request.connection_id,
                field,
            });
        }
    }
    if config.keys().any(|key| {
        !matches!(
            key.as_str(),
            "provider_config_key" | "provider_connection_id" | "connector" | "source_selection"
        )
    }) {
        return Err(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "non_secret_config",
        });
    }
    Ok(())
}

async fn load_managed_parent_claim(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<Option<ManagedParentClaimRow>> {
    sqlx::query_as(
        "SELECT request_hash, connection_uid, parent_created_by_claim \
         FROM moa.connector_managed_parent_claims \
         WHERE tenant_id = $1 AND operation_id = $2 FOR UPDATE",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(Error::from)
}

async fn insert_managed_parent(
    conn: &mut ScopedConn<'_>,
    request: &ManagedParentClaimRequest,
    owner_identity_id: Uuid,
) -> Result<(ConnectorConnection, bool)> {
    let config = serde_json::json!({
        "provider_config_key": request.provider_config_key,
        "provider_connection_id": request.provider_connection_id,
        "connector": request.connector,
    });
    let query = format!(
        "INSERT INTO moa.connector_connections (connection_uid, tenant_id, display_name, \
         built_in_key, built_in_version, non_secret_config, created_by_identity_id, \
         owner_identity_id) VALUES ($1,$2,$3,$4,1,$5,$6,$6) \
         ON CONFLICT (connection_uid) DO NOTHING RETURNING {CONNECTION_COLUMNS}"
    );
    let inserted = sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(request.connection_id.0)
        .bind(request.tenant_id.0)
        .bind(&request.display_name)
        .bind(request.definition.key())
        .bind(config)
        .bind(owner_identity_id)
        .fetch_optional(conn.as_mut())
        .await?;
    match inserted {
        Some(row) => Ok((connection_from_row(row)?, true)),
        None => {
            let parent = lock_connection(conn, request.tenant_id, request.connection_id).await?;
            validate_managed_parent_compatibility(request, &parent, false)?;
            Ok((parent, false))
        }
    }
}

fn validate_health_reason(reason: Option<&str>) -> Result<()> {
    if reason.is_some_and(|value| {
        value.trim().is_empty() || value.trim() != value || value.len() > 2_048
    }) {
        return Err(Error::InvalidContract {
            message: "connector health reason must be non-empty, trimmed, and at most 2048 bytes"
                .to_string(),
        });
    }
    Ok(())
}

struct DefinitionColumns {
    artifact_uid: Option<Uuid>,
    revision_uid: Option<Uuid>,
    built_in_key: Option<String>,
    built_in_version: Option<i64>,
}

fn definition_columns(definition: &ConnectionDefinitionRef) -> Result<DefinitionColumns> {
    match definition {
        ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        } => Ok(DefinitionColumns {
            artifact_uid: Some(*artifact_uid),
            revision_uid: Some(*revision_uid),
            built_in_key: None,
            built_in_version: None,
        }),
        ConnectionDefinitionRef::BuiltIn { key, version } => Ok(DefinitionColumns {
            artifact_uid: None,
            revision_uid: None,
            built_in_key: Some(key.clone()),
            built_in_version: Some(i64::try_from(version.get()).map_err(|_| {
                Error::InvalidContract {
                    message: "built-in connector version exceeds Postgres BIGINT".to_string(),
                }
            })?),
        }),
    }
}

fn generation_i64(generation: ConnectionGeneration) -> Result<i64> {
    i64::try_from(generation.get()).map_err(|_| Error::InvalidContract {
        message: "connector generation exceeds Postgres BIGINT".to_string(),
    })
}

fn generation_from_i64(value: i64) -> Result<ConnectionGeneration> {
    u64::try_from(value)
        .map_err(|_| Error::InvalidGeneration { value: 0 })
        .and_then(ConnectionGeneration::new)
}

fn connection_from_row(row: ConnectionRow) -> Result<ConnectorConnection> {
    let definition = match (
        row.artifact_uid,
        row.revision_uid,
        row.built_in_key,
        row.built_in_version,
    ) {
        (Some(artifact_uid), Some(revision_uid), None, None) => ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        },
        (None, None, Some(key), Some(version)) => ConnectionDefinitionRef::BuiltIn {
            key,
            version: NonZeroU64::new(u64::try_from(version).map_err(|_| {
                Error::InvalidContract {
                    message: "persisted built-in connector version is invalid".to_string(),
                }
            })?)
            .ok_or_else(|| Error::InvalidContract {
                message: "persisted built-in connector version is invalid".to_string(),
            })?,
        },
        _ => {
            return Err(Error::InvalidContract {
                message: "persisted connector definition reference is inconsistent".to_string(),
            });
        }
    };
    let health = parse_connection_health(&row.health_status)?;
    Ok(ConnectorConnection {
        connection_id: ConnectorConnectionId(row.connection_uid),
        tenant_id: TenantId::from(row.tenant_id),
        display_name: row.display_name,
        definition,
        non_secret_config: row.non_secret_config,
        generation: generation_from_i64(row.config_generation)?,
        status: parse_connection_status(&row.lifecycle_status)?,
        health,
        health_reason: row.health_reason,
        created_by_identity_id: row.created_by_identity_id,
        owner_identity_id: row.owner_identity_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn parse_connection_status(value: &str) -> Result<ConnectionStatus> {
    match value {
        "pending_auth" => Ok(ConnectionStatus::PendingAuth),
        "active" => Ok(ConnectionStatus::Active),
        "suspended" => Ok(ConnectionStatus::Suspended),
        "disconnecting" => Ok(ConnectionStatus::Disconnecting),
        "deleted" => Ok(ConnectionStatus::Deleted),
        _ => Err(Error::InvalidContract {
            message: "persisted connector lifecycle state is unknown".to_string(),
        }),
    }
}

fn parse_connection_health(value: &str) -> Result<ConnectionHealth> {
    match value {
        "pending" => Ok(ConnectionHealth::Pending),
        "ready" => Ok(ConnectionHealth::Ready),
        "degraded" => Ok(ConnectionHealth::Degraded),
        "unavailable" => Ok(ConnectionHealth::Unavailable),
        "quarantined" => Ok(ConnectionHealth::Quarantined),
        _ => Err(Error::InvalidContract {
            message: "persisted connector health state is unknown".to_string(),
        }),
    }
}

async fn lock_connection(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<ConnectorConnection> {
    let query = format!(
        "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
         WHERE tenant_id = $1 AND connection_uid = $2 FOR UPDATE"
    );
    let row = sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .fetch_optional(conn.as_mut())
        .await?
        .ok_or(Error::ConnectionNotFound { connection_id })?;
    connection_from_row(row)
}

async fn lock_connection_optional(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<Option<ConnectorConnection>> {
    let query = format!(
        "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
         WHERE tenant_id = $1 AND connection_uid = $2 FOR UPDATE"
    );
    sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .fetch_optional(conn.as_mut())
        .await?
        .map(connection_from_row)
        .transpose()
}

async fn assume_owner_role(conn: &mut ScopedConn<'_>) -> Result<()> {
    sqlx::query("RESET ROLE")
        .execute(conn.as_mut())
        .await
        .map(|_| ())
        .map_err(Error::from)
}

async fn has_enabled_current_binding(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    generation: ConnectionGeneration,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2 \
         AND connection_generation = $3 AND enabled)",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .bind(generation_i64(generation)?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}

async fn has_any_action_binding(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2)",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}

async fn has_managed_parent_dependents(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operation_id: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT \
           EXISTS (SELECT 1 FROM moa.connector_action_bindings \
                   WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.connector_connection_use_grants \
                      WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.knowledge_connections \
                      WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.knowledge_link_claims \
                      WHERE tenant_id = $1 AND connection_uid = $2 AND operation_id <> $3 \
                        AND state NOT IN ('compensated', 'finalized'))",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .bind(operation_id)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}

fn check_generation(actual: ConnectionGeneration, expected: ConnectionGeneration) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::GenerationConflict { expected, actual })
    }
}

async fn enqueue_connection_authz_delete(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Option<Uuid>,
    use_subjects: &[ConnectorUseSubject],
) -> Result<()> {
    enqueue_raw(
        conn.as_mut(),
        TupleOp::Delete,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &format!("connector_connection:{connection_id}"),
        Some(tenant_id.0),
    )
    .await?;
    if let Some(owner_identity_id) = owner_identity_id {
        let owner = TupleKey::new(
            UserType::Operator,
            owner_identity_id,
            Relation::Owner,
            ObjectType::ConnectorConnection,
            connection_id.0,
        );
        enqueue(conn.as_mut(), TupleOp::Delete, &owner, Some(tenant_id.0)).await?;
    }
    for subject in use_subjects {
        enqueue(
            conn.as_mut(),
            TupleOp::Delete,
            &subject.tuple(connection_id),
            Some(tenant_id.0),
        )
        .await?;
    }
    Ok(())
}

async fn enqueue_connection_authz_create(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Uuid,
) -> Result<()> {
    enqueue_raw(
        conn.as_mut(),
        TupleOp::Write,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &format!("connector_connection:{connection_id}"),
        Some(tenant_id.0),
    )
    .await?;
    let owner = TupleKey::new(
        UserType::Operator,
        owner_identity_id,
        Relation::Owner,
        ObjectType::ConnectorConnection,
        connection_id.0,
    );
    enqueue(conn.as_mut(), TupleOp::Write, &owner, Some(tenant_id.0)).await?;
    Ok(())
}

async fn validate_use_subject(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    subject: ConnectorUseSubject,
    require_active: bool,
) -> Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT moa.connector_use_subject_exists($1, $2, $3)")
        .bind(tenant_id.0)
        .bind(subject.kind())
        .bind(subject.id())
        .fetch_one(conn.as_mut())
        .await?;
    if !exists {
        return Err(Error::UseGrantSubjectNotFound {
            subject_kind: subject.kind(),
            subject_id: subject.id(),
        });
    }
    let eligible = if require_active {
        sqlx::query_scalar("SELECT moa.connector_use_subject_is_eligible($1, $2, $3)")
            .bind(tenant_id.0)
            .bind(subject.kind())
            .bind(subject.id())
            .fetch_one(conn.as_mut())
            .await?
    } else {
        true
    };
    if !eligible {
        return Err(Error::UseGrantSubjectInactive {
            subject_kind: subject.kind(),
            subject_id: subject.id(),
        });
    }
    Ok(())
}

async fn take_connection_use_grants(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<Vec<ConnectorUseSubject>> {
    let rows = sqlx::query_as::<_, ConnectionUseGrantRow>(
        "SELECT subject_kind, subject_id FROM moa.connector_connection_use_grants \
         WHERE tenant_id = $1 AND connection_uid = $2 \
         ORDER BY subject_kind, subject_id",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_all(conn.as_mut())
    .await?;
    let subjects = rows
        .into_iter()
        .map(|row| match row.subject_kind.as_str() {
            "operator" => Ok(ConnectorUseSubject::Operator { id: row.subject_id }),
            "agent" => Ok(ConnectorUseSubject::Agent { id: row.subject_id }),
            "contact" => Ok(ConnectorUseSubject::Contact { id: row.subject_id }),
            _ => Err(Error::CatalogInvariant {
                message: "persisted connector Use subject kind is unknown".to_string(),
            }),
        })
        .collect::<Result<Vec<_>>>()?;
    sqlx::query(
        "DELETE FROM moa.connector_connection_use_grants \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .execute(conn.as_mut())
    .await?;
    Ok(subjects)
}

fn validate_activation_bindings(
    request: &ConnectionActivation,
    expected_generation: ConnectionGeneration,
) -> Result<()> {
    if request.bindings.is_empty() {
        return Err(Error::InvalidContract {
            message: "connector activation requires at least one action binding".to_string(),
        });
    }
    let mut action_ids = std::collections::BTreeSet::new();
    for binding in &request.bindings {
        binding.validate()?;
        if binding.minimum_effect != binding.compiled_contract.operation.policy().minimum_effect {
            return Err(Error::InvalidContract {
                message: "connector binding minimum effect differs from compiled policy"
                    .to_string(),
            });
        }
        if binding.tenant_id != request.tenant_id
            || binding.connection_id != request.connection_id
            || binding.connection_generation != expected_generation
            || !binding.enabled
        {
            return Err(Error::InvalidContract {
                message: "connector activation binding identity or generation is inconsistent"
                    .to_string(),
            });
        }
        if !action_ids.insert(binding.action_id.as_str()) {
            return Err(Error::InvalidContract {
                message: "connector activation contains a duplicate action id".to_string(),
            });
        }
    }
    Ok(())
}

async fn insert_binding(conn: &mut ScopedConn<'_>, binding: &InstalledActionBinding) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.connector_action_bindings (binding_uid, tenant_id, connection_uid, \
         action_id, connection_generation, compiled_contract, contract_hash, \
         governed_contract_revision, minimum_effect, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(binding.binding_id.0)
    .bind(binding.tenant_id.0)
    .bind(binding.connection_id.0)
    .bind(&binding.action_id)
    .bind(generation_i64(binding.connection_generation)?)
    .bind(serde_json::to_value(&binding.compiled_contract)?)
    .bind(binding.contract_hash.to_string())
    .bind(&binding.governed_contract_revision)
    .bind(binding.minimum_effect.as_str())
    .bind(binding.enabled)
    .execute(conn.as_mut())
    .await?;
    Ok(())
}

fn binding_from_row(row: BindingRow) -> Result<InstalledActionBinding> {
    let binding = InstalledActionBinding {
        binding_id: InstalledActionBindingId(row.binding_uid),
        tenant_id: TenantId::from(row.tenant_id),
        connection_id: ConnectorConnectionId(row.connection_uid),
        connection_generation: generation_from_i64(row.connection_generation)?,
        action_id: row.action_id,
        compiled_contract: serde_json::from_value::<CompiledOperationContract>(
            row.compiled_contract,
        )?,
        contract_hash: OperationContractHash::from_str(&row.contract_hash)?,
        governed_contract_revision: row.governed_contract_revision,
        minimum_effect: ActionPolicyEffect::from_str(&row.minimum_effect).map_err(|_| {
            Error::InvalidContract {
                message: "persisted connector minimum effect is unknown".to_string(),
            }
        })?,
        enabled: row.enabled,
    };
    binding.validate()?;
    Ok(binding)
}

fn validate_invocation_request(request: &InvocationReservationRequest) -> Result<()> {
    if request.tool_call_id.trim().is_empty()
        || request.tool_call_id.trim() != request.tool_call_id
        || request.tool_call_id.len() > 512
    {
        return Err(Error::InvalidContract {
            message: "connector tool-call id must be non-empty, trimmed, and at most 512 bytes"
                .to_string(),
        });
    }
    if request
        .upstream_idempotency_key
        .as_deref()
        .is_some_and(|key| key.trim().is_empty() || key.trim() != key || key.len() > 512)
    {
        return Err(Error::InvalidContract {
            message: "upstream idempotency key must be non-empty, trimmed, and at most 512 bytes"
                .to_string(),
        });
    }
    Ok(())
}

fn invocation_from_row(row: InvocationRow) -> Result<ConnectorInvocationRecord> {
    Ok(ConnectorInvocationRecord {
        invocation_id: ConnectorInvocationId(row.invocation_uid),
        tenant_id: TenantId::from(row.tenant_id),
        connection_id: ConnectorConnectionId(row.connection_uid),
        binding_id: InstalledActionBindingId(row.binding_uid),
        connection_generation: generation_from_i64(row.connection_generation)?,
        tool_call_id: row.tool_call_id,
        request_hash: OperationContractHash::from_str(&row.request_hash)?,
        upstream_idempotency_key: row.upstream_idempotency_key,
        state: parse_invocation_state(&row.state)?,
        error_metadata: row.error_metadata,
        output_metadata: row.output_metadata,
        started_at: row.started_at,
        completed_at: row.completed_at,
        updated_at: row.updated_at,
    })
}

fn parse_invocation_state(value: &str) -> Result<ConnectorInvocationState> {
    match value {
        "reserved" => Ok(ConnectorInvocationState::Reserved),
        "transmitting" => Ok(ConnectorInvocationState::Transmitting),
        "succeeded" => Ok(ConnectorInvocationState::Succeeded),
        "failed_before_send" => Ok(ConnectorInvocationState::FailedBeforeSend),
        "failed" => Ok(ConnectorInvocationState::Failed),
        "unknown_outcome" => Ok(ConnectorInvocationState::UnknownOutcome),
        _ => Err(Error::InvalidContract {
            message: "persisted connector invocation state is unknown".to_string(),
        }),
    }
}

fn terminal_metadata(terminal: &ConnectorInvocationTerminal) -> (Option<&Value>, Option<&Value>) {
    match terminal {
        ConnectorInvocationTerminal::Succeeded { output_metadata } => (None, Some(output_metadata)),
        ConnectorInvocationTerminal::FailedBeforeSend { error_metadata }
        | ConnectorInvocationTerminal::Failed { error_metadata }
        | ConnectorInvocationTerminal::UnknownOutcome { error_metadata } => {
            (Some(error_metadata), None)
        }
    }
}

fn validate_terminal_metadata(terminal: &ConnectorInvocationTerminal) -> Result<()> {
    let (error_metadata, output_metadata) = terminal_metadata(terminal);
    if error_metadata.is_some_and(|value| !value.is_object())
        || output_metadata.is_some_and(|value| !value.is_object())
    {
        return Err(Error::InvalidContract {
            message: "connector invocation terminal metadata must be a JSON object".to_string(),
        });
    }
    Ok(())
}

fn terminal_matches(
    record: &ConnectorInvocationRecord,
    terminal: &ConnectorInvocationTerminal,
) -> bool {
    if record.state != terminal.state() {
        return false;
    }
    let (error_metadata, output_metadata) = terminal_metadata(terminal);
    record.error_metadata.as_ref() == error_metadata
        && record.output_metadata.as_ref() == output_metadata
}

#[async_trait]
impl InstalledConnectorCatalogSource for PostgresConnectionRepository {
    async fn candidates(
        &self,
        tenant_id: TenantId,
        connection_ids: &[ConnectorConnectionId],
    ) -> Result<Vec<(ConnectorConnection, InstalledActionBinding)>> {
        if connection_ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection_ids = connection_ids
            .iter()
            .map(|connection_id| connection_id.0)
            .collect::<Vec<_>>();
        let mut conn = self.begin(tenant_id).await?;
        let connection_query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 AND connection_uid = ANY($2) AND lifecycle_status = 'active' \
             AND health_status <> 'quarantined'"
        );
        let connections = sqlx::query_as::<_, ConnectionRow>(&connection_query)
            .bind(tenant_id.0)
            .bind(&connection_ids)
            .fetch_all(conn.as_mut())
            .await?
            .into_iter()
            .map(connection_from_row)
            .collect::<Result<Vec<_>>>()?;
        let connection_map = connections
            .into_iter()
            .map(|connection| (connection.connection_id, connection))
            .collect::<std::collections::HashMap<_, _>>();

        let bindings = sqlx::query_as::<_, BindingRow>(
            "SELECT binding.binding_uid, binding.tenant_id, binding.connection_uid, \
             binding.action_id, binding.connection_generation, binding.compiled_contract, \
             binding.contract_hash, binding.governed_contract_revision, binding.minimum_effect, \
             binding.enabled FROM moa.connector_action_bindings AS binding \
             JOIN moa.connector_connections AS connection \
               ON connection.connection_uid = binding.connection_uid \
              AND connection.tenant_id = binding.tenant_id \
              AND connection.config_generation = binding.connection_generation \
             WHERE binding.tenant_id = $1 AND binding.connection_uid = ANY($2) \
               AND binding.enabled AND connection.lifecycle_status = 'active' \
               AND connection.health_status <> 'quarantined'",
        )
        .bind(tenant_id.0)
        .bind(&connection_ids)
        .fetch_all(conn.as_mut())
        .await?;

        let mut candidates = Vec::with_capacity(bindings.len());
        for row in bindings {
            let binding = binding_from_row(row)?;
            let connection = connection_map
                .get(&binding.connection_id)
                .cloned()
                .ok_or_else(|| Error::CatalogInvariant {
                    message: "active binding has no active connection projection".to_string(),
                })?;
            candidates.push((connection, binding));
        }
        conn.commit().await?;
        Ok(candidates)
    }
}
