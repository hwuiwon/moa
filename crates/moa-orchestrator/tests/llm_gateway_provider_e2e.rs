//! End-to-end LLM gateway coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{
    events::Event, types::events_stream::EventRange, types::identifiers::ModelId,
    types::identifiers::SessionId, types::session::SessionStatus,
};
use sqlx::PgPool;
use tokio::time::sleep;

use crate::support::graph_ingest::wait_for_ingested_brain_responses;
use crate::support::restate_runtime::{
    OrchestratorPorts, deployment_endpoint_url, grant_session_participant, grant_tenant_operator,
    register_deployment, reserve_orchestrator_ports, restate_ingress_url, restate_test_admin_url,
    test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, start_turn_request,
    storage_partition_id_from_meta, test_session_meta,
};
use moa_test_support::postgres::test_database_url;
use moa_test_support::process::TestChildGuard;

#[path = "support/mod.rs"]
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
        .env("MOA_RESTATE_INGRESS_URL", restate_ingress_url())
        .env("RUST_LOG", "info")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

fn live_model() -> Option<&'static str> {
    if configured_env("MOA_ANTHROPIC_API_KEY") {
        return Some("claude-sonnet-4-6");
    }
    if configured_env("MOA_OPENAI_API_KEY") {
        return Some("gpt-5.4-mini");
    }
    if configured_env("MOA_GOOGLE_API_KEY") {
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
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, local Restate/Postgres, and a provider API key"]
async fn session_turn_round_trip_reaches_private_llm_gateway() -> Result<()> {
    // Pins: a public Session turn reaches the ingress-private LLMGateway through
    // Restate service-to-service invocation and persists real provider usage.
    if std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
        return Ok(());
    }
    let model = live_model().context(
        "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY",
    )?;

    let ports = reserve_orchestrator_ports()?;
    let _orchestrator = TestChildGuard::new(spawn_orchestrator(ports)?);
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let pool = PgPool::connect(&test_database_url())
        .await
        .context("connect to test Postgres")?;
    let result = async {
        register_deployment(&restate_test_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let mut meta = test_session_meta("llm-gateway-e2e");
        meta.model = ModelId::new(model);
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
            .error_for_status()
            .context("session creation should succeed")?
            .json::<SessionId>()
            .await
            .context("deserialize create_session response")?;
        grant_session_participant(&identity, session_id).await?;

        client
            .post(format!(
                "{}/restate/call/SessionStore/init_session_vo",
                ingress.trim_end_matches('/')
            ))
            .json(&init_session_vo_request(session_id, meta))
            .send()
            .await
            .context("initialize session VO")?
            .error_for_status()
            .context("session VO initialization should succeed")?;

        let start_turn = client.post(format!(
            "{}/restate/call/Session/{session_id}/start_turn",
            ingress.trim_end_matches('/')
        ));
        with_identity(start_turn, &identity)
            .json(&start_turn_request("What is 2 + 2? Answer briefly."))
            .send()
            .await
            .context("start public Session turn")?
            .error_for_status()
            .context("Session/start_turn should succeed")?;

        wait_for_idle(&client, ingress, &identity, session_id).await?;

        let events = wait_for_brain_response(&client, ingress, &identity, session_id).await?;
        let brain_responses = events
            .iter()
            .filter_map(|record| match &record.event {
                Event::BrainResponse {
                    text,
                    model,
                    input_tokens_uncached,
                    input_tokens_cache_write,
                    input_tokens_cache_read,
                    output_tokens,
                    ..
                } => Some((
                    text,
                    model,
                    input_tokens_uncached
                        .saturating_add(*input_tokens_cache_write)
                        .saturating_add(*input_tokens_cache_read),
                    output_tokens,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            brain_responses.len(),
            1,
            "expected exactly one persisted BrainResponse for session {session_id}"
        );
        let (text, response_model, input_tokens, output_tokens) = brain_responses[0];
        assert!(
            !text.trim().is_empty(),
            "provider response must not be empty"
        );
        assert_eq!(response_model.as_str(), model);
        assert!(input_tokens > 0, "expected non-zero provider input tokens");
        assert!(
            *output_tokens > 0,
            "expected non-zero provider output tokens"
        );
        wait_for_ingested_brain_responses(&pool, &storage_partition_id, session_id, &events)
            .await?;

        Ok(())
    }
    .await;

    pool.close().await;

    result
}

#[tokio::test]
#[ignore = "requires local restate-server and a registered moa-orchestrator deployment"]
async fn direct_llm_gateway_ingress_is_rejected() -> Result<()> {
    // Pins: provider calls are reachable only from product handlers through
    // Restate service-to-service invocation, never from public ingress.
    let response = reqwest::Client::new()
        .post(format!(
            "{}/restate/call/LLMGateway/complete",
            restate_ingress_url().trim_end_matches('/')
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .context("probe private LLMGateway ingress")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read private LLMGateway rejection")?;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "unexpected private LLMGateway rejection body: {body}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body)
            .context("decode private LLMGateway rejection")?,
        serde_json::json!({"message": "the invoked service is not public"}),
        "Restate must reject the request at its private-ingress boundary"
    );
    Ok(())
}

async fn wait_for_idle(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
) -> Result<()> {
    for _attempt in 0..60 {
        let request = client.post(format!(
            "{}/restate/call/Session/{session_id}/status",
            ingress.trim_end_matches('/')
        ));
        let status = with_identity(request, identity)
            .send()
            .await
            .context("read Session status")?
            .error_for_status()
            .context("Session/status should succeed")?
            .json::<SessionStatus>()
            .await
            .context("decode Session status")?;
        if status == SessionStatus::Idle {
            return Ok(());
        }
        sleep(Duration::from_secs(1)).await;
    }
    bail!("timed out waiting for session {session_id} to become idle")
}

async fn wait_for_brain_response(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
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
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
        {
            return Ok(events);
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for BrainResponse event for session {session_id}")
}
