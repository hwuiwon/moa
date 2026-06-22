//! SCIM API-key authorization helpers.

use moa_authz::{AuthzCheckError, require_authz};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;

use super::{ScimResponseError, ScimState};

/// Verify that the authenticated API key has `scim_admin` on its tenant.
pub async fn require_scim_admin(
    state: &ScimState,
    identity: &Identity,
) -> Result<(), ScimResponseError> {
    let fga = state
        .fga_client
        .as_ref()
        .ok_or_else(|| ScimResponseError::unavailable("authorization engine is disabled"))?;
    require_authz(
        fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(map_authz_error)
}

fn map_authz_error(error: AuthzCheckError) -> ScimResponseError {
    match error {
        AuthzCheckError::Forbidden { .. } => {
            ScimResponseError::forbidden("API key is missing scim_admin scope")
        }
        AuthzCheckError::Engine(error) => {
            tracing::error!(error = %error, "SCIM authz check failed");
            ScimResponseError::unavailable("authorization engine unavailable")
        }
    }
}
