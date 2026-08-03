// Service construction and shared caller fixtures.

fn linked_provider(provider: &str) -> moa_knowledge::domain::LinkedProviderKind {
    moa_knowledge::domain::LinkedProviderKind::from_str_exact(provider)
        .expect("test provider must be nango or merge")
}

fn provider_record_acl() -> ProviderRecordAcl {
    ProviderRecordAcl {
        provider_revision: "fixture-acl-rev".to_string(),
        complete: true,
        entries: Vec::new(),
    }
}

fn fixture_service(
    repository: Arc<InMemoryKnowledgeRepository>,
    provider: Arc<dyn LinkedIntegrationProvider>,
    max_preview_chars: usize,
) -> KnowledgeService {
    let providers = StaticKnowledgeProviders::new()
        .with_provider(linked_provider(PROVIDER), provider.clone())
        .with_provider(linked_provider("nango"), provider);
    KnowledgeService::new(
        repository.clone(),
        repository,
        Arc::new(providers),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        max_preview_chars,
    )
    .with_connector_connection_port(Arc::new(FakeKnowledgeConnectorConnections::default()))
}

fn fixture_webhook_service(
    repository: Arc<InMemoryKnowledgeRepository>,
    provider: &'static str,
    max_preview_chars: usize,
) -> KnowledgeService {
    KnowledgeService::new(
        repository.clone(),
        repository,
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_webhook_verifier(provider, Arc::new(PayloadWebhookVerifier::new(provider))),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        max_preview_chars,
    )
    .with_connector_connection_port(Arc::new(FakeKnowledgeConnectorConnections::default()))
}

/// Mirrors the service's provider-completion classification for fake providers.
fn provider_status_is_completed(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed" | "complete" | "success" | "succeeded"
    )
}

fn fake_ingestion_runner() -> Arc<dyn KnowledgeIngestionRunner> {
    Arc::new(FakeKnowledgeIngestionRunner::default())
}

#[derive(Clone, Copy, Debug, Default)]
struct ReadyConnectorCredentialSlots;

#[async_trait]
impl CredentialSlotVerifier for ReadyConnectorCredentialSlots {
    async fn credential_slot_readiness_batch(
        &self,
        _tenant_id: TenantId,
        slots: &[ConnectionCredentialSlot],
    ) -> moa_connectors::Result<Vec<ConnectionCredentialSlotReadiness>> {
        Ok(slots
            .iter()
            .map(|slot| ConnectionCredentialSlotReadiness {
                connection_id: slot.connection_id,
                slot: slot.slot.clone(),
                kind: slot.kind,
                ready: true,
            })
            .collect())
    }
}

fn postgres_connector_service(pool: sqlx::PgPool) -> ConnectorService {
    let repository = Arc::new(PostgresConnectionRepository::new(pool));
    let lifecycle: Arc<dyn ConnectionLifecycleRepository> = repository.clone();
    let managed_parents: Arc<dyn ManagedParentRepository> = repository;
    ConnectorService::new(
        lifecycle,
        managed_parents,
        Arc::new(ReadyConnectorCredentialSlots),
    )
}

async fn seed_managed_connector_parent(pool: &sqlx::PgPool, connection: &KnowledgeConnection) {
    let service = postgres_connector_service(pool.clone());
    let definition = ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())
        .expect("fixture provider should have a managed connector definition");
    let connection_id = ConnectorConnectionId(connection.connection_uid);
    let claim = service
        .claim_managed_parent(ManagedParentClaimRequest {
            tenant_id: connection.tenant_id,
            operation_id: format!("seed-managed-parent:{}", connection.connection_uid),
            request_hash: format!("{:064x}", connection.connection_uid.as_u128()),
            connection_id,
            definition,
            display_name: format!("{} {}", connection.provider, connection.connector),
            owner_identity_id: Some(Uuid::now_v7()),
        })
        .await
        .expect("seed managed connector parent");
    service
        .activate_managed_knowledge_parent(ManagedParentActivationRequest {
            tenant_id: connection.tenant_id,
            connection_id,
            expected_generation: claim.connection.generation,
            definition,
        })
        .await
        .expect("activate managed connector parent");
}

/// Builds an authorized caller context with a per-call unique operation root.
///
/// Mirrors what the Restate handler does after `(Tenant, tenant_id, Operator)`
/// authorization succeeds. The operation root is fresh per call so concurrently
/// running tests never share a credential replay key.
fn test_caller(tenant_id: TenantId) -> KnowledgeCaller {
    KnowledgeCaller::authorized(
        &Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::now_v7(),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        Uuid::now_v7().to_string(),
    )
}
