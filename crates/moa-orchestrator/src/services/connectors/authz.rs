//! Connector-management authorization ports and OpenFGA adapter.

use async_trait::async_trait;
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::types::identifiers::ConnectorConnectionId;

/// Authorization failure at the connector-management boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConnectorManagementAuthorizationError {
    /// The authenticated caller does not have the requested relationship.
    #[error("connector management authorization denied")]
    Denied,
    /// The authorization engine could not produce a trustworthy decision.
    #[error("connector management authorization unavailable")]
    Unavailable,
}
/// Authorization port whose methods are always called before protected reads.
#[async_trait]
pub trait ConnectorManagementAuthorizer: Send + Sync {
    /// Requires tenant `Admin` for definition installation and tenant-wide listing.
    async fn require_tenant_admin(
        &self,
        identity: &Identity,
    ) -> Result<(), ConnectorManagementAuthorizationError>;

    /// Requires delegated connector-connection `Manage` for an existing resource.
    async fn require_connection_manage(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
    ) -> Result<(), ConnectorManagementAuthorizationError>;
}

/// OpenFGA-backed connector-management authorizer.
#[derive(Clone)]
pub struct FgaConnectorManagementAuthorizer {
    fga: FgaClient,
}

impl FgaConnectorManagementAuthorizer {
    /// Creates an authorizer from the required OpenFGA client.
    #[must_use]
    pub fn new(fga: FgaClient) -> Self {
        Self { fga }
    }
}

#[async_trait]
impl ConnectorManagementAuthorizer for FgaConnectorManagementAuthorizer {
    async fn require_tenant_admin(
        &self,
        identity: &Identity,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        require_authz_with_delegation(
            &self.fga,
            identity,
            ObjectType::Tenant,
            identity.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(map_authz_error)
    }

    async fn require_connection_manage(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        require_authz_with_delegation(
            &self.fga,
            identity,
            ObjectType::ConnectorConnection,
            connection_id,
            Relation::Manage,
        )
        .await
        .map_err(map_authz_error)
    }
}

fn map_authz_error(error: AuthzCheckError) -> ConnectorManagementAuthorizationError {
    match error {
        AuthzCheckError::Forbidden { .. } => ConnectorManagementAuthorizationError::Denied,
        AuthzCheckError::Engine(_) => ConnectorManagementAuthorizationError::Unavailable,
    }
}
