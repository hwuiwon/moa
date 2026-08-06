//! Whole-public-path coverage for constrained HTTP connector execution.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::connector::ConnectorDefinition;
use moa_connectors::catalog::{InstalledConnectorCatalogQuery, InstalledConnectorCatalogSnapshot};
use moa_connectors::domain::{
    ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth, ConnectionStatus,
    ConnectorConnection, ConnectorInvocationId, ConnectorInvocationRecord,
    ConnectorInvocationState, ConnectorInvocationTerminal, InstalledActionBinding,
    InstalledActionBindingId,
};
use moa_connectors::executor::{
    ConnectorActionInvocation, ConnectorActionRuntime, ConnectorInvocationCompletionService,
    InstalledConnectorActionPin, PreparedConnectorAction, SecuredConnectorOutputMetadata,
};
use moa_connectors::http::HttpConnectorRuntime;
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ConnectorInvocationRepository,
    InvocationReservation, InvocationReservationRequest, NewConnectorConnection,
};
use moa_connectors::{Error, Result};
use moa_core::traits::{CredentialVault, Identity, IdentityType};
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialRef, CredentialStagingToken,
    CredentialVersion, RedactedSecret,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId, ToolCallId};
use moa_core::types::security::ToolOutputAssessment;
use moa_security::outbound_http::{
    OutboundHostResolutionError, OutboundHostResolver, OutboundHttpPolicy,
};
use moa_test_support::fixture_connector_api::{
    FixtureCapturedHeaderValue, FixtureConnectorApi, FixtureConnectorClose,
    FixtureConnectorResponse, FixtureConnectorScript,
};
use secrecy::SecretString;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
struct InMemoryRepository {
    connection: Arc<Mutex<ConnectorConnection>>,
    binding: Arc<Mutex<InstalledActionBinding>>,
    invocations: Arc<Mutex<HashMap<ConnectorInvocationId, ConnectorInvocationRecord>>>,
}

impl InMemoryRepository {
    fn new(connection: ConnectorConnection, binding: InstalledActionBinding) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
            binding: Arc::new(Mutex::new(binding)),
            invocations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn invocation_states(&self) -> Vec<ConnectorInvocationState> {
        lock(&self.invocations)
            .values()
            .map(|record| record.state)
            .collect()
    }

    fn only_invocation(&self) -> ConnectorInvocationRecord {
        let records = lock(&self.invocations);
        assert_eq!(records.len(), 1, "fixture should contain one invocation");
        records
            .values()
            .next()
            .expect("one fixture invocation should exist")
            .clone()
    }

    fn unavailable() -> Error {
        Error::CatalogInvariant {
            message: "fixture repository operation is unavailable".to_string(),
        }
    }
}

#[async_trait]
impl ConnectionLifecycleRepository for InMemoryRepository {
    async fn create(&self, _request: NewConnectorConnection) -> Result<ConnectorConnection> {
        Err(Self::unavailable())
    }

    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>> {
        let connection = lock(&self.connection).clone();
        Ok(
            (connection.tenant_id == tenant_id && connection.connection_id == connection_id)
                .then_some(connection),
        )
    }

    async fn list(
        &self,
        _tenant_id: TenantId,
        _request: moa_connectors::repository::ConnectionListRequest,
    ) -> Result<moa_connectors::repository::ConnectionListPage> {
        Err(Self::unavailable())
    }

    async fn load_pinned_action(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
    ) -> Result<Option<moa_connectors::repository::PinnedConnectorAction>> {
        let connection = lock(&self.connection).clone();
        let binding = lock(&self.binding).clone();
        Ok((connection.tenant_id == tenant_id
            && connection.connection_id == connection_id
            && binding.tenant_id == tenant_id
            && binding.connection_id == connection_id
            && binding.binding_id == binding_id)
            .then_some(moa_connectors::repository::PinnedConnectorAction {
                connection,
                binding,
            }))
    }

    async fn transition(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        _expected_generation: ConnectionGeneration,
        _target: ConnectionStatus,
    ) -> Result<ConnectorConnection> {
        Err(Self::unavailable())
    }

    async fn update_health(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        _expected_generation: ConnectionGeneration,
        _health: ConnectionHealth,
        _reason: Option<String>,
    ) -> Result<ConnectorConnection> {
        Err(Self::unavailable())
    }

    async fn advance_credential_generation(
        &self,
        _tenant_id: TenantId,
        _connection_id: ConnectorConnectionId,
        _expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        Err(Self::unavailable())
    }

    async fn activate(&self, _request: ConnectionActivation) -> Result<ConnectorConnection> {
        Err(Self::unavailable())
    }
}

#[async_trait]
impl ConnectorInvocationRepository for InMemoryRepository {
    async fn reserve_invocation(
        &self,
        request: InvocationReservationRequest,
    ) -> Result<InvocationReservation> {
        let mut records = lock(&self.invocations);
        if let Some(existing) = records
            .values()
            .find(|record| {
                record.tenant_id == request.tenant_id && record.tool_call_id == request.tool_call_id
            })
            .cloned()
        {
            if existing.connection_id != request.connection_id
                || existing.binding_id != request.binding_id
                || existing.connection_generation != request.connection_generation
                || existing.request_hash != request.request_hash
                || existing.upstream_idempotency_key != request.upstream_idempotency_key
            {
                return Err(Error::InvocationConflict {
                    tool_call_id: request.tool_call_id,
                });
            }
            return Ok(if existing.state.is_terminal() {
                InvocationReservation::Replay(existing)
            } else {
                InvocationReservation::InFlight(existing)
            });
        }
        let now = Utc::now();
        let record = ConnectorInvocationRecord {
            invocation_id: request.invocation_id,
            tenant_id: request.tenant_id,
            connection_id: request.connection_id,
            binding_id: request.binding_id,
            connection_generation: request.connection_generation,
            tool_call_id: request.tool_call_id,
            request_hash: request.request_hash,
            upstream_idempotency_key: request.upstream_idempotency_key,
            state: ConnectorInvocationState::Reserved,
            error_metadata: None,
            output_metadata: None,
            started_at: now,
            completed_at: None,
            updated_at: now,
        };
        records.insert(record.invocation_id, record.clone());
        Ok(InvocationReservation::Reserved(record))
    }

    async fn load_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<Option<ConnectorInvocationRecord>> {
        Ok(lock(&self.invocations)
            .get(&invocation_id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned())
    }

    async fn mark_transmitting(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<ConnectorInvocationRecord> {
        let mut records = lock(&self.invocations);
        let record = records
            .get_mut(&invocation_id)
            .ok_or_else(Self::unavailable)?;
        if record.tenant_id != tenant_id || record.state != ConnectorInvocationState::Reserved {
            return Err(Error::InvocationStateConflict {
                invocation_id,
                from: record.state,
                to: ConnectorInvocationState::Transmitting,
            });
        }
        record.state = ConnectorInvocationState::Transmitting;
        record.updated_at = Utc::now();
        Ok(record.clone())
    }

    async fn finish_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
        terminal: ConnectorInvocationTerminal,
    ) -> Result<ConnectorInvocationRecord> {
        let mut records = lock(&self.invocations);
        let record = records
            .get_mut(&invocation_id)
            .ok_or_else(Self::unavailable)?;
        let target = terminal.state();
        if record.tenant_id != tenant_id {
            return Err(Self::unavailable());
        }
        if record.state == target {
            return Ok(record.clone());
        }
        record.state.transition(invocation_id, target)?;
        record.state = target;
        match terminal {
            ConnectorInvocationTerminal::Succeeded { output_metadata } => {
                record.output_metadata = Some(output_metadata);
            }
            ConnectorInvocationTerminal::FailedBeforeSend { error_metadata }
            | ConnectorInvocationTerminal::Failed { error_metadata }
            | ConnectorInvocationTerminal::UnknownOutcome { error_metadata } => {
                record.error_metadata = Some(error_metadata);
            }
        }
        let now = Utc::now();
        record.completed_at = Some(now);
        record.updated_at = now;
        Ok(record.clone())
    }
}

#[derive(Default)]
struct StaticVault {
    secret: Option<String>,
    resolves: AtomicUsize,
    identities: Mutex<Vec<CredentialIdentity>>,
}

impl StaticVault {
    fn with_secret(secret: impl Into<String>) -> Self {
        Self {
            secret: Some(secret.into()),
            resolves: AtomicUsize::new(0),
            identities: Mutex::new(Vec::new()),
        }
    }

    fn identities(&self) -> Vec<CredentialIdentity> {
        lock(&self.identities).clone()
    }
}

#[async_trait]
impl CredentialVault for StaticVault {
    async fn stage(
        &self,
        _identity: CredentialIdentity,
        _material: SecretString,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialStagingToken, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn activate_staged(
        &self,
        _staged: &CredentialStagingToken,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialVersion, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn rollback_activation(
        &self,
        _candidate: CredentialRef,
        _prior_active: Option<CredentialRef>,
        _ctx: &CredentialContext,
    ) -> std::result::Result<CredentialVersion, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn has_active(
        &self,
        _identity: &CredentialIdentity,
        _ctx: &CredentialContext,
    ) -> std::result::Result<bool, CredentialError> {
        Ok(self.secret.is_some())
    }

    async fn has_active_batch(
        &self,
        identities: &[CredentialIdentity],
        _ctx: &CredentialContext,
    ) -> std::result::Result<Vec<bool>, CredentialError> {
        Ok(vec![self.secret.is_some(); identities.len()])
    }

    async fn resolve_active(
        &self,
        identity: &CredentialIdentity,
        _ctx: &CredentialContext,
    ) -> std::result::Result<RedactedSecret, CredentialError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        lock(&self.identities).push(identity.clone());
        self.secret
            .as_ref()
            .cloned()
            .map(RedactedSecret::new)
            .ok_or(CredentialError::NotFound)
    }

    async fn describe_batch(
        &self,
        _references: &[(Uuid, CredentialRef)],
        _ctx: &CredentialContext,
    ) -> std::result::Result<Vec<(Uuid, CredentialVersion)>, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn revoke(
        &self,
        _reference: CredentialRef,
        _ctx: &CredentialContext,
    ) -> std::result::Result<(), CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn revoke_connection(
        &self,
        _connection_uid: Uuid,
        _ctx: &CredentialContext,
    ) -> std::result::Result<u64, CredentialError> {
        Err(CredentialError::NotFound)
    }

    async fn purge_tenant(
        &self,
        _limit: u32,
        _ctx: &CredentialContext,
    ) -> std::result::Result<u64, CredentialError> {
        Err(CredentialError::NotFound)
    }
}

struct UnusedResolver;

#[async_trait]
impl OutboundHostResolver for UnusedResolver {
    async fn resolve(
        &self,
        _host: &str,
        _port: u16,
    ) -> std::result::Result<Vec<SocketAddr>, OutboundHostResolutionError> {
        Err(OutboundHostResolutionError::Failed)
    }
}

#[tokio::test]
async fn http_runtime_pins_transport_and_requires_post_security_completion_offline() {
    // Pins: untrusted fields cannot change the reviewed transport, the secret is
    // injected only into a redacted fixed header, and replay after a lost journal
    // acknowledgement uses the exact same upstream idempotency key. The journaled
    // completion ticket contains only semantic completion-authority fields.
    let fixture_secret = "fixture-secret-never-visible";
    let fixture = FixtureConnectorApi::start(
        FixtureConnectorScript::new(vec![FixtureConnectorResponse::json(json!({
            "data": {"accepted": true}
        }))])
        .with_sensitive_header("x-connector-key"),
    )
    .await
    .expect("connector fixture should start");
    let (runtime, repository, vault, invocation, prepared) = runtime_fixture(
        fixture.origin(),
        Arc::new(StaticVault::with_secret(fixture_secret)),
        loopback_policy(),
    );

    let result = runtime
        .invoke(invocation.clone(), prepared.clone())
        .await
        .expect("reviewed connector request should succeed");
    assert_eq!(result.output(), &json!({"accepted": true}));
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::Transmitting
    );

    let requests = fixture
        .controller()
        .wait_for_requests(1, std::time::Duration::from_secs(1))
        .await
        .expect("fixture should capture the connector request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].target,
        "/v1/accounts/acme%2F..%2Fadmin?include=a%26override%3Dtrue"
    );
    assert_eq!(requests[0].json_body, Some(json!({"amount": 42})));
    assert_eq!(
        requests[0].headers.get("x-connector-key"),
        Some(&vec![FixtureCapturedHeaderValue::Redacted])
    );
    assert_eq!(
        requests[0].headers.get("idempotency-key"),
        Some(&vec![FixtureCapturedHeaderValue::Visible(
            invocation.tool_call_id.to_string()
        )])
    );
    assert_eq!(
        requests[0].headers.get("host"),
        Some(&vec![FixtureCapturedHeaderValue::Visible(
            fixture
                .origin()
                .strip_prefix("http://")
                .expect("fixture origin should be HTTP")
                .to_string()
        )])
    );
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!format!("{result:?}").contains(fixture_secret));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 1);
    let identities = vault.identities();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].slot_name.as_str(), "primary");

    let replay_result = runtime
        .invoke(invocation.clone(), prepared.clone())
        .await
        .expect("idempotency-keyed transmitting invocation should resume safely");
    let replay_requests = fixture
        .controller()
        .wait_for_requests(2, std::time::Duration::from_secs(1))
        .await
        .expect("fixture should capture the replayed keyed request");
    assert_eq!(replay_requests.len(), 2);
    let expected_key = vec![FixtureCapturedHeaderValue::Visible(
        invocation.tool_call_id.to_string(),
    )];
    assert_eq!(
        replay_requests[0].headers.get("idempotency-key"),
        Some(&expected_key)
    );
    assert_eq!(
        replay_requests[1].headers.get("idempotency-key"),
        Some(&expected_key)
    );
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 2);

    let (output, ticket) = replay_result.into_parts();
    assert_eq!(output, json!({"accepted": true}));
    let serialized_ticket = serde_json::to_value(&ticket)
        .expect("secret-free completion ticket should serialize for journaling");
    let mut serialized_ticket_fields = serialized_ticket
        .as_object()
        .expect("completion ticket should serialize as an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    serialized_ticket_fields.sort_unstable();
    assert_eq!(
        serialized_ticket_fields,
        vec![
            "binding_id",
            "connection_generation",
            "connection_id",
            "invocation_id",
            "request_hash",
            "tenant_id",
            "tool_call_id",
        ]
    );
    ConnectorInvocationCompletionService::new(repository.clone())
        .finalize_succeeded(
            &ticket,
            SecuredConnectorOutputMetadata {
                assessment: ToolOutputAssessment::safe(),
                secured_output_bytes: 17,
            },
        )
        .await
        .expect("journaled secured output should finalize the transmitting invocation");
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::Succeeded
    );
    assert!(
        !serde_json::to_string(&repository.only_invocation())
            .expect("secret-free invocation row should serialize")
            .contains(fixture_secret)
    );
    drop(result);
}

#[tokio::test]
async fn non_idempotent_journal_gap_becomes_manual_reconciliation_offline() {
    // Pins: if a non-idempotent response was returned but its surrounding
    // Restate run result was not journaled, replay closes the transmitting row
    // as unknown and never sends the remote request again.
    let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}})),
    ]))
    .await
    .expect("connector fixture should start");
    let (runtime, repository, _, invocation, prepared) =
        runtime_fixture_without_upstream_idempotency(
            fixture.origin(),
            Arc::new(StaticVault::with_secret("journal-gap-secret")),
            loopback_policy(),
        );

    let unjournaled = runtime
        .invoke(invocation.clone(), prepared.clone())
        .await
        .expect("first non-idempotent request should reach the upstream");
    assert_eq!(unjournaled.output(), &json!({"accepted": true}));
    assert_eq!(fixture.controller().requests().len(), 1);
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::Transmitting
    );

    let replay_error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("ambiguous non-idempotent replay must require reconciliation");
    assert!(matches!(
        replay_error,
        Error::ManualReconciliationRequired { .. }
    ));
    assert_eq!(fixture.controller().requests().len(), 1);
    let record = repository.only_invocation();
    assert_eq!(record.state, ConnectorInvocationState::UnknownOutcome);
    assert_eq!(
        record.error_metadata,
        Some(json!({
            "code": "effect_journal_ambiguous",
            "manual_reconciliation_required": true,
        }))
    );
}

#[tokio::test]
async fn denied_destination_fails_before_credentials_or_transmission_offline() {
    // Pins: a production-denied loopback destination is rejected before the
    // vault is opened and before a durable invocation can reach transmission.
    let vault = Arc::new(StaticVault::with_secret("must-not-be-resolved"));
    let (runtime, repository, _, invocation, prepared) = runtime_fixture(
        "https://127.0.0.1:443",
        vault.clone(),
        OutboundHttpPolicy::production(Arc::new(UnusedResolver)),
    );

    let error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("loopback production destination must fail closed");
    assert!(matches!(
        error,
        Error::Http {
            code: "destination_rejected"
        }
    ));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    assert!(repository.invocation_states().is_empty());
}

#[tokio::test]
async fn response_redirect_content_type_and_stream_limit_fail_closed_offline() {
    // Pins: HTTP response policy never follows redirects and rejects both
    // unapproved media types and bodies that cross the streamed byte cap.
    let oversized_headers = (0..129).fold(
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}})),
        |response, index| response.with_header(format!("x-fixture-{index}"), "v"),
    );
    let cases = vec![
        (
            FixtureConnectorResponse::redirect(302, "http://127.0.0.1/other"),
            vec!["redirect_rejected"],
        ),
        (
            FixtureConnectorResponse::bytes("text/html", b"{}".to_vec()),
            vec!["response_content_type_rejected"],
        ),
        (
            FixtureConnectorResponse::chunked_oversized(65, 16),
            vec!["response_body_too_large"],
        ),
        (
            oversized_headers,
            vec!["response_headers_rejected", "transport_failed"],
        ),
    ];
    for (response, expected_codes) in cases {
        let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![response]))
            .await
            .expect("response-policy fixture should start");
        let (runtime, repository, _, invocation, prepared) =
            runtime_fixture_without_upstream_idempotency(
                fixture.origin(),
                Arc::new(StaticVault::with_secret("response-policy-secret")),
                loopback_policy(),
            );
        let error = runtime
            .invoke(invocation, prepared)
            .await
            .expect_err("unsafe response should fail closed");
        let code = match error {
            Error::Http { code } => code,
            other => panic!("unexpected response-policy failure: {other:?}"),
        };
        assert!(
            expected_codes.contains(&code),
            "unexpected response-policy failure code: {code}"
        );
        assert_eq!(fixture.controller().requests().len(), 1);
        assert_eq!(
            repository.only_invocation().state,
            if code == "transport_failed" {
                ConnectorInvocationState::UnknownOutcome
            } else {
                ConnectorInvocationState::Failed
            }
        );
        assert!(!format!("{code:?}").contains("response-policy-secret"));
    }
}

#[tokio::test]
async fn request_body_and_total_timeout_limits_preserve_send_boundary_offline() {
    // Pins: request-size rejection happens during transport preparation before
    // reservation or vault access, while timeout before headers is uncertain
    // and timeout while streaming a known response is a known failed outcome.
    let vault = Arc::new(StaticVault::with_secret("request-limit-secret"));
    let (runtime, repository, _, mut invocation, prepared) =
        runtime_fixture("http://127.0.0.1:9", vault.clone(), loopback_policy());
    invocation.input["payload"] = json!({"data": "x".repeat(256)});
    let error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("oversized request body should fail before transmission");
    assert!(matches!(
        error,
        Error::Http {
            code: "request_body_too_large"
        }
    ));
    assert_eq!(vault.resolves.load(Ordering::SeqCst), 0);
    assert!(repository.invocation_states().is_empty());

    let before_headers = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}}))
            .with_delay_before_headers(std::time::Duration::from_millis(1_500)),
    ]))
    .await
    .expect("preheader timeout fixture should start");
    let (runtime, repository, _, invocation, prepared) =
        runtime_fixture_without_upstream_idempotency(
            before_headers.origin(),
            Arc::new(StaticVault::with_secret("preheader-timeout-secret")),
            loopback_policy(),
        );
    let error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("timeout before response headers should be uncertain");
    assert!(
        matches!(
            error,
            Error::Http {
                code: "total_timeout_during_transmission"
            }
        ),
        "unexpected preheader timeout failure: {error:?}"
    );
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::UnknownOutcome
    );

    let streamed = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::chunked_oversized(32, 8)
            .with_delay_between_chunks(std::time::Duration::from_millis(1_500)),
    ]))
    .await
    .expect("stream timeout fixture should start");
    let (runtime, repository, _, invocation, prepared) = runtime_fixture(
        streamed.origin(),
        Arc::new(StaticVault::with_secret("stream-timeout-secret")),
        loopback_policy(),
    );
    let error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("timeout after response headers should fail closed");
    assert!(
        matches!(
            error,
            Error::Http {
                code: "total_timeout_after_response"
            }
        ),
        "unexpected streamed timeout failure: {error:?}"
    );
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::Failed
    );
}

#[tokio::test]
async fn credential_failure_is_before_send_and_preheader_loss_is_unknown_offline() {
    // Pins: failures after durable reservation but before send remain safely
    // retry-classified, while connection loss after the send boundary is sticky
    // unknown outcome and is never retransmitted.
    let no_secret = Arc::new(StaticVault::default());
    let (fixture_runtime, before_send_repository, _, invocation, prepared) =
        runtime_fixture("http://127.0.0.1:9", no_secret, loopback_policy());
    let credential_error = fixture_runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("missing credential should fail before send");
    assert!(matches!(
        credential_error,
        Error::Credential(CredentialError::NotFound)
    ));
    assert_eq!(
        before_send_repository.only_invocation().state,
        ConnectorInvocationState::FailedBeforeSend
    );

    let invalid_secret = "leaky-secret\nheader";
    let (runtime, repository, _, invocation, prepared) = runtime_fixture(
        "http://127.0.0.1:9",
        Arc::new(StaticVault::with_secret(invalid_secret)),
        loopback_policy(),
    );
    let error = runtime
        .invoke(invocation, prepared)
        .await
        .expect_err("invalid credential bytes should fail before send");
    assert!(matches!(
        error,
        Error::Http {
            code: "credential_header_rejected"
        }
    ));
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::FailedBeforeSend
    );
    assert!(!format!("{error:?}").contains("leaky-secret"));
    assert!(
        !serde_json::to_string(&repository.only_invocation())
            .expect("redacted failure row should serialize")
            .contains("leaky-secret")
    );

    let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({}))
            .with_connection_close(FixtureConnectorClose::BeforeHeaders),
    ]))
    .await
    .expect("preheader-loss fixture should start");
    let (runtime, repository, _, invocation, prepared) =
        runtime_fixture_without_upstream_idempotency(
            fixture.origin(),
            Arc::new(StaticVault::with_secret("unknown-outcome-secret")),
            loopback_policy(),
        );
    let replay = invocation.clone();
    let error = runtime
        .invoke(invocation, prepared.clone())
        .await
        .expect_err("connection loss before response headers should be uncertain");
    assert!(matches!(
        error,
        Error::Http {
            code: "transport_failed"
        }
    ));
    assert_eq!(
        repository.only_invocation().state,
        ConnectorInvocationState::UnknownOutcome
    );
    assert_eq!(fixture.controller().requests().len(), 1);

    let replay_error = runtime
        .invoke(replay, prepared)
        .await
        .expect_err("unknown outcome must never retransmit automatically");
    assert!(matches!(
        replay_error,
        Error::ManualReconciliationRequired { .. }
    ));
    assert_eq!(fixture.controller().requests().len(), 1);
    assert_eq!(
        repository.only_invocation().error_metadata,
        Some(json!({
            "code": "transport_outcome_unknown",
            "manual_reconciliation_required": true,
        }))
    );
}

fn runtime_fixture(
    origin: &str,
    vault: Arc<StaticVault>,
    policy: OutboundHttpPolicy,
) -> (
    HttpConnectorRuntime,
    Arc<InMemoryRepository>,
    Arc<StaticVault>,
    ConnectorActionInvocation,
    PreparedConnectorAction,
) {
    runtime_fixture_with_upstream_idempotency(origin, vault, policy, true)
}

fn runtime_fixture_without_upstream_idempotency(
    origin: &str,
    vault: Arc<StaticVault>,
    policy: OutboundHttpPolicy,
) -> (
    HttpConnectorRuntime,
    Arc<InMemoryRepository>,
    Arc<StaticVault>,
    ConnectorActionInvocation,
    PreparedConnectorAction,
) {
    runtime_fixture_with_upstream_idempotency(origin, vault, policy, false)
}

fn runtime_fixture_with_upstream_idempotency(
    origin: &str,
    vault: Arc<StaticVault>,
    policy: OutboundHttpPolicy,
    upstream_idempotency: bool,
) -> (
    HttpConnectorRuntime,
    Arc<InMemoryRepository>,
    Arc<StaticVault>,
    ConnectorActionInvocation,
    PreparedConnectorAction,
) {
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let generation = ConnectionGeneration::new(2).expect("fixture generation should be valid");
    let mut definition_json = json!({
        "display_name": "HTTP fixture",
        "auth": [{
            "type": "api_key_header",
            "slot": "primary",
            "header": "x-connector-key"
        }],
        "actions": [{
            "id": "create_item",
            "description": "Create one item",
            "contract": {
                    "method": "POST",
                    "path_template": "/v1/accounts/{account}",
                    "path_inputs": [{
                        "placeholder": "account",
                        "input_pointer": "/account"
                    }],
                    "query_inputs": [{
                        "parameter": "include",
                        "input_pointer": "/include"
                    }],
                    "body_input": {"input_pointer": "/payload"},
                    "credential_slot": "primary",
                    "upstream_idempotency_header": "idempotency-key",
                    "response_pointer": "/data",
                    "max_request_bytes": 128,
                    "max_response_bytes": 64,
                    "connect_timeout_ms": 200,
                    "total_timeout_ms": 1000,
                    "policy": {
                        "input_schema": {
                            "type": "object",
                            "required": ["account", "include", "payload"],
                            "properties": {
                                "account": {"type": "string"},
                                "include": {"type": "string"},
                                "payload": {"type": "object"}
                            }
                        },
                        "output_schema": {
                            "type": "object",
                            "required": ["accepted"],
                            "properties": {"accepted": {"type": "boolean"}}
                        },
                        "data_classes": [],
                        "idempotency": "idempotent"
                    }
                }
        }]
    });
    if !upstream_idempotency {
        let contract = definition_json["actions"][0]["contract"]
            .as_object_mut()
            .expect("fixture connector contract should be an object");
        contract.remove("upstream_idempotency_header");
        contract
            .get_mut("policy")
            .and_then(serde_json::Value::as_object_mut)
            .expect("fixture connector policy should be an object")
            .insert("idempotency".to_string(), json!("non_idempotent"));
    }
    let definition: ConnectorDefinition =
        serde_json::from_value(definition_json).expect("HTTP connector fixture should deserialize");
    let action = definition
        .actions
        .first()
        .expect("HTTP connector fixture should contain one action");
    let compiled_contract =
        moa_connectors::domain::CompiledOperationContract::compile(&definition, action)
            .expect("HTTP fixture contract should compile");
    let contract_hash = compiled_contract
        .hash()
        .expect("HTTP fixture contract should hash");
    let definition_ref = ConnectionDefinitionRef::Artifact {
        artifact_uid: Uuid::new_v4(),
        revision_uid: Uuid::new_v4(),
    };
    let connection = ConnectorConnection {
        connection_id,
        tenant_id,
        display_name: "HTTP fixture".to_string(),
        definition: definition_ref.clone(),
        origin: Some(origin.parse().expect("fixture origin should be canonical")),
        non_secret_config: json!({}),
        generation,
        status: ConnectionStatus::Active,
        health: ConnectionHealth::Ready,
        health_reason: None,
        created_by_identity_id: None,
        owner_identity_id: Some(Uuid::new_v4()),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let binding = InstalledActionBinding {
        binding_id: InstalledActionBindingId(Uuid::new_v4()),
        tenant_id,
        connection_id,
        connection_generation: generation,
        action_id: action.id.clone(),
        compiled_contract,
        contract_hash,
        governed_contract_revision: "test/http/create-item/v1".to_string(),
        minimum_effect: moa_core::types::action_policy::ActionPolicyEffect::AdminReview,
        enabled: true,
    };
    let pin = InstalledConnectorActionPin {
        connection_id,
        connection_generation: generation,
        definition: definition_ref,
        binding_id: binding.binding_id,
        action_id: binding.action_id.clone(),
        contract_hash,
        governed_contract_revision: binding.governed_contract_revision.clone(),
    };
    let repository = Arc::new(InMemoryRepository::new(connection, binding));
    let runtime = HttpConnectorRuntime::new(
        repository.clone(),
        repository.clone(),
        vault.clone(),
        policy,
    );
    let invocation = ConnectorActionInvocation {
        caller: Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::new_v4(),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        tool_call_id: ToolCallId::new(),
        action: pin,
        input: json!({
            "account": "acme/../admin",
            "include": "a&override=true",
            "payload": {"amount": 42},
            "host": "attacker.invalid",
            "method": "DELETE",
            "header": "authorization",
            "credential_slot": "attacker"
        }),
        cancellation_token: CancellationToken::new(),
    };
    let query = InstalledConnectorCatalogQuery::new(
        invocation.caller.clone(),
        [invocation.action.connection_id],
    );
    let connection = lock(&repository.connection).clone();
    let binding = lock(&repository.binding).clone();
    let snapshot =
        InstalledConnectorCatalogSnapshot::from_candidates(&query, [(connection, binding)])
            .expect("fixture catalog admission should succeed");
    let prepared = snapshot.actions()[0].prepared();
    (runtime, repository, vault, invocation, prepared)
}

fn loopback_policy() -> OutboundHttpPolicy {
    OutboundHttpPolicy::loopback_http_for_tests(Arc::new(UnusedResolver))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
