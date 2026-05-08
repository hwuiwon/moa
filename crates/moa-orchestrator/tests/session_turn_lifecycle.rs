//! End-to-end smoke coverage for the additive Session to TurnExecution path.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::{ModelId, SessionId, SessionMeta};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct StartTurnResponse {
    turn_id: Option<String>,
    queued: bool,
}

#[derive(Debug, Deserialize)]
struct QueueMessageResponse {
    queued: bool,
    started_turn_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CancelResponse {
    cancelled: bool,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct TurnOutcome {
    turn_id: String,
    kind: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SessionSnapshot {
    session_id: String,
    active_turn_id: Option<String>,
    pending_message_count: u64,
    last_outcome: Option<TurnOutcome>,
}

#[derive(Debug, Deserialize)]
struct TurnProgress {
    turn_id: String,
    phase: String,
    cancel_requested: bool,
    cancel_reason: Option<String>,
}

fn ingress_url() -> String {
    std::env::var("RESTATE_INGRESS_URL").unwrap_or_else(|_| "http://localhost:18080".to_string())
}

fn session_url(session_id: &str, handler: &str) -> String {
    format!("{}/Session/{session_id}/{handler}", ingress_url())
}

fn session_store_url(handler: &str) -> String {
    format!("{}/SessionStore/{handler}", ingress_url())
}

fn turn_url(turn_id: &str, handler: &str) -> String {
    format!("{}/TurnExecution/{turn_id}/{handler}", ingress_url())
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

    let session_id = client
        .post(session_store_url("create_session"))
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

async fn start_turn(
    client: &reqwest::Client,
    session_id: &str,
    message: &str,
) -> Result<StartTurnResponse> {
    client
        .post(session_url(session_id, "start_turn"))
        .json(&serde_json::json!({ "user_message": message }))
        .send()
        .await
        .context("send Session start_turn")?
        .error_for_status()
        .context("Session start_turn should succeed")?
        .json::<StartTurnResponse>()
        .await
        .context("deserialize Session start_turn response")
}

async fn queue_message(
    client: &reqwest::Client,
    session_id: &str,
    message: &str,
) -> Result<QueueMessageResponse> {
    client
        .post(session_url(session_id, "queue_message"))
        .json(&serde_json::json!({ "user_message": message }))
        .send()
        .await
        .context("send Session queue_message")?
        .error_for_status()
        .context("Session queue_message should succeed")?
        .json::<QueueMessageResponse>()
        .await
        .context("deserialize Session queue_message response")
}

async fn request_cancel(
    client: &reqwest::Client,
    session_id: &str,
    reason: &str,
) -> Result<CancelResponse> {
    client
        .post(session_url(session_id, "request_cancel"))
        .json(&serde_json::json!(reason))
        .send()
        .await
        .context("send Session request_cancel")?
        .error_for_status()
        .context("Session request_cancel should succeed")?
        .json::<CancelResponse>()
        .await
        .context("deserialize Session request_cancel response")
}

async fn snapshot(client: &reqwest::Client, session_id: &str) -> Result<SessionSnapshot> {
    client
        .post(session_url(session_id, "snapshot"))
        .send()
        .await
        .context("send Session snapshot")?
        .error_for_status()
        .context("Session snapshot should succeed")?
        .json::<SessionSnapshot>()
        .await
        .context("deserialize Session snapshot")
}

async fn turn_progress(client: &reqwest::Client, turn_id: &str) -> Result<TurnProgress> {
    client
        .post(turn_url(turn_id, "progress"))
        .send()
        .await
        .context("send TurnExecution progress")?
        .error_for_status()
        .context("TurnExecution progress should succeed")?
        .json::<TurnProgress>()
        .await
        .context("deserialize TurnExecution progress")
}

async fn await_turn_phase(
    client: &reqwest::Client,
    turn_id: &str,
    target: &str,
    timeout: Duration,
) -> Result<TurnProgress> {
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;
    while Instant::now() < deadline {
        let progress = turn_progress(client, turn_id).await?;
        if progress.phase == target {
            return Ok(progress);
        }
        last_progress = Some(progress);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "turn {turn_id} did not reach phase {target} within {:?}; last progress: {:?}",
        timeout,
        last_progress
    )
}

async fn await_snapshot_matching<F>(
    client: &reqwest::Client,
    session_id: &str,
    timeout: Duration,
    matches: F,
) -> Result<SessionSnapshot>
where
    F: Fn(&SessionSnapshot) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut last_snapshot = None;
    while Instant::now() < deadline {
        let current = snapshot(client, session_id).await?;
        if matches(&current) {
            return Ok(current);
        }
        last_snapshot = Some(current);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "session {session_id} did not reach expected snapshot within {:?}; last snapshot: {:?}",
        timeout,
        last_snapshot
    )
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn start_turn_returns_turn_id_immediately() -> Result<()> {
    // Pins: Session/start_turn persists active turn state and returns before turn execution.
    let client = reqwest::Client::new();
    let session_id = create_initialized_session(&client, "start-turn").await?;

    let started_at = Instant::now();
    let response = start_turn(&client, &session_id, "hi").await?;
    let elapsed = started_at.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "start_turn should be fast; elapsed={elapsed:?}"
    );
    let turn_id = response
        .turn_id
        .expect("start_turn should return the started turn ID");
    assert!(!response.queued);

    let current = snapshot(&client, &session_id).await?;
    assert_eq!(current.session_id, session_id);
    assert_eq!(current.active_turn_id.as_deref(), Some(turn_id.as_str()));
    assert_eq!(current.pending_message_count, 0);
    assert!(current.last_outcome.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn queue_message_during_active_turn_drains_after_completion() -> Result<()> {
    // Pins: a queued message is retained while a turn runs, then starts as the next workflow.
    let client = reqwest::Client::new();
    let session_id = create_initialized_session(&client, "queue").await?;

    let first = start_turn(&client, &session_id, "first").await?;
    let first_turn_id = first
        .turn_id
        .expect("first start_turn should return the active turn ID");

    let queued = queue_message(&client, &session_id, "second").await?;
    assert!(queued.queued);
    assert!(queued.started_turn_id.is_none());

    let queued_snapshot = snapshot(&client, &session_id).await?;
    assert_eq!(
        queued_snapshot.active_turn_id.as_deref(),
        Some(first_turn_id.as_str())
    );
    assert_eq!(queued_snapshot.pending_message_count, 1);

    let after_first_completion =
        await_snapshot_matching(&client, &session_id, Duration::from_secs(45), |current| {
            current.pending_message_count == 0
                && current.active_turn_id.as_deref() != Some(first_turn_id.as_str())
                && current.last_outcome.as_ref().is_some_and(|outcome| {
                    outcome.turn_id == first_turn_id && outcome.kind == "Completed"
                })
        })
        .await?;

    let next_turn_id = after_first_completion
        .active_turn_id
        .expect("queued message should start a follow-up turn");
    assert_ne!(next_turn_id, first_turn_id);
    let first_outcome = after_first_completion
        .last_outcome
        .expect("first completion should be recorded");
    assert_eq!(first_outcome.kind, "Completed");
    assert_eq!(first_outcome.turn_id, first_turn_id);
    assert!(
        !first_outcome.message.trim().is_empty(),
        "first outcome should include a non-empty assistant summary: {first_outcome:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn request_cancel_forwards_to_turn_execution() -> Result<()> {
    // Pins: Session/request_cancel resolves the active TurnExecution workflow cancellation path.
    let client = reqwest::Client::new();
    let session_id = create_initialized_session(&client, "cancel").await?;

    let started = start_turn(&client, &session_id, "long").await?;
    let turn_id = started
        .turn_id
        .expect("start_turn should return the active turn ID");

    let cancel = request_cancel(&client, &session_id, "user-requested").await?;
    assert!(cancel.cancelled);
    assert_eq!(cancel.reason, format!("cancel forwarded to turn {turn_id}"));

    let cancelled =
        await_turn_phase(&client, &turn_id, "Cancelled", Duration::from_secs(10)).await?;
    assert_eq!(cancelled.turn_id, turn_id);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.cancel_reason.as_deref(), Some("user-requested"));

    let final_snapshot =
        await_snapshot_matching(&client, &session_id, Duration::from_secs(10), |current| {
            current.active_turn_id.is_none()
                && current.last_outcome.as_ref().is_some_and(|outcome| {
                    outcome.turn_id == turn_id && outcome.kind == "Cancelled"
                })
        })
        .await?;
    assert_eq!(final_snapshot.pending_message_count, 0);
    Ok(())
}
