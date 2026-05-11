//! Placeholder authentication provider for the edge binary.
//!
//! Real local API-key authentication lands in P1.5/P1.6. Until then the edge
//! boots and rejects all presented credentials with `401 invalid credential`.

use async_trait::async_trait;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity};

/// Authentication provider that rejects every credential.
pub struct RejectAllAuthProvider;

#[async_trait]
impl AuthProvider for RejectAllAuthProvider {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Err(AuthError::Rejected)
    }

    fn name(&self) -> &'static str {
        "reject-all-placeholder"
    }
}
