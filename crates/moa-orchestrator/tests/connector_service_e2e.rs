//! Production-path coverage for connector connection management.

use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::connector::{
    ConnectorDefinition, ConnectorDefinitionVersionV1, HttpMethodV1, HttpOperationContract,
    RuntimeConnectorActionV1, RuntimeConnectorAuthRequirementV1, RuntimeOperationPolicyV1,
};
use moa_connectors::domain::{
    ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth, ConnectionStatus,
    ConnectorConnection, InstalledActionBindingId, ManagedParentClaim, ManagedParentDefinition,
    ManagedParentDeleteOutcome,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ConnectionUseGrantRepository,
    ConnectionUseRequest, ManagedParentRepository, NewConnectorConnection,
};
use moa_connectors::service::{
    ConnectorService, CredentialSlotVerifier, ManagedParentActivationRequest,
    ManagedParentClaimRequest, ManagedParentDeleteRequest,
};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::credentials::{CredentialContext, CredentialKind, CredentialSlotName};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_orchestrator::services::connectors::authz::{
    ConnectorManagementAuthorizationError, ConnectorManagementAuthorizer,
};
use moa_orchestrator::services::connectors::credentials::{
    ConnectionCredentialRevoker, ConnectorCredentialRevocationError,
};
use moa_orchestrator::services::connectors::definitions::{
    ConnectorDefinitionResolutionError, ConnectorDefinitionResolver, ResolvedConnectorDefinition,
};
use moa_orchestrator::services::connectors::management::{
    ConnectorDestinationVerificationError, ConnectorDestinationVerifier, ConnectorManagementError,
    ConnectorManagementService, ManagedKnowledgeConnectionOperationError,
};
use moa_wire::connectors::{
    ConnectorArtifactReference, ConnectorConnectionCreateRequest, ConnectorConnectionHealth,
    ConnectorConnectionMutationCommand, ConnectorConnectionMutationRequest,
    ConnectorConnectionSelector, ConnectorConnectionStatus, ConnectorConnectionUseCommand,
    ConnectorConnectionUseRequest, ConnectorCredentialWriteMetadata, ConnectorDefinitionReference,
    ConnectorUseSubject, ConnectorVerificationState,
};
use serde_json::json;
use uuid::Uuid;

type Events = Arc<Mutex<Vec<String>>>;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .expect("connector service fixture lock poisoned")
}

fn record(events: &Events, event: impl Into<String>) {
    lock(events).push(event.into());
}

fn generation(value: u64) -> ConnectionGeneration {
    ConnectionGeneration::new(value).expect("fixture generation must be positive")
}

fn identity(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x0a11ce),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn definition_ref() -> ConnectionDefinitionRef {
    ConnectionDefinitionRef::Artifact {
        artifact_uid: Uuid::from_u128(0xa471),
        revision_uid: Uuid::from_u128(0xae71),
    }
}

fn wire_definition_ref() -> ConnectorArtifactReference {
    ConnectorArtifactReference {
        artifact_uid: Uuid::from_u128(0xa471),
        revision_uid: Uuid::from_u128(0xae71),
    }
}

fn definition() -> ConnectorDefinition {
    ConnectorDefinition {
        definition_version: ConnectorDefinitionVersionV1::V1,
        display_name: "Billing".to_string(),
        description: String::new(),
        auth: vec![RuntimeConnectorAuthRequirementV1::None],
        actions: vec![RuntimeConnectorActionV1 {
            id: "create_invoice".to_string(),
            description: "Create one invoice".to_string(),
            contract: HttpOperationContract {
                method: HttpMethodV1::Post,
                path_template: "/invoices".to_string(),
                path_inputs: Vec::new(),
                query_inputs: Vec::new(),
                body_input: None,
                credential_slot: None,
                upstream_idempotency_header: None,
                response_pointer: None,
                max_request_bytes: 1024,
                max_response_bytes: 1024,
                connect_timeout_ms: 1000,
                total_timeout_ms: 2000,
                policy: RuntimeOperationPolicyV1 {
                    input_schema: json!({"type": "object"}),
                    output_schema: json!({"type": "object"}),
                    data_classes: Vec::new(),
                    idempotency: moa_core::types::tools::IdempotencyClass::NonIdempotent,
                },
            },
        }],
    }
}

fn connection(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    current_generation: u64,
    status: ConnectionStatus,
) -> ConnectorConnection {
    let now = Utc::now();
    ConnectorConnection {
        connection_id,
        tenant_id,
        display_name: "Billing account".to_string(),
        definition: definition_ref(),
        origin: Some("https://api.example.test".parse().expect("fixture origin")),
        non_secret_config: json!({}),
        generation: generation(current_generation),
        status,
        health: ConnectionHealth::Pending,
        health_reason: None,
        created_by_identity_id: Some(Uuid::from_u128(0x0a11ce)),
        owner_identity_id: Some(Uuid::from_u128(0x0a11ce)),
        created_at: now,
        updated_at: now,
    }
}

fn unavailable() -> moa_connectors::Error {
    moa_connectors::Error::InvalidContract {
        message: "fixture operation unavailable".to_string(),
    }
}

#[derive(Clone)]
struct FakeAuthorizer {
    events: Events,
    allow: Arc<Mutex<bool>>,
}

#[async_trait]
impl ConnectorManagementAuthorizer for FakeAuthorizer {
    async fn require_tenant_admin(
        &self,
        _identity: &Identity,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        record(&self.events, "auth_admin");
        if *lock(&self.allow) {
            Ok(())
        } else {
            Err(ConnectorManagementAuthorizationError::Denied)
        }
    }

    async fn require_connection_manage(
        &self,
        _identity: &Identity,
        _connection_id: ConnectorConnectionId,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        record(&self.events, "auth_manage");
        if *lock(&self.allow) {
            Ok(())
        } else {
            Err(ConnectorManagementAuthorizationError::Denied)
        }
    }
}

#[derive(Clone)]
struct FakeDefinitions {
    events: Events,
    definition: ConnectorDefinition,
}

#[async_trait]
impl ConnectorDefinitionResolver for FakeDefinitions {
    async fn resolve_for_install(
        &self,
        _tenant_id: TenantId,
        reference: &ConnectorArtifactReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        record(&self.events, "definition_install");
        let definition_ref = ConnectionDefinitionRef::Artifact {
            artifact_uid: reference.artifact_uid,
            revision_uid: reference.revision_uid,
        };
        Ok(ResolvedConnectorDefinition {
            definition_ref,
            definition: Some(self.definition.clone()),
            credential_requirements: self.definition.auth.clone(),
        })
    }

    async fn resolve_installed(
        &self,
        _tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        let managed = [
            ManagedParentDefinition::KnowledgeNangoV1,
            ManagedParentDefinition::KnowledgeMergeV1,
        ]
        .into_iter()
        .find(|managed| managed.definition_ref() == *reference);
        if managed.is_none() {
            record(&self.events, "definition_installed");
        }
        Ok(ResolvedConnectorDefinition {
            definition_ref: reference.clone(),
            definition: managed.is_none().then(|| self.definition.clone()),
            credential_requirements: managed.map_or_else(
                || self.definition.auth.clone(),
                ManagedParentDefinition::credential_requirements,
            ),
        })
    }

    async fn resolve_installed_batch(
        &self,
        tenant_id: TenantId,
        references: &[ConnectionDefinitionRef],
    ) -> Result<Vec<ResolvedConnectorDefinition>, ConnectorDefinitionResolutionError> {
        let mut resolved = Vec::with_capacity(references.len());
        for reference in references {
            resolved.push(self.resolve_installed(tenant_id, reference).await?);
        }
        Ok(resolved)
    }
}

#[derive(Clone)]
struct FakeRepository {
    events: Events,
    connection: Arc<Mutex<ConnectorConnection>>,
    fail_transition: Arc<Mutex<Option<ConnectionStatus>>>,
}

#[async_trait]
impl ConnectionLifecycleRepository for FakeRepository {
    async fn create(
        &self,
        request: NewConnectorConnection,
    ) -> moa_connectors::Result<ConnectorConnection> {
        record(&self.events, "repository_create");
        let mut created = connection(
            request.tenant_id,
            request.connection_id,
            1,
            ConnectionStatus::PendingAuth,
        );
        created.display_name = request.display_name;
        created.definition = request.definition_ref;
        created.origin = request.origin;
        created.non_secret_config = request.non_secret_config;
        created.created_by_identity_id = request.created_by_identity_id;
        created.owner_identity_id = Some(request.owner_identity_id);
        *lock(&self.connection) = created.clone();
        Ok(created)
    }

    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> moa_connectors::Result<Option<ConnectorConnection>> {
        record(&self.events, "repository_load");
        let current = lock(&self.connection).clone();
        Ok(
            (current.tenant_id == tenant_id && current.connection_id == connection_id)
                .then_some(current),
        )
    }

    async fn list(
        &self,
        tenant_id: TenantId,
        _request: moa_connectors::repository::ConnectionListRequest,
    ) -> moa_connectors::Result<moa_connectors::repository::ConnectionListPage> {
        record(&self.events, "repository_list");
        let current = lock(&self.connection).clone();
        Ok(moa_connectors::repository::ConnectionListPage {
            connections: (current.tenant_id == tenant_id
                && current.status != ConnectionStatus::Deleted)
                .then_some(current)
                .into_iter()
                .collect(),
            next_cursor: None,
        })
    }

    async fn load_pinned_action(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        _binding_id: InstalledActionBindingId,
    ) -> moa_connectors::Result<Option<moa_connectors::repository::PinnedConnectorAction>> {
        Err(unavailable())
    }

    async fn transition(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        target: ConnectionStatus,
    ) -> moa_connectors::Result<ConnectorConnection> {
        record(&self.events, format!("repository_transition:{target}"));
        if *lock(&self.fail_transition) == Some(target) {
            return Err(unavailable());
        }
        let mut current = lock(&self.connection);
        if current.tenant_id != tenant_id || current.connection_id != connection_id {
            return Err(moa_connectors::Error::ConnectionNotFound { connection_id });
        }
        if current.generation != expected_generation {
            return Err(moa_connectors::Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            });
        }
        current.status = current.status.transition(target)?;
        Ok(current.clone())
    }

    async fn update_health(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        _expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> moa_connectors::Result<ConnectorConnection> {
        record(&self.events, "repository_update_health");
        let mut current = lock(&self.connection);
        current.health = health;
        current.health_reason = reason;
        Ok(current.clone())
    }

    async fn advance_credential_generation(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection> {
        record(&self.events, "repository_advance_generation");
        let mut current = lock(&self.connection);
        if current.generation != expected_generation {
            return Err(moa_connectors::Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            });
        }
        current.generation = current.generation.next()?;
        if current.status == ConnectionStatus::Active {
            current.status = ConnectionStatus::Suspended;
        }
        Ok(current.clone())
    }

    async fn activate(
        &self,
        request: ConnectionActivation,
    ) -> moa_connectors::Result<ConnectorConnection> {
        record(&self.events, "repository_activate");
        let mut current = lock(&self.connection);
        current.generation = request.expected_generation.next()?;
        current.status = ConnectionStatus::Active;
        Ok(current.clone())
    }
}

#[async_trait]
impl ManagedParentRepository for FakeRepository {
    async fn claim_managed_parent(
        &self,
        _request: ManagedParentClaimRequest,
    ) -> moa_connectors::Result<ManagedParentClaim> {
        Err(unavailable())
    }

    async fn activate_managed_knowledge_parent(
        &self,
        _request: ManagedParentActivationRequest,
    ) -> moa_connectors::Result<ConnectorConnection> {
        Err(unavailable())
    }

    async fn delete_managed_parent_if_unused(
        &self,
        _request: ManagedParentDeleteRequest,
    ) -> moa_connectors::Result<ManagedParentDeleteOutcome> {
        Err(unavailable())
    }
}

#[derive(Clone)]
struct FakeCredentials {
    events: Events,
    ready: Arc<Mutex<bool>>,
}

#[async_trait]
impl CredentialSlotVerifier for FakeCredentials {
    async fn credential_slot_readiness_batch(
        &self,
        _tenant_id: TenantId,
        slots: &[moa_connectors::service::ConnectionCredentialSlot],
    ) -> moa_connectors::Result<Vec<moa_connectors::service::ConnectionCredentialSlotReadiness>>
    {
        record(&self.events, "credential_readiness");
        Ok(slots
            .iter()
            .map(
                |slot| moa_connectors::service::ConnectionCredentialSlotReadiness {
                    connection_id: slot.connection_id,
                    slot: slot.slot.clone(),
                    kind: slot.kind,
                    ready: *lock(&self.ready),
                },
            )
            .collect())
    }
}

#[derive(Clone)]
struct FakeUseGrants {
    events: Events,
    requests: Arc<Mutex<Vec<ConnectionUseRequest>>>,
}

#[async_trait]
impl ConnectionUseGrantRepository for FakeUseGrants {
    async fn grant_use(&self, request: ConnectionUseRequest) -> moa_connectors::Result<()> {
        record(&self.events, "grant_use");
        lock(&self.requests).push(request);
        Ok(())
    }

    async fn revoke_use(&self, request: ConnectionUseRequest) -> moa_connectors::Result<()> {
        record(&self.events, "revoke_use");
        lock(&self.requests).push(request);
        Ok(())
    }
}

#[derive(Clone)]
struct FakeDestination {
    events: Events,
    result: Arc<Mutex<Result<(), ConnectorDestinationVerificationError>>>,
}

#[async_trait]
impl ConnectorDestinationVerifier for FakeDestination {
    async fn verify_local(
        &self,
        _connection: &ConnectorConnection,
    ) -> Result<(), ConnectorDestinationVerificationError> {
        record(&self.events, "destination_verify");
        *lock(&self.result)
    }
}

#[derive(Clone)]
struct FakeRevoker {
    events: Events,
    contexts: Arc<Mutex<Vec<CredentialContext>>>,
}

#[async_trait]
impl ConnectionCredentialRevoker for FakeRevoker {
    async fn revoke_connection(
        &self,
        _connection_id: ConnectorConnectionId,
        context: &CredentialContext,
    ) -> Result<u64, ConnectorCredentialRevocationError> {
        record(&self.events, "credential_revoke_connection");
        lock(&self.contexts).push(context.clone());
        Ok(1)
    }
}

struct Fixture {
    service: ConnectorManagementService,
    events: Events,
    repository: FakeRepository,
    authorizer: FakeAuthorizer,
    use_grants: FakeUseGrants,
    revoker: FakeRevoker,
}

fn fixture(status: ConnectionStatus, current_generation: u64) -> Fixture {
    fixture_with_definition(status, current_generation, None, false, definition())
}

fn fixture_with_definition(
    status: ConnectionStatus,
    current_generation: u64,
    managed_definition: Option<ConnectionDefinitionRef>,
    use_managed_resolver: bool,
    runtime_definition: ConnectorDefinition,
) -> Fixture {
    let events = Arc::new(Mutex::new(Vec::new()));
    let tenant_id = TenantId::from(Uuid::from_u128(0x7eaa));
    let connection_id = ConnectorConnectionId(Uuid::from_u128(0xc011ec7));
    let mut initial_connection = connection(tenant_id, connection_id, current_generation, status);
    if let Some(definition) = managed_definition {
        initial_connection.definition = definition;
    }
    let repository = FakeRepository {
        events: events.clone(),
        connection: Arc::new(Mutex::new(initial_connection)),
        fail_transition: Arc::new(Mutex::new(None)),
    };
    let authorizer = FakeAuthorizer {
        events: events.clone(),
        allow: Arc::new(Mutex::new(true)),
    };
    let artifacts: Arc<dyn ConnectorDefinitionResolver> = Arc::new(FakeDefinitions {
        events: events.clone(),
        definition: runtime_definition,
    });
    let _ = use_managed_resolver;
    let definitions: Arc<dyn ConnectorDefinitionResolver> = artifacts;
    let credentials = FakeCredentials {
        events: events.clone(),
        ready: Arc::new(Mutex::new(true)),
    };
    let use_grants = FakeUseGrants {
        events: events.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let destination = FakeDestination {
        events: events.clone(),
        result: Arc::new(Mutex::new(Ok(()))),
    };
    let revoker = FakeRevoker {
        events: events.clone(),
        contexts: Arc::new(Mutex::new(Vec::new())),
    };
    let repository_port = Arc::new(repository.clone());
    let domain_service = ConnectorService::new(
        repository_port.clone(),
        repository_port,
        Arc::new(credentials),
    );
    let service = ConnectorManagementService::new(
        Arc::new(authorizer.clone()),
        definitions,
        domain_service,
        Arc::new(use_grants.clone()),
        Arc::new(destination),
        Arc::new(revoker.clone()),
    );
    Fixture {
        service,
        events,
        repository,
        authorizer,
        use_grants,
        revoker,
    }
}

fn fixture_identity(fixture: &Fixture) -> Identity {
    identity(lock(&fixture.repository.connection).tenant_id)
}

fn fixture_connection_id(fixture: &Fixture) -> ConnectorConnectionId {
    lock(&fixture.repository.connection).connection_id
}

#[test]
fn restate_command_shapes_match_secret_free_edge_translation_offline() {
    // Pins: path selectors are combined with secret-free bodies exactly once;
    // tenant identity and credential material cannot enter Restate commands.
    let connection_id = ConnectorConnectionId(Uuid::from_u128(0xc011ec7));
    let selector: ConnectorConnectionSelector = serde_json::from_value(json!({
        "connection_id": connection_id
    }))
    .expect("edge get selector should decode");
    let mutation: ConnectorConnectionMutationCommand = serde_json::from_value(json!({
        "connection_id": connection_id,
        "expected_generation": 4
    }))
    .expect("edge lifecycle command should decode");
    let use_command: ConnectorConnectionUseCommand = serde_json::from_value(json!({
        "connection_id": connection_id,
        "subject": {"type": "contact", "id": Uuid::from_u128(0xc07ac7)}
    }))
    .expect("edge direct-use command should decode");

    assert_eq!(selector.connection_id, connection_id);
    assert_eq!(mutation.connection_id, connection_id);
    assert_eq!(mutation.expected_generation, 4);
    assert_eq!(use_command.connection_id, connection_id);
    assert!(matches!(
        use_command.request.subject,
        ConnectorUseSubject::Contact { id } if id == Uuid::from_u128(0xc07ac7)
    ));
    for forbidden in ["tenant_id", "credential", "credential_ref", "material"] {
        let mut value = json!({
            "connection_id": connection_id,
            "expected_generation": 4
        });
        value
            .as_object_mut()
            .expect("fixture command is an object")
            .insert(forbidden.to_string(), json!("must-not-cross-restate"));
        assert!(
            serde_json::from_value::<ConnectorConnectionMutationCommand>(value).is_err(),
            "forbidden Restate command field `{forbidden}` must be rejected"
        );
    }
}

#[tokio::test]
async fn create_authorizes_admin_before_definition_and_repository_offline() {
    // Pins: a create request cannot use definition lookup as an authorization oracle.
    let fixture = fixture(ConnectionStatus::PendingAuth, 1);
    let identity = fixture_identity(&fixture);
    let connection_id = ConnectorConnectionId(Uuid::from_u128(0xc0ffee));
    let response = fixture
        .service
        .create(
            &identity,
            ConnectorConnectionCreateRequest {
                connection_id,
                display_name: "Billing account".to_string(),
                definition_ref: wire_definition_ref(),
                origin: "https://billing.example.com".to_string(),
                non_secret_config: json!({"region": "us-east-1"}),
            },
        )
        .await
        .expect("authorized exact published connector create should succeed");

    assert_eq!(
        lock(&fixture.events).as_slice(),
        ["auth_admin", "definition_install", "repository_create"]
    );
    assert_eq!(response.connection_id, connection_id);
    assert_eq!(response.status, ConnectorConnectionStatus::PendingAuth);
    assert!(response.credential_slots.is_empty());
    let encoded = serde_json::to_value(&response).expect("response should serialize");
    let response_fields = encoded
        .as_object()
        .expect("connector response should be a JSON object");
    for forbidden in [
        "credential",
        "credential_ref",
        "credential_version",
        "material",
    ] {
        assert!(
            !response_fields.contains_key(forbidden),
            "public connector response exposed forbidden field `{forbidden}`"
        );
    }
}

#[tokio::test]
async fn denied_manage_stops_before_connection_definition_or_credential_reads_offline() {
    // Pins: every existing-resource path denies before the first protected read.
    let fixture = fixture(ConnectionStatus::Active, 1);
    *lock(&fixture.authorizer.allow) = false;
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);

    let error = fixture
        .service
        .get(&identity, connection_id)
        .await
        .expect_err("denied Manage must reject metadata reads");

    assert!(matches!(
        error,
        ConnectorManagementError::Authorization(ConnectorManagementAuthorizationError::Denied)
    ));
    assert_eq!(lock(&fixture.events).as_slice(), ["auth_manage"]);
}

#[tokio::test]
async fn managed_knowledge_parent_resolves_for_installed_list_and_get_offline() {
    // Pins: `knowledge:nango@1` is a code-owned definition only after the
    // exact parent exists; normal list/get remain observable to its manager.
    let managed = ManagedParentDefinition::KnowledgeNangoV1;
    let fixture = fixture_with_definition(
        ConnectionStatus::PendingAuth,
        1,
        Some(managed.definition_ref()),
        true,
        definition(),
    );
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);

    let listed = fixture
        .service
        .list(&identity, Default::default())
        .await
        .expect("installed managed parent should remain listable");
    assert_eq!(listed.connections.len(), 1);
    assert_eq!(
        listed.connections[0].definition_ref,
        ConnectorDefinitionReference::BuiltIn {
            key: managed.key().to_string(),
            version: NonZeroU64::MIN,
        }
    );
    assert_eq!(
        lock(&fixture.events).as_slice(),
        ["auth_admin", "repository_list", "credential_readiness",]
    );

    lock(&fixture.events).clear();
    let fetched = fixture
        .service
        .get(&identity, connection_id)
        .await
        .expect("installed managed parent should remain gettable");
    assert_eq!(
        fetched.definition_ref,
        ConnectorDefinitionReference::BuiltIn {
            key: managed.key().to_string(),
            version: NonZeroU64::MIN,
        }
    );
    assert_eq!(
        lock(&fixture.events).as_slice(),
        ["auth_manage", "repository_load", "credential_readiness",]
    );
}

#[tokio::test]
async fn managed_knowledge_parent_generic_operations_stop_after_auth_and_load_offline() {
    // Pins: every generic mutation rejects both closed managed providers after
    // authorization and protected load, before definition, vault, or writes.
    #[derive(Clone, Copy, Debug)]
    enum Operation {
        Activate,
        Suspend,
        Resume,
        Verify,
        GrantUse,
        RevokeUse,
        PrepareCredential,
        AdvanceCredential,
        Disconnect,
        Delete,
    }

    let operations = [
        Operation::Activate,
        Operation::Suspend,
        Operation::Resume,
        Operation::Verify,
        Operation::GrantUse,
        Operation::RevokeUse,
        Operation::PrepareCredential,
        Operation::AdvanceCredential,
        Operation::Disconnect,
        Operation::Delete,
    ];
    for managed in [
        ManagedParentDefinition::KnowledgeNangoV1,
        ManagedParentDefinition::KnowledgeMergeV1,
    ] {
        for operation in operations {
            let status = match operation {
                Operation::Resume => ConnectionStatus::Suspended,
                Operation::Suspend
                | Operation::GrantUse
                | Operation::RevokeUse
                | Operation::AdvanceCredential
                | Operation::Disconnect => ConnectionStatus::Active,
                Operation::Activate
                | Operation::Verify
                | Operation::PrepareCredential
                | Operation::Delete => ConnectionStatus::PendingAuth,
            };
            let fixture = if matches!(operation, Operation::AdvanceCredential) {
                fixture(status, 1)
            } else {
                fixture_with_definition(
                    status,
                    1,
                    Some(managed.definition_ref()),
                    true,
                    definition(),
                )
            };
            let identity = fixture_identity(&fixture);
            let connection_id = fixture_connection_id(&fixture);
            let mutation = ConnectorConnectionMutationRequest {
                expected_generation: 1,
            };
            let use_request = ConnectorConnectionUseRequest {
                subject: ConnectorUseSubject::Agent {
                    id: Uuid::from_u128(0xa6e17),
                },
            };
            let credential_metadata = ConnectorCredentialWriteMetadata {
                connection_id,
                expected_generation: 1,
                slot_name: CredentialSlotName::PRIMARY,
                kind: CredentialKind::OAuth,
                operation_id: Uuid::from_u128(0xc1a1),
            };

            let result: Result<(), ConnectorManagementError> = match operation {
                Operation::Activate => fixture
                    .service
                    .activate(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
                Operation::Suspend => fixture
                    .service
                    .suspend(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
                Operation::Resume => fixture
                    .service
                    .resume(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
                Operation::Verify => fixture
                    .service
                    .verify(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
                Operation::GrantUse => {
                    fixture
                        .service
                        .grant_use(&identity, connection_id, use_request)
                        .await
                }
                Operation::RevokeUse => {
                    fixture
                        .service
                        .revoke_use(&identity, connection_id, use_request)
                        .await
                }
                Operation::PrepareCredential => fixture
                    .service
                    .prepare_credential_write(&identity, &credential_metadata)
                    .await
                    .map(|_| ()),
                Operation::AdvanceCredential => {
                    let prepared = fixture
                        .service
                        .prepare_credential_write(&identity, &credential_metadata)
                        .await
                        .expect("artifact credential write should prepare before managed swap");
                    lock(&fixture.repository.connection).definition = managed.definition_ref();
                    lock(&fixture.events).clear();
                    fixture
                        .service
                        .advance_credential_generation(&identity, &prepared)
                        .await
                        .map(|_| ())
                }
                Operation::Disconnect => fixture
                    .service
                    .disconnect(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
                Operation::Delete => fixture
                    .service
                    .delete(&identity, connection_id, mutation)
                    .await
                    .map(|_| ()),
            };

            assert!(
                matches!(
                    result,
                    Err(ConnectorManagementError::ManagedKnowledgeOperation(
                        ManagedKnowledgeConnectionOperationError
                    ))
                ),
                "{managed:?} {operation:?} must remain knowledge-owned"
            );
            assert_eq!(
                lock(&fixture.events).as_slice(),
                ["auth_manage", "repository_load"],
                "{managed:?} {operation:?} must stop after authorization and protected load"
            );
        }
    }
}

#[tokio::test]
async fn credential_fence_crash_resume_reuses_exact_next_suspended_generation_offline() {
    // Pins: a crash after fencing but before staged activation resumes without
    // attempting a second generation advance or serializing a staging token.
    let fixture = fixture(ConnectionStatus::Active, 7);
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);
    let metadata = ConnectorCredentialWriteMetadata {
        connection_id,
        expected_generation: 7,
        slot_name: CredentialSlotName::PRIMARY,
        kind: CredentialKind::OAuth,
        operation_id: Uuid::from_u128(0xfece),
    };

    let prepared = fixture
        .service
        .prepare_credential_write(&identity, &metadata)
        .await
        .expect("authorized exact slot should prepare");
    let first = fixture
        .service
        .advance_credential_generation(&identity, &prepared)
        .await
        .expect("first fence should advance generation");
    assert_eq!(first.generation(), generation(8));
    assert_eq!(first.status(), ConnectionStatus::Suspended);

    lock(&fixture.events).clear();
    let replay_prepared = fixture
        .service
        .prepare_credential_write(&identity, &metadata)
        .await
        .expect("same staged operation should prepare after a crash");
    let replay = fixture
        .service
        .advance_credential_generation(&identity, &replay_prepared)
        .await
        .expect("already-fenced retry should resume");

    assert_eq!(replay.connection_id(), connection_id);
    assert_eq!(replay.generation(), generation(8));
    assert_eq!(replay.status(), ConnectionStatus::Suspended);
    assert_eq!(
        lock(&fixture.events).as_slice(),
        [
            "auth_manage",
            "repository_load",
            "definition_installed",
            "auth_manage",
            "repository_load",
        ]
    );
}

#[tokio::test]
async fn credential_fence_replay_rejects_next_generation_that_is_active_offline() {
    // Pins: next-generation replay is accepted only in the repository's exact
    // fenced states; a newly active generation is never mistaken for this write.
    let fixture = fixture(ConnectionStatus::Active, 8);
    let identity = fixture_identity(&fixture);
    let metadata = ConnectorCredentialWriteMetadata {
        connection_id: fixture_connection_id(&fixture),
        expected_generation: 7,
        slot_name: CredentialSlotName::PRIMARY,
        kind: CredentialKind::OAuth,
        operation_id: Uuid::from_u128(0xbad),
    };

    let error = fixture
        .service
        .prepare_credential_write(&identity, &metadata)
        .await
        .expect_err("an active next generation belongs to a completed operation");
    assert!(matches!(
        error,
        ConnectorManagementError::Connector(moa_connectors::Error::GenerationConflict { .. })
    ));
}

#[tokio::test]
async fn disconnect_fences_before_audit_preserving_connection_revocation_offline() {
    // Pins: no credential revocation begins until the connection and bindings
    // have crossed the durable Disconnecting execution fence.
    let fixture = fixture(ConnectionStatus::Active, 3);
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);
    let response = fixture
        .service
        .disconnect(
            &identity,
            connection_id,
            ConnectorConnectionMutationRequest {
                expected_generation: 3,
            },
        )
        .await
        .expect("authorized disconnect should fence and revoke");

    assert_eq!(response.status, ConnectorConnectionStatus::Disconnecting);
    assert_eq!(response.credential_slots.len(), 1);
    assert!(!response.credential_slots[0].ready);
    assert_eq!(
        lock(&fixture.events).as_slice(),
        [
            "auth_manage",
            "repository_load",
            "definition_installed",
            "repository_transition:disconnecting",
            "credential_revoke_connection",
        ]
    );
    let contexts = lock(&fixture.revoker.contexts);
    assert_eq!(contexts.len(), 1);
    assert_eq!(
        contexts[0].operation,
        moa_core::types::credentials::CredentialOperation::Revoke
    );
    assert_eq!(contexts[0].tenant_id, identity.tenant_id);
    assert_eq!(contexts[0].request_hash.len(), 64);
}

#[tokio::test]
async fn delete_fences_pending_and_active_connections_before_revocation_offline() {
    // Pins: every delete path durably disables execution before vault teardown,
    // then records the terminal lifecycle only after revocation succeeds.
    for initial_status in [ConnectionStatus::PendingAuth, ConnectionStatus::Active] {
        let fixture = fixture(initial_status, 3);
        let identity = fixture_identity(&fixture);
        let connection_id = fixture_connection_id(&fixture);

        let response = fixture
            .service
            .delete(
                &identity,
                connection_id,
                ConnectorConnectionMutationRequest {
                    expected_generation: 3,
                },
            )
            .await
            .expect("authorized delete should fence, revoke, and terminate");

        assert_eq!(response.status, ConnectorConnectionStatus::Deleted);
        assert_eq!(response.credential_slots.len(), 1);
        assert!(!response.credential_slots[0].ready);
        assert_eq!(
            lock(&fixture.events).as_slice(),
            [
                "auth_manage",
                "repository_load",
                "definition_installed",
                "repository_transition:disconnecting",
                "credential_revoke_connection",
                "repository_transition:deleted",
            ],
            "{initial_status} delete must preserve the teardown boundary"
        );
        assert_eq!(lock(&fixture.revoker.contexts).len(), 1);
    }
}

#[tokio::test]
async fn delete_does_not_revoke_credentials_when_disconnect_fence_fails_offline() {
    // Pins: a failed optimistic transition cannot tear down credentials while
    // the connection may still admit executions at its observed generation.
    let fixture = fixture(ConnectionStatus::Active, 3);
    *lock(&fixture.repository.fail_transition) = Some(ConnectionStatus::Disconnecting);
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);

    let result = fixture
        .service
        .delete(
            &identity,
            connection_id,
            ConnectorConnectionMutationRequest {
                expected_generation: 3,
            },
        )
        .await;

    assert!(matches!(
        result,
        Err(ConnectorManagementError::Connector(
            moa_connectors::Error::InvalidContract { .. }
        ))
    ));
    assert_eq!(
        lock(&fixture.events).as_slice(),
        [
            "auth_manage",
            "repository_load",
            "definition_installed",
            "repository_transition:disconnecting",
        ]
    );
    assert!(lock(&fixture.revoker.contexts).is_empty());
    assert_eq!(
        lock(&fixture.repository.connection).status,
        ConnectionStatus::Active
    );
}

#[tokio::test]
async fn verification_is_sanitized_unverified_without_reviewed_remote_contract_offline() {
    // Pins: current V1 definitions never invent a remote authentication probe;
    // local admission plus ready slots remains explicitly unverified.
    let fixture = fixture(ConnectionStatus::PendingAuth, 1);
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);
    let response = fixture
        .service
        .verify(
            &identity,
            connection_id,
            ConnectorConnectionMutationRequest {
                expected_generation: 1,
            },
        )
        .await
        .expect("local verification should produce a sanitized response");

    assert_eq!(
        response.verification,
        ConnectorVerificationState::Unverified
    );
    assert_eq!(response.health, ConnectorConnectionHealth::Pending);
    assert_eq!(
        response.reason.as_deref(),
        Some("remote_verification_not_configured")
    );
    assert_eq!(
        lock(&fixture.events).as_slice(),
        [
            "auth_manage",
            "repository_load",
            "definition_installed",
            "credential_readiness",
            "destination_verify",
            "repository_update_health",
        ]
    );
}

#[tokio::test]
async fn direct_use_grant_authorizes_manage_before_repository_write_offline() {
    // Pins: relationship administration cannot mutate desired state before the
    // exact connector-Manage decision succeeds.
    let fixture = fixture(ConnectionStatus::Active, 1);
    let identity = fixture_identity(&fixture);
    let connection_id = fixture_connection_id(&fixture);
    let agent_id = Uuid::from_u128(0xa6e17);
    fixture
        .service
        .grant_use(
            &identity,
            connection_id,
            ConnectorConnectionUseRequest {
                subject: ConnectorUseSubject::Agent { id: agent_id },
            },
        )
        .await
        .expect("authorized same-tenant direct Use grant should succeed");

    assert_eq!(
        lock(&fixture.events).as_slice(),
        ["auth_manage", "repository_load", "grant_use"]
    );
    let requests = lock(&fixture.use_grants.requests);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tenant_id, identity.tenant_id);
    assert_eq!(requests[0].connection_id, connection_id);
    assert!(matches!(
        requests[0].subject,
        moa_connectors::repository::ConnectorUseSubject::Agent { id } if id == agent_id
    ));
}
