//! End-to-end SessionStore coverage through a local Restate ingress.

use anyhow::{Context, Result, bail};
use moa_core::{Event, EventRange, EventRecord, SessionId};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};
use crate::support::session_store_service::{
    append_event_request, get_events_request, test_session_meta, user_message_event,
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

#[tokio::test]
#[ignore = "requires a local restate-server and a reachable Postgres instance"]
async fn session_store_round_trip_through_restate() -> Result<()> {
    let ports = reserve_orchestrator_ports()?;
    let mut orchestrator = spawn_orchestrator(ports)?;
    let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);
    let result = async {
        register_deployment(endpoint_url.as_str()).await?;

        let client = reqwest::Client::new();
        let ingress = "http://127.0.0.1:8080";
        let meta = test_session_meta("restate-e2e");

        let create_response = client
            .post(format!("{ingress}/SessionStore/create_session"))
            .json(&meta)
            .send()
            .await
            .context("create session via restate ingress")?;
        let session_id = create_response
            .json::<SessionId>()
            .await
            .context("deserialize create_session ingress response")?;

        for message in ["first", "second", "third"] {
            let append_response = client
                .post(format!("{ingress}/SessionStore/append_event"))
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

        let get_events_response = client
            .post(format!("{ingress}/SessionStore/get_events"))
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
