//! End-to-end LLM gateway coverage through a local Restate ingress.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{
    CompletionRequest, CompletionResponse, ContextMessage, Event, EventRange, SessionId,
};
use serde_json::json;
use sqlx::PgPool;
use tokio::time::sleep;

use crate::support::graph_ingest::wait_for_ingested_brain_responses;
use crate::support::restate_runtime::{
    OrchestratorPorts, deployment_endpoint_url, grant_session_participant, grant_tenant_operator,
    register_deployment, reserve_orchestrator_ports, restate_admin_url, restate_ingress_url,
    test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, test_session_meta, workspace_id_from_meta,
};
use moa_test_support::postgres::test_database_url;

mod support;

fn spawn_orchestrator(ports: OrchestratorPorts) -> Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
        .env("MOA_RESTATE_ADMIN_URL", restate_admin_url())
        .env("MOA_RESTATE_INGRESS_URL", restate_ingress_url())
        .env("RUST_LOG", "info")
        .env_remove("COHERE_API_KEY")
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
        return Some("gemini-3-flash-preview");
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
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let pool = PgPool::connect(&test_database_url())
        .await
        .context("connect to test Postgres")?;
    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let meta = test_session_meta("llm-gateway-e2e");
        let workspace_id = workspace_id_from_meta(&meta);
        let mut identity = test_user_identity();
        identity.tenant_id = meta.tenant_id;
        grant_tenant_operator(&identity, &workspace_id).await?;

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
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        let mut metadata = HashMap::new();
        metadata.insert("_moa.session_id".to_string(), json!(session_id.to_string()));
        metadata.insert(
            "_moa.workspace_id".to_string(),
            json!(workspace_id.to_string()),
        );
        metadata.insert("_moa.user_id".to_string(), json!(identity.id.to_string()));
        metadata.insert("_moa.channel".to_string(), json!(meta.channel.as_str()));

        let request = CompletionRequest {
            model: Some(model.into()),
            messages: vec![ContextMessage::user("What is 2 + 2? Answer briefly.")],
            tools: Vec::new(),
            max_output_tokens: Some(64),
            temperature: None,
            response_format: None,
            metadata,
        };

        let response = client
            .post(format!(
                "{}/LLMGateway/complete",
                ingress.trim_end_matches('/')
            ))
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

        let events = wait_for_brain_response(&client, ingress, &identity, session_id).await?;
        assert!(
            events
                .iter()
                .any(|record| matches!(record.event, Event::BrainResponse { .. })),
            "expected a persisted BrainResponse event for session {session_id}"
        );
        wait_for_ingested_brain_responses(&pool, &workspace_id, session_id, &events).await?;

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();
    pool.close().await;

    result
}

async fn wait_for_brain_response(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
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
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for BrainResponse event for session {session_id}")
}
