//! End-to-end approval-flow coverage through a local Restate ingress.

use std::time::Duration;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, bail};
use moa_core::{
    ApprovalDecision, Event, EventRange, EventRecord, ModelId, SessionId, SessionStatus,
};
use sqlx::PgPool;
use tokio::time::sleep;

use crate::support::graph_ingest::wait_for_ingested_brain_responses;
use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_workspace_member, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, test_session_meta, user_message,
};
use moa_test_support::postgres::test_database_url;

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &tempfile::TempDir,
    sandbox_dir: &tempfile::TempDir,
    provider_override_fixture: Option<&Path>,
) -> Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
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
        .env("RUST_LOG", "info")
        .env_remove("COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = provider_override_fixture {
        command
            .env(
                "MOA_PROVIDERS_OVERRIDE",
                format!("scripted:{}", path.display()),
            )
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GOOGLE_API_KEY");
    }

    command
        .spawn()
        .context("spawn moa-orchestrator binary for approval integration")
}

fn object_url(ingress: &str, session_id: SessionId, handler: &str) -> String {
    format!(
        "{}/Session/{session_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and provider-overrides feature"]
async fn approval_allow_once_round_trip_through_restate() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut meta = test_session_meta("session-approval-e2e");
    meta.model = ModelId::new("scripted-loadtest");
    let identity = test_user_identity();
    grant_workspace_member(&identity, &meta.workspace_id).await?;
    let pool = PgPool::connect(&test_database_url())
        .await
        .context("connect to test Postgres")?;
    let approval_token = format!("APPROVAL-{}", uuid::Uuid::now_v7());
    let fixture_path = memory_dir.path().join("approval-script.json");
    write_scripted_approval_fixture(&fixture_path, &approval_token)?;
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let create_request = client.post(format!(
            "{}/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let session_id = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        client
            .post(format!("{}/SessionStore/init_session_vo", ingress.trim_end_matches('/')))
            .json(&init_session_vo_request(session_id, meta.clone()))
            .send()
            .await
            .context("initialize session VO state")?
            .error_for_status()
            .context("init_session_vo should succeed")?;

        let prompt = format!(
            "Use the bash tool exactly once to run `printf '{approval_token}\\n'`. \
             Do not answer from memory. After the tool succeeds, answer with exactly {approval_token}."
        );
        let post_message = client.post(object_url(ingress, session_id, "post_message"));
        with_identity(post_message, &identity)
            .json(&user_message(prompt))
            .send()
            .await
            .context("call Session/post_message")?
            .error_for_status()
            .context("post_message should succeed")?;

        let approval_events = wait_for_approval_request(&client, ingress, &identity, session_id).await?;
        let approval_event = approval_events
            .iter()
            .find(|record| matches!(record.event, Event::ApprovalRequested { .. }))
            .context("expected approval request event")?;
        match &approval_event.event {
            Event::ApprovalRequested { awakeable_id, .. } => {
                assert!(
                    awakeable_id.as_ref().is_some_and(|value| !value.is_empty()),
                    "expected approval event to carry a non-empty awakeable id"
                );
            }
            other => bail!("expected approval request event, got {other:?}"),
        }

        client
            .post(object_url(ingress, session_id, "approve"))
            .json(&ApprovalDecision::AllowOnce)
            .send()
            .await
            .context("call Session/approve")?
            .error_for_status()
            .context("approve should succeed")?;

        wait_for_status(&client, ingress, session_id, SessionStatus::Paused).await?;
        let events = wait_for_brain_response_count(&client, ingress, &identity, session_id, 2).await?;
        assert!(
            events
                .iter()
                .any(|record| matches!(
                    &record.event,
                    Event::ApprovalDecided {
                        decision: ApprovalDecision::AllowOnce,
                        ..
                    }
                )),
            "expected ApprovalDecided(AllowOnce) event for session {session_id}"
        );
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::ToolResult { success: true, output, .. }
                    if output.to_text().contains(&approval_token)
            )),
            "expected successful ToolResult containing approval token for session {session_id}"
        );
        wait_for_ingested_brain_responses(&pool, &meta.workspace_id, session_id, &events).await?;

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    pool.close().await;

    result
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and provider-overrides feature"]
async fn approval_wait_cancel_records_system_denial_without_running_tool() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    if !cfg!(feature = "provider-overrides") {
        return Ok(());
    }

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut meta = test_session_meta("session-approval-cancel-e2e");
    meta.model = ModelId::new("scripted-loadtest");
    let identity = test_user_identity();
    grant_workspace_member(&identity, &meta.workspace_id).await?;
    let approval_token = format!("APPROVAL-CANCEL-{}", uuid::Uuid::now_v7());
    let fixture_path = memory_dir.path().join("approval-cancel-script.json");
    write_scripted_approval_fixture(&fixture_path, &approval_token)?;
    let mut orchestrator =
        spawn_orchestrator(ports, &memory_dir, &sandbox_dir, Some(&fixture_path))?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let create_request = client.post(format!(
            "{}/SessionStore/create_session",
            ingress.trim_end_matches('/')
        ));
        let session_id = with_identity(create_request, &identity)
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        client
            .post(format!(
                "{}/SessionStore/init_session_vo",
                ingress.trim_end_matches('/')
            ))
            .json(&init_session_vo_request(session_id, meta.clone()))
            .send()
            .await
            .context("initialize session VO state")?
            .error_for_status()
            .context("init_session_vo should succeed")?;

        let prompt = format!(
            "Use the bash tool exactly once to run `printf '{approval_token}\\n'`. \
             Do not answer from memory. Wait for approval before running the tool. \
             After the tool succeeds, answer with exactly {approval_token}."
        );
        let post_message = client.post(object_url(ingress, session_id, "post_message"));
        with_identity(post_message, &identity)
            .json(&user_message(prompt))
            .send()
            .await
            .context("call Session/post_message")?
            .error_for_status()
            .context("post_message should succeed")?;

        wait_for_approval_request(&client, ingress, &identity, session_id).await?;

        client
            .post(object_url(ingress, session_id, "request_cancel"))
            .json("cancel while approval is pending")
            .send()
            .await
            .context("call Session/request_cancel during approval wait")?
            .error_for_status()
            .context("request_cancel should succeed")?;

        wait_for_status(&client, ingress, session_id, SessionStatus::Cancelled).await?;
        let events = wait_for_approval_decision(&client, ingress, &identity, session_id).await?;
        assert!(
            events.iter().any(|record| matches!(
                &record.event,
                Event::ApprovalDecided {
                    sub_agent_id: None,
                    decision: ApprovalDecision::Deny { reason: Some(reason) },
                    decided_by,
                    ..
                } if reason == "Cancelled while waiting for approval: cancel while approval is pending"
                    && decided_by == "system:cancel"
            )),
            "expected system:cancel ApprovalDecided denial for session {session_id}"
        );
        assert!(
            !events.iter().any(|record| matches!(
                &record.event,
                Event::ToolResult { success: true, output, .. }
                    if output.to_text().contains(&approval_token)
            )),
            "cancelled approval wait must not execute the approved tool for session {session_id}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn wait_for_status(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<SessionStatus> {
    for _attempt in 0..60 {
        let status = client
            .post(object_url(ingress, session_id, "status"))
            .send()
            .await
            .context("call Session/status")?
            .error_for_status()
            .context("status should succeed")?
            .json::<SessionStatus>()
            .await
            .context("deserialize status response")?;
        if status == expected {
            return Ok(status);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for status {expected:?} for session {session_id}")
}

async fn wait_for_approval_decision(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..60 {
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
            .any(|record| matches!(record.event, Event::ApprovalDecided { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for approval decision for session {session_id}")
}

async fn wait_for_approval_request(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
) -> Result<Vec<moa_core::EventRecord>> {
    let mut last_events = Vec::new();
    for _attempt in 0..60 {
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
            .any(|record| matches!(record.event, Event::ApprovalRequested { .. }))
        {
            return Ok(events);
        }
        last_events = events;

        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "timed out waiting for approval request for session {session_id}; observed events: {}",
        summarize_events(&last_events)
    )
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

fn write_scripted_approval_fixture(path: &Path, approval_token: &str) -> Result<()> {
    let fixture = serde_json::json!({
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
                        "name": "bash",
                        "id": "approval-e2e-tool-call",
                        "input": {
                            "cmd": format!("printf '{}\\n'", approval_token)
                        }
                    }]
                }
            },
            {
                "completion": {
                    "content": approval_token,
                    "tool_calls": []
                }
            }
        ]
    });
    let body =
        serde_json::to_vec_pretty(&fixture).context("serialize scripted approval fixture")?;
    fs::write(path, body).context("write scripted approval fixture")
}

async fn wait_for_brain_response_count(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
    expected: usize,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..60 {
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
        let brain_response_count = events
            .iter()
            .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
            .count();
        if brain_response_count >= expected {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for {expected} BrainResponse events for session {session_id}")
}
