//! Tenant-owned MCP connection bindings and their credential ownership scopes.
//!
//! An MCP server is invoked with exactly one owner's credential, and which owner
//! is a configuration decision, not a runtime guess:
//!
//! - [`McpServerCredentialScope::DeploymentOwned`] servers use one operator
//!   credential read from deployment environment for every tenant.
//! - [`McpServerCredentialScope::TenantOwned`] servers require the invoking
//!   tenant to hold a *binding*: a secret-free row naming the tenant's own
//!   connection, the exact stored credential version, and the closed set of
//!   operations that credential may be used for.
//!
//! This module owns the binding table (`tenant_mcp_connection_bindings`) and
//! the two narrow ports the dispatch path needs — the binding owner and the
//! delegated tenant-operator authorizer. Neither the binding row nor anything in
//! this module ever holds credential material: a binding names an opaque
//! [`CredentialRef`], which only the trusted MCP credential proxy can resolve.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    error::MoaError, error::Result, traits::CredentialVault, traits::Identity,
    types::credentials::CredentialContext, types::credentials::CredentialIdentity,
    types::credentials::CredentialKind, types::credentials::CredentialOperation,
    types::credentials::CredentialPrincipal, types::credentials::CredentialRef,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::memory::RlsContext,
};
use moa_db::ScopedConn;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Credential ownership scope of one dispatched tool invocation.
///
/// Every invocation carries one of these three values, derived from the
/// registered tool rather than supplied by a caller: a model, a worker, or a
/// replayed durable request cannot choose which credential owner its call is
/// served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCredentialScope {
    /// A built-in or hand-routed tool. No MCP credential is involved.
    NonMcp,
    /// An MCP tool on a server served by one deployment-owned credential.
    DeploymentOwnedMcp,
    /// An MCP tool on a server served by the invoking tenant's own credential.
    TenantOwnedMcp,
}

impl ToolCredentialScope {
    /// Returns the stable telemetry/audit name for this scope.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonMcp => "non_mcp",
            Self::DeploymentOwnedMcp => "deployment_owned_mcp",
            Self::TenantOwnedMcp => "tenant_owned_mcp",
        }
    }

    /// Returns the invocation scope implied by an MCP server's ownership scope.
    #[must_use]
    pub fn for_server(scope: moa_config::McpServerCredentialScope) -> Self {
        match scope {
            moa_config::McpServerCredentialScope::DeploymentOwned => Self::DeploymentOwnedMcp,
            moa_config::McpServerCredentialScope::TenantOwned => Self::TenantOwnedMcp,
        }
    }
}

/// Lifecycle state of one tenant MCP connection binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TenantMcpBindingStatus {
    /// The binding may serve dispatches.
    Active,
    /// The binding is retained for operator history and never dispatches.
    Disabled,
}

impl TenantMcpBindingStatus {
    /// Returns the stable stored name for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    /// Parses a stored status, rejecting anything outside the closed set.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// One tenant's binding of an MCP server to a tenant-owned connection credential.
///
/// The row is secret-free by construction: `credential_ref` is the opaque handle
/// minted by the durable credential vault, resolvable only inside the trusted MCP
/// proxy under this tenant's own row-level-security context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantMcpConnectionBinding {
    /// Tenant that owns the binding and the credential it names.
    pub tenant_id: TenantId,
    /// Tenant connection whose credential serves this server.
    pub connection_uid: Uuid,
    /// Configured MCP server name this binding serves.
    pub server_name: String,
    /// Exact stored credential version presented to the server.
    pub credential_ref: CredentialRef,
    /// Whether the binding may serve dispatches.
    pub status: TenantMcpBindingStatus,
    /// Closed allowlist of canonical operations this credential may be used for.
    pub allowed_operations: Vec<String>,
}

impl TenantMcpConnectionBinding {
    /// Returns whether this binding permits one canonical operation.
    ///
    /// The allowlist is closed and matched exactly: an operation the operator did
    /// not list is denied, and there is no wildcard, prefix, or case-insensitive
    /// form that could widen it.
    #[must_use]
    pub fn permits(&self, operation: &str) -> bool {
        self.allowed_operations
            .iter()
            .any(|allowed| allowed == operation)
    }

    /// Returns the credential identity this binding's reference must resolve to.
    ///
    /// Used by the trusted proxy to reject a reference that has drifted onto a
    /// different connection or material kind before any plaintext is opened.
    #[must_use]
    pub fn credential_identity(&self) -> CredentialIdentity {
        CredentialIdentity {
            tenant_id: self.tenant_id,
            connection_uid: self.connection_uid,
            // MCP credential material is stored under one kind regardless of
            // which header presents it; the header shape is server configuration,
            // not a property of the stored secret.
            kind: CredentialKind::McpBearer,
        }
    }
}

/// Durable owner of tenant MCP connection bindings.
#[async_trait]
pub trait TenantMcpConnectionBindingStore: Send + Sync {
    /// Loads the binding that serves `server_name` for one tenant, if any.
    ///
    /// Returns the tenant's active binding when one exists and otherwise the most
    /// recently updated disabled binding, so a disabled connection is reported as
    /// disabled instead of being indistinguishable from an unknown one. Callers
    /// must still require [`TenantMcpBindingStatus::Active`]: this is a read, not
    /// an authorization.
    async fn binding_for_server(
        &self,
        tenant_id: TenantId,
        server_name: &str,
    ) -> Result<Option<TenantMcpConnectionBinding>>;

    /// Creates or replaces one tenant's binding for a `(connection, server)` pair.
    async fn upsert_binding(&self, binding: &TenantMcpConnectionBinding) -> Result<()>;
}

/// Authorization port consulted before a tenant's MCP binding is read.
///
/// Implemented over delegated OpenFGA authorization by runtime composition. It
/// exists as a port so tool routing does not depend on the authorization engine,
/// and so a deployment that configures a tenant-owned MCP server without an
/// authorizer fails when the router is built rather than at dispatch.
#[async_trait]
pub trait TenantMcpAuthorizer: Send + Sync {
    /// Requires `identity` to hold the tenant-operator relation on `tenant_id`,
    /// including the delegation check when an agent acts on a user's behalf.
    async fn require_tenant_operator(&self, identity: &Identity, tenant_id: TenantId)
    -> Result<()>;
}

/// The complete set of owners required to serve tenant-owned MCP servers.
///
/// All three are required together: resolving a tenant credential without its
/// binding, or reading a binding without authorizing the caller, is not a
/// degraded mode this deployment is allowed to run in.
#[derive(Clone)]
pub struct TenantMcpCredentialOwners {
    /// Durable tenant credential owner backing the trusted MCP proxy.
    pub vault: Arc<dyn CredentialVault>,
    /// Durable binding owner.
    pub bindings: Arc<dyn TenantMcpConnectionBindingStore>,
    /// Delegated tenant-operator authorizer.
    pub authorizer: Arc<dyn TenantMcpAuthorizer>,
}

/// Builds the replay-stable credential context for one tenant-owned MCP call.
///
/// `operation_id` is derived from the durable tool-call identity, so a Restate
/// replay of the same tool call replays one audit row instead of appending a new
/// one, and two different calls can never share an audit identity.
///
/// `request_hash` covers the full selector this dispatch resolved against. A
/// replayed tool call whose binding has since been repointed at a different
/// credential therefore fails as a typed idempotency conflict rather than
/// silently resolving different material under an identity that says otherwise;
/// the next call, which has its own tool-call identity, uses the new version.
pub(crate) fn tenant_resolve_context(
    binding: &TenantMcpConnectionBinding,
    operation: &str,
    tool_call_id: ToolCallId,
    caller_identity: &Identity,
) -> CredentialContext {
    CredentialContext {
        tenant_id: binding.tenant_id,
        principal: CredentialPrincipal::Caller {
            identity_id: caller_identity.id,
            delegated_by: caller_identity.acting_on_behalf_of,
        },
        operation: CredentialOperation::Resolve,
        operation_id: format!("mcp-tool-call:{tool_call_id}"),
        request_hash: tenant_resolve_request_hash(binding, operation),
    }
}

/// Hashes the exact selector one tenant-owned MCP resolve was performed against.
fn tenant_resolve_request_hash(binding: &TenantMcpConnectionBinding, operation: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        binding.tenant_id.to_string().as_str(),
        binding.connection_uid.to_string().as_str(),
        binding.server_name.as_str(),
        operation,
        binding.credential_ref.to_string().as_str(),
    ] {
        hasher.update(field.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

/// Postgres-backed tenant MCP connection binding owner.
///
/// Every statement runs inside a tenant-scoped transaction that assumes the
/// `moa_app` role, so the table's forced row-level-security policy — not this
/// code — is what makes another tenant's binding unreadable.
#[derive(Clone)]
pub struct PostgresTenantMcpConnectionBindings {
    pool: PgPool,
}

impl PostgresTenantMcpConnectionBindings {
    /// Creates a binding owner over an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Begins a tenant-scoped, `moa_app`-role transaction for one tenant.
    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await
    }
}

#[async_trait]
impl TenantMcpConnectionBindingStore for PostgresTenantMcpConnectionBindings {
    async fn binding_for_server(
        &self,
        tenant_id: TenantId,
        server_name: &str,
    ) -> Result<Option<TenantMcpConnectionBinding>> {
        let mut conn = self.begin(tenant_id).await?;
        // At most one active binding per (tenant, server) exists — the partial
        // unique index guarantees it — so ordering active first selects that row
        // deterministically, and falls back to the newest disabled binding only
        // when the tenant has no active one.
        let row = sqlx::query(
            r#"
            SELECT tenant_id, connection_uid, server_name, credential_ref, status, allowed_operations
            FROM tenant_mcp_connection_bindings
            WHERE tenant_id = $1 AND server_name = $2
            ORDER BY (status = 'active') DESC, updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(tenant_id.0)
        .bind(server_name)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_storage_error)?;
        conn.commit().await?;

        row.map(|row| binding_from_row(&row)).transpose()
    }

    async fn upsert_binding(&self, binding: &TenantMcpConnectionBinding) -> Result<()> {
        let mut conn = self.begin(binding.tenant_id).await?;
        sqlx::query(
            r#"
            INSERT INTO tenant_mcp_connection_bindings (
                tenant_id, connection_uid, server_name, credential_ref, status, allowed_operations
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (tenant_id, connection_uid, server_name) DO UPDATE
            SET credential_ref = EXCLUDED.credential_ref,
                status = EXCLUDED.status,
                allowed_operations = EXCLUDED.allowed_operations,
                updated_at = now()
            "#,
        )
        .bind(binding.tenant_id.0)
        .bind(binding.connection_uid)
        .bind(binding.server_name.as_str())
        .bind(binding.credential_ref.as_uuid())
        .bind(binding.status.as_str())
        .bind(&binding.allowed_operations)
        .execute(conn.as_mut())
        .await
        .map_err(map_storage_error)?;
        conn.commit().await
    }
}

impl PostgresTenantMcpConnectionBindings {
    /// Removes up to `limit` of one tenant's bindings, returning how many were
    /// removed so the tenant-purge lifecycle can loop until the tenant owns none.
    ///
    /// This is deliberately not part of [`TenantMcpConnectionBindingStore`]:
    /// tool routing never deletes a binding, and the purge workflow holds this
    /// concrete owner because forced row-level security means only an
    /// `moa_app`-scoped transaction can see the rows to delete.
    pub async fn purge_tenant_bindings(&self, tenant_id: TenantId, limit: u32) -> Result<u64> {
        let mut conn = self.begin(tenant_id).await?;
        let removed = sqlx::query(
            r#"
            DELETE FROM tenant_mcp_connection_bindings
            WHERE ctid IN (
                SELECT ctid
                FROM tenant_mcp_connection_bindings
                WHERE tenant_id = $1
                LIMIT $2
            )
            "#,
        )
        .bind(tenant_id.0)
        .bind(i64::from(limit))
        .execute(conn.as_mut())
        .await
        .map_err(map_storage_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(removed)
    }
}

/// Rebuilds one binding from its stored row, rejecting values outside the closed
/// status set rather than treating an unknown status as usable.
fn binding_from_row(row: &sqlx::postgres::PgRow) -> Result<TenantMcpConnectionBinding> {
    let status: String = row.try_get("status").map_err(map_storage_error)?;
    let status = TenantMcpBindingStatus::from_str_exact(&status).ok_or_else(|| {
        MoaError::StorageError(format!("unknown MCP connection binding status: {status}"))
    })?;
    let tenant_id: Uuid = row.try_get("tenant_id").map_err(map_storage_error)?;
    let credential_ref: Uuid = row.try_get("credential_ref").map_err(map_storage_error)?;
    Ok(TenantMcpConnectionBinding {
        tenant_id: TenantId::from(tenant_id),
        connection_uid: row.try_get("connection_uid").map_err(map_storage_error)?,
        server_name: row.try_get("server_name").map_err(map_storage_error)?,
        credential_ref: CredentialRef::from_uuid(credential_ref),
        status,
        allowed_operations: row
            .try_get("allowed_operations")
            .map_err(map_storage_error)?,
    })
}

fn map_storage_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(operations: &[&str]) -> TenantMcpConnectionBinding {
        TenantMcpConnectionBinding {
            tenant_id: TenantId::new(),
            connection_uid: Uuid::new_v4(),
            server_name: "external-search".to_string(),
            credential_ref: CredentialRef::from_uuid(Uuid::new_v4()),
            status: TenantMcpBindingStatus::Active,
            allowed_operations: operations.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    #[test]
    fn binding_operation_allowlist_is_closed_and_exact() {
        // Pins: the allowlist matches operation names exactly — no wildcard, no
        // prefix, and no case-folded form can widen what a tenant credential is
        // allowed to be used for.
        let binding = binding(&["search_documents"]);

        assert!(binding.permits("search_documents"));
        assert!(!binding.permits("Search_Documents"));
        assert!(!binding.permits("search"));
        assert!(!binding.permits("search_documents_admin"));
        assert!(!binding.permits("*"));
        assert!(!binding.permits(""));
    }

    #[test]
    fn binding_credential_identity_binds_tenant_and_connection() {
        // Pins: the identity a resolved reference is checked against carries the
        // binding's own tenant and connection, so a reference that has drifted
        // onto another connection cannot satisfy it.
        let binding = binding(&["search_documents"]);

        let identity = binding.credential_identity();

        assert_eq!(identity.tenant_id, binding.tenant_id);
        assert_eq!(identity.connection_uid, binding.connection_uid);
        assert_eq!(identity.kind, CredentialKind::McpBearer);
    }

    #[test]
    fn stored_binding_status_outside_the_closed_set_is_rejected() {
        // Pins: an unrecognized stored status is a storage error, never a value
        // that could be treated as usable.
        assert_eq!(
            TenantMcpBindingStatus::from_str_exact("active"),
            Some(TenantMcpBindingStatus::Active)
        );
        assert_eq!(
            TenantMcpBindingStatus::from_str_exact("disabled"),
            Some(TenantMcpBindingStatus::Disabled)
        );
        assert_eq!(TenantMcpBindingStatus::from_str_exact("enabled"), None);
        assert_eq!(TenantMcpBindingStatus::from_str_exact(""), None);
    }
}
