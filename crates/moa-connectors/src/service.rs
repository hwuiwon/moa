//! Application service for safe connector installation and atomic activation.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_artifacts::connector::{ConnectorDefinition, RuntimeConnectorAuthRequirementV1};
use moa_core::types::action_policy::ActionPolicyEffect;
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
use crate::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ConnectionListPage, ConnectionListRequest,
    ManagedParentRepository, NewConnectorConnection,
};
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

/// One connection-qualified credential series requested by a batch readiness read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionCredentialSlot {
    /// Connection owning the credential series.
    pub connection_id: ConnectorConnectionId,
    /// Logical connector slot.
    pub slot: CredentialSlotName,
    /// Material kind required in that slot.
    pub kind: CredentialKind,
}

/// Connection-qualified readiness returned in exact request order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionCredentialSlotReadiness {
    /// Connection owning the credential series.
    pub connection_id: ConnectorConnectionId,
    /// Logical connector slot.
    pub slot: CredentialSlotName,
    /// Material kind required in that slot.
    pub kind: CredentialKind,
    /// Whether an active credential version is available.
    pub ready: bool,
}

/// Host-owned status boundary for connector credential slots.
///
/// The port verifies availability only. It never returns plaintext and does not
/// open the vault's intentionally absent enumeration surface.
#[async_trait]
pub trait CredentialSlotVerifier: Send + Sync {
    /// Returns secret-free availability in exact request order using one set-based read.
    async fn credential_slot_readiness_batch(
        &self,
        tenant_id: TenantId,
        slots: &[ConnectionCredentialSlot],
    ) -> Result<Vec<ConnectionCredentialSlotReadiness>>;
}

const ACTION_BINDING_NAMESPACE: &str = "https://moa.ai/connector/action-binding/v1";
/// Maximum action bindings accepted for one connector connection generation.
pub const MAX_CONNECTOR_ACTION_BINDINGS: usize = 64;

/// Typed connection-create request that cannot carry an unparsed origin URL.
#[derive(Clone, Debug)]
pub struct CreateConnectionRequest {
    /// Replay-stable connection identity.
    pub connection_id: ConnectorConnectionId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Operator-visible connection label.
    pub display_name: String,
    /// Exact immutable artifact definition reference.
    pub definition_ref: ConnectionDefinitionRef,
    /// Syntactically validated fixed HTTP origin.
    pub origin: ConnectionOrigin,
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
    pub definition: ConnectorDefinition,
}

/// Successful activation plus the readiness observation that admitted it.
#[derive(Clone, Debug)]
pub struct ActivatedConnection {
    /// Connection after the atomic generation transition.
    pub connection: ConnectorConnection,
    /// Exact credential readiness used to admit the activation.
    pub credential_readiness: Vec<CredentialSlotReadiness>,
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
    lifecycle: Arc<dyn ConnectionLifecycleRepository>,
    managed_parents: Arc<dyn ManagedParentRepository>,
    credentials: Arc<dyn CredentialSlotVerifier>,
}

impl ConnectorService {
    /// Creates the service with explicit persistence and credential-status ports.
    #[must_use]
    pub fn new(
        lifecycle: Arc<dyn ConnectionLifecycleRepository>,
        managed_parents: Arc<dyn ManagedParentRepository>,
        credentials: Arc<dyn CredentialSlotVerifier>,
    ) -> Self {
        Self {
            lifecycle,
            managed_parents,
            credentials,
        }
    }

    /// Creates one pending connection after building its secret-free configuration.
    pub async fn create(&self, request: CreateConnectionRequest) -> Result<ConnectorConnection> {
        if request.non_secret_config.contains_key("origin") {
            return Err(Error::InvalidContract {
                message: "connector origin must use its typed field".to_string(),
            });
        }
        self.lifecycle
            .create(NewConnectorConnection {
                connection_id: request.connection_id,
                tenant_id: request.tenant_id,
                display_name: request.display_name,
                definition_ref: request.definition_ref,
                origin: Some(request.origin),
                non_secret_config: Value::Object(request.non_secret_config),
                created_by_identity_id: request.created_by_identity_id,
                owner_identity_id: request.owner_identity_id,
            })
            .await
    }

    /// Lists one deterministic page of non-deleted connections in an authorized tenant scope.
    pub async fn list(
        &self,
        tenant_id: TenantId,
        request: ConnectionListRequest,
    ) -> Result<ConnectionListPage> {
        self.lifecycle.list(tenant_id, request).await
    }

    /// Loads one connection from an already-authorized tenant scope.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>> {
        self.lifecycle.load(tenant_id, connection_id).await
    }

    /// Claims or exactly resumes the generic parent of one linked knowledge account.
    pub async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim> {
        self.managed_parents.claim_managed_parent(request).await
    }

    /// Verifies required slots, compiles deterministic bindings, and atomically activates.
    pub async fn activate(
        &self,
        request: ActivateConnectionRequest,
    ) -> Result<ActivatedConnection> {
        if request.definition.actions.len() > MAX_CONNECTOR_ACTION_BINDINGS {
            return Err(Error::InvalidContract {
                message: "connector activation accepts at most 64 action bindings".to_string(),
            });
        }
        let connection = self
            .lifecycle
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
                minimum_effect: ActionPolicyEffect::AdminReview,
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

        let connection = self
            .lifecycle
            .activate(ConnectionActivation {
                tenant_id: request.tenant_id,
                connection_id: request.connection_id,
                expected_generation: request.expected_generation,
                bindings,
            })
            .await?;
        Ok(ActivatedConnection {
            connection,
            credential_readiness: readiness,
        })
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
            .lifecycle
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
        let auth = request.definition.credential_requirements();
        let required = required_credential_slots_for_requirements(&auth)?;
        if !required.is_empty() {
            let readiness = self
                .credential_slot_readiness_for_requirements(
                    request.tenant_id,
                    request.connection_id,
                    &auth,
                )
                .await?;
            if let Some(missing) = readiness.iter().find(|reported| !reported.ready) {
                return Err(Error::CredentialSlotMissing {
                    slot: missing.slot.clone(),
                });
            }
        }
        self.managed_parents
            .activate_managed_knowledge_parent(request)
            .await
    }

    /// Marks an exact claim-created managed parent deleted only when it is unused.
    pub async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome> {
        self.managed_parents
            .delete_managed_parent_if_unused(request)
            .await
    }

    /// Returns deterministic secret-free readiness for all definition-required slots.
    pub async fn credential_slot_readiness(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        definition: &ConnectorDefinition,
    ) -> Result<Vec<CredentialSlotReadiness>> {
        self.credential_slot_readiness_for_requirements(tenant_id, connection_id, &definition.auth)
            .await
    }

    /// Returns deterministic readiness for one closed set of credential requirements.
    pub async fn credential_slot_readiness_for_requirements(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        auth: &[RuntimeConnectorAuthRequirementV1],
    ) -> Result<Vec<CredentialSlotReadiness>> {
        let required = required_credential_slots_for_requirements(auth)?;
        let reported = self
            .credentials
            .credential_slot_readiness_batch(
                tenant_id,
                &required
                    .iter()
                    .map(|slot| ConnectionCredentialSlot {
                        connection_id,
                        slot: slot.slot.clone(),
                        kind: slot.kind,
                    })
                    .collect::<Vec<_>>(),
            )
            .await?;
        normalize_slot_readiness(
            &required,
            reported
                .into_iter()
                .map(|slot| CredentialSlotReadiness {
                    slot: slot.slot,
                    kind: slot.kind,
                    ready: slot.ready,
                })
                .collect(),
        )
    }

    /// Returns connection-qualified readiness in exact request order using one vault read.
    pub async fn credential_slot_readiness_batch(
        &self,
        tenant_id: TenantId,
        requested: &[ConnectionCredentialSlot],
    ) -> Result<Vec<ConnectionCredentialSlotReadiness>> {
        let reported = self
            .credentials
            .credential_slot_readiness_batch(tenant_id, requested)
            .await?;
        if reported.len() != requested.len() {
            return Err(Error::InvalidContract {
                message: "credential readiness batch returned a different result count".to_string(),
            });
        }
        for (expected, actual) in requested.iter().zip(&reported) {
            if expected.connection_id != actual.connection_id
                || expected.slot != actual.slot
                || expected.kind != actual.kind
            {
                return Err(Error::InvalidContract {
                    message: "credential readiness batch changed request identity".to_string(),
                });
            }
        }
        Ok(reported)
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
        self.lifecycle
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
        self.lifecycle
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
        self.lifecycle
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
        self.lifecycle
            .transition(
                tenant_id,
                connection_id,
                expected_generation,
                ConnectionStatus::Disconnecting,
            )
            .await
    }

    /// Marks a disconnecting connection deleted and revokes its authz tuples.
    pub async fn delete(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        self.lifecycle
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
        self.lifecycle
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
    definition: &ConnectorDefinition,
) -> Result<Vec<RequiredCredentialSlot>> {
    required_credential_slots_for_requirements(&definition.auth)
}

/// Derives the unique credential series declared by closed auth requirements.
pub fn required_credential_slots_for_requirements(
    auth: &[RuntimeConnectorAuthRequirementV1],
) -> Result<Vec<RequiredCredentialSlot>> {
    let mut slots = BTreeMap::<CredentialSlotName, CredentialKind>::new();
    for requirement in auth {
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
