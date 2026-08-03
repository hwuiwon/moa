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
    /// Canonical HTTP origin for an artifact-backed connector.
    pub origin: Option<ConnectionOrigin>,
    /// Secret-free runtime configuration.
    pub non_secret_config: Value,
    /// Identity that initiated creation, when the caller is a durable identity.
    pub created_by_identity_id: Option<Uuid>,
    /// Operator that directly owns the OpenFGA resource.
    pub owner_identity_id: Uuid,
}

/// Maximum number of connection rows returned by one public page.
pub const MAX_CONNECTION_LIST_LIMIT: u16 = 100;

/// Repository page request for non-deleted tenant connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionListRequest {
    /// Exclusive connection-UUID cursor.
    pub after: Option<ConnectorConnectionId>,
    /// Number of visible rows requested, in `1..=100`.
    pub limit: u16,
}

/// One deterministic page of non-deleted tenant connections.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionListPage {
    /// Visible rows ordered by ascending connection UUID.
    pub connections: Vec<ConnectorConnection>,
    /// Exclusive cursor for the following page when more rows exist.
    pub next_cursor: Option<ConnectorConnectionId>,
}

/// One connection and exact installed binding loaded from the same database snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedConnectorAction {
    /// Tenant-owned connection carrying lifecycle, generation, and definition state.
    pub connection: ConnectorConnection,
    /// Exact immutable action binding selected beneath the connection.
    pub binding: InstalledActionBinding,
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

/// Persistence contract for tenant connector lifecycle and immutable bindings.
#[async_trait]
pub trait ConnectionLifecycleRepository: Send + Sync {
    /// Creates a pending connection and its tenant/owner authorization intents atomically.
    async fn create(&self, request: NewConnectorConnection) -> Result<ConnectorConnection>;

    /// Loads one tenant-scoped connection.
    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>>;

    /// Lists tenant connections in deterministic connection-ID order.
    async fn list(
        &self,
        tenant_id: TenantId,
        request: ConnectionListRequest,
    ) -> Result<ConnectionListPage>;

    /// Loads one connection and exact binding together for the final pre-send pin check.
    async fn load_pinned_action(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
    ) -> Result<Option<PinnedConnectorAction>>;

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
}

/// Persistence contract for closed managed knowledge-parent claims.
#[async_trait]
pub trait ManagedParentRepository: Send + Sync {
    /// Atomically claims or creates one exact code-owned managed parent.
    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim>;

    /// Activates an exact knowledge-only managed parent without changing generation.
    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> Result<ConnectorConnection>;

    /// Deletes an exact claim-created managed parent only when no capability depends on it.
    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome>;
}

/// Persistence contract for the one-way connector invocation ledger.
#[async_trait]
pub trait ConnectorInvocationRepository: Send + Sync {
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

mod catalog_batch;
mod invocation;
mod lifecycle;
mod managed_parents;
mod model;
mod use_grants;
