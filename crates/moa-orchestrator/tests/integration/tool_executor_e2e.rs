//! End-to-end tool executor coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{Event, EventRange, ToolCallId, ToolCallRequest, ToolOutput};
use moa_test_support::postgres::test_database_url;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::sleep;

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
    session_id: moa_core::SessionId,
    meta: &moa_core::SessionMeta,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        provider_tool_use_id: None,
        tool_name: tool_name.to_string(),
        input,
        active_canary: None,
        session_id: Some(session_id),
        tenant_id: meta.tenant_id,
        user_id: fallback_tool_user_id(meta),
        idempotency_key: None,
        trusted_sandbox_manifest: None,
        worker_id: None,
    }
}

fn tool_request_with_provider_id(
    tool_call_id: ToolCallId,
    provider_tool_use_id: Option<&str>,
    tool_name: &str,
    input: serde_json::Value,
    session_id: moa_core::SessionId,
    meta: &moa_core::SessionMeta,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id,
        provider_tool_use_id: provider_tool_use_id.map(ToOwned::to_owned),
        tool_name: tool_name.to_string(),
        input,
        active_canary: None,
        session_id: Some(session_id),
        tenant_id: meta.tenant_id,
        user_id: fallback_tool_user_id(meta),
        idempotency_key: None,
        trusted_sandbox_manifest: None,
        worker_id: None,
    }
}

fn fallback_tool_user_id(meta: &moa_core::SessionMeta) -> moa_core::UserId {
    match &meta.created_by {
        Some(moa_core::SessionActorRef::Identity { id }) => moa_core::UserId::new(id.to_string()),
        Some(moa_core::SessionActorRef::Contact { id }) => {
            moa_core::UserId::new(format!("contact:{id}"))
        }
        Some(moa_core::SessionActorRef::Anonymous) | None => {
            moa_core::UserId::new(format!("tenant:{}", meta.tenant_id))
        }
    }
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
            "{}/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        let write_request = tool_request(
            ToolCallId::new(),
            "file_write",
            json!({
                "path": "note.txt",
                "content": "hello from tool executor"
            }),
            session_id,
            &meta,
        );
        let write_output = client
            .post(format!(
                "{}/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&write_request)
            .send()
            .await
            .context("call ToolExecutor/file_write via restate ingress")?
            .error_for_status()
            .context("file_write should succeed")?
            .json::<ToolOutput>()
            .await
            .context("deserialize file_write output")?;
        assert!(write_output.to_text().contains("note.txt"));

        let read_request = tool_request(
            ToolCallId::new(),
            "file_read",
            json!({ "path": "note.txt" }),
            session_id,
            &meta,
        );
        let read_output = client
            .post(format!(
                "{}/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&read_request)
            .send()
            .await
            .context("call ToolExecutor/file_read via restate ingress")?
            .error_for_status()
            .context("file_read should succeed")?
            .json::<ToolOutput>()
            .await
            .context("deserialize file_read output")?;
        assert!(read_output.to_text().contains("hello from tool executor"));

        let bash_call_id = ToolCallId::new();
        let bash_request = tool_request(
            bash_call_id,
            "bash",
            json!({ "cmd": "printf hello-from-bash" }),
            session_id,
            &meta,
        );
        let bash_output = client
            .post(format!(
                "{}/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&bash_request)
            .send()
            .await
            .context("call ToolExecutor/bash via restate ingress")?
            .error_for_status()
            .context("bash should succeed")?
            .json::<ToolOutput>()
            .await
            .context("deserialize bash output")?;
        assert!(bash_output.to_text().contains("hello-from-bash"));

        let duplicate_response = client
            .post(format!(
                "{}/ToolExecutor/execute",
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
                "{}/ToolExecutor/list_tools",
                ingress.trim_end_matches('/')
            ))
            .json(&meta.tenant_id)
            .send()
            .await
            .context("list registered tools")?;
        let descriptors = list_response
            .error_for_status()
            .context("list_tools should succeed")?
            .json::<Vec<moa_core::wire::tools::ToolDescriptor>>()
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
            "{}/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

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
            &meta,
        );
        write_request.active_canary = Some(canary);

        let write_output = client
            .post(format!(
                "{}/ToolExecutor/execute",
                ingress.trim_end_matches('/')
            ))
            .json(&write_request)
            .send()
            .await
            .context("call ToolExecutor/file_write with canary input")?
            .error_for_status()
            .context("canary block should return a successful handler response")?
            .json::<ToolOutput>()
            .await
            .context("deserialize canary block output")?;
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
            "{}/SessionStore/get_events",
            ingress.trim_end_matches('/')
        ));
        let events = with_identity(request, &identity)
            .json(&get_events_request(session_id, EventRange::all()))
            .send()
            .await
            .context("fetch canary block events via restate ingress")?
            .json::<Vec<moa_core::EventRecord>>()
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
            "{}/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let create_response = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        let tool_call_id = ToolCallId::new();
        let provider_tool_use_id = "toolu_preexisting_restate_call";
        let input = json!({ "cmd": "printf duplicate-check" });
        let request = tool_request_with_provider_id(
            tool_call_id,
            Some(provider_tool_use_id),
            "bash",
            input.clone(),
            session_id,
            &meta,
        );

        client
            .post(format!("{}/SessionStore/append_event", ingress.trim_end_matches('/')))
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
            .post(format!("{}/ToolExecutor/execute", ingress.trim_end_matches('/')))
            .json(&request)
            .send()
            .await
            .context("call ToolExecutor/bash with preexisting ToolCall")?
            .error_for_status()
            .context("bash should succeed")?
            .json::<ToolOutput>()
            .await
            .context("deserialize bash output")?;
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
    session_id: moa_core::SessionId,
    expected_results: usize,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..30 {
        let request = client.post(format!(
            "{}/SessionStore/get_events",
            ingress.trim_end_matches('/')
        ));
        let response = with_identity(request, identity)
            .json(&get_events_request(session_id, EventRange::all()))
            .send()
            .await
            .context("fetch events via restate ingress")?;
        let events = response
            .json::<Vec<moa_core::EventRecord>>()
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
