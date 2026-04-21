//! End-to-end tool executor coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{Event, EventRange, ToolCallId, ToolCallRequest, ToolOutput};
use serde_json::json;
use tempfile::TempDir;
use tokio::time::sleep;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};
use crate::support::session_store_service::{
    append_event_request, get_events_request, test_session_meta,
};

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa:moa@127.0.0.1:5432/moa";

async fn register_deployment(endpoint_url: &str) -> Result<()> {
    for _attempt in 0..15 {
        let output = Command::new("restate")
            .args([
                "--connect-timeout",
                "10000",
                "--request-timeout",
                "30000",
                "deployments",
                "register",
                endpoint_url,
                "--yes",
            ])
            .output()
            .context("register deployment with local restate-server")?;

        if output.status.success() {
            return Ok(());
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("deployment registration did not succeed before retry budget was exhausted")
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    let postgres_url = std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string());

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("MOA_MEMORY_DIR", memory_dir.path())
        .env("MOA_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_DOCKER_ENABLED", "false")
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
        session_id: Some(session_id),
        workspace_id: meta.workspace_id.clone(),
        user_id: meta.user_id.clone(),
        idempotency_key: None,
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
        session_id: Some(session_id),
        workspace_id: meta.workspace_id.clone(),
        user_id: meta.user_id.clone(),
        idempotency_key: None,
    }
}

#[tokio::test]
#[ignore = "requires local restate-server and Postgres"]
async fn tool_executor_round_trip_through_restate() -> Result<()> {
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);

    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = "http://127.0.0.1:8080";
        let meta = test_session_meta("tool-executor-e2e");

        let create_response = client
            .post(format!("{ingress}/SessionStore/create_session"))
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::SessionId>()
            .await
            .context("deserialize create_session response")?;

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
            .post(format!("{ingress}/ToolExecutor/execute"))
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
            .post(format!("{ingress}/ToolExecutor/execute"))
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
            .post(format!("{ingress}/ToolExecutor/execute"))
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
            .post(format!("{ingress}/ToolExecutor/execute"))
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
            .post(format!("{ingress}/ToolExecutor/list_tools"))
            .json(&meta.workspace_id)
            .send()
            .await
            .context("list registered tools")?;
        let descriptors = list_response
            .error_for_status()
            .context("list_tools should succeed")?
            .json::<Vec<moa_orchestrator::services::tool_executor::ToolDescriptor>>()
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

        let events = wait_for_tool_result_events(&client, ingress, session_id, 3).await?;
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
async fn tool_executor_does_not_duplicate_preexisting_tool_call_event() -> Result<()> {
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);

    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = "http://127.0.0.1:8080";
        let meta = test_session_meta("tool-executor-preexisting-call");

        let create_response = client
            .post(format!("{ingress}/SessionStore/create_session"))
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<moa_core::SessionId>()
            .await
            .context("deserialize create_session response")?;

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
            .post(format!("{ingress}/SessionStore/append_event"))
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
            .post(format!("{ingress}/ToolExecutor/execute"))
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

        let events = wait_for_tool_result_events(&client, ingress, session_id, 1).await?;
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

async fn wait_for_tool_result_events(
    client: &reqwest::Client,
    ingress: &str,
    session_id: moa_core::SessionId,
    expected_results: usize,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..30 {
        let response = client
            .post(format!("{ingress}/SessionStore/get_events"))
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
