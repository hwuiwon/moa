//! Authorization-first connector management application service.

use std::sync::Arc;

use async_trait::async_trait;
use moa_artifacts::connector::RuntimeConnectorAuthRequirement;
use moa_connectors::domain::{
    ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth, ConnectionStatus,
    ConnectorConnection,
};
use moa_connectors::repository::{
    ConnectionListRequest, ConnectionUseGrantRepository, ConnectionUseRequest,
    MAX_CONNECTION_LIST_LIMIT,
};
use moa_connectors::service::{
    ActivateConnectionRequest, ConnectionCredentialSlot, ConnectorService, CreateConnectionRequest,
    CredentialGenerationFenceRequest, CredentialSlotReadiness,
};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::credentials::CredentialIdentity;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_security::outbound_http::{OutboundHttpAdmissionError, OutboundHttpPolicy};
use moa_wire::connectors::{
    ConnectorConnectionCreateRequest, ConnectorConnectionListRequest as WireConnectionListRequest,
    ConnectorConnectionListResponse, ConnectorConnectionMutationRequest,
    ConnectorConnectionResponse, ConnectorConnectionUseRequest,
    ConnectorConnectionVerificationResponse, ConnectorCredentialWriteMetadata,
    ConnectorVerificationState,
};

use super::authz::{ConnectorManagementAuthorizationError, ConnectorManagementAuthorizer};
use super::credentials::{
    ConnectionCredentialRevoker, ConnectorCredentialRevocationError,
    CredentialGenerationFenceResult, PreparedCredentialWrite, credential_revocation_context,
};
use super::definitions::{
    ConnectorDefinitionResolutionError, ConnectorDefinitionResolver, ResolvedConnectorDefinition,
    managed_knowledge_definition,
};
use super::wire::{
    connection_response, use_subject, wire_artifact_definition_to_domain, wire_health, wire_slot,
};
use super::{DEFAULT_CONNECTION_LIST_LIMIT, DESTINATION_ADMISSION_TIMEOUT};

/// Rejects a generic management operation that would bypass knowledge's
/// provider journal and child lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed knowledge connections are operated by knowledge orchestration")]
pub struct ManagedKnowledgeConnectionOperationError;

/// Sanitized local destination-verification failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConnectorDestinationVerificationError {
    /// The reviewed destination contract was rejected by local policy.
    #[error("connector destination rejected")]
    Rejected,
    /// Local DNS or admission infrastructure could not complete verification.
    #[error("connector destination verification unavailable")]
    Unavailable,
}

/// Failure returned by the connector-management application service.
#[derive(Debug, thiserror::Error)]
pub enum ConnectorManagementError {
    /// Authorization failed before a protected read or mutation.
    #[error(transparent)]
    Authorization(#[from] ConnectorManagementAuthorizationError),
    /// The exact definition could not be resolved safely.
    #[error(transparent)]
    Definition(#[from] ConnectorDefinitionResolutionError),
    /// A local destination check failed before activation.
    #[error(transparent)]
    Destination(#[from] ConnectorDestinationVerificationError),
    /// Connection-scoped credential revocation could not complete.
    #[error(transparent)]
    CredentialRevocation(#[from] ConnectorCredentialRevocationError),
    /// The authenticated identity cannot own a connector installation.
    #[error("connector creation requires an operator or a delegated operator owner")]
    UnsupportedOwnerIdentity,
    /// A definition resolver returned a different immutable definition pin.
    #[error("resolved connector definition does not match the requested reference")]
    DefinitionReferenceMismatch,
    /// Credential metadata did not name an exact slot/kind pair in the installed definition.
    #[error("credential slot or kind is not declared by the installed connector definition")]
    CredentialSlotMismatch,
    /// A knowledge-managed parent cannot be mutated through generic management.
    #[error(transparent)]
    ManagedKnowledgeOperation(#[from] ManagedKnowledgeConnectionOperationError),
    /// Connector domain or persistence rejected the command.
    #[error(transparent)]
    Connector(#[from] moa_connectors::Error),
}

/// Result returned by connector-management operations.
pub type ConnectorManagementResult<T> = Result<T, ConnectorManagementError>;
/// Local destination admission used by create verification and activation.
#[async_trait]
pub trait ConnectorDestinationVerifier: Send + Sync {
    /// Verifies only local, reviewed destination constraints and never sends a request.
    async fn verify_local(
        &self,
        connection: &ConnectorConnection,
    ) -> Result<(), ConnectorDestinationVerificationError>;
}

/// Strict outbound-policy-backed local destination verifier.
#[derive(Clone)]
pub struct PolicyConnectorDestinationVerifier {
    policy: OutboundHttpPolicy,
}

impl PolicyConnectorDestinationVerifier {
    /// Creates a local verifier from the deployment's outbound HTTP policy.
    #[must_use]
    pub fn new(policy: OutboundHttpPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl ConnectorDestinationVerifier for PolicyConnectorDestinationVerifier {
    async fn verify_local(
        &self,
        connection: &ConnectorConnection,
    ) -> Result<(), ConnectorDestinationVerificationError> {
        let origin = connection
            .origin
            .as_ref()
            .ok_or(ConnectorDestinationVerificationError::Rejected)?;
        self.policy
            .admit(origin.as_str(), DESTINATION_ADMISSION_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(map_destination_error)
    }
}

fn map_destination_error(
    error: OutboundHttpAdmissionError,
) -> ConnectorDestinationVerificationError {
    match error {
        OutboundHttpAdmissionError::ResolutionFailed
        | OutboundHttpAdmissionError::ResolutionTimedOut
        | OutboundHttpAdmissionError::EmptyAddressSet => {
            ConnectorDestinationVerificationError::Unavailable
        }
        OutboundHttpAdmissionError::InvalidOrigin
        | OutboundHttpAdmissionError::HttpsRequired
        | OutboundHttpAdmissionError::NonCanonicalHost
        | OutboundHttpAdmissionError::AddressDenied
        | OutboundHttpAdmissionError::PortMismatch => {
            ConnectorDestinationVerificationError::Rejected
        }
    }
}
/// Secret-free, authorization-first connection-management application service.
#[derive(Clone)]
pub struct ConnectorManagementService {
    authorizer: Arc<dyn ConnectorManagementAuthorizer>,
    definitions: Arc<dyn ConnectorDefinitionResolver>,
    connections: ConnectorService,
    use_grants: Arc<dyn ConnectionUseGrantRepository>,
    destinations: Arc<dyn ConnectorDestinationVerifier>,
    credential_revoker: Arc<dyn ConnectionCredentialRevoker>,
}

impl ConnectorManagementService {
    /// Composes the independently injectable authorization, definition,
    /// connection, destination, direct-use, and credential-lifecycle ports.
    #[must_use]
    pub fn new(
        authorizer: Arc<dyn ConnectorManagementAuthorizer>,
        definitions: Arc<dyn ConnectorDefinitionResolver>,
        connections: ConnectorService,
        use_grants: Arc<dyn ConnectionUseGrantRepository>,
        destinations: Arc<dyn ConnectorDestinationVerifier>,
        credential_revoker: Arc<dyn ConnectionCredentialRevoker>,
    ) -> Self {
        Self {
            authorizer,
            definitions,
            connections,
            use_grants,
            destinations,
            credential_revoker,
        }
    }

    /// Creates one pending connection after tenant-Admin authorization and
    /// exact published-definition resolution.
    pub async fn create(
        &self,
        identity: &Identity,
        request: ConnectorConnectionCreateRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer.require_tenant_admin(identity).await?;
        let owner_identity_id = connection_owner(identity)?;
        let resolved = self
            .definitions
            .resolve_for_install(identity.tenant_id, &request.definition_ref)
            .await?;
        let requested_ref = wire_artifact_definition_to_domain(&request.definition_ref);
        ensure_definition_ref(&resolved, &requested_ref)?;
        let origin = request.origin.parse()?;
        let non_secret_config =
            request
                .non_secret_config
                .as_object()
                .cloned()
                .ok_or_else(|| moa_connectors::Error::InvalidContract {
                    message: "connector non-secret config must be a JSON object".to_string(),
                })?;
        let connection = self
            .connections
            .create(CreateConnectionRequest {
                connection_id: request.connection_id,
                tenant_id: identity.tenant_id,
                display_name: request.display_name,
                definition_ref: requested_ref,
                origin,
                non_secret_config,
                created_by_identity_id: Some(identity.id),
                owner_identity_id,
            })
            .await?;
        let readiness = declared_slot_readiness(&resolved.credential_requirements, false)?;
        Ok(connection_response(connection, readiness))
    }

    /// Lists connections only after tenant-Admin authorization has bounded the
    /// entire tenant-wide read.
    pub async fn list(
        &self,
        identity: &Identity,
        request: WireConnectionListRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionListResponse> {
        self.authorizer.require_tenant_admin(identity).await?;
        let limit = request.limit.unwrap_or(DEFAULT_CONNECTION_LIST_LIMIT);
        if limit == 0 || limit > MAX_CONNECTION_LIST_LIMIT {
            return Err(moa_connectors::Error::InvalidContract {
                message: "connector list limit must be in 1..=100".to_string(),
            }
            .into());
        }
        let page = self
            .connections
            .list(
                identity.tenant_id,
                ConnectionListRequest {
                    after: request.cursor,
                    limit,
                },
            )
            .await?;
        let references = page
            .connections
            .iter()
            .map(|connection| connection.definition.clone())
            .collect::<Vec<_>>();
        let resolved = self
            .definitions
            .resolve_installed_batch(identity.tenant_id, &references)
            .await?;
        if resolved.len() != page.connections.len() {
            return Err(ConnectorDefinitionResolutionError::Unavailable.into());
        }
        let mut slot_requests = Vec::new();
        let mut slot_counts = Vec::with_capacity(resolved.len());
        for (connection, definition) in page.connections.iter().zip(&resolved) {
            ensure_definition_ref(definition, &connection.definition)?;
            let required = moa_connectors::service::required_credential_slots_for_requirements(
                &definition.credential_requirements,
            )?;
            slot_counts.push(required.len());
            slot_requests.extend(required.into_iter().map(|slot| ConnectionCredentialSlot {
                connection_id: connection.connection_id,
                slot: slot.slot,
                kind: slot.kind,
            }));
        }
        let readiness = self
            .connections
            .credential_slot_readiness_batch(identity.tenant_id, &slot_requests)
            .await?;
        let mut readiness = readiness.into_iter();
        let mut responses = Vec::with_capacity(page.connections.len());
        for (connection, slot_count) in page.connections.into_iter().zip(slot_counts) {
            let connection_readiness = readiness
                .by_ref()
                .take(slot_count)
                .map(|slot| CredentialSlotReadiness {
                    slot: slot.slot,
                    kind: slot.kind,
                    ready: slot.ready,
                })
                .collect();
            responses.push(connection_response(connection, connection_readiness));
        }
        Ok(ConnectorConnectionListResponse {
            connections: responses,
            next_cursor: page.next_cursor,
        })
    }

    /// Loads one connection only after delegated `Manage` authorization.
    pub async fn get(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let connection = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        let resolved = self.resolve_connection_definition(&connection).await?;
        let readiness = self
            .connections
            .credential_slot_readiness_for_requirements(
                identity.tenant_id,
                connection_id,
                &resolved.credential_requirements,
            )
            .await?;
        Ok(connection_response(connection, readiness))
    }

    /// Locally admits the destination, verifies required slots, compiles
    /// immutable bindings, and activates the next generation.
    pub async fn activate(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(request.expected_generation)?;
        let current = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&current)?;
        require_generation(&current, expected_generation)?;
        let resolved = self.resolve_connection_definition(&current).await?;
        self.destinations.verify_local(&current).await?;
        let activated = self
            .connections
            .activate(ActivateConnectionRequest {
                tenant_id: identity.tenant_id,
                connection_id,
                expected_generation,
                definition_ref: current.definition.clone(),
                definition: resolved.artifact_definition()?.clone(),
            })
            .await?;
        Ok(connection_response(
            activated.connection,
            activated.credential_readiness,
        ))
    }

    /// Suspends an active connection under its exact generation fence.
    pub async fn suspend(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.transition_with_readiness(
            identity,
            connection_id,
            request,
            ConnectionStatus::Suspended,
        )
        .await
    }

    /// Resumes a suspended connection under its exact generation fence.
    pub async fn resume(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.transition_with_readiness(identity, connection_id, request, ConnectionStatus::Active)
            .await
    }

    /// Fences execution before revoking all connection credentials while
    /// preserving vault rows and operation audit.
    pub async fn disconnect(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(request.expected_generation)?;
        let current = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&current)?;
        require_generation(&current, expected_generation)?;
        let resolved = self.resolve_connection_definition(&current).await?;
        let fenced = if current.status == ConnectionStatus::Disconnecting {
            current
        } else {
            current.status.transition(ConnectionStatus::Disconnecting)?;
            self.connections
                .disconnect(identity.tenant_id, connection_id, expected_generation)
                .await?
        };
        self.revoke_connection_credentials(identity, &fenced, "disconnect")
            .await?;
        let readiness = declared_slot_readiness(&resolved.credential_requirements, false)?;
        Ok(connection_response(fenced, readiness))
    }

    /// Fences a connection, revokes its credentials, and marks it deleted.
    /// Connector and vault audit are retained.
    pub async fn delete(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(request.expected_generation)?;
        let current = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&current)?;
        require_generation(&current, expected_generation)?;
        let resolved = self.resolve_connection_definition(&current).await?;
        let fenced = if current.status == ConnectionStatus::Disconnecting {
            current
        } else {
            current.status.transition(ConnectionStatus::Disconnecting)?;
            self.connections
                .disconnect(identity.tenant_id, connection_id, expected_generation)
                .await?
        };
        self.revoke_connection_credentials(identity, &fenced, "delete")
            .await?;
        let deleted = self
            .connections
            .delete(identity.tenant_id, connection_id, expected_generation)
            .await?;
        let readiness = declared_slot_readiness(&resolved.credential_requirements, false)?;
        Ok(connection_response(deleted, readiness))
    }

    /// Runs local destination admission and credential readiness. Because the
    /// current reviewed definition has no connector-level remote verification
    /// contract, successful local verification returns typed `Unverified`.
    pub async fn verify(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
    ) -> ConnectorManagementResult<ConnectorConnectionVerificationResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(request.expected_generation)?;
        let connection = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&connection)?;
        require_generation(&connection, expected_generation)?;
        let resolved = self.resolve_connection_definition(&connection).await?;
        let readiness = self
            .connections
            .credential_slot_readiness_for_requirements(
                identity.tenant_id,
                connection_id,
                &resolved.credential_requirements,
            )
            .await?;
        let (verification, health, reason) = match self.destinations.verify_local(&connection).await
        {
            Ok(()) if readiness.iter().all(|slot| slot.ready) => (
                ConnectorVerificationState::Unverified,
                ConnectionHealth::Pending,
                Some("remote_verification_not_configured".to_string()),
            ),
            Ok(()) => (
                ConnectorVerificationState::Unverified,
                ConnectionHealth::Pending,
                Some("credential_slots_missing".to_string()),
            ),
            Err(ConnectorDestinationVerificationError::Rejected) => (
                ConnectorVerificationState::Pending,
                ConnectionHealth::Quarantined,
                Some("destination_rejected".to_string()),
            ),
            Err(ConnectorDestinationVerificationError::Unavailable) => (
                ConnectorVerificationState::Pending,
                ConnectionHealth::Unavailable,
                Some("destination_admission_unavailable".to_string()),
            ),
        };
        self.connections
            .update_health(
                identity.tenant_id,
                connection_id,
                expected_generation,
                health,
                reason.clone(),
            )
            .await?;
        Ok(ConnectorConnectionVerificationResponse {
            generation: expected_generation.get(),
            verification,
            health: wire_health(health),
            reason,
            credential_slots: readiness.into_iter().map(wire_slot).collect(),
        })
    }

    /// Grants one direct same-tenant `Use` relationship after delegated
    /// connection-`Manage` authorization.
    pub async fn grant_use(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionUseRequest,
    ) -> ConnectorManagementResult<()> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let connection = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&connection)?;
        self.use_grants
            .grant_use(ConnectionUseRequest {
                tenant_id: identity.tenant_id,
                connection_id,
                subject: use_subject(request.subject),
            })
            .await?;
        Ok(())
    }

    /// Revokes one direct same-tenant `Use` relationship after delegated
    /// connection-`Manage` authorization.
    pub async fn revoke_use(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionUseRequest,
    ) -> ConnectorManagementResult<()> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let connection = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&connection)?;
        self.use_grants
            .revoke_use(ConnectionUseRequest {
                tenant_id: identity.tenant_id,
                connection_id,
                subject: use_subject(request.subject),
            })
            .await?;
        Ok(())
    }

    /// Authorizes and validates one credential write before the private ingress
    /// stages any material in the vault.
    pub async fn prepare_credential_write(
        &self,
        identity: &Identity,
        metadata: &ConnectorCredentialWriteMetadata,
    ) -> ConnectorManagementResult<PreparedCredentialWrite> {
        self.authorizer
            .require_connection_manage(identity, metadata.connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(metadata.expected_generation)?;
        let connection = self
            .load_connection(identity.tenant_id, metadata.connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&connection)?;
        if matches!(
            connection.status,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted
        ) {
            return Err(moa_connectors::Error::InvalidContract {
                message: "credential writes are disabled during connector teardown".to_string(),
            }
            .into());
        }
        require_credential_fence_generation(&connection, expected_generation)?;
        let resolved = self.resolve_connection_definition(&connection).await?;
        let readiness = declared_slot_readiness(&resolved.credential_requirements, false)?;
        if !readiness
            .iter()
            .any(|slot| slot.slot == metadata.slot_name && slot.kind == metadata.kind)
        {
            return Err(ConnectorManagementError::CredentialSlotMismatch);
        }
        Ok(PreparedCredentialWrite {
            identity: CredentialIdentity {
                tenant_id: identity.tenant_id,
                connection_uid: metadata.connection_id.0,
                kind: metadata.kind,
                slot_name: metadata.slot_name.clone(),
            },
            expected_generation,
        })
    }

    /// Reauthorizes the prepared resource and advances its secret-free
    /// generation fence after the private ingress has staged material locally.
    pub async fn advance_credential_generation(
        &self,
        identity: &Identity,
        prepared: &PreparedCredentialWrite,
    ) -> ConnectorManagementResult<CredentialGenerationFenceResult> {
        let connection_id = ConnectorConnectionId(prepared.identity.connection_uid);
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        if identity.tenant_id != prepared.identity.tenant_id {
            return Err(ConnectorManagementAuthorizationError::Denied.into());
        }
        let current = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&current)?;
        let connection = if current.generation == prepared.expected_generation {
            self.connections
                .advance_credential_generation(CredentialGenerationFenceRequest {
                    tenant_id: identity.tenant_id,
                    connection_id,
                    expected_generation: prepared.expected_generation,
                })
                .await?
        } else {
            require_completed_credential_fence(&current, prepared.expected_generation)?;
            current
        };
        Ok(CredentialGenerationFenceResult {
            connection_id: connection.connection_id,
            generation: connection.generation,
            status: connection.status,
        })
    }

    async fn transition_with_readiness(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        request: ConnectorConnectionMutationRequest,
        target: ConnectionStatus,
    ) -> ConnectorManagementResult<ConnectorConnectionResponse> {
        self.authorizer
            .require_connection_manage(identity, connection_id)
            .await?;
        let expected_generation = ConnectionGeneration::new(request.expected_generation)?;
        let current = self
            .load_connection(identity.tenant_id, connection_id)
            .await?;
        reject_generic_management_of_managed_knowledge_parent(&current)?;
        require_generation(&current, expected_generation)?;
        let resolved = self.resolve_connection_definition(&current).await?;
        let readiness = self
            .connections
            .credential_slot_readiness_for_requirements(
                identity.tenant_id,
                connection_id,
                &resolved.credential_requirements,
            )
            .await?;
        let connection = match target {
            ConnectionStatus::Suspended => {
                self.connections
                    .suspend(identity.tenant_id, connection_id, expected_generation)
                    .await?
            }
            ConnectionStatus::Active => {
                self.connections
                    .resume(identity.tenant_id, connection_id, expected_generation)
                    .await?
            }
            ConnectionStatus::PendingAuth
            | ConnectionStatus::Disconnecting
            | ConnectionStatus::Deleted => {
                return Err(moa_connectors::Error::InvalidContract {
                    message: "unsupported management lifecycle target".to_string(),
                }
                .into());
            }
        };
        Ok(connection_response(connection, readiness))
    }

    async fn load_connection(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> ConnectorManagementResult<ConnectorConnection> {
        self.connections
            .get(tenant_id, connection_id)
            .await?
            .ok_or_else(|| moa_connectors::Error::ConnectionNotFound { connection_id }.into())
    }

    async fn resolve_connection_definition(
        &self,
        connection: &ConnectorConnection,
    ) -> ConnectorManagementResult<ResolvedConnectorDefinition> {
        let resolved = self
            .definitions
            .resolve_installed(connection.tenant_id, &connection.definition)
            .await?;
        ensure_definition_ref(&resolved, &connection.definition)?;
        Ok(resolved)
    }

    async fn revoke_connection_credentials(
        &self,
        identity: &Identity,
        connection: &ConnectorConnection,
        phase: &'static str,
    ) -> ConnectorManagementResult<()> {
        let context = credential_revocation_context(identity, connection, phase);
        self.credential_revoker
            .revoke_connection(connection.connection_id, &context)
            .await?;
        Ok(())
    }
}
fn connection_owner(identity: &Identity) -> ConnectorManagementResult<uuid::Uuid> {
    match (identity.identity_type, identity.acting_on_behalf_of) {
        (IdentityType::Operator, None) => Ok(identity.id),
        (IdentityType::Agent, Some(operator_id)) => Ok(operator_id),
        _ => Err(ConnectorManagementError::UnsupportedOwnerIdentity),
    }
}
fn reject_generic_management_of_managed_knowledge_parent(
    connection: &ConnectorConnection,
) -> ConnectorManagementResult<()> {
    if managed_knowledge_definition(&connection.definition).is_some() {
        return Err(ManagedKnowledgeConnectionOperationError.into());
    }
    Ok(())
}

fn ensure_definition_ref(
    resolved: &ResolvedConnectorDefinition,
    expected: &ConnectionDefinitionRef,
) -> ConnectorManagementResult<()> {
    if &resolved.definition_ref == expected {
        Ok(())
    } else {
        Err(ConnectorManagementError::DefinitionReferenceMismatch)
    }
}
fn require_generation(
    connection: &ConnectorConnection,
    expected: ConnectionGeneration,
) -> ConnectorManagementResult<()> {
    if connection.generation == expected {
        Ok(())
    } else {
        Err(moa_connectors::Error::GenerationConflict {
            expected,
            actual: connection.generation,
        }
        .into())
    }
}

fn require_credential_fence_generation(
    connection: &ConnectorConnection,
    expected: ConnectionGeneration,
) -> ConnectorManagementResult<()> {
    if connection.generation == expected {
        return Ok(());
    }
    require_completed_credential_fence(connection, expected)
}

fn require_completed_credential_fence(
    connection: &ConnectorConnection,
    expected: ConnectionGeneration,
) -> ConnectorManagementResult<()> {
    let fenced_generation = expected.next()?;
    if connection.generation == fenced_generation
        && matches!(
            connection.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended
        )
    {
        Ok(())
    } else {
        Err(moa_connectors::Error::GenerationConflict {
            expected,
            actual: connection.generation,
        }
        .into())
    }
}

fn declared_slot_readiness(
    auth: &[RuntimeConnectorAuthRequirement],
    ready: bool,
) -> ConnectorManagementResult<Vec<CredentialSlotReadiness>> {
    Ok(
        moa_connectors::service::required_credential_slots_for_requirements(auth)?
            .into_iter()
            .map(|required| CredentialSlotReadiness {
                slot: required.slot,
                kind: required.kind,
                ready,
            })
            .collect(),
    )
}
