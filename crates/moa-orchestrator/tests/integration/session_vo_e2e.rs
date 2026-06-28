//! End-to-end Session virtual object coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::{CancelMode, Event, EventRange, ModelId, SessionId, SessionStatus};
use reqwest::StatusCode;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::time::sleep;

use crate::support::graph_ingest::wait_for_ingested_brain_responses;
use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_operator, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, storage_partition_id_from_meta, test_session_meta,
    user_message,
};
use moa_test_support::postgres::test_database_url;

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
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
        .env("RUST_LOG", "info")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

fn object_url(ingress: &str, session_id: SessionId, handler: &str) -> String {
    format!(
        "{}/Session/{session_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

fn configured_env(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
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

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, and at least one provider API key"]
async fn session_vo_round_trip_through_restate() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let Some(model) = live_model() else {
        return Ok(());
    };

    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let ingress = ingress.as_str();
    let client = reqwest::Client::new();
    let mut meta = test_session_meta("session-vo-e2e");
    meta.model = ModelId::new(model);
    let storage_partition_id = storage_partition_id_from_meta(&meta);
    let mut identity = test_user_identity();
    identity.tenant_id = meta.tenant_id;
    grant_tenant_operator(&identity, &storage_partition_id).await?;
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
    let pool = PgPool::connect(&test_database_url())
        .await
        .context("connect to test Postgres")?;

    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

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

        let unauthorized_calls = [
            ("status", None),
            ("request_cancel", Some(serde_json::json!("unauthorized"))),
            ("destroy", None),
        ];
        for (handler, body) in unauthorized_calls {
            let request = client.post(object_url(ingress, session_id, handler));
            let response = if let Some(body) = body {
                request.json(&body).send().await
            } else {
                request.send().await
            }
            .with_context(|| format!("call unauthorized Session/{handler}"))?;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "Session/{handler} must reject direct calls without caller identity"
            );
        }

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

        let post_message = client.post(object_url(ingress, session_id, "post_message"));
        with_identity(post_message, &identity)
            .json(&user_message("hello from session vo"))
            .send()
            .await
            .context("call Session/post_message")?
            .error_for_status()
            .context("post_message should succeed")?;

        let status = wait_for_status(
            &client,
            ingress,
            &identity,
            session_id,
            SessionStatus::Paused,
        )
        .await?;
        assert_eq!(
            status,
            SessionStatus::Paused,
            "post_message through TurnExecution eventually maps idle sessions to Paused"
        );

        let events = wait_for_brain_response(&client, ingress, &identity, session_id).await?;
        assert!(
            events
                .iter()
                .any(|record| matches!(record.event, Event::UserMessage { .. })),
            "expected a persisted UserMessage event for session {session_id}"
        );
        wait_for_ingested_brain_responses(&pool, &storage_partition_id, session_id, &events)
            .await?;

        let _ = orchestrator.kill();
        let _ = orchestrator.wait();
        orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir)?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let status_after_restart_request = client.post(object_url(ingress, session_id, "status"));
        let status_after_restart = with_identity(status_after_restart_request, &identity)
            .send()
            .await
            .context("call Session/status after orchestrator restart")?
            .error_for_status()
            .context("status should succeed after restart")?
            .json::<SessionStatus>()
            .await
            .context("deserialize restarted status response")?;
        assert_eq!(status_after_restart, SessionStatus::Paused);

        let cancel_request = client
            .post(object_url(ingress, session_id, "cancel"))
            .json(&CancelMode::Soft);
        with_identity(cancel_request, &identity)
            .send()
            .await
            .context("call Session/cancel")?
            .error_for_status()
            .context("cancel should succeed")?;
        let post_message = client.post(object_url(ingress, session_id, "post_message"));
        with_identity(post_message, &identity)
            .json(&user_message("message after cancel"))
            .send()
            .await
            .context("call Session/post_message after cancel")?
            .error_for_status()
            .context("post_message after cancel should succeed")?;

        let resumed_status = wait_for_status(
            &client,
            ingress,
            &identity,
            session_id,
            SessionStatus::Paused,
        )
        .await?;
        assert_eq!(
            resumed_status,
            SessionStatus::Paused,
            "a stale cancel without an active turn must not prevent a later message from completing"
        );

        let destroy_request = client.post(object_url(ingress, session_id, "destroy"));
        with_identity(destroy_request, &identity)
            .send()
            .await
            .context("call Session/destroy")?
            .error_for_status()
            .context("destroy should succeed")?;

        let reset_status_request = client.post(object_url(ingress, session_id, "status"));
        let reset_status = with_identity(reset_status_request, &identity)
            .send()
            .await
            .context("call Session/status after destroy")?
            .error_for_status()
            .context("status after destroy should succeed")?
            .json::<SessionStatus>()
            .await
            .context("deserialize reset status response")?;
        assert_eq!(reset_status, SessionStatus::Created);

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

async fn wait_for_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &moa_core::traits::Identity,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<SessionStatus> {
    for _attempt in 0..60 {
        let request = client.post(object_url(ingress, session_id, "status"));
        let status = with_identity(request, identity)
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
