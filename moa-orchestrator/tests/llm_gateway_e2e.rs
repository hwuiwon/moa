//! End-to-end LLM gateway coverage through a local Restate ingress.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{
    CompletionRequest, CompletionResponse, ContextMessage, Event, EventRange, SessionId,
};
use serde_json::json;
use tokio::time::sleep;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};
use crate::support::session_store_service::{get_events_request, test_session_meta};

mod support;

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

fn spawn_orchestrator(ports: OrchestratorPorts) -> Result<Child> {
    let postgres_url = std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string());

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
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

fn configured_env(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and at least one provider API key"]
async fn llm_gateway_round_trip_through_restate() -> Result<()> {
    let Some(model) = live_model() else {
        return Ok(());
    };

    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports)?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);
    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = "http://127.0.0.1:8080";
        let meta = test_session_meta("llm-gateway-e2e");

        let create_response = client
            .post(format!("{ingress}/SessionStore/create_session"))
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;

        let mut metadata = HashMap::new();
        metadata.insert("_moa.session_id".to_string(), json!(session_id.to_string()));
        metadata.insert(
            "_moa.workspace_id".to_string(),
            json!(meta.workspace_id.to_string()),
        );
        metadata.insert("_moa.user_id".to_string(), json!(meta.user_id.to_string()));
        metadata.insert("_moa.platform".to_string(), json!(meta.platform.as_str()));

        let request = CompletionRequest {
            model: Some(model.into()),
            messages: vec![ContextMessage::user("What is 2 + 2? Answer briefly.")],
            tools: Vec::new(),
            max_output_tokens: Some(64),
            temperature: None,
            response_format: None,
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            metadata,
        };

        let response = client
            .post(format!("{ingress}/LLMGateway/complete"))
            .json(&request)
            .send()
            .await
            .context("call LLMGateway/complete via restate ingress")?
            .json::<CompletionResponse>()
            .await
            .context("deserialize llm gateway response")?;

        assert!(
            !response.text.trim().is_empty(),
            "expected provider text response"
        );
        let usage = response.token_usage();
        assert!(
            usage.total_input_tokens() > 0,
            "expected non-zero input tokens"
        );
        assert!(usage.output_tokens > 0, "expected non-zero output tokens");

        let events = wait_for_brain_response(&client, ingress, session_id).await?;
        assert!(
            events
                .iter()
                .any(|record| matches!(record.event, Event::BrainResponse { .. })),
            "expected a persisted BrainResponse event for session {session_id}"
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

async fn wait_for_brain_response(
    client: &reqwest::Client,
    ingress: &str,
    session_id: SessionId,
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
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for BrainResponse event for session {session_id}")
}
