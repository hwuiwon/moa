//! End-to-end smoke coverage for the additive Session to TurnExecution path.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::{ModelId, SessionActorRef, SessionId, SessionMeta, TenantId};
use serde::Deserialize;

use crate::support::restate_runtime::{grant_tenant_operator, test_user_identity, with_identity};

mod support;

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
struct SessionProgress {
    snapshot: SessionSnapshot,
    active_turn_progress: Option<TurnProgress>,
    events: Vec<serde_json::Value>,
}

struct InitializedSession {
    id: String,
    identity: Identity,
}

#[derive(Debug, Deserialize)]
struct TurnProgress {
    turn_id: String,
    phase: String,
    cancel_requested: bool,
    cancel_reason: Option<String>,
}

fn ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".to_string())
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

async fn create_initialized_session(
    client: &reqwest::Client,
    _label: &str,
) -> Result<InitializedSession> {
    let tenant_id = TenantId::new();
    let mut identity = test_user_identity();
    identity.tenant_id = tenant_id;
    let meta = SessionMeta {
        tenant_id,
        model: ModelId::new(live_model()),
        created_by: Some(SessionActorRef::Identity { id: identity.id }),
        ..SessionMeta::default()
    };
    grant_tenant_operator(&identity, tenant_id).await?;

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

    Ok(InitializedSession {
        id: session_id.to_string(),
        identity,
    })
}

async fn start_turn(
    client: &reqwest::Client,
    session: &InitializedSession,
    message: &str,
) -> Result<StartTurnResponse> {
    let request = client.post(session_url(&session.id, "start_turn"));
    with_identity(request, &session.identity)
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
    session: &InitializedSession,
    message: &str,
) -> Result<QueueMessageResponse> {
    let request = client.post(session_url(&session.id, "queue_message"));
    with_identity(request, &session.identity)
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
    session: &InitializedSession,
    reason: &str,
) -> Result<CancelResponse> {
    client
        .post(session_url(&session.id, "request_cancel"))
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

async fn snapshot(
    client: &reqwest::Client,
    session: &InitializedSession,
) -> Result<SessionSnapshot> {
    let request = client.post(session_url(&session.id, "snapshot"));
    with_identity(request, &session.identity)
        .send()
        .await
        .context("send Session snapshot")?
        .error_for_status()
        .context("Session snapshot should succeed")?
        .json::<SessionSnapshot>()
        .await
        .context("deserialize Session snapshot")
}

async fn session_progress(
    client: &reqwest::Client,
    session: &InitializedSession,
) -> Result<SessionProgress> {
    let request = client.post(session_url(&session.id, "progress"));
    with_identity(request, &session.identity)
        .json(&serde_json::json!({ "event_range": { "limit": 20 } }))
        .send()
        .await
        .context("send Session progress")?
        .error_for_status()
        .context("Session progress should succeed")?
        .json::<SessionProgress>()
        .await
        .context("deserialize Session progress")
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
    session: &InitializedSession,
    timeout: Duration,
    matches: F,
) -> Result<SessionSnapshot>
where
    F: Fn(&SessionSnapshot) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut last_snapshot = None;
    while Instant::now() < deadline {
        let current = snapshot(client, session).await?;
        if matches(&current) {
            return Ok(current);
        }
        last_snapshot = Some(current);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "session {} did not reach expected snapshot within {:?}; last snapshot: {:?}",
        session.id,
        timeout,
        last_snapshot
    )
}

async fn await_session_progress_matching<F>(
    client: &reqwest::Client,
    session: &InitializedSession,
    timeout: Duration,
    matches: F,
) -> Result<SessionProgress>
where
    F: Fn(&SessionProgress) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut last_progress = None;
    while Instant::now() < deadline {
        let current = session_progress(client, session).await?;
        if matches(&current) {
            return Ok(current);
        }
        last_progress = Some(current);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "session {} did not reach expected progress within {:?}; last progress: {:?}",
        session.id,
        timeout,
        last_progress
    )
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn start_turn_returns_turn_id_immediately() -> Result<()> {
    // Pins: Session/start_turn persists active turn state and returns before turn execution.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "start-turn").await?;

    let started_at = Instant::now();
    let response = start_turn(&client, &session, "hi").await?;
    let elapsed = started_at.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "start_turn should be fast; elapsed={elapsed:?}"
    );
    let turn_id = response
        .turn_id
        .expect("start_turn should return the started turn ID");
    assert!(!response.queued);

    let current = snapshot(&client, &session).await?;
    assert_eq!(current.session_id, session.id);
    assert_eq!(current.active_turn_id.as_deref(), Some(turn_id.as_str()));
    assert_eq!(current.pending_message_count, 0);
    assert!(current.last_outcome.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn session_progress_combines_snapshot_turn_progress_and_events() -> Result<()> {
    // Pins: Session/progress saves clients from calling Session/snapshot, TurnExecution/progress, and SessionStore/get_events separately.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "session-progress").await?;

    let started = start_turn(&client, &session, "hi").await?;
    let turn_id = started
        .turn_id
        .expect("start_turn should return the active turn ID");

    let initial_progress = session_progress(&client, &session).await?;
    assert_eq!(initial_progress.snapshot.session_id, session.id);
    assert_eq!(
        initial_progress.snapshot.active_turn_id.as_deref(),
        Some(turn_id.as_str())
    );
    assert_eq!(
        initial_progress
            .active_turn_progress
            .as_ref()
            .map(|turn| turn.turn_id.as_str()),
        Some(turn_id.as_str())
    );

    let progress =
        await_session_progress_matching(&client, &session, Duration::from_secs(10), |current| {
            !current.events.is_empty()
        })
        .await?;

    assert_eq!(progress.snapshot.session_id, session.id);
    assert_eq!(progress.snapshot.pending_message_count, 0);
    assert!(
        progress.snapshot.active_turn_id.as_deref() == Some(turn_id.as_str())
            || progress
                .snapshot
                .last_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.turn_id == turn_id),
        "progress should describe the active turn or its terminal outcome: {progress:?}"
    );
    assert!(
        !progress.events.is_empty(),
        "Session/progress should include durable event history"
    );
    if let Some(active_turn_progress) = progress.active_turn_progress.as_ref() {
        assert_eq!(active_turn_progress.turn_id, turn_id);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn queue_message_during_active_turn_is_drained_after_completion() -> Result<()> {
    // Pins: a queued message is retained while a turn runs, then drains after completion.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "queue").await?;

    let first = start_turn(&client, &session, "first").await?;
    let first_turn_id = first
        .turn_id
        .expect("first start_turn should return the active turn ID");

    let queued = queue_message(&client, &session, "second").await?;
    assert!(queued.queued);
    assert!(queued.started_turn_id.is_none());

    let queued_snapshot = snapshot(&client, &session).await?;
    assert_eq!(
        queued_snapshot.active_turn_id.as_deref(),
        Some(first_turn_id.as_str())
    );
    assert_eq!(queued_snapshot.pending_message_count, 1);

    let drained_after_first_completion =
        await_snapshot_matching(&client, &session, Duration::from_secs(45), |current| {
            current.pending_message_count == 0
                && current.active_turn_id.as_deref() != Some(first_turn_id.as_str())
                && current
                    .last_outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.kind == "Completed")
        })
        .await?;

    let completion_outcome = drained_after_first_completion
        .last_outcome
        .expect("a completion should be recorded");
    assert_eq!(completion_outcome.kind, "Completed");
    assert!(!completion_outcome.turn_id.trim().is_empty());
    assert!(
        !completion_outcome.message.trim().is_empty(),
        "completion should include a non-empty assistant summary: {completion_outcome:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn request_cancel_forwards_to_turn_execution() -> Result<()> {
    // Pins: Session/request_cancel resolves the active TurnExecution workflow cancellation path.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "cancel").await?;

    let started = start_turn(&client, &session, "long").await?;
    let turn_id = started
        .turn_id
        .expect("start_turn should return the active turn ID");

    let cancel = request_cancel(&client, &session, "user-requested").await?;
    assert!(cancel.cancelled);
    assert_eq!(cancel.reason, format!("cancel forwarded to turn {turn_id}"));

    let cancelled =
        await_turn_phase(&client, &turn_id, "Cancelled", Duration::from_secs(10)).await?;
    assert_eq!(cancelled.turn_id, turn_id);
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.cancel_reason.as_deref(), Some("user-requested"));

    let final_snapshot =
        await_snapshot_matching(&client, &session, Duration::from_secs(10), |current| {
            current.active_turn_id.is_none()
                && current.last_outcome.as_ref().is_some_and(|outcome| {
                    outcome.turn_id == turn_id && outcome.kind == "Cancelled"
                })
        })
        .await?;
    assert_eq!(final_snapshot.pending_message_count, 0);
    Ok(())
}
