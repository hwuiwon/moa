//! Tenant-authorized installed connector action catalog boundary.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};

use crate::domain::{
    ConnectionDefinitionRef, ConnectionHealth, ConnectionStatus, ConnectorConnection,
    InstalledActionBinding,
};
use crate::{Error, Result};

/// Scope for one installed-action catalog read.
///
/// Tenant scope is derived exclusively from the authenticated caller. The
/// requested connection identifiers are candidates for delegated
/// `connector_connection#use` authorization; implementations must never treat
/// their presence as proof of authorization.
#[derive(Clone, Debug)]
pub struct InstalledConnectorCatalogQuery {
    /// Authenticated principal whose delegated access governs the projection.
    pub caller: Identity,
    /// Exact connection IDs requested by the catalog consumer.
    pub requested_connection_ids: HashSet<ConnectorConnectionId>,
}

impl InstalledConnectorCatalogQuery {
    /// Builds a catalog query for an authenticated caller and selected connections.
    #[must_use]
    pub fn new(
        caller: Identity,
        requested_connection_ids: impl IntoIterator<Item = ConnectorConnectionId>,
    ) -> Self {
        Self {
            caller,
            requested_connection_ids: requested_connection_ids.into_iter().collect(),
        }
    }

    /// Returns the tenant derived from the authenticated caller.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.caller.tenant_id
    }
}

/// One executable, revision-pinned installed connector action.
///
/// Construction is restricted to [`InstalledConnectorCatalogSnapshot`] so an
/// action cannot become model-visible without passing every catalog fence.
#[derive(Clone, Debug, PartialEq)]
pub struct InstalledConnectorAction {
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    connection_display_name: String,
    definition: ConnectionDefinitionRef,
    binding: InstalledActionBinding,
}

impl InstalledConnectorAction {
    /// Returns the tenant that owns this installed action.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the exact tenant connection identity.
    #[must_use]
    pub const fn connection_id(&self) -> ConnectorConnectionId {
        self.connection_id
    }

    /// Returns the operator-visible account label safe for tool descriptions.
    #[must_use]
    pub fn connection_display_name(&self) -> &str {
        &self.connection_display_name
    }

    /// Returns the immutable artifact revision or built-in definition pin.
    #[must_use]
    pub const fn definition(&self) -> &ConnectionDefinitionRef {
        &self.definition
    }

    /// Returns the compiled action binding pinned by this catalog entry.
    #[must_use]
    pub const fn binding(&self) -> &InstalledActionBinding {
        &self.binding
    }
}

/// Immutable installed-action publication for one tenant and authorization set.
#[derive(Clone, Debug, PartialEq)]
pub struct InstalledConnectorCatalogSnapshot {
    tenant_id: TenantId,
    actions: Vec<InstalledConnectorAction>,
}

impl InstalledConnectorCatalogSnapshot {
    /// Applies tenant, authorization, lifecycle, enabled, and generation fences
    /// to repository candidates and returns one deterministic publication.
    ///
    /// Unauthorized, inactive, and disabled candidates are expected omissions.
    /// Cross-tenant or identity/generation mismatches indicate corrupted adapter
    /// output and fail the complete snapshot closed instead of serving a partial
    /// catalog.
    pub fn from_candidates(
        query: &InstalledConnectorCatalogQuery,
        candidates: impl IntoIterator<Item = (ConnectorConnection, InstalledActionBinding)>,
    ) -> Result<Self> {
        let mut actions = Vec::new();
        let mut identities = HashSet::new();

        for (connection, binding) in candidates {
            validate_candidate(query.tenant_id(), &connection, &binding)?;

            if !query
                .requested_connection_ids
                .contains(&connection.connection_id)
                || connection.status != ConnectionStatus::Active
                || connection.health == ConnectionHealth::Quarantined
                || !binding.enabled
            {
                continue;
            }

            let identity = (connection.connection_id, binding.action_id.clone());
            if !identities.insert(identity) {
                return Err(catalog_invariant(format!(
                    "duplicate active binding for connection {} action {}",
                    connection.connection_id, binding.action_id
                )));
            }
            actions.push(InstalledConnectorAction {
                tenant_id: connection.tenant_id,
                connection_id: connection.connection_id,
                connection_display_name: connection.display_name,
                definition: connection.definition,
                binding,
            });
        }

        actions.sort_by(|left, right| {
            left.connection_id
                .0
                .cmp(&right.connection_id.0)
                .then_with(|| left.binding.action_id.cmp(&right.binding.action_id))
                .then_with(|| left.binding.binding_id.cmp(&right.binding.binding_id))
        });

        Ok(Self {
            tenant_id: query.tenant_id(),
            actions,
        })
    }

    /// Returns the tenant whose authorization scope produced this snapshot.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the deterministic active action entries.
    #[must_use]
    pub fn actions(&self) -> &[InstalledConnectorAction] {
        &self.actions
    }

    /// Returns whether this authorization scope exposes no connector actions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Returns the number of active authorized connector actions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }
}

/// Tenant-installed connector catalog read port.
#[async_trait]
pub trait InstalledConnectorCatalog: Send + Sync {
    /// Returns one immutable snapshot containing only delegated-use-authorized,
    /// active, enabled, current-generation, revision-pinned actions.
    async fn snapshot(
        &self,
        query: InstalledConnectorCatalogQuery,
    ) -> Result<InstalledConnectorCatalogSnapshot>;
}

/// Delegated authorization boundary for using one installed connection.
#[async_trait]
pub trait ConnectorUseAuthorizer: Send + Sync {
    /// Requires `connector_connection#use` for the authenticated caller and
    /// exact connection before any protected connection or binding read.
    async fn require_use(
        &self,
        caller: &Identity,
        connection_id: ConnectorConnectionId,
    ) -> Result<()>;
}

/// OpenFGA-backed delegated-use authorization adapter for installed catalogs.
#[derive(Clone)]
pub struct FgaConnectorUseAuthorizer {
    fga_client: Option<FgaClient>,
}

impl FgaConnectorUseAuthorizer {
    /// Creates a fail-closed connector-use authorizer.
    #[must_use]
    pub const fn new(fga_client: Option<FgaClient>) -> Self {
        Self { fga_client }
    }
}

#[async_trait]
impl ConnectorUseAuthorizer for FgaConnectorUseAuthorizer {
    async fn require_use(
        &self,
        caller: &Identity,
        connection_id: ConnectorConnectionId,
    ) -> Result<()> {
        let Some(fga_client) = self.fga_client.as_ref() else {
            return Err(Error::AuthorizationUnavailable);
        };
        match require_authz_with_delegation(
            fga_client,
            caller,
            ObjectType::ConnectorConnection,
            connection_id,
            Relation::Use,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(AuthzCheckError::Forbidden { .. }) => Err(Error::AuthorizationDenied),
            Err(AuthzCheckError::Engine(_)) => Err(Error::AuthorizationUnavailable),
        }
    }
}

/// Protected persistence source behind the governed catalog adapter.
#[async_trait]
pub trait InstalledConnectorCatalogSource: Send + Sync {
    /// Loads active current-generation candidates for the exact already-authorized IDs.
    async fn candidates(
        &self,
        tenant_id: TenantId,
        connection_ids: &[ConnectorConnectionId],
    ) -> Result<Vec<(ConnectorConnection, InstalledActionBinding)>>;
}

/// Catalog adapter that authorizes every selected connection before protected reads.
pub struct GovernedInstalledConnectorCatalog {
    source: Arc<dyn InstalledConnectorCatalogSource>,
    authorizer: Arc<dyn ConnectorUseAuthorizer>,
}

impl GovernedInstalledConnectorCatalog {
    /// Composes a protected persistence source with delegated-use authorization.
    #[must_use]
    pub fn new(
        source: Arc<dyn InstalledConnectorCatalogSource>,
        authorizer: Arc<dyn ConnectorUseAuthorizer>,
    ) -> Self {
        Self { source, authorizer }
    }
}

#[async_trait]
impl InstalledConnectorCatalog for GovernedInstalledConnectorCatalog {
    async fn snapshot(
        &self,
        query: InstalledConnectorCatalogQuery,
    ) -> Result<InstalledConnectorCatalogSnapshot> {
        let mut connection_ids = query
            .requested_connection_ids
            .iter()
            .copied()
            .collect::<Vec<_>>();
        connection_ids.sort_by_key(|connection_id| connection_id.0);

        for connection_id in &connection_ids {
            self.authorizer
                .require_use(&query.caller, *connection_id)
                .await?;
        }

        let candidates = self
            .source
            .candidates(query.tenant_id(), &connection_ids)
            .await?;
        InstalledConnectorCatalogSnapshot::from_candidates(&query, candidates)
    }
}

fn validate_candidate(
    tenant_id: TenantId,
    connection: &ConnectorConnection,
    binding: &InstalledActionBinding,
) -> Result<()> {
    if connection.tenant_id != tenant_id || binding.tenant_id != tenant_id {
        return Err(catalog_invariant(
            "catalog candidate does not belong to the requested tenant",
        ));
    }
    if binding.connection_id != connection.connection_id {
        return Err(catalog_invariant(
            "catalog binding belongs to a different connection",
        ));
    }
    if binding.connection_generation != connection.generation {
        return Err(catalog_invariant(format!(
            "catalog binding generation {} does not match connection generation {}",
            binding.connection_generation.get(),
            connection.generation.get()
        )));
    }
    binding.validate()?;
    Ok(())
}

fn catalog_invariant(message: impl Into<String>) -> Error {
    Error::CatalogInvariant {
        message: message.into(),
    }
}
