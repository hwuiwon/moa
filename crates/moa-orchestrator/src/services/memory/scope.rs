//! User and contact scope validation for memory service requests.

use moa_core::traits::{Identity, IdentityType};
use moa_core::{ContactId, TenantId, UserId};
use moa_memory_types::MemoryScope;

/// User-scope validation error for memory requests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UserScopeError {
    /// The caller requested a user id that does not match the trusted identity.
    #[error("requested user_id {requested} does not match trusted caller user {effective}")]
    Mismatch {
        /// User id supplied by the request.
        requested: UserId,
        /// User id derived from the trusted identity, when one exists.
        effective: String,
    },
}

/// Returns the user id represented by a trusted identity or agent delegation.
#[must_use]
pub fn effective_user_id(identity: &Identity) -> Option<UserId> {
    match identity.identity_type {
        IdentityType::User => Some(UserId::new(identity.id.to_string())),
        IdentityType::Agent => identity
            .acting_on_behalf_of
            .map(|user_id| UserId::new(user_id.to_string())),
        IdentityType::Service | IdentityType::Contact => None,
    }
}

/// Builds the memory read scope after validating any requested user scope.
pub fn checked_memory_scope(
    tenant_id: TenantId,
    requested_contact_id: Option<ContactId>,
    identity: &Identity,
) -> Result<MemoryScope, UserScopeError> {
    match requested_contact_id {
        Some(requested) => {
            if identity.identity_type == IdentityType::Contact
                && ContactId(identity.id) != requested
            {
                return Err(UserScopeError::Mismatch {
                    requested: UserId::new(requested.to_string()),
                    effective: identity.id.to_string(),
                });
            }
            Ok(MemoryScope::Contact {
                tenant_id,
                contact_id: requested,
            })
        }
        None => Ok(MemoryScope::Tenant { tenant_id }),
    }
}

/// Returns the trusted user id to attach to a document ingestion turn.
pub fn checked_ingest_contact_id(
    requested_contact_id: Option<ContactId>,
    identity: &Identity,
) -> Result<ContactId, UserScopeError> {
    if let Some(requested) = requested_contact_id {
        if identity.identity_type == IdentityType::Contact && ContactId(identity.id) != requested {
            return Err(UserScopeError::Mismatch {
                requested: UserId::new(requested.to_string()),
                effective: identity.id.to_string(),
            });
        }
        return Ok(requested);
    }

    Ok(ContactId(identity.id))
}
