//! Trusted identity header helpers for Restate e2e tests.

use moa_core::traits::{Identity, IdentityType};
use uuid::Uuid;

/// Return a fresh operator identity suitable for direct Restate e2e calls.
pub fn test_user_identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

/// Attach trusted MOA identity headers to a Restate ingress request.
pub fn with_identity(
    request: reqwest::RequestBuilder,
    identity: &Identity,
) -> reqwest::RequestBuilder {
    request
        .header("x-moa-identity-type", "operator")
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string())
}
