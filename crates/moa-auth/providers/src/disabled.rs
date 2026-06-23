//! Disabled authentication provider for explicitly unauthenticated deployments.

use async_trait::async_trait;
use moa_core::{
    TenantId,
    traits::{AuthError, AuthProvider, Credential, Identity, IdentityType},
};
use uuid::Uuid;

/// Authentication provider that accepts every request as a fixed service identity.
///
/// This is intended for local development and isolated tests only. Production
/// deployments should use local API keys or OIDC-backed authentication.
pub struct DisabledAuthProvider;

#[async_trait]
impl AuthProvider for DisabledAuthProvider {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Ok(Identity {
            identity_type: IdentityType::Service,
            id: Uuid::nil(),
            tenant_id: TenantId::from(Uuid::nil()),
            api_key_id: None,
            acting_on_behalf_of: None,
        })
    }

    fn name(&self) -> &'static str {
        "disabled"
    }

    fn requires_credentials(&self) -> bool {
        false
    }
}
