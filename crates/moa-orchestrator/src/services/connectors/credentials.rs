//! Connector credential readiness, revocation, and generation-fence types.

use std::sync::Arc;

use async_trait::async_trait;
use moa_connectors::domain::{ConnectionGeneration, ConnectionStatus, ConnectorConnection};
use moa_connectors::service::{ConnectionCredentialSlot, ConnectionCredentialSlotReadiness};
use moa_core::traits::{CredentialVault, Identity};
use moa_core::types::credentials::{
    CredentialContext, CredentialIdentity, CredentialOperation, CredentialPrincipal,
    CredentialServiceActor,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};

use super::{CREDENTIAL_READINESS_HASH_DOMAIN, CREDENTIAL_REVOCATION_HASH_DOMAIN};

/// Sanitized audit-preserving credential-revocation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("connector credential revocation unavailable")]
pub struct ConnectorCredentialRevocationError;
/// Vault-owned, connection-scoped credential revocation boundary.
///
/// Implementations revoke active versions without deleting credential rows or
/// operation audit. They return only a count and never expose references.
#[async_trait]
pub trait ConnectionCredentialRevoker: Send + Sync {
    /// Revokes every active version for one exact tenant connection.
    async fn revoke_connection(
        &self,
        connection_id: ConnectorConnectionId,
        context: &CredentialContext,
    ) -> Result<u64, ConnectorCredentialRevocationError>;
}

/// Thin adapter from the core credential-vault contract to management revocation.
#[derive(Clone)]
pub struct VaultConnectionCredentialRevoker {
    vault: Arc<dyn CredentialVault>,
}

impl VaultConnectionCredentialRevoker {
    /// Creates a revoker around the orchestrator-owned credential vault.
    #[must_use]
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self { vault }
    }
}

#[async_trait]
impl ConnectionCredentialRevoker for VaultConnectionCredentialRevoker {
    async fn revoke_connection(
        &self,
        connection_id: ConnectorConnectionId,
        context: &CredentialContext,
    ) -> Result<u64, ConnectorCredentialRevocationError> {
        self.vault
            .revoke_connection(connection_id.0, context)
            .await
            .map_err(|_| ConnectorCredentialRevocationError)
    }
}

/// Secret-free exact-series readiness adapter over the orchestrator-owned vault.
#[derive(Clone)]
pub struct VaultCredentialSlotVerifier {
    vault: Arc<dyn CredentialVault>,
}

impl VaultCredentialSlotVerifier {
    /// Creates a slot verifier around the orchestrator-owned credential vault.
    #[must_use]
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self { vault }
    }
}

#[async_trait]
impl moa_connectors::service::CredentialSlotVerifier for VaultCredentialSlotVerifier {
    async fn credential_slot_readiness_batch(
        &self,
        tenant_id: TenantId,
        slots: &[ConnectionCredentialSlot],
    ) -> moa_connectors::Result<Vec<ConnectionCredentialSlotReadiness>> {
        let identities = slots
            .iter()
            .map(|slot| CredentialIdentity {
                tenant_id,
                connection_uid: slot.connection_id.0,
                kind: slot.kind,
                slot_name: slot.slot.clone(),
            })
            .collect::<Vec<_>>();
        let operation_id = uuid::Uuid::now_v7().to_string();
        let context = CredentialContext {
            tenant_id,
            principal: CredentialPrincipal::Service {
                actor: CredentialServiceActor::ConnectorManagementReadiness,
            },
            operation: CredentialOperation::Resolve,
            request_hash: credential_readiness_hash(&identities, &operation_id),
            operation_id,
        };
        let active = self.vault.has_active_batch(&identities, &context).await?;
        if active.len() != slots.len() {
            return Err(moa_connectors::Error::InvalidContract {
                message: "credential vault returned a different readiness result count".to_string(),
            });
        }
        Ok(slots
            .iter()
            .zip(active)
            .map(|(slot, ready)| ConnectionCredentialSlotReadiness {
                connection_id: slot.connection_id,
                slot: slot.slot.clone(),
                kind: slot.kind,
                ready,
            })
            .collect())
    }
}

/// Authorization-approved credential selector retained by the private ingress.
///
/// The value contains no material, reference, or version and does not implement
/// serialization. Only this module can construct it, so a caller cannot bypass
/// definition slot validation before requesting the generation fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCredentialWrite {
    pub(super) identity: CredentialIdentity,
    pub(super) expected_generation: ConnectionGeneration,
}

impl PreparedCredentialWrite {
    /// Returns the exact credential series the private vault stage must use.
    #[must_use]
    pub const fn credential_identity(&self) -> &CredentialIdentity {
        &self.identity
    }

    /// Returns the connection generation observed during authorized preparation.
    #[must_use]
    pub const fn expected_generation(&self) -> ConnectionGeneration {
        self.expected_generation
    }
}

/// Successful secret-free generation fence returned to the private ingress.
///
/// This host-local acknowledgement is intentionally not serializable. It lets
/// the ingress activate its retained staging token only after the connection
/// repository has durably invalidated the prior generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialGenerationFenceResult {
    pub(super) connection_id: ConnectorConnectionId,
    pub(super) generation: ConnectionGeneration,
    pub(super) status: ConnectionStatus,
}

impl CredentialGenerationFenceResult {
    /// Returns the fenced connection identity.
    #[must_use]
    pub const fn connection_id(&self) -> ConnectorConnectionId {
        self.connection_id
    }

    /// Returns the new durable generation.
    #[must_use]
    pub const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the lifecycle state after fencing.
    #[must_use]
    pub const fn status(&self) -> ConnectionStatus {
        self.status
    }
}
pub(super) fn credential_revocation_context(
    identity: &Identity,
    connection: &ConnectorConnection,
    phase: &'static str,
) -> CredentialContext {
    let mut hash = blake3::Hasher::new();
    for part in [
        CREDENTIAL_REVOCATION_HASH_DOMAIN.as_bytes(),
        phase.as_bytes(),
        identity.tenant_id.to_string().as_bytes(),
        connection.connection_id.to_string().as_bytes(),
        connection.generation.to_string().as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    CredentialContext {
        tenant_id: identity.tenant_id,
        principal: CredentialPrincipal::Caller {
            identity_id: identity.id,
            delegated_by: identity.acting_on_behalf_of,
        },
        operation: CredentialOperation::Revoke,
        operation_id: format!(
            "connector-{phase}-{}-{}",
            connection.connection_id, connection.generation
        ),
        request_hash: hash.finalize().to_hex().to_string(),
    }
}

fn credential_readiness_hash(identities: &[CredentialIdentity], operation_id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    for part in [
        CREDENTIAL_READINESS_HASH_DOMAIN.as_bytes(),
        operation_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    for identity in identities {
        for part in [
            identity.tenant_id.to_string().as_bytes(),
            identity.connection_uid.to_string().as_bytes(),
            identity.kind.as_str().as_bytes(),
            identity.slot_name.as_str().as_bytes(),
        ] {
            hash.update(&(part.len() as u64).to_be_bytes());
            hash.update(part);
        }
    }
    hash.finalize().to_hex().to_string()
}
