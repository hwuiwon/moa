//! Application service for safe connector installation and atomic activation.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_artifacts::connector::{
    RuntimeConnectorAuthRequirementV1, RuntimeConnectorDefinitionV1, RuntimeConnectorKindV1,
};
use moa_core::types::credentials::{CredentialKind, CredentialSlotName};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth,
    ConnectionOrigin, ConnectionStatus, ConnectorConnection, InstalledActionBinding,
    InstalledActionBindingId, ManagedParentClaim, ManagedParentDefinition,
    ManagedParentDeleteOutcome, OperationContractHash,
};
use crate::repository::{ConnectionActivation, ConnectionRepository, NewConnectorConnection};
use crate::{Error, Result};

/// One exact credential series that must be active before a connection can activate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredCredentialSlot {
    /// Logical connector slot.
    pub slot: CredentialSlotName,
    /// Material kind required in that slot.
    pub kind: CredentialKind,
}

/// Secret-free availability of one exact credential series.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialSlotReadiness {
    /// Logical connector slot.
    pub slot: CredentialSlotName,
    /// Material kind required in that slot.
    pub kind: CredentialKind,
    /// Whether an active credential version is available to the authorized caller.
    pub ready: bool,
}

/// Host-owned status boundary for connector credential slots.
///
/// The port verifies availability only. It never returns plaintext and does not
/// open the vault's intentionally absent enumeration surface.
#[async_trait]
pub trait CredentialSlotVerifier: Send + Sync {
    /// Returns secret-free availability for the exact requested slots.
    async fn credential_slot_readiness(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        slots: &[RequiredCredentialSlot],
    ) -> Result<Vec<CredentialSlotReadiness>>;
}

const ACTION_BINDING_NAMESPACE: &str = "https://moa.ai/connector/action-binding/v1";

/// Typed connection-create request that cannot carry an unparsed origin URL.
#[derive(Clone, Debug)]
pub struct CreateConnectionRequest {
    /// Replay-stable connection identity.
    pub connection_id: ConnectorConnectionId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Operator-visible connection label.
    pub display_name: String,
    /// Exact immutable artifact or built-in definition reference.
    pub definition_ref: ConnectionDefinitionRef,
    /// Definition being installed; its runtime decides whether an origin is required.
    pub definition: RuntimeConnectorDefinitionV1,
    /// Syntactically validated fixed origin for HTTP runtimes.
    pub origin: Option<ConnectionOrigin>,
    /// Additional secret-free configuration fields.
    pub non_secret_config: Map<String, Value>,
    /// Identity initiating installation, when durable.
    pub created_by_identity_id: Option<Uuid>,
    /// Operator granted direct ownership.
    pub owner_identity_id: Uuid,
}

/// Generation-fenced activation of one exact runtime definition.
#[derive(Clone, Debug)]
pub struct ActivateConnectionRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Installed connection.
    pub connection_id: ConnectorConnectionId,
    /// Generation observed by the management caller.
    pub expected_generation: ConnectionGeneration,
    /// Exact definition reference expected on the connection row.
    pub definition_ref: ConnectionDefinitionRef,
    /// Validated runtime definition compiled into immutable bindings.
    pub definition: RuntimeConnectorDefinitionV1,
}

/// Generation-fenced notification that a credential write has committed.
///
/// The request deliberately carries neither a credential reference nor a
/// credential version. The credential-ingress boundary retains that local
/// information solely to compensate the vault write if this CAS fails.
#[derive(Clone, Copy, Debug)]
pub struct CredentialGenerationFenceRequest {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Connection whose credential material changed.
    pub connection_id: ConnectorConnectionId,
    /// Generation observed before the credential write began.
    pub expected_generation: ConnectionGeneration,
}

/// Replay-safe request to claim the generic parent of one linked knowledge account.
#[derive(Clone, Debug)]
pub struct ManagedParentClaimRequest {
    /// Owning tenant and RLS boundary.
    pub tenant_id: TenantId,
    /// Replay-stable link operation identifier.
    pub operation_id: String,
    /// Canonical lowercase hexadecimal hash of the immutable link inputs.
    pub request_hash: String,
    /// Knowledge and connector identity shared without translation.
    pub connection_id: ConnectorConnectionId,
    /// Closed code-owned knowledge connector definition.
    pub definition: ManagedParentDefinition,
    /// Operator-visible label persisted on the generic parent.
    pub display_name: String,
    /// Immutable provider integration/configuration selector.
    pub provider_config_key: String,
    /// Immutable provider-native linked-account selector.
    pub provider_connection_id: String,
    /// Immutable knowledge connector/category selected within the provider account.
    pub connector: String,
    /// Authenticated operator owner; required only when this operation creates the parent.
    pub owner_identity_id: Option<Uuid>,
}

/// Generation-fenced activation of a managed knowledge-only parent.
#[derive(Clone, Copy, Debug)]
pub struct ManagedParentActivationRequest {
    /// Owning tenant and RLS boundary.
    pub tenant_id: TenantId,
    /// Managed parent being activated.
    pub connection_id: ConnectorConnectionId,
    /// Exact current generation, which this activation deliberately preserves.
    pub expected_generation: ConnectionGeneration,
    /// Closed code-owned definition expected on the parent.
    pub definition: ManagedParentDefinition,
}

/// Replay-safe request to compensate one managed-parent claim.
#[derive(Clone, Debug)]
pub struct ManagedParentDeleteRequest {
    /// Owning tenant and RLS boundary.
    pub tenant_id: TenantId,
    /// Operation whose durable claim proves parent ownership.
    pub operation_id: String,
    /// Exact canonical hash recorded by that operation.
    pub request_hash: String,
    /// Exact managed parent selected by the operation.
    pub connection_id: ConnectorConnectionId,
}

/// Connector connection lifecycle application service.
#[derive(Clone)]
pub struct ConnectorService {
    repository: Arc<dyn ConnectionRepository>,
    credentials: Arc<dyn CredentialSlotVerifier>,
}

impl ConnectorService {
    /// Creates the service with explicit persistence and credential-status ports.
    #[must_use]
    pub fn new(
        repository: Arc<dyn ConnectionRepository>,
        credentials: Arc<dyn CredentialSlotVerifier>,
    ) -> Self {
        Self {
            repository,
            credentials,
        }
    }

    /// Creates one pending connection after building its secret-free configuration.
    pub async fn create(&self, request: CreateConnectionRequest) -> Result<ConnectorConnection> {
        match &request.definition.runtime {
            RuntimeConnectorKindV1::ConstrainedHttp => {
                if request.origin.is_none() {
                    return Err(Error::InvalidConnectionOrigin {
                        reason: "constrained HTTP runtime requires a fixed HTTP(S) origin",
                    });
                }
            }
            RuntimeConnectorKindV1::BuiltInManaged { .. } => {
                if request.origin.is_some() {
                    return Err(Error::InvalidConnectionOrigin {
                        reason: "managed connector runtime does not accept an origin",
                    });
                }
            }
        }
        if request.non_secret_config.contains_key("origin") {
            return Err(Error::InvalidContract {
                message: "connector origin must use its typed field".to_string(),
            });
        }
        let mut config = request.non_secret_config;
        if let Some(origin) = request.origin {
            config.insert("origin".to_string(), Value::String(origin.to_string()));
        }
        self.repository
            .create(NewConnectorConnection {
                connection_id: request.connection_id,
                tenant_id: request.tenant_id,
                display_name: request.display_name,
                definition_ref: request.definition_ref,
                non_secret_config: Value::Object(config),
                created_by_identity_id: request.created_by_identity_id,
                owner_identity_id: request.owner_identity_id,
            })
            .await
    }

    /// Lists every connection in one already-authorized tenant scope.
    pub async fn list(&self, tenant_id: TenantId) -> Result<Vec<ConnectorConnection>> {
        self.repository.list(tenant_id).await
    }

    /// Loads one connection from an already-authorized tenant scope.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>> {
        self.repository.load(tenant_id, connection_id).await
    }

    /// Claims or exactly resumes the generic parent of one linked knowledge account.
    pub async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim> {
        self.repository.claim_managed_parent(request).await
    }

    /// Verifies required slots, compiles deterministic bindings, and atomically activates.
    pub async fn activate(
        &self,
        request: ActivateConnectionRequest,
    ) -> Result<ConnectorConnection> {
        let connection = self
            .repository
            .load(request.tenant_id, request.connection_id)
            .await?
            .ok_or(Error::ConnectionNotFound {
                connection_id: request.connection_id,
            })?;
        if connection.generation != request.expected_generation {
            return Err(Error::GenerationConflict {
                expected: request.expected_generation,
                actual: connection.generation,
            });
        }
        if connection.definition != request.definition_ref {
            return Err(Error::InvalidContract {
                message: "activation definition reference differs from installed connection"
                    .to_string(),
            });
        }
        let next_generation = request.expected_generation.next()?;
        let mut bindings = Vec::with_capacity(request.definition.actions.len());
        for action in &request.definition.actions {
            let compiled = CompiledOperationContract::compile(&request.definition, action)?;
            let contract_hash = compiled.hash()?;
            bindings.push(InstalledActionBinding {
                binding_id: derive_binding_id(
                    request.connection_id,
                    next_generation,
                    &action.id,
                    contract_hash,
                ),
                tenant_id: request.tenant_id,
                connection_id: request.connection_id,
                connection_generation: next_generation,
                action_id: action.id.clone(),
                compiled_contract: compiled,
                contract_hash,
                governed_contract_revision: governed_revision(
                    &request.definition_ref,
                    contract_hash,
                ),
                minimum_effect: action.policy().minimum_effect,
                enabled: true,
            });
        }
        let readiness = self
            .credential_slot_readiness(
                request.tenant_id,
                request.connection_id,
                &request.definition,
            )
            .await?;
        if let Some(missing) = readiness.iter().find(|slot| !slot.ready) {
            return Err(Error::CredentialSlotMissing {
                slot: missing.slot.clone(),
            });
        }

        self.repository
            .activate(ConnectionActivation {
                tenant_id: request.tenant_id,
                connection_id: request.connection_id,
                expected_generation: request.expected_generation,
                bindings,
            })
            .await
    }

    /// Activates a managed knowledge-only parent without inventing an action binding.
    ///
    /// The credential series must already be active. Unlike ordinary action
    /// activation this transition preserves the current generation, and the
    /// repository rejects a parent that has any action-binding dependency.
    pub async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> Result<ConnectorConnection> {
        let connection = self
            .repository
            .load(request.tenant_id, request.connection_id)
            .await?
            .ok_or(Error::ConnectionNotFound {
                connection_id: request.connection_id,
            })?;
        if connection.generation != request.expected_generation {
            return Err(Error::GenerationConflict {
                expected: request.expected_generation,
                actual: connection.generation,
            });
        }
        if connection.definition != request.definition.definition_ref() {
            return Err(Error::ManagedParentMismatch {
                connection_id: request.connection_id,
                field: "definition",
            });
        }
        let definition = request.definition.runtime_definition();
        let required = required_credential_slots(&definition)?;
        if !required.is_empty() {
            let readiness = self
                .credentials
                .credential_slot_readiness(request.tenant_id, request.connection_id, &required)
                .await?;
            let readiness = normalize_slot_readiness(&required, readiness)?;
            if let Some(missing) = readiness.iter().find(|reported| !reported.ready) {
                return Err(Error::CredentialSlotMissing {
                    slot: missing.slot.clone(),
                });
            }
        }
        self.repository
            .activate_managed_knowledge_parent(request)
            .await
    }

    /// Marks an exact claim-created managed parent deleted only when it is unused.
    pub async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome> {
        self.repository
            .delete_managed_parent_if_unused(request)
            .await
    }

    /// Returns deterministic secret-free readiness for all definition-required slots.
    pub async fn credential_slot_readiness(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        definition: &RuntimeConnectorDefinitionV1,
    ) -> Result<Vec<CredentialSlotReadiness>> {
        let required = required_credential_slots(definition)?;
        let reported = self
            .credentials
            .credential_slot_readiness(tenant_id, connection_id, &required)
            .await?;
        normalize_slot_readiness(&required, reported)
    }

    /// Advances the connection generation after a credential write commits.
    ///
    /// The repository atomically disables bindings and suspends an active
    /// connection. Pending-auth and already-suspended connections retain their
    /// lifecycle state but still advance generation, forcing explicit
    /// activation before any binding can use the new credential material.
    pub async fn advance_credential_generation(
        &self,
        request: CredentialGenerationFenceRequest,
    ) -> Result<ConnectorConnection> {
        self.repository
            .advance_credential_generation(
                request.tenant_id,
                request.connection_id,
                request.expected_generation,
            )
            .await
    }

    /// Suspends an active connection under its current generation fence.
    pub async fn suspend(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        self.repository
            .transition(
                tenant_id,
                connection_id,
                expected_generation,
                ConnectionStatus::Suspended,
            )
            .await
    }

    /// Resumes a suspended connection without recompiling its immutable bindings.
    pub async fn resume(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        self.repository
            .transition(
                tenant_id,
                connection_id,
                expected_generation,
                ConnectionStatus::Active,
            )
            .await
    }

    /// Begins teardown of an active or suspended connection.
    pub async fn disconnect(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        self.repository
            .transition(
                tenant_id,
                connection_id,
                expected_generation,
                ConnectionStatus::Disconnecting,
            )
            .await
    }

    /// Marks a pending-auth or disconnecting connection deleted and revokes its authz tuples.
    pub async fn delete(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        self.repository
            .transition(
                tenant_id,
                connection_id,
                expected_generation,
                ConnectionStatus::Deleted,
            )
            .await
    }

    /// Records secret-free health state independently from lifecycle state.
    pub async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> Result<ConnectorConnection> {
        self.repository
            .update_health(
                tenant_id,
                connection_id,
                expected_generation,
                health,
                reason,
            )
            .await
    }
}

/// Derives the unique credential series declared by a runtime definition.
///
/// This is the authoritative mapping from connector authentication contracts
/// to vault material kinds for both activation and management projections.
pub fn required_credential_slots(
    definition: &RuntimeConnectorDefinitionV1,
) -> Result<Vec<RequiredCredentialSlot>> {
    let mut slots = BTreeMap::<CredentialSlotName, CredentialKind>::new();
    for requirement in &definition.auth {
        let pair = match requirement {
            RuntimeConnectorAuthRequirementV1::None => None,
            RuntimeConnectorAuthRequirementV1::Bearer { slot }
            | RuntimeConnectorAuthRequirementV1::ApiKeyHeader { slot, .. } => {
                Some((slot.clone(), CredentialKind::ProviderApiKey))
            }
            RuntimeConnectorAuthRequirementV1::ManagedOauth { slot } => {
                Some((slot.clone(), CredentialKind::OAuth))
            }
        };
        let Some((slot, kind)) = pair else {
            continue;
        };
        if let Some(existing) = slots.insert(slot.clone(), kind)
            && existing != kind
        {
            return Err(Error::InvalidContract {
                message: format!("credential slot `{slot}` declares conflicting material kinds"),
            });
        }
    }
    Ok(slots
        .into_iter()
        .map(|(slot, kind)| RequiredCredentialSlot { slot, kind })
        .collect())
}

fn normalize_slot_readiness(
    required: &[RequiredCredentialSlot],
    reported: Vec<CredentialSlotReadiness>,
) -> Result<Vec<CredentialSlotReadiness>> {
    let mut by_slot = BTreeMap::new();
    for readiness in reported {
        let slot = readiness.slot.clone();
        if by_slot.insert(slot.clone(), readiness).is_some() {
            return Err(Error::InvalidContract {
                message: format!("credential readiness repeated slot `{slot}`"),
            });
        }
    }

    let mut ordered = Vec::with_capacity(required.len());
    for requirement in required {
        let Some(readiness) = by_slot.remove(&requirement.slot) else {
            ordered.push(CredentialSlotReadiness {
                slot: requirement.slot.clone(),
                kind: requirement.kind,
                ready: false,
            });
            continue;
        };
        if readiness.kind != requirement.kind {
            return Err(Error::InvalidContract {
                message: format!(
                    "credential readiness for slot `{}` returned a different material kind",
                    requirement.slot
                ),
            });
        }
        ordered.push(readiness);
    }
    if let Some(unexpected) = by_slot.into_keys().next() {
        return Err(Error::InvalidContract {
            message: format!("credential readiness returned unexpected slot `{unexpected}`"),
        });
    }
    Ok(ordered)
}

fn governed_revision(
    definition: &ConnectionDefinitionRef,
    contract_hash: crate::domain::OperationContractHash,
) -> String {
    match definition {
        ConnectionDefinitionRef::Artifact { revision_uid, .. } => {
            format!("connector-artifact:{revision_uid}:{contract_hash}")
        }
        ConnectionDefinitionRef::BuiltIn { key, version } => {
            format!("connector-built-in:{key}:{version}:{contract_hash}")
        }
    }
}

fn derive_binding_id(
    connection_id: ConnectorConnectionId,
    generation: ConnectionGeneration,
    action_id: &str,
    contract_hash: OperationContractHash,
) -> InstalledActionBindingId {
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, ACTION_BINDING_NAMESPACE.as_bytes());
    let mut name = Vec::with_capacity(80 + action_id.len());
    append_frame(&mut name, connection_id.0.as_bytes());
    append_frame(&mut name, &generation.get().to_be_bytes());
    append_frame(&mut name, action_id.as_bytes());
    append_frame(&mut name, contract_hash.as_bytes());
    InstalledActionBindingId(Uuid::new_v5(&namespace, &name))
}

fn append_frame(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}
