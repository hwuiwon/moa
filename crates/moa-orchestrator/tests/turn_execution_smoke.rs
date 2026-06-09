//! End-to-end smoke coverage for `TurnExecution` cancellation plumbing.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::{ModelId, SessionId, SessionMeta};
use serde::Deserialize;
use uuid::Uuid;

use crate::support::restate_runtime::{grant_workspace_member, test_user_identity, with_identity};

mod support;

#[derive(Debug, Deserialize)]
struct ProgressResponse {
    turn_id: String,
    phase: String,
    cancel_requested: bool,
    cancel_reason: Option<String>,
}

fn ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".to_string())
}

fn workflow_url(turn_id: &str, handler: &str) -> String {
    format!("{}/TurnExecution/{turn_id}/{handler}", ingress_url())
}

fn session_store_url(handler: &str) -> String {
    format!("{}/SessionStore/{handler}", ingress_url())
}

fn live_model() -> &'static str {
    if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "claude-sonnet-4-6";
    }
    if std::env::var("OPENAI_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "gpt-5.4-mini";
    }
    if std::env::var("GOOGLE_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "gemini-3-flash-preview";
    }
    "gpt-5.4-mini"
}

async fn create_initialized_session(client: &reqwest::Client, label: &str) -> Result<String> {
    let meta = SessionMeta {
        workspace_id: format!("workspace-{label}").into(),
        user_id: "user-1".into(),
        model: ModelId::new(live_model()),
        ..SessionMeta::default()
    };
    let identity = test_user_identity();
    grant_workspace_member(&identity, &meta.workspace_id).await?;

    let create_request = client.post(session_store_url("create_session"));
    let session_id = with_identity(create_request, &identity)
        .json(&meta)
        .send()
        .await
        .context("send SessionStore create_session")?
        .error_for_status()
        .context("SessionStore create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize created session id")?;

    client
        .post(session_store_url("init_session_vo"))
        .json(&serde_json::json!({
            "session_id": session_id,
            "meta": meta,
        }))
        .send()
        .await
        .context("send SessionStore init_session_vo")?
        .error_for_status()
        .context("SessionStore init_session_vo should succeed")?;

    Ok(session_id.to_string())
}

async fn fire_run(client: &reqwest::Client, session_id: &str, turn_id: &str) -> Result<()> {
    let body = serde_json::json!({
        "session_id": session_id,
        "turn_id": turn_id,
        "user_message": "smoke test"
    });
    client
        .post(format!("{}/send", workflow_url(turn_id, "run")))
        .json(&body)
        .send()
        .await
        .context("send TurnExecution run")?
        .error_for_status()
        .context("TurnExecution run send should succeed")?;
    Ok(())
}

async fn request_cancel(client: &reqwest::Client, turn_id: &str, reason: &str) -> Result<()> {
    client
        .post(workflow_url(turn_id, "request_cancel"))
        .json(&serde_json::json!(reason))
        .send()
        .await
        .context("send TurnExecution request_cancel")?
        .error_for_status()
        .context("TurnExecution request_cancel should succeed")?;
    Ok(())
}

async fn poll_progress(client: &reqwest::Client, turn_id: &str) -> Result<ProgressResponse> {
    client
        .post(workflow_url(turn_id, "progress"))
        .send()
        .await
        .context("send TurnExecution progress")?
        .error_for_status()
        .context("TurnExecution progress should succeed")?
        .json::<ProgressResponse>()
        .await
        .context("deserialize TurnExecution progress")
}

async fn await_phase(
    client: &reqwest::Client,
    turn_id: &str,
    target: &str,
    timeout: Duration,
) -> Result<ProgressResponse> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;
    while Instant::now() < deadline {
        let progress = poll_progress(client, turn_id).await?;
        if progress.phase == target {
            return Ok(progress);
        }
        last_progress = Some(progress);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    bail!(
        "turn {turn_id} did not reach phase {target} within {:?}; last progress: {:?}",
        timeout,
        last_progress
    )
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn cancel_after_run_dispatch_short_circuits() -> Result<()> {
    // Pins: a cancel after run dispatch moves the turn to Cancelled.
    let client = reqwest::Client::new();
    let turn_id = format!("smoke-after-{}", Uuid::now_v7());
    let session_id = create_initialized_session(&client, "turn-after").await?;

    fire_run(&client, &session_id, &turn_id).await?;
    request_cancel(&client, &turn_id, "after-init").await?;
    let cancelled = await_phase(&client, &turn_id, "Cancelled", Duration::from_secs(5)).await?;
    assert_eq!(cancelled.turn_id, turn_id);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.cancel_reason.as_deref(), Some("after-init"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn cancel_before_init_short_circuits_via_self_resolve() -> Result<()> {
    // Pins: a cancel recorded before run starts is observed by the body after awakeable publish.
    let client = reqwest::Client::new();
    let turn_id = format!("smoke-before-{}", Uuid::now_v7());
    let session_id = create_initialized_session(&client, "turn-before").await?;

    request_cancel(&client, &turn_id, "before-init").await?;
    fire_run(&client, &session_id, &turn_id).await?;

    let cancelled = await_phase(&client, &turn_id, "Cancelled", Duration::from_secs(10)).await?;
    assert_eq!(cancelled.turn_id, turn_id);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.cancel_reason.as_deref(), Some("before-init"));
    Ok(())
}
