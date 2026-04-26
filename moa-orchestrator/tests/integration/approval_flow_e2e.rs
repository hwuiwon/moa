//! End-to-end approval-flow coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{ApprovalDecision, Event, EventRange, ModelId, SessionId, SessionStatus};
use tokio::time::sleep;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, test_session_meta, user_message,
};

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:5432/moa";

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
    memory_dir: &tempfile::TempDir,
    sandbox_dir: &tempfile::TempDir,
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
        .context("spawn moa-orchestrator binary for approval integration")
}

fn configured_env(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn live_model() -> Option<&'static str> {
    if configured_env("ANTHROPIC_API_KEY") {
        return Some("claude-sonnet-4-6");
    }
    if configured_env("OPENAI_API_KEY") {
        return Some("gpt-5.4-mini");
    }
    if configured_env("GOOGLE_API_KEY") {
        return Some("gemini-2.5-flash");
    }

    None
}

fn object_url(ingress: &str, session_id: SessionId, handler: &str) -> String {
    format!("{ingress}/Session/{session_id}/{handler}")
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and at least one provider API key"]
async fn approval_allow_once_round_trip_through_restate() -> Result<()> {
    let Some(model) = live_model() else {
        return Ok(());
    };

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);
    let ingress = "http://127.0.0.1:8080";
    let client = reqwest::Client::new();
    let mut meta = test_session_meta("session-approval-e2e");
    meta.model = ModelId::new(model);
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let approval_token = format!("APPROVAL-{}", uuid::Uuid::now_v7());

    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        let session_id = client
            .post(format!("{ingress}/SessionStore/create_session"))
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;

        client
            .post(format!("{ingress}/SessionStore/init_session_vo"))
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
        client
            .post(object_url(ingress, session_id, "post_message"))
            .json(&user_message(prompt))
            .send()
            .await
            .context("call Session/post_message")?
            .error_for_status()
            .context("post_message should succeed")?;

        wait_for_status(&client, ingress, session_id, SessionStatus::WaitingApproval).await?;
        let approval_events = wait_for_approval_request(&client, ingress, session_id).await?;
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
        let events = wait_for_tool_result(&client, ingress, session_id).await?;
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

async fn wait_for_approval_request(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..60 {
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
            .any(|record| matches!(record.event, Event::ApprovalRequested { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for approval request for session {session_id}")
}

async fn wait_for_tool_result(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
) -> Result<Vec<moa_core::EventRecord>> {
    for _attempt in 0..60 {
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
            .any(|record| matches!(record.event, Event::ToolResult { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for tool result for session {session_id}")
}
