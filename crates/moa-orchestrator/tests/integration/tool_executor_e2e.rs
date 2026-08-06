//! End-to-end tool executor coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::connector::ConnectorDefinition;
use moa_artifacts::document::{
    ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactMetadata, ArtifactStatus,
    ArtifactUi,
};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_artifacts::validation::validate_for_status;
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectorInvocationId,
    ConnectorInvocationState, InstalledActionBinding, InstalledActionBindingId,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ConnectorInvocationRepository,
    NewConnectorConnection, PostgresConnectionRepository,
};
use moa_core::types::action_policy::{ActionPolicyEffect, ActionRuleScope, CallOrigin};
use moa_core::types::agent::{
    AgentActionPolicy, AgentConnectorBinding, AgentContext, AgentPolicySnapshot,
};
use moa_core::types::identifiers::{ConnectorConnectionId, ModelId, SessionId, TenantId};
use moa_core::types::tools::IdempotencyClass;
use moa_core::{
    events::Event, traits::Identity, types::events_stream::EventRange,
    types::identifiers::ToolCallId, types::session::SessionStatus,
};
use moa_execution::capability::{CapabilitiesListResponse, CapabilitySource};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_orchestrator::services::action_reviews::{
    ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
    ListActionReviewsRequest,
};
use moa_test_support::fixture_connector_api::{
    FixtureCapturedHeaderValue, FixtureConnectorApi, FixtureConnectorResponse,
    FixtureConnectorScript,
};
use moa_test_support::fixtures::fresh_client_message_id;
use moa_test_support::postgres::test_database_url;
use moa_test_support::{IsolatedTest, OrchestratorTestFixture, TestApiClient};
use moa_wire::turn::{StartTurnRequest, TurnOutcomeKind};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::grant_connector_connection_use;
use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_operator, register_deployment, reserve_orchestrator_ports, restate_ingress_url,
    restate_test_admin_url, test_user_identity, with_identity,
};

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    let postgres_url = test_database_url();

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .arg("--credential-port")
        .arg(ports.credential.to_string())
        .env("MOA_DATABASE_URL", postgres_url)
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("MOA_INTEGRATION_CONNECTOR_LOOPBACK_ENABLED", "1")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

struct SeededConnector {
    connection_id: ConnectorConnectionId,
    artifact_uid: Uuid,
    revision_uid: Uuid,
}

struct ConnectorFinalizationBarrier {
    connection: PgConnection,
    lock_key: i64,
    holder_pid: i32,
}

impl ConnectorFinalizationBarrier {
    async fn install(database_url: &str, pool: &sqlx::PgPool) -> Result<Self> {
        let lock_uuid = Uuid::now_v7();
        let lock_key = i64::from_be_bytes(
            lock_uuid.as_bytes()[..8]
                .try_into()
                .context("derive connector finalization barrier key")?,
        );
        let mut connection = PgConnection::connect(database_url)
            .await
            .context("connect connector finalization barrier")?;
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .context("load connector finalization barrier backend pid")?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(lock_key)
            .execute(&mut connection)
            .await
            .context("hold connector finalization advisory lock")?;
        let ddl = format!(
            r#"
            CREATE OR REPLACE FUNCTION moa.block_connector_finalization_recovery_matrix()
            RETURNS TRIGGER
            LANGUAGE plpgsql
            AS $$
            BEGIN
                IF OLD.state = 'transmitting'
                   AND NEW.state = 'succeeded'
                THEN
                    PERFORM pg_advisory_xact_lock({lock_key});
                END IF;
                RETURN NEW;
            END;
            $$;
            DROP TRIGGER IF EXISTS block_connector_finalization_recovery_matrix
                ON moa.connector_action_invocations;
            CREATE TRIGGER block_connector_finalization_recovery_matrix
                BEFORE UPDATE ON moa.connector_action_invocations
                FOR EACH ROW
                EXECUTE FUNCTION moa.block_connector_finalization_recovery_matrix();
            "#
        );
        sqlx::raw_sql(&ddl)
            .execute(pool)
            .await
            .context("install connector finalization trigger")?;
        Ok(Self {
            connection,
            lock_key,
            holder_pid,
        })
    }

    async fn wait_for_blocked_update(&self, pool: &sqlx::PgPool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS ( \
                    SELECT 1 FROM pg_locks AS held \
                    JOIN pg_locks AS waiting \
                      ON waiting.locktype = held.locktype \
                     AND waiting.database IS NOT DISTINCT FROM held.database \
                     AND waiting.classid IS NOT DISTINCT FROM held.classid \
                     AND waiting.objid IS NOT DISTINCT FROM held.objid \
                     AND waiting.objsubid IS NOT DISTINCT FROM held.objsubid \
                    WHERE held.pid = $1 AND held.granted AND NOT waiting.granted \
                )",
            )
            .bind(self.holder_pid)
            .fetch_one(pool)
            .await
            .context("inspect connector finalization lock waiter")?;
            if waiting {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("connector finalization did not reach the advisory-lock barrier");
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn release(&mut self) -> Result<()> {
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(self.lock_key)
            .fetch_one(&mut self.connection)
            .await
            .context("release connector finalization advisory lock")?;
        if !unlocked {
            anyhow::bail!("connector finalization advisory lock was not held");
        }
        Ok(())
    }

    async fn remove(pool: &sqlx::PgPool) -> Result<()> {
        sqlx::raw_sql(
            r#"
            DROP TRIGGER IF EXISTS block_connector_finalization_recovery_matrix
                ON moa.connector_action_invocations;
            DROP FUNCTION IF EXISTS moa.block_connector_finalization_recovery_matrix();
            "#,
        )
        .execute(pool)
        .await
        .context("remove connector finalization trigger")?;
        Ok(())
    }
}

async fn seed_published_http_connector(
    pool: &sqlx::PgPool,
    identity: &Identity,
    origin: &str,
    idempotency: IdempotencyClass,
    connection_id: ConnectorConnectionId,
    minimum_effect: ActionPolicyEffect,
) -> Result<SeededConnector> {
    let upstream_idempotency_header = match idempotency {
        IdempotencyClass::Idempotent => Some("idempotency-key"),
        IdempotencyClass::NonIdempotent => None,
    };
    let definition: ConnectorDefinition = serde_json::from_value(json!({
        "display_name": "ToolExecutor connector fixture",
        "auth": [{"type": "none"}],
        "actions": [{
            "id": "create_record",
            "description": "Create one reviewed fixture record.",
            "contract": {
                    "method": "POST",
                    "path_template": "/v1/records/{record_id}",
                    "path_inputs": [{
                        "placeholder": "record_id",
                        "input_pointer": "/record_id"
                    }],
                    "body_input": {"input_pointer": "/payload"},
                    "upstream_idempotency_header": upstream_idempotency_header,
                    "response_pointer": "/data",
                    "max_request_bytes": 4096,
                    "max_response_bytes": 4096,
                    "connect_timeout_ms": 1000,
                    "total_timeout_ms": 5000,
                    "policy": {
                        "input_schema": {
                            "type": "object",
                            "required": ["record_id", "payload"],
                            "properties": {
                                "record_id": {"type": "string"},
                                "payload": {
                                    "type": "object",
                                    "required": ["name"],
                                    "properties": {"name": {"type": "string"}},
                                    "additionalProperties": false
                                }
                            },
                            "additionalProperties": false
                        },
                        "output_schema": {
                            "type": "object",
                            "required": ["accepted"],
                            "properties": {"accepted": {"type": "boolean"}},
                            "additionalProperties": false
                        },
                        "data_classes": ["pii"],
                        "idempotency": idempotency
                    }
                }
        }]
    }))
    .context("deserialize reviewed connector definition")?;
    let document = ArtifactDocument {
        api_version: "moa.artifact/v1".to_string(),
        kind: ArtifactKind::Connector,
        metadata: ArtifactMetadata {
            name: format!("tool-executor-connector-{}", Uuid::now_v7().simple()),
            description: "Published connector used by the ToolExecutor service E2E.".to_string(),
            tags: Vec::new(),
            version: None,
        },
        status: ArtifactStatus::Draft,
        definition: ArtifactDefinition::Connector(definition.clone()),
        ui: ArtifactUi::default(),
        reference_resolutions: Vec::new(),
    };
    let source_text = document.to_json().context("serialize connector artifact")?;
    let scope = ActionRuleScope::Tenant {
        tenant_id: identity.tenant_id,
    };
    let registry = ArtifactRegistry::new(pool.clone());
    let draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "json",
                source_text: source_text.as_bytes(),
                files: &[],
            },
        )
        .await
        .context("persist connector artifact draft")?;
    let published_report = validate_for_status(&document, ArtifactStatus::Published);
    if !published_report.is_ok() {
        bail!("connector artifact should validate for publication: {published_report:?}");
    }
    let published = registry
        .publish_unserved_revision(&scope, draft.revision_uid, &published_report)
        .await
        .context("publish reviewed connector artifact")?;

    let repository = PostgresConnectionRepository::new(pool.clone());
    let connection = repository
        .create(NewConnectorConnection {
            connection_id,
            tenant_id: identity.tenant_id,
            display_name: "ToolExecutor fixture account".to_string(),
            definition_ref: ConnectionDefinitionRef::Artifact {
                artifact_uid: published.artifact_uid,
                revision_uid: published.revision_uid,
            },
            origin: Some(origin.parse().context("parse connector fixture origin")?),
            non_secret_config: json!({}),
            created_by_identity_id: Some(identity.id),
            owner_identity_id: identity.id,
        })
        .await
        .context("create tenant connector connection")?;
    let action = definition
        .actions
        .first()
        .context("reviewed connector should declare one action")?;
    let compiled_contract = CompiledOperationContract::compile(&definition, action)
        .context("compile reviewed connector action")?;
    let contract_hash = compiled_contract
        .hash()
        .context("hash reviewed connector action")?;
    let binding_id = InstalledActionBindingId(Uuid::now_v7());
    let next_generation = connection
        .generation
        .next()
        .context("advance fixture connection generation")?;
    repository
        .activate(ConnectionActivation {
            tenant_id: identity.tenant_id,
            connection_id,
            expected_generation: connection.generation,
            bindings: vec![InstalledActionBinding {
                binding_id,
                tenant_id: identity.tenant_id,
                connection_id,
                connection_generation: next_generation,
                action_id: action.id.clone(),
                compiled_contract,
                contract_hash,
                governed_contract_revision: "fixture/create-record/v1".to_string(),
                minimum_effect,
                enabled: true,
            }],
        })
        .await
        .context("activate exact connector generation")?;

    Ok(SeededConnector {
        connection_id,
        artifact_uid: published.artifact_uid,
        revision_uid: published.revision_uid,
    })
}

fn connector_agent_context(connector: &SeededConnector) -> AgentContext {
    let snapshot = AgentPolicySnapshot {
        action_policy: AgentActionPolicy {
            connector_bindings: vec![AgentConnectorBinding {
                connector_ref: "connector://fixture-records".to_string(),
                connection_id: connector.connection_id,
                artifact_uid: connector.artifact_uid,
                revision_uid: connector.revision_uid,
            }],
            ..AgentActionPolicy::default()
        },
        ..AgentPolicySnapshot::default()
    };
    let mut context = AgentContext::system_default();
    context.policy_snapshot =
        serde_json::to_value(snapshot).expect("fixture connector agent policy should serialize");
    context
}

struct PreparedConnectorTurn {
    session_id: moa_core::types::identifiers::SessionId,
    review_id: Uuid,
}

fn connector_recovery_script(
    tool_name: &str,
    tool_call_id: ToolCallId,
    objective: &str,
) -> serde_json::Value {
    json!({
        "default": {
            "completion": {"content": "unexpected connector recovery fallback", "tool_calls": []}
        },
        "keyed": [
            {
                "match": "You classify one user turn into MOA's public execution decision.",
                "completion": {
                    "content": json!({
                        "label": "execute",
                        "strategy": "inline",
                        "rationale": "The connector call fits a bounded recovery scenario.",
                        "confidence_bps": 10_000,
                        "missing_inputs": []
                    }).to_string(),
                    "tool_calls": []
                }
            },
            {
                "match": "manual_reconciliation_required",
                "completion": {
                    "content": "The connector outcome requires manual reconciliation.",
                    "tool_calls": []
                }
            },
            {
                "match": "accepted",
                "completion": {
                    "content": "The connector operation completed.",
                    "tool_calls": []
                }
            },
            {
                "match": "action_review_continuation",
                "completion": {
                    "content": "The connector outcome requires manual reconciliation.",
                    "tool_calls": []
                }
            },
            {
                "match": "pending tenant admin review",
                "completion": {
                    "content": "The connector action is awaiting tenant review.",
                    "tool_calls": []
                }
            },
            {
                "match": objective,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": tool_name,
                        "id": tool_call_id.to_string(),
                        "input": {
                            "record_id": objective,
                            "payload": {"name": "recovery matrix"}
                        }
                    }]
                }
            }
        ]
    })
}

async fn start_connector_recovery_turn(
    fixture: &OrchestratorTestFixture,
    test: &IsolatedTest<'_>,
    connector: &SeededConnector,
    tool_call_id: ToolCallId,
    objective: &str,
) -> Result<PreparedConnectorTurn> {
    let identity = test
        .client()
        .identity()
        .context("connector recovery fixture must carry identity headers")?;
    fixture
        .grant_default_connector_connection_use(connector.connection_id)
        .await?;
    fixture
        .grant_default_tenant_admin(identity.tenant_id)
        .await?;
    let tool_name =
        moa_hands::core::installed_connector_tool_name(connector.connection_id, "create_record")?;
    test.client()
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id: identity.tenant_id,
                contact_id: None,
                tool_name,
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("deterministic connector recovery fixture".to_string()),
            },
        )
        .await
        .context("seed connector recovery Allow policy")?;
    let session_id = test
        .create_session_with_agent_context(
            "connector-recovery",
            ModelId::new("scripted-loadtest"),
            CallOrigin::Production,
            connector_agent_context(connector),
        )
        .await?;
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: objective.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                resource_budget: Default::default(),
                execution_template: None,
            },
            None,
        )
        .await
        .context("start connector recovery through public Session ingress")?;
    started
        .turn_id
        .context("connector recovery turn should start immediately")?;
    Ok(PreparedConnectorTurn {
        session_id,
        review_id: tool_call_id.0,
    })
}

async fn wait_for_pending_connector_review(
    client: &TestApiClient,
    tenant_id: TenantId,
    review_id: Uuid,
) -> Result<ActionReviewSummary> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let pending: Vec<ActionReviewSummary> = client
            .post_call(
                "/ActionReviews/list_pending",
                &ListActionReviewsRequest { tenant_id },
            )
            .await?;
        if let Some(review) = pending.into_iter().find(|review| review.id == review_id) {
            return Ok(review);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("connector action review {review_id} did not become pending");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn approve_connector_review(
    client: &TestApiClient,
    tenant_id: TenantId,
    review_id: Uuid,
) -> tokio::task::JoinHandle<Result<()>> {
    let client = client.clone();
    tokio::spawn(async move {
        client
            .post_void(
                "/ActionReviews/decide",
                &DecideActionReviewRequest {
                    tenant_id,
                    review_id,
                    decision: ActionReviewDecisionKind::Cleared,
                    reason: None,
                },
            )
            .await
            .context("clear connector action review")
    })
}

async fn await_connector_review_continuation(
    client: &TestApiClient,
    session_id: moa_core::types::identifiers::SessionId,
    review_id: Uuid,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let events = client.get_events(session_id, EventRange::all()).await?;
        if let Some(turn_id) = events.iter().find_map(|record| match &record.event {
            Event::ActionReviewContinuationRequested {
                review_id: id,
                turn_id,
                ..
            } if *id == review_id => Some(turn_id.clone()),
            _ => None,
        }) {
            return Ok(turn_id);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("connector review {review_id} did not schedule a continuation turn");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn executed_connector_tool_call_id(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
) -> Result<ToolCallId> {
    let value: String = sqlx::query_scalar(
        "SELECT tool_call_id FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 ORDER BY started_at DESC LIMIT 1",
    )
    .bind(tenant_id.0)
    .fetch_one(pool)
    .await
    .context("load reviewed connector execution tool-call id")?;
    Ok(ToolCallId(Uuid::parse_str(&value).context(
        "parse reviewed connector execution tool-call id",
    )?))
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, OpenFGA, and integration connector loopback"]
async fn execution_catalog_does_not_invent_tenant_connector_authority_service_e2e() -> Result<()> {
    // Pins: tenant operator and connection Use alone do not invent connector
    // authority for the tenant-wide public catalog. An installed connector
    // overlay requires the exact agent connector binding.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let connector_api = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}})),
    ]))
    .await
    .context("start isolated connector API")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url())
        .await
        .context("connect to connector service E2E Postgres")?;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut identity = test_user_identity();
    let tenant_id = TenantId::new();
    identity.tenant_id = tenant_id;
    grant_tenant_operator(&identity, tenant_id).await?;
    let connector = seed_published_http_connector(
        &pool,
        &identity,
        connector_api.origin(),
        IdempotencyClass::Idempotent,
        ConnectorConnectionId::new(),
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    grant_connector_connection_use(&identity, connector.connection_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_test_admin_url(), endpoint_url.as_str()).await?;
        let ingress = restate_ingress_url();
        let response = with_identity(
            reqwest::Client::new().post(format!(
                "{}/restate/call/Execution/list_capabilities",
                ingress.as_str().trim_end_matches("/")
            )),
            &identity,
        )
        .json(&json!({"tenant_id": tenant_id}))
        .send()
        .await
        .context("list public Execution capabilities")?
        .error_for_status()
        .context("Execution/list_capabilities should succeed")?
        .json::<CapabilitiesListResponse>()
        .await
        .context("decode public execution capability catalog")?;

        assert!(
            !response.catalog.capabilities.is_empty(),
            "public Execution catalog must retain deployment capabilities"
        );
        let installed = response.catalog.capabilities.iter().any(|capability| {
            matches!(
                &capability.source,
                CapabilitySource::InstalledConnectorAction { connection_id, .. }
                    if *connection_id == connector.connection_id
            )
        });
        assert!(
            !installed,
            "tenant-wide catalog must not invent installed connector authority"
        );
        assert_eq!(connector_api.controller().requests().len(), 0);
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    result
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, OpenFGA, and integration connector loopback"]
async fn recovery_matrix_idempotent_connector_replays_same_key_before_journal_commit_service_e2e()
-> Result<()> {
    // Pins: a crash while the first keyed upstream response is blocked may replay transport, but
    // the stable upstream idempotency key keeps one logical effect and one durable result.
    const OBJECTIVE: &str = "record/idempotent-before-journal";
    let connection_id = ConnectorConnectionId::new();
    let tool_call_id = ToolCallId::new();
    let tool_name = moa_hands::core::installed_connector_tool_name(connection_id, "create_record")?;
    let connector_api = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}}))
            .with_delay_before_headers(Duration::from_secs(300)),
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}})),
    ]))
    .await
    .context("start idempotent pre-journal connector API")?;
    let fixture = OrchestratorTestFixture::with_script_and_env(
        connector_recovery_script(&tool_name, tool_call_id, OBJECTIVE),
        vec![(
            "MOA_INTEGRATION_CONNECTOR_LOOPBACK_ENABLED".to_string(),
            "1".to_string(),
        )],
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&fixture.postgres_url)
        .await
        .context("connect to idempotent pre-journal E2E Postgres")?;
    let identity = test
        .client()
        .identity()
        .context("connector recovery fixture must carry identity")?
        .clone();
    let tenant_id = identity.tenant_id;
    let connector = seed_published_http_connector(
        &pool,
        &identity,
        connector_api.origin(),
        IdempotencyClass::Idempotent,
        connection_id,
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    let prepared =
        start_connector_recovery_turn(&fixture, &test, &connector, tool_call_id, OBJECTIVE).await?;
    let pending =
        wait_for_pending_connector_review(test.client(), tenant_id, prepared.review_id).await?;
    assert_eq!(pending.tool_call_id, tool_call_id);
    let approval = approve_connector_review(test.client(), tenant_id, prepared.review_id);

    connector_api
        .controller()
        .wait_for_requests(1, Duration::from_secs(10))
        .await
        .context("wait for first idempotent upstream transport")?;
    let executed_tool_call_id = executed_connector_tool_call_id(&pool, tenant_id).await?;
    let transmitting: (String, Option<String>) = sqlx::query_as(
        "SELECT state, upstream_idempotency_key \
         FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("load idempotent invocation before journal commit")?;
    assert_eq!(transmitting.0, "transmitting");
    let expected_idempotency_key = executed_tool_call_id.to_string();
    assert_eq!(
        transmitting.1.as_deref(),
        Some(expected_idempotency_key.as_str())
    );

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("restart before idempotent run result journal commit")?;

    let requests = connector_api
        .controller()
        .wait_for_requests(2, Duration::from_secs(30))
        .await
        .context("wait for keyed idempotent transport replay")?;
    assert_eq!(
        requests.len(),
        2,
        "pre-commit recovery should replay transport once"
    );
    let expected_key = vec![FixtureCapturedHeaderValue::Visible(
        executed_tool_call_id.to_string(),
    )];
    assert_eq!(
        requests[0].headers.get("idempotency-key"),
        Some(&expected_key)
    );
    assert_eq!(
        requests[1].headers.get("idempotency-key"),
        Some(&expected_key)
    );
    assert_eq!(
        connector_api.controller().effect_count(),
        1,
        "upstream idempotency must collapse both transports into one logical effect"
    );
    let effects = connector_api.controller().effects();
    assert_eq!(
        effects[0].idempotency_key.as_deref(),
        Some(expected_idempotency_key.as_str())
    );
    assert_eq!(requests[0].logical_effect_order, 1);
    assert!(!requests[0].is_replay);
    assert_eq!(requests[1].logical_effect_order, 1);
    assert!(requests[1].is_replay);

    approval.await.context("join connector review approval")??;
    let continuation_turn_id =
        await_connector_review_continuation(test.client(), prepared.session_id, prepared.review_id)
            .await?;
    let outcome = test
        .client()
        .session(prepared.session_id.to_string())
        .await_turn_outcome(
            &continuation_turn_id,
            Duration::from_secs(30),
            Duration::from_millis(100),
        )
        .await
        .context("idempotent pre-journal Session turn did not recover")?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let invocation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2 AND state = 'succeeded'",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("count idempotent pre-journal durable results")?;
    assert_eq!(
        invocation_count, 1,
        "recovery must persist one connector result"
    );
    let events = test
        .client()
        .get_events(prepared.session_id, EventRange::all())
        .await?;
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::ToolResult { tool_id, .. } if tool_id == executed_tool_call_id
            ))
            .count(),
        1,
        "idempotent replay must append one product ToolResult"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, OpenFGA, and integration connector loopback"]
async fn recovery_matrix_idempotent_connector_does_not_resend_after_journal_commit_service_e2e()
-> Result<()> {
    // Pins: reaching the blocked finalization update proves the safe response is already in the
    // Restate journal; a crash there replays finalization without another upstream request.
    const OBJECTIVE: &str = "record/idempotent-after-journal";
    let connection_id = ConnectorConnectionId::new();
    let tool_call_id = ToolCallId::new();
    let tool_name = moa_hands::core::installed_connector_tool_name(connection_id, "create_record")?;
    let connector_api = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}})),
    ]))
    .await
    .context("start idempotent post-journal connector API")?;
    let fixture = OrchestratorTestFixture::with_script_and_env(
        connector_recovery_script(&tool_name, tool_call_id, OBJECTIVE),
        vec![(
            "MOA_INTEGRATION_CONNECTOR_LOOPBACK_ENABLED".to_string(),
            "1".to_string(),
        )],
    )
    .await?;
    let test = fixture.isolated().await;
    let database_url = fixture.postgres_url.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url)
        .await
        .context("connect to idempotent post-journal E2E Postgres")?;
    let identity = test
        .client()
        .identity()
        .context("connector recovery fixture must carry identity")?
        .clone();
    let tenant_id = identity.tenant_id;
    let connector = seed_published_http_connector(
        &pool,
        &identity,
        connector_api.origin(),
        IdempotencyClass::Idempotent,
        connection_id,
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    // This fixture owns an isolated Postgres container and creates exactly one
    // connector invocation, so the finalization trigger cannot catch another test's row.
    let mut barrier = ConnectorFinalizationBarrier::install(&database_url, &pool).await?;
    let prepared =
        start_connector_recovery_turn(&fixture, &test, &connector, tool_call_id, OBJECTIVE).await?;
    let pending =
        wait_for_pending_connector_review(test.client(), tenant_id, prepared.review_id).await?;
    assert_eq!(pending.tool_call_id, tool_call_id);
    let approval = approve_connector_review(test.client(), tenant_id, prepared.review_id);

    let requests = connector_api
        .controller()
        .wait_for_requests(1, Duration::from_secs(10))
        .await
        .context("wait for sole post-journal upstream effect")?;
    assert_eq!(requests.len(), 1);
    let executed_tool_call_id = executed_connector_tool_call_id(&pool, tenant_id).await?;
    barrier
        .wait_for_blocked_update(&pool)
        .await
        .context("wait for post-journal connector finalization boundary")?;
    let transmitting: String = sqlx::query_scalar(
        "SELECT state FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("load connector ledger while finalization is blocked")?;
    assert_eq!(transmitting, "transmitting");

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("restart after journal commit before connector finalization")?;
    assert_eq!(
        connector_api.controller().request_count(),
        1,
        "journaled connector output must suppress upstream replay"
    );
    assert_eq!(connector_api.controller().effect_count(), 1);
    barrier.release().await?;

    approval.await.context("join connector review approval")??;
    let continuation_turn_id =
        await_connector_review_continuation(test.client(), prepared.session_id, prepared.review_id)
            .await?;
    let recovered = test
        .client()
        .session(prepared.session_id.to_string())
        .await_turn_outcome(
            &continuation_turn_id,
            Duration::from_secs(30),
            Duration::from_millis(100),
        )
        .await
        .context("post-journal connector Session turn did not recover");
    let cleanup = ConnectorFinalizationBarrier::remove(&pool).await;
    let recovered = recovered?;
    cleanup?;
    assert_eq!(recovered.kind, TurnOutcomeKind::Completed);
    assert_eq!(connector_api.controller().request_count(), 1);
    assert_eq!(connector_api.controller().effect_count(), 1);

    let repository = PostgresConnectionRepository::new(pool.clone());
    let invocation_uid: Uuid = sqlx::query_scalar(
        "SELECT invocation_uid FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("load post-journal invocation id")?;
    let invocation = repository
        .load_invocation(tenant_id, ConnectorInvocationId(invocation_uid))
        .await?
        .context("post-journal invocation should remain durable")?;
    assert_eq!(invocation.state, ConnectorInvocationState::Succeeded);
    let events = test
        .client()
        .get_events(prepared.session_id, EventRange::all())
        .await?;
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::ToolResult { tool_id, .. } if tool_id == executed_tool_call_id
            ))
            .count(),
        1,
        "post-journal recovery must append one product ToolResult"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, OpenFGA, and integration connector loopback"]
async fn recovery_matrix_non_idempotent_connector_crash_records_unknown_outcome_without_resend_service_e2e()
-> Result<()> {
    // Pins: once a non-idempotent installed connector request reaches the upstream boundary,
    // an orchestrator SIGKILL makes the replay consume the durable transmitting reservation as
    // UnknownOutcome instead of sending the request again.
    const OBJECTIVE: &str = "record/non-idempotent-unknown-outcome";
    let connection_id = ConnectorConnectionId::new();
    let tool_call_id = ToolCallId::new();
    let tool_name = moa_hands::core::installed_connector_tool_name(connection_id, "create_record")?;
    let connector_api = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
        FixtureConnectorResponse::json(json!({"data": {"accepted": true}}))
            .with_delay_before_headers(Duration::from_secs(300)),
    ]))
    .await
    .context("start blocked non-idempotent connector API")?;
    let fixture = OrchestratorTestFixture::with_script_and_env(
        connector_recovery_script(&tool_name, tool_call_id, OBJECTIVE),
        vec![(
            "MOA_INTEGRATION_CONNECTOR_LOOPBACK_ENABLED".to_string(),
            "1".to_string(),
        )],
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&fixture.postgres_url)
        .await
        .context("connect to connector recovery E2E Postgres")?;
    let identity = test
        .client()
        .identity()
        .context("connector recovery fixture must carry identity")?
        .clone();
    let tenant_id = identity.tenant_id;
    let connector = seed_published_http_connector(
        &pool,
        &identity,
        connector_api.origin(),
        IdempotencyClass::NonIdempotent,
        connection_id,
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    let prepared =
        start_connector_recovery_turn(&fixture, &test, &connector, tool_call_id, OBJECTIVE).await?;
    let pending =
        wait_for_pending_connector_review(test.client(), tenant_id, prepared.review_id).await?;
    assert_eq!(pending.tool_call_id, tool_call_id);
    let approval = approve_connector_review(test.client(), tenant_id, prepared.review_id);

    let requests = connector_api
        .controller()
        .wait_for_requests(1, Duration::from_secs(10))
        .await
        .context("wait for the sole non-idempotent upstream effect")?;
    assert_eq!(
        requests.len(),
        1,
        "upstream effect must be applied exactly once"
    );
    assert!(
        !requests[0].headers.contains_key("idempotency-key"),
        "non-idempotent action must not invent an upstream replay key"
    );
    let executed_tool_call_id = executed_connector_tool_call_id(&pool, tenant_id).await?;

    let before_crash = sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        "SELECT invocation_uid, state, upstream_idempotency_key \
         FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("load connector invocation at the exact transmitting barrier")?;
    assert_eq!(before_crash.1, "transmitting");
    assert_eq!(before_crash.2, None);

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("restart at connector transmitting barrier")?;

    approval.await.context("join connector review approval")??;
    let continuation_turn_id =
        await_connector_review_continuation(test.client(), prepared.session_id, prepared.review_id)
            .await?;
    let outcome = test
        .client()
        .session(prepared.session_id.to_string())
        .await_turn_outcome(
            &continuation_turn_id,
            Duration::from_secs(30),
            Duration::from_millis(100),
        )
        .await
        .context("non-idempotent connector Session turn did not recover")?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert!(
        outcome.message.contains("manual reconciliation"),
        "unexpected non-idempotent continuation outcome: {outcome:?}"
    );

    let repository = PostgresConnectionRepository::new(pool.clone());
    let invocation = repository
        .load_invocation(tenant_id, ConnectorInvocationId(before_crash.0))
        .await
        .context("load typed connector invocation after replay")?
        .context("connector invocation should remain durable after restart")?;
    assert_eq!(invocation.state, ConnectorInvocationState::UnknownOutcome);
    assert_eq!(
        invocation.error_metadata,
        Some(json!({
            "code": "effect_journal_ambiguous",
            "manual_reconciliation_required": true
        }))
    );
    assert_eq!(
        connector_api.controller().request_count(),
        1,
        "recovery must consume the durable reservation without a blind resend"
    );
    assert_eq!(connector_api.controller().effect_count(), 1);
    let invocation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.connector_action_invocations \
         WHERE tenant_id = $1 AND tool_call_id = $2",
    )
    .bind(tenant_id.0)
    .bind(executed_tool_call_id.to_string())
    .fetch_one(&pool)
    .await
    .context("count durable connector effects after replay")?;
    assert_eq!(
        invocation_count, 1,
        "replay must retain one durable effect record"
    );
    let events = test
        .client()
        .get_events(prepared.session_id, EventRange::all())
        .await?;
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ToolError { tool_id: id, error, .. }
                    if *id == executed_tool_call_id
                        && error.contains("manual reconciliation required")
            ))
            .count(),
        1,
        "public Session path must durably expose one reconciliation-required tool error"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the local Restate/Postgres/OpenFGA/Valkey fixture"]
async fn direct_tool_executor_ingress_is_rejected() -> Result<()> {
    // Pins: ToolExecutor is reachable only from product handlers through
    // service-to-service invocation. Public tool success is covered through
    // Session in execution_run_service_e2e::routing.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_test_admin_url(), endpoint_url.as_str()).await?;
        let ingress = restate_ingress_url();
        let tenant_id = TenantId::new();
        let mut identity = test_user_identity();
        identity.tenant_id = tenant_id;
        grant_tenant_operator(&identity, tenant_id).await?;
        let client = reqwest::Client::new();
        let session_id = SessionId::new();
        grant_session_participant(&identity, session_id).await?;
        assert_eq!(
            with_identity(
                client.post(format!(
                    "{}/restate/call/Session/{session_id}/status",
                    ingress.trim_end_matches('/')
                )),
                &identity,
            )
            .send()
            .await
            .context("call public Session status handler")?
            .error_for_status()
            .context("Session/status should succeed")?
            .json::<SessionStatus>()
            .await
            .context("decode public Session status")?,
            SessionStatus::Created,
            "normal public Session ingress must remain available"
        );

        let invocations_before = client
            .post(format!(
                "{}/query",
                restate_test_admin_url().trim_end_matches('/')
            ))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({
                "query": "SELECT id FROM sys_invocation WHERE target_service_name = 'ToolExecutor' ORDER BY id"
            }))
            .send()
            .await
            .context("query Restate for existing ToolExecutor invocations")?
            .error_for_status()
            .context("Restate invocation baseline query should succeed")?
            .json::<Value>()
            .await
            .context("decode Restate invocation baseline query")?;

        let response = client
            .post(format!(
                "{}/restate/call/ToolExecutor/list_tools",
                ingress.trim_end_matches('/')
            ))
            .json(&json!({}))
            .send()
            .await
            .context("probe private ToolExecutor ingress")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read private ToolExecutor rejection")?;
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "unexpected private ToolExecutor rejection body: {body}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body)
                .context("decode private ToolExecutor rejection")?,
            json!({"message": "the invoked service is not public"}),
            "Restate must reject the request at its private-ingress boundary"
        );
        let invocations = client
            .post(format!(
                "{}/query",
                restate_test_admin_url().trim_end_matches('/')
            ))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({
                "query": "SELECT id FROM sys_invocation WHERE target_service_name = 'ToolExecutor' ORDER BY id"
            }))
            .send()
            .await
            .context("query Restate for rejected ToolExecutor invocations")?
            .error_for_status()
            .context("Restate invocation query should succeed")?
            .json::<Value>()
            .await
            .context("decode Restate invocation query")?;
        assert_eq!(
            invocations.get("rows"),
            invocations_before.get("rows"),
            "private ingress rejection must not create a new ToolExecutor invocation: before={invocations_before}, after={invocations}"
        );
        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    result
}
