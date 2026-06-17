//! End-to-end coverage for agent-loop experiment target execution through Restate.

#![cfg(feature = "integration")]

use std::time::Duration;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_core::traits::Identity;
use moa_core::wire::{
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, SkillImportRequest, SkillImportResponse, SkillPackageDocument,
    SkillPackageDocumentFile,
};
use moa_core::{Event, EventRange, EventRecord, MemoryScope, WorkspaceId};
use moa_test_support::postgres::test_database_url;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_workspace_editor,
    register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
    test_user_identity, with_identity,
};
use crate::support::session_store_service::get_events_request;

mod support;

const SUPPORT_SKILL_PATH: &str = ".moa/skills/delivery-support/SKILL.md";
const SUPPORT_SKILL_PROVIDER_ID: &str = "read_delivery_support_skill";

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    provider_override_fixture: &Path,
) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", provider_override_fixture.display()),
        )
        .env("RUST_LOG", "info")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for experiment e2e")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn agent_loop_experiment_creates_session_and_persists_scripted_response() -> Result<()> {
    // Pins: ExperimentRun drives Session/TurnExecution and links the created session to the run.
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let fixture_path = memory_dir.path().join("experiment-agent-loop-script.json");
    let final_text = "The delivery support runbook says this spilled order qualifies for a replacement after confirming the photo and order id.";
    write_scripted_fixture(&fixture_path, final_text)?;

    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let identity = test_user_identity();
    let workspace_id = WorkspaceId::new(format!("experiment-agent-loop-{}", Uuid::now_v7()));
    grant_workspace_editor(&identity, &workspace_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &fixture_path)?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        import_support_skill(&client, ingress, &identity, &workspace_id).await?;

        let run = run_agent_loop_experiment(&client, ingress, &identity, &workspace_id).await?;
        assert_eq!(run.status, "accepted");
        assert_ne!(run.score_run_id, Uuid::nil());
        assert!(
            run.session_id.is_none(),
            "session should be attached by the workflow after acceptance"
        );

        let status =
            wait_for_experiment_status(&client, ingress, &identity, &workspace_id, run.run_uid)
                .await?;
        assert_eq!(status.score_run_id, Some(run.score_run_id));
        assert!(
            matches!(status.status.as_str(), "completed" | "waiting_approval"),
            "experiment should complete or block on normal approval flow, got {status:?}"
        );
        let session_id = status
            .session_id
            .context("experiment status should expose linked session_id")?;

        let events =
            wait_for_brain_response_text(&client, ingress, &identity, session_id, final_text)
                .await?;
        assert!(
            saw_successful_skill_file_read(&events),
            "expected the imported support skill to be read through normal tool routing; observed events: {}",
            summarize_events(&events)
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn import_support_skill(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
) -> Result<()> {
    let request = SkillImportRequest {
        workspace_id: workspace_id.clone(),
        scope: MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        },
        packages: vec![support_skill_package()],
    };
    let imported = post_json_with_identity(client, ingress, "Skills", "import", identity, &request)
        .await?
        .json::<SkillImportResponse>()
        .await
        .context("deserialize skill import response")?;
    assert_eq!(imported.imported, 1);
    Ok(())
}

async fn run_agent_loop_experiment(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
) -> Result<ExperimentRunResponse> {
    let request = ExperimentRunRequest {
        workspace_id: workspace_id.clone(),
        name: "spilled-order-support-agent-loop".to_string(),
        target: json!({
            "kind": "agent_loop",
            "prompt": "A customer says soup spilled across the delivery bag. They have a clear photo and ask whether we can replace the order.",
            "session_id": null,
            "model": "scripted-loadtest",
            "attachments": []
        }),
        variant: json!({
            "name": "delivery-support-skill",
            "model": "scripted-loadtest",
            "artifact_revision_uids": [],
            "skill_refs": ["skill://delivery-support"],
            "workflow_ref": null,
            "metadata": { "lane": "deterministic-e2e" }
        }),
        scorecard: json!({
            "score_names": ["support_resolution"],
            "evaluator_metadata": { "mode": "manual-or-later" }
        }),
        score_run_id: None,
        idempotency_key: Some(format!("agent-loop-{}", Uuid::now_v7())),
    };
    post_json_with_identity(client, ingress, "Experiments", "run", identity, &request)
        .await?
        .json::<ExperimentRunResponse>()
        .await
        .context("deserialize experiment run response")
}

async fn wait_for_experiment_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
) -> Result<ExperimentRunStatusResponse> {
    let request = ExperimentRunStatusRequest {
        workspace_id: workspace_id.clone(),
        run_uid,
    };
    let mut last_status = None;
    for _attempt in 0..90 {
        let status =
            post_json_with_identity(client, ingress, "Experiments", "status", identity, &request)
                .await?
                .json::<ExperimentRunStatusResponse>()
                .await
                .context("deserialize experiment status response")?;
        if status.session_id.is_some()
            && matches!(status.status.as_str(), "completed" | "waiting_approval")
        {
            return Ok(status);
        }
        last_status = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for experiment {run_uid} to link a session and finish; last status: {last_status:?}"
    )
}

async fn wait_for_brain_response_text(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: moa_core::SessionId,
    expected_text: &str,
) -> Result<Vec<EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..90 {
        let events = fetch_events(client, ingress, identity, session_id).await?;
        if events.iter().any(|record| {
            matches!(&record.event, Event::BrainResponse { text, .. } if text == expected_text)
        }) {
            return Ok(events);
        }
        last_events = events;
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for BrainResponse `{expected_text}` in session {session_id}; observed events: {}",
        summarize_events(&last_events)
    )
}

async fn fetch_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: moa_core::SessionId,
) -> Result<Vec<EventRecord>> {
    post_json_with_identity(
        client,
        ingress,
        "SessionStore",
        "get_events",
        identity,
        &get_events_request(session_id, EventRange::all()),
    )
    .await?
    .json::<Vec<EventRecord>>()
    .await
    .context("deserialize session events")
}

async fn post_json_with_identity<T: serde::Serialize + ?Sized>(
    client: &reqwest::Client,
    ingress: &str,
    service: &str,
    handler: &str,
    identity: &Identity,
    request: &T,
) -> Result<reqwest::Response> {
    let response = with_identity(
        client.post(service_url(ingress, service, handler)),
        identity,
    )
    .json(request)
    .send()
    .await
    .with_context(|| format!("call {service}/{handler}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
    bail!("{service}/{handler} returned {status}: {body}")
}

fn service_url(ingress: &str, service: &str, handler: &str) -> String {
    format!("{}/{service}/{handler}", ingress.trim_end_matches('/'))
}

fn saw_successful_skill_file_read(events: &[EventRecord]) -> bool {
    let read_tool_ids = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } if tool_name == "file_read"
                && input.get("path").and_then(serde_json::Value::as_str)
                    == Some(SUPPORT_SKILL_PATH) =>
            {
                Some(*tool_id)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();

    events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult {
                tool_id,
                output,
                success: true,
                ..
            } if read_tool_ids.contains(tool_id) && output.to_text().contains("Delivery Support")
        )
    })
}

fn summarize_events(events: &[EventRecord]) -> String {
    if events.is_empty() {
        return "<none>".to_string();
    }

    events
        .iter()
        .map(|record| format!("#{} {:?}", record.sequence_num, record.event_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_scripted_fixture(path: &Path, final_text: &str) -> Result<()> {
    let fixture = json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "responses": [
            {
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "file_read",
                        "id": SUPPORT_SKILL_PROVIDER_ID,
                        "input": { "path": SUPPORT_SKILL_PATH }
                    }]
                }
            },
            {
                "completion": {
                    "content": final_text,
                    "tool_calls": []
                }
            }
        ]
    });
    let body = serde_json::to_vec_pretty(&fixture).context("serialize scripted fixture")?;
    fs::write(path, body).context("write scripted fixture")
}

fn support_skill_package() -> SkillPackageDocument {
    let skill_md = r#"---
name: delivery-support
description: "Resolve damaged or spilled food delivery support requests from clear customer evidence."
allowed-tools: file_read
metadata:
  moa-tags: "support,delivery,refund,replacement,food"
  moa-use-count: "4"
  moa-success-rate: "0.94"
---

# Delivery Support

Use this when a customer reports a delivery that arrived spilled, crushed, leaking, missing items, unsafe, or otherwise damaged.

When there is a clear customer photo and an order id, recommend a replacement or refund review. Ask for clearer evidence only when the description or image is ambiguous.
"#;
    SkillPackageDocument {
        name: Some("delivery-support".to_string()),
        description: Some(
            "Resolve damaged or spilled food delivery support requests from clear customer evidence."
                .to_string(),
        ),
        files: vec![SkillPackageDocumentFile {
            path: "SKILL.md".to_string(),
            content_base64: BASE64.encode(skill_md.as_bytes()),
            content_type: Some("text/markdown".to_string()),
            executable: false,
        }],
        source_uri: Some("test://experiment-agent-loop/delivery-support".to_string()),
        metadata: json!({}),
    }
}
