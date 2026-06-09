//! End-to-end SessionStore coverage through a local Restate ingress.

use anyhow::{Context, Result};
use moa_core::{Event, EventRange, EventRecord, SessionId};
use std::process::{Child, Command, Stdio};

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_workspace_member, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    append_event_request, get_events_request, test_session_meta, user_message_event,
};

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

fn spawn_orchestrator(ports: OrchestratorPorts) -> Result<Child> {
    let postgres_url = std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string());

    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn moa-orchestrator binary for Restate integration")
}

#[tokio::test]
#[ignore = "requires a local restate-server and a reachable Postgres instance"]
async fn session_store_round_trip_through_restate() -> Result<()> {
    let _guard = RESTATE_E2E_LOCK.lock().await;
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports)?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let result = async {
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = restate_ingress_url();
        let ingress = ingress.as_str();
        let meta = test_session_meta("restate-e2e");
        let identity = test_user_identity();
        grant_workspace_member(&identity, &meta.workspace_id).await?;

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
            .context("deserialize create_session ingress response")?;
        grant_session_participant(&identity, session_id).await?;

        for message in ["first", "second", "third"] {
            let append_response = client
                .post(format!(
                    "{}/SessionStore/append_event",
                    ingress.trim_end_matches('/')
                ))
                .json(&append_event_request(
                    session_id,
                    user_message_event(message),
                ))
                .send()
                .await
                .with_context(|| format!("append event `{message}` via restate ingress"))?;
            let sequence_num = append_response
                .json::<u64>()
                .await
                .context("deserialize append_event ingress response")?;
            assert!(
                sequence_num <= 2,
                "expected zero-based sequence numbers 0..=2, got {sequence_num}"
            );
        }

        let get_events_request_builder = client.post(format!(
            "{}/SessionStore/get_events",
            ingress.trim_end_matches('/')
        ));
        let get_events_response = with_identity(get_events_request_builder, &identity)
            .json(&get_events_request(session_id, EventRange::all()))
            .send()
            .await
            .context("get events via restate ingress")?;
        let events = get_events_response
            .json::<Vec<EventRecord>>()
            .await
            .context("deserialize get_events ingress response")?;

        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|record| matches!(record.event, Event::UserMessage { .. }))
        );

        Ok(())
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}
