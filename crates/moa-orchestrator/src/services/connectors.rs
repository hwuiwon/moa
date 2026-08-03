//! Authorized orchestration for tenant connector connection management.
//!
//! This module deliberately separates the secret-free management application
//! service from Restate and HTTP adapters. Public handlers and the private
//! credential ingress both supply an authenticated [`Identity`]; credential
//! plaintext and host-local staging tokens never enter this API.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_artifacts::connector::{RuntimeConnectorDefinitionV1, RuntimeConnectorKindV1};
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_connectors::domain::{
    ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth, ConnectionStatus,
    ConnectorConnection, ManagedParentDefinition,
};
use moa_connectors::repository::{
    ConnectionUseGrantRepository, ConnectionUseRequest, ConnectorUseSubject,
};
use moa_connectors::service::{
    ActivateConnectionRequest, ConnectorService, CreateConnectionRequest,
    CredentialGenerationFenceRequest, CredentialSlotReadiness,
};
use moa_core::traits::{CredentialVault, Identity, IdentityType};
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::credentials::{
    CredentialContext, CredentialIdentity, CredentialOperation, CredentialPrincipal,
    CredentialServiceActor,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_security::outbound_http::{OutboundHttpAdmissionError, OutboundHttpPolicy};
use moa_wire::connectors::{
    ConnectorConnectionCreateRequest, ConnectorConnectionHealth, ConnectorConnectionListResponse,
    ConnectorConnectionMutationRequest, ConnectorConnectionResponse, ConnectorConnectionStatus,
    ConnectorConnectionUseRequest, ConnectorConnectionVerificationResponse,
    ConnectorCredentialSlotResponse, ConnectorCredentialWriteMetadata,
    ConnectorDefinitionReference, ConnectorUseSubject as WireUseSubject,
    ConnectorVerificationState,
};

const DESTINATION_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);
const CREDENTIAL_REVOCATION_HASH_DOMAIN: &str = "moa.connector.connection-credential-revoke.v1";
const CREDENTIAL_READINESS_HASH_DOMAIN: &str = "moa.connector.credential-readiness.v1";

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

/// Exact connector-definition resolution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConnectorDefinitionResolutionError {
    /// The exact definition does not exist in the caller's visible scope.
    #[error("connector definition not found")]
    NotFound,
    /// The exact definition exists but is not eligible for a new installation.
    #[error("connector definition is not published")]
    NotPublished,
    /// The referenced artifact is not a connection-installable connector.
    #[error("connector definition is not installable")]
    NotInstallable,
    /// The code-owned built-in definition has no configured resolver.
    #[error("built-in connector definition unavailable")]
    BuiltInUnavailable,
    /// Definition persistence could not produce a trustworthy result.
    #[error("connector definition resolution unavailable")]
    Unavailable,
}

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

/// Sanitized audit-preserving credential-revocation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("connector credential revocation unavailable")]
pub struct ConnectorCredentialRevocationError;

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

/// One exact immutable connector definition resolved from an approved source.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedConnectorDefinition {
    /// Domain definition reference expected on the connection row.
    pub definition_ref: ConnectionDefinitionRef,
    /// Validated, connection-installable runtime definition.
    pub definition: RuntimeConnectorDefinitionV1,
}

/// Definition lookup port separating artifact and code-owned catalogs.
#[async_trait]
pub trait ConnectorDefinitionResolver: Send + Sync {
    /// Resolves an exact definition eligible for a new installation.
    async fn resolve_for_install(
        &self,
        tenant_id: TenantId,
        reference: &ConnectorDefinitionReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError>;

    /// Resolves the exact immutable definition already pinned by a connection.
    async fn resolve_installed(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError>;
}

/// Artifact-backed resolver for tenant-visible connector revisions.
///
/// Built-in definitions remain code-owned and require a separate composite
/// resolver; this adapter never guesses a built-in body from a string key.
#[derive(Clone)]
pub struct ArtifactConnectorDefinitionResolver {
    registry: ArtifactRegistry,
}

impl ArtifactConnectorDefinitionResolver {
    /// Creates an artifact-backed resolver.
    #[must_use]
    pub fn new(registry: ArtifactRegistry) -> Self {
        Self { registry }
    }

    async fn load_artifact(
        &self,
        tenant_id: TenantId,
        artifact_uid: uuid::Uuid,
        revision_uid: uuid::Uuid,
        require_published: bool,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        let scope = ActionRuleScope::Tenant { tenant_id };
        let stored = self
            .registry
            .load_revision(&scope, revision_uid)
            .await
            .map_err(|_| ConnectorDefinitionResolutionError::Unavailable)?
            .ok_or(ConnectorDefinitionResolutionError::NotFound)?;
        if stored.artifact_uid != artifact_uid || stored.kind != ArtifactKind::Connector {
            return Err(ConnectorDefinitionResolutionError::NotFound);
        }
        if require_published && stored.status != ArtifactStatus::Published {
            return Err(ConnectorDefinitionResolutionError::NotPublished);
        }
        let ArtifactDefinition::Connector(connector) = stored.document.definition else {
            return Err(ConnectorDefinitionResolutionError::NotInstallable);
        };
        let definition = connector
            .runtime_v1()
            .cloned()
            .ok_or(ConnectorDefinitionResolutionError::NotInstallable)?;
        Ok(ResolvedConnectorDefinition {
            definition_ref: ConnectionDefinitionRef::Artifact {
                artifact_uid,
                revision_uid,
            },
            definition,
        })
    }
}

#[async_trait]
impl ConnectorDefinitionResolver for ArtifactConnectorDefinitionResolver {
    async fn resolve_for_install(
        &self,
        tenant_id: TenantId,
        reference: &ConnectorDefinitionReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        match reference {
            ConnectorDefinitionReference::Artifact {
                artifact_uid,
                revision_uid,
            } => {
                self.load_artifact(tenant_id, *artifact_uid, *revision_uid, true)
                    .await
            }
            ConnectorDefinitionReference::BuiltIn { .. } => {
                Err(ConnectorDefinitionResolutionError::BuiltInUnavailable)
            }
        }
    }

    async fn resolve_installed(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        match reference {
            ConnectionDefinitionRef::Artifact {
                artifact_uid,
                revision_uid,
            } => {
                self.load_artifact(tenant_id, *artifact_uid, *revision_uid, false)
                    .await
            }
            ConnectionDefinitionRef::BuiltIn { .. } => {
                Err(ConnectorDefinitionResolutionError::BuiltInUnavailable)
            }
        }
    }
}

/// Composite resolver for artifact definitions and installed-only managed
/// knowledge parents.
///
/// Managed definitions are unavailable to public creation: knowledge linking
/// owns their exact configuration, provider journal, and child projection.
/// Artifact installation and resolution remain delegated unchanged.
#[derive(Clone)]
pub struct ManagedKnowledgeConnectorDefinitionResolver {
    artifacts: Arc<dyn ConnectorDefinitionResolver>,
}

impl ManagedKnowledgeConnectorDefinitionResolver {
    /// Composes installed-only managed knowledge definitions with artifact resolution.
    #[must_use]
    pub fn new(artifacts: Arc<dyn ConnectorDefinitionResolver>) -> Self {
        Self { artifacts }
    }
}

#[async_trait]
impl ConnectorDefinitionResolver for ManagedKnowledgeConnectorDefinitionResolver {
    async fn resolve_for_install(
        &self,
        tenant_id: TenantId,
        reference: &ConnectorDefinitionReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        self.artifacts
            .resolve_for_install(tenant_id, reference)
            .await
    }

    async fn resolve_installed(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        if let Some(managed) = managed_knowledge_definition(reference) {
            return Ok(ResolvedConnectorDefinition {
                definition_ref: managed.definition_ref(),
                definition: managed.runtime_definition(),
            });
        }
        self.artifacts.resolve_installed(tenant_id, reference).await
    }
}

/// Local destination admission used by create verification and activation.
#[async_trait]
pub trait ConnectorDestinationVerifier: Send + Sync {
    /// Verifies only local, reviewed destination constraints and never sends a request.
    async fn verify_local(
        &self,
        definition: &RuntimeConnectorDefinitionV1,
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
        definition: &RuntimeConnectorDefinitionV1,
        connection: &ConnectorConnection,
    ) -> Result<(), ConnectorDestinationVerificationError> {
        if matches!(
            definition.runtime,
            RuntimeConnectorKindV1::BuiltInManaged { .. }
        ) {
            return Ok(());
        }
        let origin = connection
            .non_secret_config
            .get("origin")
            .and_then(serde_json::Value::as_str)
            .ok_or(ConnectorDestinationVerificationError::Rejected)?;
        self.policy
            .admit(origin, DESTINATION_ADMISSION_TIMEOUT)
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
    async fn credential_slot_readiness(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        slots: &[moa_connectors::service::RequiredCredentialSlot],
    ) -> moa_connectors::Result<Vec<CredentialSlotReadiness>> {
        let mut readiness = Vec::with_capacity(slots.len());
        for slot in slots {
            let identity = CredentialIdentity {
                tenant_id,
                connection_uid: connection_id.0,
                kind: slot.kind,
                slot_name: slot.slot.clone(),
            };
            let operation_id = uuid::Uuid::now_v7().to_string();
            let context = CredentialContext {
                tenant_id,
                principal: CredentialPrincipal::Service {
                    actor: CredentialServiceActor::ConnectorManagementReadiness,
                },
                operation: CredentialOperation::Resolve,
                request_hash: credential_readiness_hash(&identity, &operation_id),
                operation_id,
            };
            let ready = self.vault.has_active(&identity, &context).await?;
            readiness.push(CredentialSlotReadiness {
                slot: slot.slot.clone(),
                kind: slot.kind,
                ready,
            });
        }
        Ok(readiness)
    }
}

/// Authorization-approved credential selector retained by the private ingress.
///
/// The value contains no material, reference, or version and does not implement
/// serialization. Only this module can construct it, so a caller cannot bypass
/// definition slot validation before requesting the generation fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCredentialWrite {
    identity: CredentialIdentity,
    expected_generation: ConnectionGeneration,
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
    connection_id: ConnectorConnectionId,
    generation: ConnectionGeneration,
    status: ConnectionStatus,
}

/// Internal Restate selector corresponding to one public connection path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionSelector {
    /// Exact connection selected by the authenticated public route.
    pub connection_id: ConnectorConnectionId,
}

/// Internal Restate lifecycle command assembled from path and body fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionMutationCommand {
    /// Exact connection selected by the authenticated public route.
    pub connection_id: ConnectorConnectionId,
    /// Generation observed by the management caller.
    pub expected_generation: u64,
}

/// Internal Restate direct-use command assembled from path and body fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionUseCommand {
    /// Exact connection selected by the authenticated public route.
    pub connection_id: ConnectorConnectionId,
    /// Exact closed same-tenant subject receiving or losing direct use.
    #[serde(flatten)]
    pub request: ConnectorConnectionUseRequest,
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
        let requested_ref = wire_definition_to_domain(&request.definition_ref)?;
        ensure_definition_ref(&resolved, &requested_ref)?;
        let origin = request.origin.as_deref().map(str::parse).transpose()?;
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
                definition: resolved.definition.clone(),
                origin,
                non_secret_config,
                created_by_identity_id: Some(identity.id),
                owner_identity_id,
            })
            .await?;
        let readiness = declared_slot_readiness(&resolved.definition, false)?;
        Ok(connection_response(connection, readiness))
    }

    /// Lists connections only after tenant-Admin authorization has bounded the
    /// entire tenant-wide read.
    pub async fn list(
        &self,
        identity: &Identity,
    ) -> ConnectorManagementResult<ConnectorConnectionListResponse> {
        self.authorizer.require_tenant_admin(identity).await?;
        let connections = self.connections.list(identity.tenant_id).await?;
        let mut responses = Vec::with_capacity(connections.len());
        for connection in connections {
            let resolved = self.resolve_connection_definition(&connection).await?;
            let readiness = self
                .connections
                .credential_slot_readiness(
                    identity.tenant_id,
                    connection.connection_id,
                    &resolved.definition,
                )
                .await?;
            responses.push(connection_response(connection, readiness));
        }
        Ok(ConnectorConnectionListResponse {
            connections: responses,
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
            .credential_slot_readiness(identity.tenant_id, connection_id, &resolved.definition)
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
        self.destinations
            .verify_local(&resolved.definition, &current)
            .await?;
        let readiness = self
            .connections
            .credential_slot_readiness(identity.tenant_id, connection_id, &resolved.definition)
            .await?;
        require_ready_slots(&readiness)?;
        let connection = self
            .connections
            .activate(ActivateConnectionRequest {
                tenant_id: identity.tenant_id,
                connection_id,
                expected_generation,
                definition_ref: current.definition.clone(),
                definition: resolved.definition.clone(),
            })
            .await?;
        Ok(connection_response(connection, readiness))
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
        let readiness = declared_slot_readiness(&resolved.definition, false)?;
        Ok(connection_response(fenced, readiness))
    }

    /// Marks a pending-auth or disconnecting connection deleted after ensuring
    /// no active credential remains. Connector and vault audit are retained.
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
        current.status.transition(ConnectionStatus::Deleted)?;
        self.revoke_connection_credentials(identity, &current, "delete")
            .await?;
        let deleted = self
            .connections
            .delete(identity.tenant_id, connection_id, expected_generation)
            .await?;
        let readiness = declared_slot_readiness(&resolved.definition, false)?;
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
        require_generation(&connection, expected_generation)?;
        let resolved = self.resolve_connection_definition(&connection).await?;
        let readiness = self
            .connections
            .credential_slot_readiness(identity.tenant_id, connection_id, &resolved.definition)
            .await?;
        let (verification, health, reason) = match self
            .destinations
            .verify_local(&resolved.definition, &connection)
            .await
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
        let readiness = declared_slot_readiness(&resolved.definition, false)?;
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
        if target == ConnectionStatus::Active {
            reject_generic_management_of_managed_knowledge_parent(&current)?;
        }
        require_generation(&current, expected_generation)?;
        let resolved = self.resolve_connection_definition(&current).await?;
        let readiness = self
            .connections
            .credential_slot_readiness(identity.tenant_id, connection_id, &resolved.definition)
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

mod restate_adapter {
    use moa_observability::restate_observability::annotate_restate_handler_span;
    use restate_sdk::prelude::*;

    use super::*;
    use crate::handlers::authz_shim::require_identity;

    /// Restate surface for secret-free connector connection management.
    #[restate_sdk::service]
    #[name = "ConnectorConnections"]
    pub trait ConnectorConnections {
        /// Creates one pending connection from an exact published definition.
        async fn create(
            request: Json<ConnectorConnectionCreateRequest>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Lists connections in the authenticated caller's tenant.
        async fn list() -> Result<Json<ConnectorConnectionListResponse>, HandlerError>;

        /// Gets one exact connection.
        async fn get(
            selector: Json<ConnectorConnectionSelector>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Verifies local destination admission and credential readiness.
        async fn verify(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionVerificationResponse>, HandlerError>;

        /// Compiles bindings and activates one connection generation.
        async fn activate(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Suspends one active connection.
        async fn suspend(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Resumes one suspended connection.
        async fn resume(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Fences and disconnects one connection while preserving audit.
        async fn disconnect(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Deletes one pending-auth or already-disconnecting connection projection.
        async fn delete(
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

        /// Grants one direct same-tenant connection-use relationship.
        async fn grant_use(
            command: Json<ConnectorConnectionUseCommand>,
        ) -> Result<(), HandlerError>;

        /// Revokes one direct same-tenant connection-use relationship.
        async fn revoke_use(
            command: Json<ConnectorConnectionUseCommand>,
        ) -> Result<(), HandlerError>;
    }

    /// Concrete Restate adapter around the independently testable application service.
    #[derive(Clone)]
    pub struct ConnectorConnectionsImpl {
        service: ConnectorManagementService,
    }

    #[derive(Clone, Copy)]
    enum MutationOperation {
        Activate,
        Suspend,
        Resume,
        Disconnect,
        Delete,
    }

    impl ConnectorConnectionsImpl {
        /// Creates the Restate adapter.
        #[must_use]
        pub const fn new(service: ConnectorManagementService) -> Self {
            Self { service }
        }
    }

    impl ConnectorConnections for ConnectorConnectionsImpl {
        #[tracing::instrument(skip(self, ctx, request))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before definition or connection access.
        async fn create(
            &self,
            ctx: Context<'_>,
            request: Json<ConnectorConnectionCreateRequest>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "create");
            let identity = require_identity(&ctx)?;
            let service = self.service.clone();
            let request = request.into_inner();
            Ok(ctx
                .run(|| async move {
                    service
                        .create(&identity, request)
                        .await
                        .map(Json)
                        .map_err(management_error_to_handler_error)
                })
                .name("connector_connections_create")
                .await?)
        }

        #[tracing::instrument(skip(self, ctx))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before tenant connection reads.
        async fn list(
            &self,
            ctx: Context<'_>,
        ) -> Result<Json<ConnectorConnectionListResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "list");
            let identity = require_identity(&ctx)?;
            let service = self.service.clone();
            Ok(ctx
                .run(|| async move {
                    service
                        .list(&identity)
                        .await
                        .map(Json)
                        .map_err(management_error_to_handler_error)
                })
                .name("connector_connections_list")
                .await?)
        }

        #[tracing::instrument(skip(self, ctx, selector))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before connection reads.
        async fn get(
            &self,
            ctx: Context<'_>,
            selector: Json<ConnectorConnectionSelector>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "get");
            let identity = require_identity(&ctx)?;
            let service = self.service.clone();
            let selector = selector.into_inner();
            Ok(ctx
                .run(|| async move {
                    service
                        .get(&identity, selector.connection_id)
                        .await
                        .map(Json)
                        .map_err(management_error_to_handler_error)
                })
                .name("connector_connections_get")
                .await?)
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before connection or credential-metadata reads.
        async fn verify(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionVerificationResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "verify");
            let identity = require_identity(&ctx)?;
            let service = self.service.clone();
            let command = command.into_inner();
            Ok(ctx
                .run(|| async move {
                    service
                        .verify(&identity, command.connection_id, mutation(&command))
                        .await
                        .map(Json)
                        .map_err(management_error_to_handler_error)
                })
                .name("connector_connections_verify")
                .await?)
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before activation reads or writes.
        async fn activate(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "activate");
            mutation_response(
                self.service.clone(),
                ctx,
                command,
                MutationOperation::Activate,
            )
            .await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle reads or writes.
        async fn suspend(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "suspend");
            mutation_response(
                self.service.clone(),
                ctx,
                command,
                MutationOperation::Suspend,
            )
            .await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle reads or writes.
        async fn resume(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "resume");
            mutation_response(
                self.service.clone(),
                ctx,
                command,
                MutationOperation::Resume,
            )
            .await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle and credential-revocation access.
        async fn disconnect(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "disconnect");
            mutation_response(
                self.service.clone(),
                ctx,
                command,
                MutationOperation::Disconnect,
            )
            .await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before deletion reads or writes.
        async fn delete(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionMutationCommand>,
        ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "delete");
            mutation_response(
                self.service.clone(),
                ctx,
                command,
                MutationOperation::Delete,
            )
            .await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before direct-use writes.
        async fn grant_use(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionUseCommand>,
        ) -> Result<(), HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "grant_use");
            use_response(self.service.clone(), ctx, command, true).await
        }

        #[tracing::instrument(skip(self, ctx, command))]
        // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before direct-use writes.
        async fn revoke_use(
            &self,
            ctx: Context<'_>,
            command: Json<ConnectorConnectionUseCommand>,
        ) -> Result<(), HandlerError> {
            annotate_restate_handler_span("ConnectorConnections", "revoke_use");
            use_response(self.service.clone(), ctx, command, false).await
        }
    }

    async fn mutation_response(
        service: ConnectorManagementService,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
        operation: MutationOperation,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        let identity = require_identity(&ctx)?;
        let command = command.into_inner();
        Ok(ctx
            .run(|| async move {
                let request = mutation(&command);
                let result = match operation {
                    MutationOperation::Activate => {
                        service
                            .activate(&identity, command.connection_id, request)
                            .await
                    }
                    MutationOperation::Suspend => {
                        service
                            .suspend(&identity, command.connection_id, request)
                            .await
                    }
                    MutationOperation::Resume => {
                        service
                            .resume(&identity, command.connection_id, request)
                            .await
                    }
                    MutationOperation::Disconnect => {
                        service
                            .disconnect(&identity, command.connection_id, request)
                            .await
                    }
                    MutationOperation::Delete => {
                        service
                            .delete(&identity, command.connection_id, request)
                            .await
                    }
                };
                result.map(Json).map_err(management_error_to_handler_error)
            })
            .name(match operation {
                MutationOperation::Activate => "connector_connections_activate",
                MutationOperation::Suspend => "connector_connections_suspend",
                MutationOperation::Resume => "connector_connections_resume",
                MutationOperation::Disconnect => "connector_connections_disconnect",
                MutationOperation::Delete => "connector_connections_delete",
            })
            .await?)
    }

    async fn use_response(
        service: ConnectorManagementService,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionUseCommand>,
        grant: bool,
    ) -> Result<(), HandlerError> {
        let identity = require_identity(&ctx)?;
        let command = command.into_inner();
        Ok(ctx
            .run(|| async move {
                let result = if grant {
                    service
                        .grant_use(&identity, command.connection_id, command.request)
                        .await
                } else {
                    service
                        .revoke_use(&identity, command.connection_id, command.request)
                        .await
                };
                result.map_err(management_error_to_handler_error)
            })
            .name(if grant {
                "connector_connections_grant_use"
            } else {
                "connector_connections_revoke_use"
            })
            .await?)
    }

    const fn mutation(
        command: &ConnectorConnectionMutationCommand,
    ) -> ConnectorConnectionMutationRequest {
        ConnectorConnectionMutationRequest {
            expected_generation: command.expected_generation,
        }
    }

    fn management_error_to_handler_error(error: ConnectorManagementError) -> HandlerError {
        let (code, message) = match error {
            ConnectorManagementError::Authorization(
                ConnectorManagementAuthorizationError::Denied,
            ) => (403, "forbidden"),
            ConnectorManagementError::Authorization(
                ConnectorManagementAuthorizationError::Unavailable,
            ) => (503, "authorization unavailable"),
            ConnectorManagementError::Definition(ConnectorDefinitionResolutionError::NotFound)
            | ConnectorManagementError::Connector(moa_connectors::Error::ConnectionNotFound {
                ..
            }) => (404, "connector resource not found"),
            ConnectorManagementError::Definition(
                ConnectorDefinitionResolutionError::Unavailable
                | ConnectorDefinitionResolutionError::BuiltInUnavailable,
            )
            | ConnectorManagementError::Destination(
                ConnectorDestinationVerificationError::Unavailable,
            )
            | ConnectorManagementError::CredentialRevocation(_)
            | ConnectorManagementError::Connector(
                moa_connectors::Error::DatabaseScope(_)
                | moa_connectors::Error::Authorization(_)
                | moa_connectors::Error::AuthorizationUnavailable
                | moa_connectors::Error::ManagedParentRepositoryUnavailable
                | moa_connectors::Error::Storage(_),
            ) => (503, "connector management unavailable"),
            ConnectorManagementError::Connector(
                moa_connectors::Error::GenerationConflict { .. }
                | moa_connectors::Error::InvalidTransition { .. }
                | moa_connectors::Error::InvocationConflict { .. }
                | moa_connectors::Error::InvocationStateConflict { .. },
            ) => (409, "connector state conflict"),
            ConnectorManagementError::Definition(
                ConnectorDefinitionResolutionError::NotPublished
                | ConnectorDefinitionResolutionError::NotInstallable,
            )
            | ConnectorManagementError::Destination(
                ConnectorDestinationVerificationError::Rejected,
            )
            | ConnectorManagementError::CredentialSlotMismatch
            | ConnectorManagementError::ManagedKnowledgeOperation(_)
            | ConnectorManagementError::Connector(
                moa_connectors::Error::InvalidConnectionOrigin { .. }
                | moa_connectors::Error::InvalidGeneration { .. }
                | moa_connectors::Error::GenerationExhausted
                | moa_connectors::Error::InvalidContract { .. }
                | moa_connectors::Error::CredentialSlotMissing { .. }
                | moa_connectors::Error::UseGrantConnectionUnavailable { .. }
                | moa_connectors::Error::UseGrantSubjectNotFound { .. }
                | moa_connectors::Error::UseGrantSubjectInactive { .. },
            ) => (400, "invalid connector management request"),
            ConnectorManagementError::UnsupportedOwnerIdentity => {
                (403, "connector owner identity is not permitted")
            }
            ConnectorManagementError::DefinitionReferenceMismatch
            | ConnectorManagementError::Connector(_) => {
                (500, "connector management invariant failed")
            }
        };
        TerminalError::new_with_code(code, message).into()
    }
}

pub use restate_adapter::{ConnectorConnections, ConnectorConnectionsImpl};

fn connection_owner(identity: &Identity) -> ConnectorManagementResult<uuid::Uuid> {
    match (identity.identity_type, identity.acting_on_behalf_of) {
        (IdentityType::Operator, None) => Ok(identity.id),
        (IdentityType::Agent, Some(operator_id)) => Ok(operator_id),
        _ => Err(ConnectorManagementError::UnsupportedOwnerIdentity),
    }
}

fn managed_knowledge_definition(
    reference: &ConnectionDefinitionRef,
) -> Option<ManagedParentDefinition> {
    [
        ManagedParentDefinition::KnowledgeNangoV1,
        ManagedParentDefinition::KnowledgeMergeV1,
    ]
    .into_iter()
    .find(|managed| &managed.definition_ref() == reference)
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

fn wire_definition_to_domain(
    reference: &ConnectorDefinitionReference,
) -> ConnectorManagementResult<ConnectionDefinitionRef> {
    match reference {
        ConnectorDefinitionReference::Artifact {
            artifact_uid,
            revision_uid,
        } => Ok(ConnectionDefinitionRef::Artifact {
            artifact_uid: *artifact_uid,
            revision_uid: *revision_uid,
        }),
        ConnectorDefinitionReference::BuiltIn { key, version } => Ok(
            ConnectionDefinitionRef::built_in(key.clone(), version.get())?,
        ),
    }
}

fn domain_definition_to_wire(reference: &ConnectionDefinitionRef) -> ConnectorDefinitionReference {
    match reference {
        ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        } => ConnectorDefinitionReference::Artifact {
            artifact_uid: *artifact_uid,
            revision_uid: *revision_uid,
        },
        ConnectionDefinitionRef::BuiltIn { key, version } => {
            ConnectorDefinitionReference::BuiltIn {
                key: key.clone(),
                version: *version,
            }
        }
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

fn require_ready_slots(readiness: &[CredentialSlotReadiness]) -> ConnectorManagementResult<()> {
    if let Some(missing) = readiness.iter().find(|slot| !slot.ready) {
        return Err(moa_connectors::Error::CredentialSlotMissing {
            slot: missing.slot.clone(),
        }
        .into());
    }
    Ok(())
}

fn use_subject(subject: WireUseSubject) -> ConnectorUseSubject {
    match subject {
        WireUseSubject::Operator { id } => ConnectorUseSubject::Operator { id },
        WireUseSubject::Agent { id } => ConnectorUseSubject::Agent { id },
        WireUseSubject::Contact { id } => ConnectorUseSubject::Contact { id },
    }
}

fn credential_revocation_context(
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

fn credential_readiness_hash(identity: &CredentialIdentity, operation_id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    for part in [
        CREDENTIAL_READINESS_HASH_DOMAIN.as_bytes(),
        identity.tenant_id.to_string().as_bytes(),
        identity.connection_uid.to_string().as_bytes(),
        identity.kind.as_str().as_bytes(),
        identity.slot_name.as_str().as_bytes(),
        operation_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().to_hex().to_string()
}

fn declared_slot_readiness(
    definition: &RuntimeConnectorDefinitionV1,
    ready: bool,
) -> ConnectorManagementResult<Vec<CredentialSlotReadiness>> {
    Ok(
        moa_connectors::service::required_credential_slots(definition)?
            .into_iter()
            .map(|required| CredentialSlotReadiness {
                slot: required.slot,
                kind: required.kind,
                ready,
            })
            .collect(),
    )
}

fn connection_response(
    connection: ConnectorConnection,
    readiness: Vec<CredentialSlotReadiness>,
) -> ConnectorConnectionResponse {
    ConnectorConnectionResponse {
        connection_id: connection.connection_id,
        display_name: connection.display_name,
        definition_ref: domain_definition_to_wire(&connection.definition),
        non_secret_config: connection.non_secret_config,
        generation: connection.generation.get(),
        status: wire_status(connection.status),
        health: wire_health(connection.health),
        health_reason: connection.health_reason,
        credential_slots: readiness.into_iter().map(wire_slot).collect(),
        created_at: connection.created_at,
        updated_at: connection.updated_at,
    }
}

fn wire_status(status: ConnectionStatus) -> ConnectorConnectionStatus {
    match status {
        ConnectionStatus::PendingAuth => ConnectorConnectionStatus::PendingAuth,
        ConnectionStatus::Active => ConnectorConnectionStatus::Active,
        ConnectionStatus::Suspended => ConnectorConnectionStatus::Suspended,
        ConnectionStatus::Disconnecting => ConnectorConnectionStatus::Disconnecting,
        ConnectionStatus::Deleted => ConnectorConnectionStatus::Deleted,
    }
}

fn wire_health(health: ConnectionHealth) -> ConnectorConnectionHealth {
    match health {
        ConnectionHealth::Pending => ConnectorConnectionHealth::Pending,
        ConnectionHealth::Ready => ConnectorConnectionHealth::Ready,
        ConnectionHealth::Degraded => ConnectorConnectionHealth::Degraded,
        ConnectionHealth::Unavailable => ConnectorConnectionHealth::Unavailable,
        ConnectionHealth::Quarantined => ConnectorConnectionHealth::Quarantined,
    }
}

fn wire_slot(readiness: CredentialSlotReadiness) -> ConnectorCredentialSlotResponse {
    ConnectorCredentialSlotResponse {
        slot_name: readiness.slot,
        kind: readiness.kind,
        ready: readiness.ready,
    }
}
