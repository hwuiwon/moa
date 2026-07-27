//! Operation-fenced link claim behavior for the tenant Knowledge service.

use super::*;

/// Builds a service whose provider can be configured per test.
fn link_service(
    repository: Arc<InMemoryKnowledgeRepository>,
    provider: Arc<FakeLinkedIntegrationProvider>,
    credentials: Arc<FakeKnowledgeCredentialStore>,
) -> KnowledgeService {
    KnowledgeService::new(
        repository.clone(),
        repository,
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider)),
        credentials,
        fake_ingestion_runner(),
        80,
    )
}

fn exchange_request(tenant_id: TenantId) -> KnowledgeExchangeTokenRequest {
    KnowledgeExchangeTokenRequest {
        tenant_id,
        provider: PROVIDER.to_string(),
        connector: CONNECTOR.to_string(),
        exchange_token: "public-token".to_string(),
        source_selection: json!({}),
        information_barrier: None,
    }
}

#[tokio::test]
async fn replaying_one_link_operation_finalizes_once_and_writes_one_credential() {
    // Pins: the link is idempotent under replay. Repeating the same operation id
    // returns the finalized result without exchanging again, writing a second
    // credential version, or starting a second provider sync — the failure mode
    // that leaves an orphaned vault version behind.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(repository.clone(), provider.clone(), credentials.clone());

    let first = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("first link should finalize");
    let second = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("replaying the same operation should return the finalized result");

    assert_eq!(first.connection_uid, second.connection_uid);
    assert_eq!(first.sync_run_uid, second.sync_run_uid);
    assert_eq!(credentials.stored_account_count(), 1);
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(
        provider.start_initial_sync_count(),
        1,
        "a finalized claim must not start the provider sync again"
    );
    assert_eq!(
        repository.only_link_claim().state,
        LinkClaimState::Finalized
    );
}

#[tokio::test]
async fn relinking_the_same_provider_account_binds_the_credential_to_the_kept_connection() {
    // Pins: re-linking keeps the existing connection identifier, and the claim
    // resolves that identifier before writing anything. Minting a fresh
    // identifier here would bind the new credential to a connection the upsert
    // never creates, orphaning the version immediately.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(repository.clone(), provider, credentials.clone());

    let first = service
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect("first link should finalize");
    repository.finish_sync_run(first.sync_run_uid.expect("link should start a sync run"));

    let relink = service
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect("re-link under a new operation should finalize");

    assert_eq!(
        relink.connection_uid, first.connection_uid,
        "a re-link must keep the connection the upsert conflict target resolves to"
    );
    let connection = repository
        .connection(relink.connection_uid)
        .expect("connection should be stored");
    assert_eq!(
        credentials.connection_for_reference(&connection.credential_ref),
        Some(connection.connection_uid),
        "the credential the connection points at must be bound to that connection"
    );
}

#[tokio::test]
async fn reusing_a_link_operation_id_for_a_different_account_is_a_typed_conflict() {
    // Pins: the claim's request hash fences the operation. Reusing an id after
    // the linked account changed is rejected rather than silently adopting the
    // new account under the old claim.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::default()),
        credentials.clone(),
    );
    service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("first link should finalize");

    // A different provider account under the same operation id resolves to a
    // different connection identifier, so the recorded hash cannot match.
    let claim = repository.only_link_claim();
    let mut conflicting = repository.only_link_claim();
    conflicting.request_hash = format!("{}-different", claim.request_hash);
    repository.overwrite_link_claim(conflicting);

    let error = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect_err("a reused operation id with different inputs must be rejected");

    assert!(
        error.to_string().contains("reused"),
        "conflict should be reported as an idempotency conflict: {error}"
    );
}

#[tokio::test]
async fn failed_relink_revokes_only_its_candidate_and_restores_the_previous_reference() {
    // Pins: a post-write failure durably compensates. The candidate this
    // operation wrote is revoked and the exact reference it superseded comes
    // back, so a failed re-link never leaves an unclaimed active credential or
    // a connection pointing at dead material.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let healthy = link_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::default()),
        credentials.clone(),
    );
    let first = healthy
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect("first link should finalize");
    let original_ref = repository
        .connection(first.connection_uid)
        .expect("connection should be stored")
        .credential_ref;
    repository.finish_sync_run(first.sync_run_uid.expect("link should start a sync run"));

    let failing = link_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::with_initial_sync_error(
            "provider refused the initial sync",
        )),
        credentials.clone(),
    );
    failing
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect_err("a failed initial sync must fail the link");

    let connection = repository
        .connection(first.connection_uid)
        .expect("connection should still be stored");
    assert_eq!(
        connection.credential_ref, original_ref,
        "compensation must restore the exact previous reference"
    );
    assert_eq!(
        connection.status,
        ConnectionStatus::Active,
        "a restored re-link leaves the surviving connection usable"
    );
    assert_eq!(
        credentials.revoked_references(),
        credentials
            .references()
            .into_iter()
            .filter(|reference| *reference != original_ref)
            .collect::<Vec<_>>(),
        "only the candidate this operation wrote may be revoked"
    );
    let claims = repository.link_claim_states();
    assert!(
        claims.contains(&LinkClaimState::Compensated),
        "the failed operation must end compensated, found {claims:?}"
    );
}

#[tokio::test]
async fn a_compensated_link_operation_is_terminal_under_replay() {
    // Pins: compensation is not a retry. Replaying a compensated operation id
    // fails instead of writing a second credential under the same fence.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::with_initial_sync_error(
            "provider refused the initial sync",
        )),
        credentials.clone(),
    );
    service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect_err("the first attempt must fail");
    let credentials_after_failure = credentials.stored_account_count();

    let error = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect_err("a compensated operation must not be retried under the same id");

    assert!(
        error.to_string().contains("compensated"),
        "replay of a compensated operation should report the terminal state: {error}"
    );
    assert_eq!(
        credentials.stored_account_count(),
        credentials_after_failure,
        "a terminal claim must not write more credential state"
    );
}

#[tokio::test]
async fn a_crash_between_sync_run_claim_and_dispatch_replays_the_exact_trigger() {
    // Pins: a persisted queued sync run is not evidence that the provider was
    // ever called. With the durable trigger boundary cleared — a crash between
    // claiming the run and dispatching — replay must perform the idempotent
    // initial-sync call again for that exact run before the link finalizes.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(repository.clone(), provider.clone(), credentials);

    let first = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("first link should finalize");
    let sync_run_uid = first.sync_run_uid.expect("link should start a sync run");
    assert_eq!(provider.start_initial_sync_count(), 1);

    // Rewind to the crash: the run is claimed and active, the claim is still
    // `credential_written`, and no dispatch was ever recorded.
    repository.clear_provider_trigger_boundary(sync_run_uid);
    repository.rewind_link_claim_to_credential_written();

    let resumed = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("replay should resume the link");

    assert_eq!(
        resumed.sync_run_uid,
        Some(sync_run_uid),
        "replay must resume the run it already claimed, not start another"
    );
    assert_eq!(repository.sync_run_count(), 1);
    assert_eq!(
        provider.start_initial_sync_count(),
        2,
        "the exact idempotent trigger must be replayed for the claimed run"
    );
    assert!(
        repository
            .sync_run(sync_run_uid)
            .expect("sync run should exist")
            .provider_trigger_completed_at
            .is_some(),
        "the replayed dispatch must leave the boundary durable"
    );
    assert_eq!(
        repository.only_link_claim().state,
        LinkClaimState::Finalized
    );
}

#[tokio::test]
async fn a_run_with_a_durable_trigger_boundary_is_not_dispatched_again() {
    // Pins: the boundary is what distinguishes "never dispatched" from "already
    // dispatched". A claimed run that records a dispatch is returned as-is,
    // so ordinary replay does not re-call the provider.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let caller = test_caller(tenant_id);
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let provider = Arc::new(FakeLinkedIntegrationProvider::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(repository.clone(), provider.clone(), credentials);

    let first = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("first link should finalize");
    repository.rewind_link_claim_to_credential_written();

    let resumed = service
        .exchange_public_token(exchange_request(tenant_id), &caller)
        .await
        .expect("replay should resume the link");

    assert_eq!(resumed.sync_run_uid, first.sync_run_uid);
    assert_eq!(
        provider.start_initial_sync_count(),
        1,
        "a durable boundary must suppress a second dispatch"
    );
}

#[tokio::test]
async fn a_link_cannot_finalize_on_a_sync_run_it_does_not_own() {
    // Pins: `AlreadyRunning` is not evidence. A run another operation claimed was
    // dispatched with a different credential, so a link that adopted it would
    // report an initial sync its own candidate never had.
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = Arc::new(InMemoryKnowledgeRepository::default());
    let credentials = Arc::new(FakeKnowledgeCredentialStore::default());
    let service = link_service(
        repository.clone(),
        Arc::new(FakeLinkedIntegrationProvider::default()),
        credentials.clone(),
    );
    let first = service
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect("first link should finalize");
    let original_ref = repository
        .connection(first.connection_uid)
        .expect("connection should be stored")
        .credential_ref;

    // The first link's run is deliberately left active.
    let error = service
        .exchange_public_token(exchange_request(tenant_id), &test_caller(tenant_id))
        .await
        .expect_err("a re-link must not finalize on another operation's active run");

    assert!(
        error.to_string().contains("another sync run is active"),
        "the link should refuse unrelated evidence: {error}"
    );
    assert_eq!(
        repository
            .connection(first.connection_uid)
            .expect("connection should still be stored")
            .credential_ref,
        original_ref,
        "the refused re-link must compensate back to the previous reference"
    );
}
