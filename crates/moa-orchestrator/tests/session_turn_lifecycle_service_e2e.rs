//! End-to-end smoke coverage for the additive Session to TurnExecution path.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::{
    types::contact::SessionActorRef, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_test_support::fixtures::fresh_client_message_id;
use serde::Deserialize;

use crate::support::restate_runtime::{grant_tenant_operator, test_user_identity, with_identity};

#[path = "support/mod.rs"]
mod support;

#[derive(Debug, Deserialize)]
struct StartTurnResponse {
    turn_id: Option<String>,
    queued: bool,
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
}

fn ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".to_string())
}

fn session_url(session_id: &str, handler: &str) -> String {
    format!(
        "{}/restate/call/Session/{session_id}/{handler}",
        ingress_url()
    )
}

fn session_store_url(handler: &str) -> String {
    format!("{}/restate/call/SessionStore/{handler}", ingress_url())
}

fn live_model() -> &'static str {
    if std::env::var("MOA_ANTHROPIC_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "claude-sonnet-4-6";
    }
    if std::env::var("MOA_OPENAI_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return "gpt-5.4-mini";
    }
    if std::env::var("MOA_GOOGLE_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
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
        .json(&serde_json::json!({
            "client_message_id": fresh_client_message_id(),
            "user_message": message,
        }))
        .send()
        .await
        .context("send Session start_turn")?
        .error_for_status()
        .context("Session start_turn should succeed")?
        .json::<StartTurnResponse>()
        .await
        .context("deserialize Session start_turn response")
}

async fn start_queued_turn(
    client: &reqwest::Client,
    session: &InitializedSession,
    message: &str,
) -> Result<StartTurnResponse> {
    let request = client.post(session_url(&session.id, "start_turn"));
    with_identity(request, &session.identity)
        .json(&serde_json::json!({
            "client_message_id": fresh_client_message_id(),
            "user_message": message,
        }))
        .send()
        .await
        .context("send queued Session start_turn")?
        .error_for_status()
        .context("queued Session start_turn should succeed")?
        .json::<StartTurnResponse>()
        .await
        .context("deserialize queued Session start_turn response")
}

/// Submits one `start_turn` under a caller-chosen retry identity, without asserting success.
///
/// Retry and conflict coverage needs both the raw status and the ability to reuse one
/// client message id across attempts, which the success-only helpers deliberately hide.
async fn post_start_turn(
    client: &reqwest::Client,
    session: &InitializedSession,
    client_message_id: &str,
    message: &str,
) -> Result<reqwest::Response> {
    let request = client.post(session_url(&session.id, "start_turn"));
    with_identity(request, &session.identity)
        .json(&serde_json::json!({
            "client_message_id": client_message_id,
            "user_message": message,
        }))
        .send()
        .await
        .context("send Session start_turn")
}

/// Counts durable user-message events carrying `text`.
///
/// Persisted events are externally tagged as `{"type": ..., "data": ...}` with a
/// snake_case `event_type` discriminator, which is the shape a client actually reads.
fn user_message_count(progress: &SessionProgress, text: &str) -> usize {
    progress
        .events
        .iter()
        .filter(|event| {
            event.pointer("/event/type") == Some(&serde_json::json!("UserMessage"))
                && event.pointer("/event/data/text") == Some(&serde_json::json!(text))
        })
        .count()
}

async fn request_cancel(
    client: &reqwest::Client,
    session: &InitializedSession,
    reason: &str,
) -> Result<CancelResponse> {
    let request = client.post(session_url(&session.id, "request_cancel"));
    with_identity(request, &session.identity)
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
async fn retried_start_turn_replays_one_admission_instead_of_buying_a_second_turn() -> Result<()> {
    // Pins: the reason this task exists. A client that never saw its first response resends the
    // same message identity; the session must answer with the original turn and leave exactly
    // one user message in durable history, not run and bill a second turn.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "retry-fence").await?;
    let client_message_id = format!("retry-fence:{}", session.id);
    let text = "summarize the attached policy";

    let first = post_start_turn(&client, &session, &client_message_id, text).await?;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    let first: StartTurnResponse = first
        .json()
        .await
        .context("deserialize first start_turn response")?;
    let turn_id = first
        .turn_id
        .clone()
        .expect("first submission should start a turn");
    assert!(!first.queued);

    let retry = post_start_turn(&client, &session, &client_message_id, text).await?;
    assert_eq!(retry.status(), reqwest::StatusCode::OK);
    let retry: StartTurnResponse = retry
        .json()
        .await
        .context("deserialize retried start_turn response")?;

    assert_eq!(
        retry.turn_id.as_deref(),
        Some(turn_id.as_str()),
        "a retry must return the original turn id"
    );
    assert_eq!(retry.queued, first.queued);

    let progress =
        await_session_progress_matching(&client, &session, Duration::from_secs(30), |current| {
            user_message_count(current, text) > 0
        })
        .await?;
    assert_eq!(
        user_message_count(&progress, text),
        1,
        "a retried admission must not append a second user message: {progress:?}"
    );
    assert!(
        progress.snapshot.active_turn_id.as_deref() == Some(turn_id.as_str())
            || progress
                .snapshot
                .last_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.turn_id == turn_id),
        "only the original turn may exist for this message: {progress:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn reusing_a_client_message_id_for_different_text_conflicts_without_mutation() -> Result<()> {
    // Pins: one caller identity means one request. Reusing it for different work is a typed
    // conflict decided before any Session mutation, so the second text never reaches history.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "hash-conflict").await?;
    let client_message_id = format!("hash-conflict:{}", session.id);
    let admitted_text = "check the invoice total";
    let changed_text = "wire the invoice total";

    let admitted = post_start_turn(&client, &session, &client_message_id, admitted_text).await?;
    assert_eq!(admitted.status(), reqwest::StatusCode::OK);

    let conflicted = post_start_turn(&client, &session, &client_message_id, changed_text).await?;
    assert_eq!(
        conflicted.status(),
        reqwest::StatusCode::CONFLICT,
        "a changed request under an admitted id must be refused"
    );

    let progress =
        await_session_progress_matching(&client, &session, Duration::from_secs(30), |current| {
            user_message_count(current, admitted_text) > 0
        })
        .await?;
    assert_eq!(
        user_message_count(&progress, changed_text),
        0,
        "a conflicted submission must not append its text: {progress:?}"
    );
    assert_eq!(progress.snapshot.pending_message_count, 0);
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
async fn start_turn_during_active_turn_is_drained_after_completion() -> Result<()> {
    // Pins: a queued message is retained while a turn runs, then drains after completion.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "queue").await?;

    let first = start_turn(&client, &session, "first").await?;
    let first_turn_id = first
        .turn_id
        .expect("first start_turn should return the active turn ID");

    let queued = start_queued_turn(&client, &session, "second").await?;
    assert!(queued.queued);
    assert!(queued.turn_id.is_none());

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
    // Pins: Session/request_cancel resolves its private TurnExecution child and
    // publishes the exact cancellation outcome through the public Session projection.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client, "cancel").await?;

    let started = start_turn(&client, &session, "long").await?;
    let turn_id = started
        .turn_id
        .expect("start_turn should return the active turn ID");

    let cancel = request_cancel(&client, &session, "user-requested").await?;
    assert!(cancel.cancelled);
    assert_eq!(cancel.reason, format!("cancel forwarded to turn {turn_id}"));

    let final_snapshot =
        await_snapshot_matching(&client, &session, Duration::from_secs(10), |current| {
            current.active_turn_id.is_none()
                && current.last_outcome.as_ref().is_some_and(|outcome| {
                    outcome.turn_id == turn_id && outcome.kind == "Cancelled"
                })
        })
        .await?;
    assert_eq!(final_snapshot.pending_message_count, 0);
    let outcome = final_snapshot
        .last_outcome
        .expect("cancelled turn should publish a Session outcome");
    assert_eq!(outcome.turn_id, turn_id);
    assert_eq!(outcome.kind, "Cancelled");
    assert_eq!(outcome.message, "user-requested");
    Ok(())
}
