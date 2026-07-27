//! Connections behavior for the tenant Knowledge service.

use super::*;

#[tokio::test]
async fn link_and_sync_attribute_credential_work_to_the_authorized_caller() {
    // Pins: credential create and the resolve that authorizes the initial
    // provider trigger are attributed to the caller identity that passed tenant
    // authorization, not to a durable service actor. A service actor is a
    // read-only allowlist entry for reconstructed workflows; using one here
    // would let a caller-facing route write credential state anonymously.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: Some(Uuid::now_v7()),
    };
    let caller = KnowledgeCaller::authorized(&identity, Uuid::now_v7().to_string());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_provider(PROVIDER, Arc::new(FakeLinkedIntegrationProvider::default())),
        ),
        credentials.clone(),
        fake_ingestion_runner(),
        80,
    );

    service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: PROVIDER.to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "public-token".to_string(),
                source_selection: json!({}),
                information_barrier: None,
            },
            &caller,
        )
        .await
        .expect("token exchange should link the connection");

    let expected = CredentialPrincipal::Caller {
        identity_id: identity.id,
        delegated_by: identity.acting_on_behalf_of,
    };
    assert_eq!(
        credentials.principals_for_step("credential-create"),
        vec![expected],
        "the linked credential must be created on the authorized caller's authority"
    );
    assert_eq!(
        credentials.principals_for_step("credential-resolve"),
        vec![expected],
        "the initial provider trigger must resolve on the authorized caller's authority"
    );
}

#[tokio::test]
async fn list_integrations_merges_providers_sorted_and_honors_provider_filter() {
    // Pins: connect UIs get every enabled provider's integrations, provider-tagged
    // and deterministically sorted, and an explicit provider filter narrows the list.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let nango_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations(vec![
        ProviderIntegration {
            id: "notion".to_string(),
            display_name: "Notion".to_string(),
            logo_url: None,
        },
        ProviderIntegration {
            id: "google-drive".to_string(),
            display_name: "Google Drive".to_string(),
            logo_url: Some("https://logos.example/drive.png".to_string()),
        },
    ]));
    let merge_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations(vec![
        ProviderIntegration {
            id: "filestorage".to_string(),
            display_name: "File Storage".to_string(),
            logo_url: None,
        },
    ]));
    let broken_like = Arc::new(FakeLinkedIntegrationProvider::with_integrations_error(
        "catalog endpoint returned 500",
    ));
    let service = KnowledgeService::new(
        Arc::new(InMemoryKnowledgeRepository::default()),
        Arc::new(InMemoryKnowledgeRepository::default()),
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_provider("nango", nango_like)
                .with_provider("merge", merge_like)
                .with_provider("broken", broken_like),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );

    let all = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: None,
        })
        .await
        .expect("list integrations across providers");
    let flattened: Vec<(String, String)> = all
        .integrations
        .iter()
        .map(|entry| (entry.provider.clone(), entry.id.clone()))
        .collect();
    assert_eq!(
        flattened,
        vec![
            ("merge".to_string(), "filestorage".to_string()),
            ("nango".to_string(), "google-drive".to_string()),
            ("nango".to_string(), "notion".to_string()),
        ],
        "integrations should be sorted by provider then integration id"
    );
    assert_eq!(
        all.integrations[1].logo_url.as_deref(),
        Some("https://logos.example/drive.png")
    );
    assert_eq!(
        all.unavailable_providers.len(),
        1,
        "a failing enabled provider must be reported, not silently dropped"
    );
    assert_eq!(all.unavailable_providers[0].provider, "broken");
    assert!(
        all.unavailable_providers[0]
            .reason
            .contains("catalog endpoint returned 500"),
        "reason should carry the provider failure message"
    );

    let filtered = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("merge".to_string()),
        })
        .await
        .expect("list integrations for one provider");
    assert_eq!(filtered.integrations.len(), 1);
    assert_eq!(filtered.integrations[0].provider, "merge");
    assert_eq!(filtered.integrations[0].id, "filestorage");

    let unknown = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("unknown".to_string()),
        })
        .await;
    assert!(
        unknown.is_err(),
        "explicit unknown provider filter must surface an error"
    );

    let broken = service
        .list_integrations(KnowledgeIntegrationListRequest {
            tenant_id,
            provider: Some("broken".to_string()),
        })
        .await;
    assert!(
        broken.is_err(),
        "explicit provider filter must propagate the provider failure"
    );
}

#[tokio::test]
async fn exchange_stores_only_credential_reference_on_connection() {
    // Pins: public-token exchange persists credential material through the credential store only.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider.clone())),
        credentials.clone(),
        fake_ingestion_runner(),
        80,
    );
    let information_barrier =
        InformationBarrierId::parse("finance-restricted").expect("valid barrier");

    let response = service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: PROVIDER.to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "public-token".to_string(),
                source_selection: json!({
                    "metadata": {
                        "selected_folder_ids": ["folder-1"]
                    }
                }),
                information_barrier: Some(information_barrier.clone()),
            },
            &caller,
        )
        .await
        .expect("token exchange should persist a connection");
    let connection = repository
        .connection(response.connection_uid)
        .expect("connection should be stored");

    assert_eq!(provider.exchange_count(), 1);
    assert_eq!(provider.apply_source_selection_count(), 1);
    // The initial link uses the replay-safe start, never the one-off re-sync
    // trigger, because the owning claim may replay it after a crash.
    assert_eq!(provider.start_initial_sync_count(), 1);
    assert_eq!(provider.trigger_sync_count(), 0);
    assert_eq!(response.sync_status.as_deref(), Some("provider_syncing"));
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(
        provider.applied_source_selections(),
        vec![json!({ "metadata": { "selected_folder_ids": ["folder-1"] } })]
    );
    assert_eq!(credentials.stored_account_count(), 1);
    assert_eq!(
        Some(connection.credential_ref.clone()),
        credentials.reference_for_connection(connection.connection_uid),
        "the connection must carry the exact reference issued for it"
    );
    assert_ne!(connection.credential_ref, SECRET_TOKEN);
    assert!(!connection.credential_ref.contains(SECRET_TOKEN));
    // The persisted reference is an opaque identifier, not a parseable address a
    // caller could edit to reach another tenant's credential.
    assert!(Uuid::parse_str(&connection.credential_ref).is_ok());
    assert!(!connection.credential_ref.contains("://"));
    assert_eq!(
        connection.source_selection,
        json!({ "metadata": { "selected_folder_ids": ["folder-1"] } })
    );
    assert_eq!(response.provider, PROVIDER);
    assert_eq!(response.connector, CONNECTOR);
    assert_eq!(
        connection.information_barrier,
        Some(information_barrier.clone())
    );

    let sync_run_uid = response
        .sync_run_uid
        .expect("token exchange should start a sync run");
    let original_run = repository
        .sync_run(sync_run_uid)
        .expect("sync run should be stored");
    assert_eq!(
        original_run.information_barrier,
        Some(information_barrier.clone())
    );
}

#[tokio::test]
async fn mid_sync_connection_barrier_change_keeps_persisted_run_snapshot_db_memory() {
    // Pins: separate service replicas can update and read the same Postgres connection/run,
    // while an in-flight run retains the barrier it claimed at creation time.
    let db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated connection snapshot DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let original_barrier =
        InformationBarrierId::parse("finance-restricted").expect("valid original barrier");
    let replacement_barrier =
        InformationBarrierId::parse("legal-restricted").expect("valid replacement barrier");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = KnowledgeService::from_postgres_pool(
        pool.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        80,
    );

    let exchange = service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: PROVIDER.to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "public-token".to_string(),
                source_selection: json!({}),
                information_barrier: Some(original_barrier.clone()),
            },
            &caller,
        )
        .await
        .expect("token exchange should persist a connection and sync run");
    let sync_run_uid = exchange
        .sync_run_uid
        .expect("token exchange should start a sync run");

    let mutator = PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    );
    let mut connection = mutator
        .get_connection(exchange.connection_uid)
        .await
        .expect("load persisted connection from a second repository")
        .expect("persisted connection should exist");
    connection.information_barrier = Some(replacement_barrier.clone());
    mutator
        .upsert_connection(connection)
        .await
        .expect("persist replacement connection barrier");

    let reader =
        PostgresKnowledgeRepository::scoped_for_app_role(pool, RlsContext::tenant(tenant_id));
    assert_eq!(
        reader
            .get_connection(exchange.connection_uid)
            .await
            .expect("read updated connection from a third repository")
            .expect("updated connection should exist")
            .information_barrier,
        Some(replacement_barrier)
    );
    assert_eq!(
        reader
            .get_sync_run(sync_run_uid)
            .await
            .expect("read persisted sync run from a third repository")
            .expect("persisted sync run should exist")
            .information_barrier,
        Some(original_barrier),
        "changing a connection mid-sync must not rewrite the run's barrier snapshot"
    );
}

#[tokio::test]
async fn disconnect_connection_deletes_vault_ref_and_disables_connection() {
    // Pins: disconnecting a linked knowledge connection revokes MOA-managed credential material.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = KnowledgeService::new(
        repository.clone(),
        repository.clone(),
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider)),
        credentials.clone(),
        fake_ingestion_runner(),
        80,
    );
    let exchange = service
        .exchange_public_token(
            KnowledgeExchangeTokenRequest {
                tenant_id,
                provider: PROVIDER.to_string(),
                connector: CONNECTOR.to_string(),
                exchange_token: "public-token".to_string(),
                source_selection: json!({}),
                information_barrier: None,
            },
            &caller,
        )
        .await
        .expect("token exchange should persist a linked connection");

    let listed_before = service
        .list_connections(
            KnowledgeConnectionListRequest {
                tenant_id,
                provider: Some(PROVIDER.to_string()),
            },
            &caller,
        )
        .await
        .expect("listed connection should resolve credential metadata before disconnect");
    assert_eq!(listed_before.connections.len(), 1);
    assert_eq!(
        listed_before.connections[0].credential_status.as_deref(),
        Some("present")
    );

    let response = service
        .disconnect_connection(
            KnowledgeDisconnectConnectionRequest {
                tenant_id,
                connection_uid: exchange.connection_uid,
            },
            &caller,
        )
        .await
        .expect("disconnect should disable the connection and revoke credential material");
    let connection = repository
        .connection(exchange.connection_uid)
        .expect("connection should still be stored for audit/history");

    assert_eq!(response.connection_uid, exchange.connection_uid);
    assert_eq!(response.status, "disabled");
    assert!(response.credential_revoked);
    assert_eq!(connection.status, ConnectionStatus::Disabled);
    assert_eq!(credentials.stored_account_count(), 0);
    assert_eq!(repository.op_count("disable_connection"), 1);

    let listed_after = service
        .list_connections(
            KnowledgeConnectionListRequest {
                tenant_id,
                provider: Some(PROVIDER.to_string()),
            },
            &caller,
        )
        .await
        .expect("listed connection should expose missing managed credential after disconnect");
    assert_eq!(listed_after.connections.len(), 1);
    assert_eq!(
        listed_after.connections[0].credential_status.as_deref(),
        Some("missing")
    );
}

#[tokio::test]
async fn disconnect_connection_leaves_external_credential_ref_and_disables_connection() {
    // Pins: disconnecting a connection with provider-owned credential refs does not invent vault deletes.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let mut connection = fixture_connection(tenant_id);
    connection.credential_ref = "provider-owned-credential-ref".to_string();
    let connection_uid = connection.connection_uid;
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection)
        .expect("fixture connection should be inserted");
    let service = fixture_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::default()),
        80,
    );

    let response = service
        .disconnect_connection(
            KnowledgeDisconnectConnectionRequest {
                tenant_id,
                connection_uid,
            },
            &caller,
        )
        .await
        .expect("disconnect should still disable an external-ref connection");
    let connection = repository
        .connection(connection_uid)
        .expect("connection should still be stored for audit/history");

    assert_eq!(response.connection_uid, connection_uid);
    assert_eq!(response.status, "disabled");
    assert!(!response.credential_revoked);
    assert_eq!(connection.status, ConnectionStatus::Disabled);
    assert_eq!(repository.op_count("disable_connection"), 1);
}

#[tokio::test]
async fn update_source_selection_persists_applies_and_optionally_syncs() {
    // Pins: tenant admins can update provider-native selected sources and trigger ingestion follow-up.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let mut connection = fixture_connection(tenant_id);
    connection.last_synced_at = Some(moa_test_support::fixtures::pg_now());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    repository
        .insert_connection(connection.clone())
        .expect("fixture connection should be inserted");
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let service = fixture_service(repository.clone(), provider.clone(), 80);
    let source_selection = json!({
        "metadata": {
            "selected_folder_ids": ["folder-a", "folder-b"]
        },
        "variant": "selected-sources"
    });

    let response = service
        .update_connection_source_selection(
            KnowledgeUpdateConnectionSourceSelectionRequest {
                tenant_id,
                connection_uid: connection.connection_uid,
                source_selection: source_selection.clone(),
                sync: true,
            },
            &caller,
        )
        .await
        .expect("source selection update should persist and trigger sync");
    let stored = repository
        .connection(connection.connection_uid)
        .expect("updated connection should stay stored");

    assert_eq!(response.connection_uid, connection.connection_uid);
    assert_eq!(response.source_selection, source_selection);
    assert_eq!(response.sync_status.as_deref(), Some("provider_syncing"));
    assert!(response.sync_run_uid.is_some());
    assert_eq!(stored.source_selection, source_selection);
    assert_eq!(stored.last_synced_at, None);
    assert_eq!(provider.apply_source_selection_count(), 1);
    assert_eq!(
        provider.applied_source_selections(),
        vec![stored.source_selection]
    );
    assert_eq!(provider.trigger_sync_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
}
