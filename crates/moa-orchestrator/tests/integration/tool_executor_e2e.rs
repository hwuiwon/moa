//! End-to-end tool executor coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::connector::{ConnectorDefinition, RuntimeConnectorDefinitionV1};
use moa_artifacts::document::{
    ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactMetadata, ArtifactStatus,
    ArtifactUi,
};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_artifacts::validation::validate_for_status;
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, InstalledActionBinding,
    InstalledActionBindingId,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionRepository, NewConnectorConnection,
    PostgresConnectionRepository,
};
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::agent::{
    AgentActionPolicy, AgentConnectorBinding, AgentContext, AgentPolicySnapshot,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_core::{
    events::Event, traits::Identity, types::events_stream::EventRange,
    types::identifiers::ToolCallId, types::tools::SecuredToolOutput, types::tools::ToolCallRequest,
};
use moa_hands::{PinnedToolOwner, ToolCatalogPin};
use moa_test_support::fixture_connector_api::{
    FixtureCapturedHeaderValue, FixtureConnectorApi, FixtureConnectorResponse,
    FixtureConnectorScript,
};
use moa_test_support::postgres::test_database_url;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::grant_connector_connection_use;
use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_operator, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    append_event_request, get_events_request, storage_partition_id_from_meta, test_session_meta,
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

fn tool_request(
    tool_call_id: ToolCallId,
    tool_name: &str,
    input: serde_json::Value,
    session_id: moa_core::types::identifiers::SessionId,
    identity: &Identity,
    catalog: &ToolCatalogPin,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        caller_identity: identity.clone(),
        provider_tool_use_id: None,
        tool_name: tool_name.to_string(),
        expected_tool_contract_revision: catalog
            .contract_revision(tool_name)
            .expect("E2E tool should have an activated contract")
            .to_owned(),
        input,
        active_canary: None,
        session_id,
        trusted_sandbox_manifest: None,
        worker_id: None,
        resource_budget: Default::default(),
    }
}

fn tool_request_with_provider_id(
    tool_call_id: ToolCallId,
    provider_tool_use_id: Option<&str>,
    tool_name: &str,
    input: serde_json::Value,
    session_id: moa_core::types::identifiers::SessionId,
    identity: &Identity,
    catalog: &ToolCatalogPin,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        caller_identity: identity.clone(),
        provider_tool_use_id: provider_tool_use_id.map(ToOwned::to_owned),
        tool_name: tool_name.to_string(),
        expected_tool_contract_revision: catalog
            .contract_revision(tool_name)
            .expect("E2E tool should have an activated contract")
            .to_owned(),
        input,
        active_canary: None,
        session_id,
        trusted_sandbox_manifest: None,
        worker_id: None,
        resource_budget: Default::default(),
    }
}

async fn activated_tool_catalog(
    client: &reqwest::Client,
    ingress: &str,
    session_id: moa_core::types::identifiers::SessionId,
    identity: &Identity,
) -> Result<ToolCatalogPin> {
    with_identity(
        client.post(format!(
            "{}/restate/call/ToolExecutor/activated_tool_catalog",
            ingress.trim_end_matches('/')
        )),
        identity,
    )
    .json(&json!({
        "session_id": session_id,
        "caller_identity": identity,
    }))
    .send()
    .await
    .context("load activated tool catalog")?
    .error_for_status()
    .context("activated tool catalog should load")?
    .json()
    .await
    .context("deserialize activated tool catalog")
}

struct SeededConnector {
    connection_id: ConnectorConnectionId,
    binding_id: InstalledActionBindingId,
    connection_generation: u64,
    artifact_uid: Uuid,
    revision_uid: Uuid,
}

async fn seed_published_http_connector(
    pool: &sqlx::PgPool,
    identity: &Identity,
    origin: &str,
) -> Result<SeededConnector> {
    let definition: RuntimeConnectorDefinitionV1 = serde_json::from_value(json!({
        "definition_version": "v1",
        "display_name": "ToolExecutor connector fixture",
        "runtime": {"type": "constrained_http"},
        "auth": [{"type": "none"}],
        "actions": [{
            "id": "create_record",
            "description": "Create one reviewed fixture record.",
            "binding": {
                "type": "http",
                "contract": {
                    "method": "POST",
                    "path_template": "/v1/records/{record_id}",
                    "path_inputs": [{
                        "placeholder": "record_id",
                        "input_pointer": "/record_id"
                    }],
                    "body_input": {"input_pointer": "/payload"},
                    "upstream_idempotency_header": "idempotency-key",
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
                        "action_class": "external_write",
                        "risk_level": "high",
                        "minimum_effect": "admin_review",
                        "idempotency": "idempotent"
                    }
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
        definition: ArtifactDefinition::Connector(ConnectorDefinition::RuntimeV1(
            definition.clone(),
        )),
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
    let connection_id = ConnectorConnectionId::new();
    let connection = repository
        .create(NewConnectorConnection {
            connection_id,
            tenant_id: identity.tenant_id,
            display_name: "ToolExecutor fixture account".to_string(),
            definition_ref: ConnectionDefinitionRef::Artifact {
                artifact_uid: published.artifact_uid,
                revision_uid: published.revision_uid,
            },
            non_secret_config: json!({"origin": origin}),
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
    let activated = repository
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
                minimum_effect: action.policy().minimum_effect,
                enabled: true,
            }],
        })
        .await
        .context("activate exact connector generation")?;

    Ok(SeededConnector {
        connection_id,
        binding_id,
        connection_generation: activated.generation.get(),
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

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, OpenFGA, and integration connector loopback"]
async fn connector_action_catalog_authorization_dispatch_and_ledger_are_tenant_isolated_service_e2e()
-> Result<()> {
    // Pins: one published, agent-bound connector action crosses the real
    // ToolExecutor Restate journal exactly once, finalizes its durable ledger,
    // and remains unavailable to a caller from another tenant.
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

    let mut meta = test_session_meta("tool-executor-connector-service-e2e");
    let tenant_id = meta.tenant_id;
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let connector = seed_published_http_connector(&pool, &identity, connector_api.origin()).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress_url = restate_ingress_url();
        let ingress = ingress_url.as_str();
        meta.agent_context = Some(connector_agent_context(&connector));
        let storage_partition_id = storage_partition_id_from_meta(&meta);
        grant_tenant_operator(&identity, &storage_partition_id).await?;
        grant_connector_connection_use(&identity, connector.connection_id).await?;

        let create_response = with_identity(
            client.post(format!(
                "{}/restate/call/SessionStore/create_session",
                ingress.trim_end_matches('/')
            )),
            &identity,
        )
        .json(&meta)
        .send()
        .await
        .context("create connector-bound session via Restate ingress")?
        .error_for_status()
        .context("connector-bound session should be created")?;
        let session_id = create_response
            .json::<moa_core::types::identifiers::SessionId>()
            .await
            .context("deserialize connector-bound session id")?;
        grant_session_participant(&identity, session_id).await?;

        let catalog = activated_tool_catalog(&client, ingress, session_id, &identity).await?;
        let installed = catalog
            .tools
            .iter()
            .filter_map(|tool| match &tool.owner {
                PinnedToolOwner::InstalledConnectorAction {
                    connector_ref,
                    connection_id,
                    binding_id,
                    connection_generation,
                    definition_artifact_uid,
                    definition_revision_uid,
                    action_id,
                    contract_hash,
                    governed_contract_revision,
                    minimum_effect,
                } => Some((
                    tool,
                    connector_ref,
                    connection_id,
                    binding_id,
                    connection_generation,
                    definition_artifact_uid,
                    definition_revision_uid,
                    action_id,
                    contract_hash,
                    governed_contract_revision,
                    minimum_effect,
                )),
                PinnedToolOwner::BuiltIn
                | PinnedToolOwner::Hand
                | PinnedToolOwner::Connector { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(installed.len(), 1, "agent binding should expose one connector action");
        let (
            pinned,
            connector_ref,
            connection_id,
            binding_id,
            connection_generation,
            definition_artifact_uid,
            definition_revision_uid,
            action_id,
            _contract_hash,
            governed_contract_revision,
            minimum_effect,
        ) = installed[0];
        assert_eq!(connector_ref, "connector://fixture-records");
        assert_eq!(*connection_id, connector.connection_id);
        assert_eq!(*binding_id, connector.binding_id.0);
        assert_eq!(*connection_generation, connector.connection_generation);
        assert_eq!(*definition_artifact_uid, connector.artifact_uid);
        assert_eq!(*definition_revision_uid, connector.revision_uid);
        assert_eq!(action_id, "create_record");
        assert_eq!(governed_contract_revision, "fixture/create-record/v1");
        assert_eq!(minimum_effect.as_str(), "admin_review");

        let tool_call_id = ToolCallId::new();
        let input = json!({
            "record_id": "record/42",
            "payload": {"name": "service e2e"}
        });
        assert!(
            input.get("origin").is_none() && input.get("auth").is_none(),
            "model input must contain neither destination nor credentials"
        );
        let request = tool_request(
            tool_call_id,
            &pinned.tool,
            input,
            session_id,
            &identity,
            &catalog,
        );
        let output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&request)
            .send()
            .await
            .context("invoke installed connector through ToolExecutor")?
            .error_for_status()
            .context("installed connector invocation should succeed")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize secured connector output")?;
        assert!(output.assessment.is_safe());
        assert!(!output.safe_output.is_error);
        assert_eq!(
            output.safe_output.structured,
            Some(json!({"accepted": true}))
        );

        let requests = connector_api
            .controller()
            .wait_for_requests(1, Duration::from_secs(2))
            .await
            .context("wait for one connector HTTP dispatch")?;
        assert_eq!(requests.len(), 1, "connector action must dispatch exactly once");
        assert_eq!(requests[0].arrival_order, 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/v1/records/record%2F42");
        assert_eq!(
            requests[0].json_body,
            Some(json!({"name": "service e2e"}))
        );
        assert_eq!(
            requests[0].headers.get("idempotency-key"),
            Some(&vec![FixtureCapturedHeaderValue::Visible(
                tool_call_id.to_string()
            )])
        );
        assert!(!requests[0].headers.contains_key("authorization"));

        let invocation = sqlx::query_as::<_, (String, Option<serde_json::Value>)>(
            "SELECT state, output_metadata FROM moa.connector_action_invocations \
             WHERE tenant_id = $1 AND tool_call_id = $2",
        )
        .bind(tenant_id.0)
        .bind(tool_call_id.to_string())
        .fetch_one(&pool)
        .await
        .context("load terminal connector invocation ledger")?;
        assert_eq!(invocation.0, "succeeded");
        let output_metadata = invocation
            .1
            .context("succeeded connector invocation should store output metadata")?;
        assert_eq!(output_metadata["assessment"]["class"], json!("safe"));
        assert!(
            output_metadata["secured_output_bytes"]
                .as_u64()
                .is_some_and(|bytes| bytes > 0),
            "terminal connector metadata should retain a positive secured output size"
        );

        let events = wait_for_tool_result_events(&client, ingress, &identity, session_id, 1).await?;
        let tool_calls = events
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::ToolCall { tool_id, .. } if *tool_id == tool_call_id)
            })
            .count();
        let tool_results = events
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::ToolResult { tool_id, .. } if *tool_id == tool_call_id)
            })
            .count();
        assert_eq!(tool_calls, 1, "ToolExecutor should persist one connector ToolCall");
        assert_eq!(tool_results, 1, "ToolExecutor should persist one connector ToolResult");

        let mut other_identity = test_user_identity();
        other_identity.tenant_id = TenantId::new();
        let denied_catalog = with_identity(
            client.post(format!(
                "{}/restate/call/ToolExecutor/activated_tool_catalog",
                ingress.trim_end_matches('/')
            )),
            &other_identity,
        )
        .json(&json!({
            "session_id": session_id,
            "caller_identity": other_identity,
        }))
        .send()
        .await
        .context("attempt cross-tenant connector catalog read")?;
        assert!(
            !denied_catalog.status().is_success(),
            "another tenant must not receive the connector catalog"
        );

        let mut cross_tenant_request = request.clone();
        cross_tenant_request.tool_call_id = ToolCallId::new();
        cross_tenant_request.caller_identity = other_identity;
        let denied_dispatch = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&cross_tenant_request)
            .send()
            .await
            .context("attempt cross-tenant connector dispatch")?;
        assert!(
            !denied_dispatch.status().is_success(),
            "another tenant must not dispatch the connector action"
        );
        assert_eq!(
            connector_api.controller().requests().len(),
            1,
            "cross-tenant denial must happen before another network send"
        );
        let invocation_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.connector_action_invocations \
             WHERE connection_uid = $1",
        )
        .bind(connector.connection_id.0)
        .fetch_one(&pool)
        .await
        .context("count connector invocation ledgers after cross-tenant refusal")?;
        assert_eq!(
            invocation_count, 1,
            "cross-tenant denial must happen before another ledger reservation"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires local restate-server and Postgres"]
async fn tool_executor_round_trip_through_restate() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let meta = test_session_meta("tool-executor-e2e");
        let storage_partition_id = storage_partition_id_from_meta(&meta);
        let mut identity = test_user_identity();
        identity.tenant_id = meta.tenant_id;
        grant_tenant_operator(&identity, &storage_partition_id).await?;

        let create_request = client.post(format!(
            "{}/restate/call/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::types::identifiers::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;
        let catalog = activated_tool_catalog(&client, ingress, session_id, &identity).await?;

        // Workers own their sandboxes; the root coordinator may only read
        // trusted skill package files. Scope the write/read pair to one worker
        // so the round trip exercises the worker sandbox path.
        let worker_id = "worker-tool-executor-e2e".to_string();
        let mut write_request = tool_request(
            ToolCallId::new(),
            "file_write",
            json!({
                "path": "note.txt",
                "content": "hello from tool executor"
            }),
            session_id,
            &identity,
            &catalog,
        );
        write_request.worker_id = Some(worker_id.clone());
        let write_output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&write_request)
            .send()
            .await
            .context("call ToolExecutor/file_write via restate ingress")?
            .error_for_status()
            .context("file_write should succeed")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize file_write output")?
            .safe_output;
        assert!(write_output.to_text().contains("note.txt"));

        let mut read_request = tool_request(
            ToolCallId::new(),
            "file_read",
            json!({ "path": "note.txt" }),
            session_id,
            &identity,
            &catalog,
        );
        read_request.worker_id = Some(worker_id.clone());
        let read_output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&read_request)
            .send()
            .await
            .context("call ToolExecutor/file_read via restate ingress")?
            .error_for_status()
            .context("file_read should succeed")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize file_read output")?
            .safe_output;
        let read_text = read_output.to_text();
        assert!(
            read_text.contains("hello from tool executor"),
            "file_read output missing written content; got: {read_text}"
        );

        // Pins: a root-scoped file_read without a trusted sandbox manifest is
        // denied instead of reaching a sandbox.
        let root_read_request = tool_request(
            ToolCallId::new(),
            "file_read",
            json!({ "path": "note.txt" }),
            session_id,
            &identity,
            &catalog,
        );
        let root_read_output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&root_read_request)
            .send()
            .await
            .context("call ToolExecutor/file_read at root scope via restate ingress")?
            .error_for_status()
            .context("root file_read call should return a tool output")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize root file_read output")?
            .safe_output;
        let root_read_text = root_read_output.to_text();
        assert!(
            root_read_text.contains("root coordinator"),
            "root-scoped file_read should be denied without a trusted manifest; got: {root_read_text}"
        );

        let bash_call_id = ToolCallId::new();
        let bash_request = tool_request(
            bash_call_id,
            "bash",
            json!({ "cmd": "printf hello-from-bash" }),
            session_id,
            &identity,
            &catalog,
        );
        let bash_output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&bash_request)
            .send()
            .await
            .context("call ToolExecutor/bash via restate ingress")?
            .error_for_status()
            .context("bash should succeed")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize bash output")?
            .safe_output;
        assert!(bash_output.to_text().contains("hello-from-bash"));

        let duplicate_response = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&bash_request)
            .send()
            .await
            .context("repeat bash tool call with same tool_call_id")?;
        let duplicate_status = duplicate_response.status();
        let duplicate_body = duplicate_response
            .text()
            .await
            .context("read duplicate bash error body")?;
        assert!(!duplicate_status.is_success());
        assert!(duplicate_body.contains("prior result already exists"));

        let list_response = client
            .post(format!(
                "{}/restate/call/ToolExecutor/list_tools",
                ingress.trim_end_matches('/')
            ))
            .json(&json!({
                "session_id": session_id,
                "caller_identity": identity,
            }))
            .send()
            .await
            .context("list registered tools")?;
        let descriptors = list_response
            .error_for_status()
            .context("list_tools should succeed")?
            .json::<Vec<moa_wire::tools::ToolDescriptor>>()
            .await
            .context("deserialize tool descriptors")?;
        for expected in ["bash", "file_read", "file_write"] {
            assert!(
                descriptors
                    .iter()
                    .any(|descriptor| descriptor.name == expected),
                "expected tool {expected} to be listed"
            );
        }

        let events =
            wait_for_tool_result_events(&client, ingress, &identity, session_id, 3).await?;
        assert!(
            events
                .iter()
                .filter(|record| matches!(record.event, Event::ToolResult { .. }))
                .count()
                >= 3,
            "expected at least three persisted ToolResult events"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires local restate-server and Postgres"]
async fn tool_executor_blocks_canary_input_before_backend_execution() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let meta = test_session_meta("tool-executor-canary-block");
        let storage_partition_id = storage_partition_id_from_meta(&meta);
        let mut identity = test_user_identity();
        identity.tenant_id = meta.tenant_id;
        grant_tenant_operator(&identity, &storage_partition_id).await?;

        let create_request = client.post(format!(
            "{}/restate/call/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::types::identifiers::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;
        let catalog = activated_tool_catalog(&client, ingress, session_id, &identity).await?;

        let canary = moa_security::new_canary_token();
        let tool_call_id = ToolCallId::new();
        let mut write_request = tool_request(
            tool_call_id,
            "file_write",
            json!({
                "path": "blocked-canary.txt",
                "content": canary.clone(),
            }),
            session_id,
            &identity,
            &catalog,
        );
        write_request.active_canary = Some(canary);

        let write_output = client
            .post(format!(
                "{}/restate/call/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&write_request)
            .send()
            .await
            .context("call ToolExecutor/file_write with canary input")?
            .error_for_status()
            .context("canary block should return a successful handler response")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize canary block output")?
            .safe_output;
        assert!(write_output.is_error);
        assert!(
            write_output.to_text().contains("protected canary token"),
            "expected blocked output to name the canary leak"
        );
        assert!(
            !file_named_exists_under(sandbox_dir.path(), "blocked-canary.txt")?,
            "blocked file_write must not reach the sandbox backend"
        );

        let request = client.post(format!(
            "{}/restate/call/SessionStore/get_events",
            ingress.trim_end_matches('/')
        ));
        let events = with_identity(request, &identity)
            .json(&get_events_request(session_id, EventRange::all()))
            .send()
            .await
            .context("fetch canary block events via restate ingress")?
            .json::<Vec<moa_core::types::events_stream::EventRecord>>()
            .await
            .context("deserialize canary block event response")?;

        let warning_count = events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::Warning { message }
                    if message.contains("active canary leaked into tool input")
                )
            })
            .count();
        let error_count = events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolError {
                        tool_id,
                        error,
                        retryable,
                        ..
                    } if *tool_id == tool_call_id
                        && error.contains("protected canary token")
                        && !retryable
                )
            })
            .count();
        let result_count = events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolResult { tool_id, .. } if *tool_id == tool_call_id
                )
            })
            .count();

        assert_eq!(warning_count, 1, "expected one persisted canary warning");
        assert_eq!(error_count, 1, "expected one persisted canary ToolError");
        assert_eq!(
            result_count, 0,
            "blocked canary calls must not persist a ToolResult"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

#[tokio::test]
#[ignore = "requires local restate-server and Postgres"]
async fn tool_executor_does_not_duplicate_preexisting_tool_call_event() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let meta = test_session_meta("tool-executor-preexisting-call");
        let storage_partition_id = storage_partition_id_from_meta(&meta);
        let mut identity = test_user_identity();
        identity.tenant_id = meta.tenant_id;
        grant_tenant_operator(&identity, &storage_partition_id).await?;

        let create_request = client.post(format!(
            "{}/restate/call/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::types::identifiers::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;
        let catalog = activated_tool_catalog(&client, ingress, session_id, &identity).await?;

        let tool_call_id = ToolCallId::new();
        let provider_tool_use_id = "toolu_preexisting_restate_call";
        let input = json!({ "cmd": "printf duplicate-check" });
        let request = tool_request_with_provider_id(
            tool_call_id,
            Some(provider_tool_use_id),
            "bash",
            input.clone(),
            session_id,
            &identity,
            &catalog,
        );

        client
            .post(format!("{}/restate/call/SessionStore/append_event", ingress.trim_end_matches('/')))
            .json(&append_event_request(
                session_id,
                Event::ToolCall {
                    tool_id: tool_call_id,
                    provider_tool_use_id: Some(provider_tool_use_id.to_string()),
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input,
                    hand_id: None,
                },
            ))
            .send()
            .await
            .context("persist preexisting ToolCall event")?
            .error_for_status()
            .context("append_event should succeed")?;

        let output = client
            .post(format!("{}/restate/call/ToolExecutor/execute", ingress.trim_end_matches('/')))
            .json(&request)
            .send()
            .await
            .context("call ToolExecutor/bash with preexisting ToolCall")?
            .error_for_status()
            .context("bash should succeed")?
            .json::<SecuredToolOutput>()
            .await
            .context("deserialize bash output")?
            .safe_output;
        assert!(output.to_text().contains("duplicate-check"));

        let events = wait_for_tool_result_events(&client, ingress, &identity, session_id, 1).await?;
        let matching_tool_calls = events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolCall {
                        tool_id,
                        provider_tool_use_id: Some(existing_provider_id),
                        ..
                    } if *tool_id == tool_call_id && existing_provider_id == provider_tool_use_id
                )
            })
            .count();
        assert_eq!(
            matching_tool_calls, 1,
            "expected ToolExecutor to reuse the existing ToolCall event instead of appending a duplicate"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

fn file_named_exists_under(root: &std::path::Path, file_name: &str) -> Result<bool> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("read sandbox directory {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("read entry under {}", path.display()))?;
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()) == Some(file_name) {
                return Ok(true);
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(false)
}

async fn wait_for_tool_result_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: moa_core::types::identifiers::SessionId,
    expected_results: usize,
) -> Result<Vec<moa_core::types::events_stream::EventRecord>> {
    for _attempt in 0..30 {
        let request = client.post(format!(
            "{}/restate/call/SessionStore/get_events",
            ingress.trim_end_matches('/')
        ));
        let response = with_identity(request, identity)
            .json(&get_events_request(session_id, EventRange::all()))
            .send()
            .await
            .context("fetch events via restate ingress")?;
        let events = response
            .json::<Vec<moa_core::types::events_stream::EventRecord>>()
            .await
            .context("deserialize event response")?;
        if events
            .iter()
            .filter(|record| matches!(record.event, Event::ToolResult { .. }))
            .count()
            >= expected_results
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for {expected_results} ToolResult events for session {session_id}")
}
