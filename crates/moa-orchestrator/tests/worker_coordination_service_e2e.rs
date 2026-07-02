//! End-to-end coverage for the durable worker coordination wiring on the
//! Session VO (child registry, control-plane signal dedupe, scoped cancellation,
//! and terminal-result caching) driven against a real Restate ingress + Postgres.
//!
//! These tests intentionally exercise the coordination control plane directly
//! through the `Session` (and `Worker`) virtual-object handlers rather than
//! through a coordinator turn that emits `spawn_worker`. The clean-e2e harness
//! runs the orchestrator with the deterministic mock provider
//! (`MOA_PROVIDERS_OVERRIDE=mock:...`), whose only completion is fixed text — it
//! never emits a delegation tool call, so a worker is never spawned through the
//! brain loop. Driving the durable handlers directly lets every assertion land on
//! durable, deterministic outcomes (the Postgres event log and persisted VO state),
//! which is exactly what these wiring tests pin. See the module-level report for the
//! one scenario this constraint forces us to skip (a child still running while the
//! spawn tool returns).

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use moa_core::traits::Identity;
use moa_core::{
    AgentSignalId, CancelScope, ChildSignalKind, ConsumeWorkerChildResultInput,
    ConsumeWorkerChildResultOutput, MarkWorkerChildTerminalInput, ModelId, ParentResumePolicy,
    SessionActorRef, SessionId, SessionMeta, SignalSeverity, TenantId, WorkerChildRef,
    WorkerProgressSummary, WorkerResult, WorkerSignal, WorkerState, WorkerStatus,
    WorkerTerminalResult,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::support::restate_runtime::{grant_tenant_operator, test_user_identity, with_identity};

#[path = "support/mod.rs"]
mod support;

/// Local projection of `Session/progress` carrying only the fields these tests
/// assert on. Extra fields (snapshot, active turn progress) are ignored, and
/// `child_progress` is omitted from the wire payload when empty.
#[derive(Debug, Deserialize)]
struct ProgressView {
    #[serde(default)]
    child_progress: Vec<WorkerProgressSummary>,
    events: Vec<serde_json::Value>,
}

/// A created, authz-granted session used to drive coordination handlers.
struct InitializedSession {
    session_id: SessionId,
    identity: Identity,
}

impl InitializedSession {
    fn key(&self) -> String {
        self.session_id.to_string()
    }
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

fn worker_url(worker_id: &str, handler: &str) -> String {
    format!(
        "{}/restate/call/Worker/{worker_id}/{handler}",
        ingress_url()
    )
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

/// Creates a fresh tenant + session and grants the test identity operator access,
/// mirroring `session_turn_lifecycle_service_e2e`'s setup so each test owns its own
/// session/tenant ids and stays parallel-safe under nextest.
async fn create_initialized_session(client: &reqwest::Client) -> Result<InitializedSession> {
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
        session_id,
        identity,
    })
}

/// Registers a root-owned child ref on the session via the internal
/// `Session/register_child` handler (the same write `TurnExecution` performs).
async fn register_child(
    client: &reqwest::Client,
    session: &InitializedSession,
    child: &WorkerChildRef,
) -> Result<()> {
    client
        .post(session_url(&session.key(), "register_child"))
        .json(child)
        .send()
        .await
        .context("send Session register_child")?
        .error_for_status()
        .context("Session register_child should succeed")?;
    Ok(())
}

/// Caches a terminal child result on the parent via `Session/mark_child_terminal`.
async fn mark_child_terminal(
    client: &reqwest::Client,
    session: &InitializedSession,
    input: &MarkWorkerChildTerminalInput,
) -> Result<()> {
    client
        .post(session_url(&session.key(), "mark_child_terminal"))
        .json(input)
        .send()
        .await
        .context("send Session mark_child_terminal")?
        .error_for_status()
        .context("Session mark_child_terminal should succeed")?;
    Ok(())
}

/// Consumes a cached terminal child result via `Session/consume_child_result`.
async fn consume_child_result(
    client: &reqwest::Client,
    session: &InitializedSession,
    worker_id: &str,
) -> Result<ConsumeWorkerChildResultOutput> {
    client
        .post(session_url(&session.key(), "consume_child_result"))
        .json(&ConsumeWorkerChildResultInput {
            worker_id: worker_id.to_string(),
        })
        .send()
        .await
        .context("send Session consume_child_result")?
        .error_for_status()
        .context("Session consume_child_result should succeed")?
        .json::<ConsumeWorkerChildResultOutput>()
        .await
        .context("deserialize Session consume_child_result response")
}

/// Records a child→parent control-plane signal via `Session/record_child_signal`.
async fn record_child_signal(
    client: &reqwest::Client,
    session: &InitializedSession,
    signal: &WorkerSignal,
) -> Result<()> {
    client
        .post(session_url(&session.key(), "record_child_signal"))
        .json(signal)
        .send()
        .await
        .context("send Session record_child_signal")?
        .error_for_status()
        .context("Session record_child_signal should succeed")?;
    Ok(())
}

/// Registers a deterministic auto-delegation run via `Session/register_auto_delegation_run`
/// (the same write `TurnExecution` performs after scheduling the plan's ready workers).
async fn register_auto_delegation_run(
    client: &reqwest::Client,
    session: &InitializedSession,
    user_sequence_num: u64,
    worker_ids: &[String],
) -> Result<()> {
    client
        .post(session_url(&session.key(), "register_auto_delegation_run"))
        .json(&serde_json::json!({
            "user_sequence_num": user_sequence_num,
            "worker_ids": worker_ids,
        }))
        .send()
        .await
        .context("send Session register_auto_delegation_run")?
        .error_for_status()
        .context("Session register_auto_delegation_run should succeed")?;
    Ok(())
}

/// Polls run-owned fan-in status via `Session/poll_auto_delegation_fan_in`, returning the raw
/// `AutoDelegationFanInStatus` (`"Ready"` / `"NotRunning"` / `{"Pending": {...}}`).
async fn poll_auto_delegation_fan_in(
    client: &reqwest::Client,
    session: &InitializedSession,
    user_sequence_num: u64,
    root_turn_id: &str,
    force_complete: bool,
) -> Result<serde_json::Value> {
    client
        .post(session_url(&session.key(), "poll_auto_delegation_fan_in"))
        .json(&serde_json::json!({
            "user_sequence_num": user_sequence_num,
            "root_turn_id": root_turn_id,
            "force_complete": force_complete,
        }))
        .send()
        .await
        .context("send Session poll_auto_delegation_fan_in")?
        .error_for_status()
        .context("Session poll_auto_delegation_fan_in should succeed")?
        .json::<serde_json::Value>()
        .await
        .context("deserialize Session poll_auto_delegation_fan_in response")
}

/// Reads the root-owned active child registry via the shared `Session/child_refs`.
async fn child_refs(
    client: &reqwest::Client,
    session: &InitializedSession,
) -> Result<Vec<WorkerChildRef>> {
    client
        .post(session_url(&session.key(), "child_refs"))
        .send()
        .await
        .context("send Session child_refs")?
        .error_for_status()
        .context("Session child_refs should succeed")?
        .json::<Vec<WorkerChildRef>>()
        .await
        .context("deserialize Session child_refs response")
}

/// Requests a scoped cancellation via `Session/cancel` (participant-gated).
async fn cancel_session(
    client: &reqwest::Client,
    session: &InitializedSession,
    scope: CancelScope,
) -> Result<()> {
    let request = client.post(session_url(&session.key(), "cancel"));
    with_identity(request, &session.identity)
        .json(&scope)
        .send()
        .await
        .context("send Session cancel")?
        .error_for_status()
        .context("Session cancel should succeed")?;
    Ok(())
}

/// Reads combined `Session/progress` (participant-gated) into the local projection.
async fn session_progress(
    client: &reqwest::Client,
    session: &InitializedSession,
) -> Result<ProgressView> {
    let request = client.post(session_url(&session.key(), "progress"));
    with_identity(request, &session.identity)
        .json(&serde_json::json!({ "event_range": { "limit": 50 } }))
        .send()
        .await
        .context("send Session progress")?
        .error_for_status()
        .context("Session progress should succeed")?
        .json::<ProgressView>()
        .await
        .context("deserialize Session progress")
}

/// Reads a child VO's read-only status via the shared `Worker/status` handler.
async fn worker_status(client: &reqwest::Client, worker_id: &str) -> Result<WorkerStatus> {
    client
        .post(worker_url(worker_id, "status"))
        .send()
        .await
        .context("send Worker status")?
        .error_for_status()
        .context("Worker status should succeed")?
        .json::<WorkerStatus>()
        .await
        .context("deserialize Worker status")
}

/// Counts `WorkerSignalReceived` events for one `signal_id` in a progress payload.
///
/// Each progress event is a serialized `EventRecord`; the durable `event` payload is
/// adjacently tagged (`{"type": ..., "data": {...}}`), so the signal's identity lives
/// at `event.data.signal_id`.
fn count_signal_events(progress: &ProgressView, signal_id: &str) -> usize {
    progress
        .events
        .iter()
        .filter(|record| {
            let event = &record["event"];
            event["type"] == "WorkerSignalReceived"
                && event["data"]["signal_id"] == serde_json::Value::String(signal_id.to_string())
        })
        .count()
}

/// Returns the `WorkerResultBundle` events for one `user_sequence_num` in a progress payload.
///
/// Each progress event is a serialized `EventRecord`; the durable `event` payload is adjacently
/// tagged (`{"type": ..., "data": {...}}`), so the bundle's sequence lives at
/// `event.data.user_sequence_num` and its ordered results at `event.data.results`.
fn bundle_events(progress: &ProgressView, user_sequence_num: u64) -> Vec<serde_json::Value> {
    progress
        .events
        .iter()
        .filter(|record| {
            let event = &record["event"];
            event["type"] == "WorkerResultBundle"
                && event["data"]["user_sequence_num"] == serde_json::json!(user_sequence_num)
        })
        .map(|record| record["event"].clone())
        .collect()
}

/// Polls `Session/progress` until `predicate` holds or `timeout` elapses.
async fn await_progress_matching<F>(
    client: &reqwest::Client,
    session: &InitializedSession,
    timeout: Duration,
    predicate: F,
) -> Result<ProgressView>
where
    F: Fn(&ProgressView) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let progress = session_progress(client, session).await?;
        if predicate(&progress) {
            return Ok(progress);
        }
        last = Some(progress);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!(
        "session {} progress did not match within {:?}; last child_progress={:?}, events={}",
        session.key(),
        timeout,
        last.as_ref().map(|p| &p.child_progress),
        last.as_ref().map_or(0, |p| p.events.len()),
    )
}

/// Polls a child VO's status until it reaches `target` or `timeout` elapses.
async fn await_worker_state(
    client: &reqwest::Client,
    worker_id: &str,
    target: WorkerState,
    timeout: Duration,
) -> Result<WorkerStatus> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let status = worker_status(client, worker_id).await?;
        if status.state == target {
            return Ok(status);
        }
        last = Some(status);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("worker {worker_id} did not reach {target:?} within {timeout:?}; last status: {last:?}")
}

fn unique_child_id() -> String {
    format!("child-{}", Uuid::now_v7())
}

fn completed_terminal(worker_id: &str, output: &str) -> WorkerTerminalResult {
    WorkerTerminalResult {
        state: WorkerState::Completed,
        result: WorkerResult {
            worker_id: worker_id.to_string(),
            success: true,
            output: output.to_string(),
            tokens_used: 42,
            tools_invoked: 1,
            error: None,
        },
    }
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn session_progress_includes_child_progress_service_e2e() -> Result<()> {
    // Pins: after a child is registered and reaches terminal on the parent, Session/progress
    // surfaces a non-empty child_progress fan-in carrying that child's terminal summary
    // (synthesized from the cached parent ref, no live child call).
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let child_id = unique_child_id();

    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: child_id.clone(),
            task_hash: "task-hash-progress".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;
    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: child_id.clone(),
            terminal: completed_terminal(&child_id, "child finished the research"),
        },
    )
    .await?;

    let progress = await_progress_matching(&client, &session, Duration::from_secs(10), |p| {
        !p.child_progress.is_empty()
    })
    .await?;

    let summary = progress
        .child_progress
        .iter()
        .find(|summary| summary.worker_id == child_id)
        .with_context(|| format!("child_progress should include {child_id}"))?;
    assert_eq!(summary.state, WorkerState::Completed);
    assert_eq!(
        summary.last_summary.as_deref(),
        Some("child finished the research"),
        "terminal child summary should carry the child's output: {summary:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn coordinator_only_cancel_leaves_child_active_service_e2e() -> Result<()> {
    // Pins: Session/cancel(CoordinatorOnly) cancels the coordinator turn only — it does NOT
    // cascade to registered children. The child registry stays intact and the child VO is
    // never told to cancel (its status stays Uninitialized, never Cancelled). This is the
    // deliberate contrast to the TaskTree cascade test below.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let child_id = unique_child_id();

    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: child_id.clone(),
            task_hash: "task-hash-coordinator-only".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;

    cancel_session(&client, &session, CancelScope::CoordinatorOnly).await?;

    let refs = child_refs(&client, &session).await?;
    let child = refs
        .iter()
        .find(|child| child.id == child_id)
        .with_context(|| format!("CoordinatorOnly cancel must keep child {child_id} registered"))?;
    assert!(
        child.terminal.is_none(),
        "CoordinatorOnly cancel must not terminate the child: {child:?}"
    );

    // The child VO received no cancel: an uninitialized child reports Uninitialized, never
    // Cancelled. (No background path cancels a child, so this single read is deterministic.)
    let status = worker_status(&client, &child_id).await?;
    assert_eq!(
        status.state,
        WorkerState::Uninitialized,
        "CoordinatorOnly cancel must not cascade to the child VO: {status:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn task_tree_cancel_cancels_child_service_e2e() -> Result<()> {
    // Pins: Session/cancel(TaskTree) cascades cancellation to every registered child. The
    // cascade is a fire-and-forget VO call, so we poll the child VO until it durably reports
    // Cancelled.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let child_id = unique_child_id();

    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: child_id.clone(),
            task_hash: "task-hash-task-tree".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;

    cancel_session(&client, &session, CancelScope::TaskTree).await?;

    let status = await_worker_state(
        &client,
        &child_id,
        WorkerState::Cancelled,
        Duration::from_secs(10),
    )
    .await?;
    assert_eq!(status.state, WorkerState::Cancelled);
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn duplicate_child_signal_is_idempotent_service_e2e() -> Result<()> {
    // Pins: delivering the same control-plane signal twice (same signal_id) appends exactly
    // one durable WorkerSignalReceived event — the dedupe key (worker_signal:{id}) makes
    // the retried delivery a no-op at the event log. A Finding under a Never resume policy
    // arms no coordinator resume, so the only signal-derived event is the deduped one.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let child_id = unique_child_id();
    let signal_id = AgentSignalId::new();

    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: child_id.clone(),
            task_hash: "task-hash-signal-dedupe".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;

    let signal = WorkerSignal {
        signal_id,
        worker_id: child_id,
        parent_session: session.session_id,
        kind: ChildSignalKind::Finding,
        severity: SignalSeverity::Info,
        summary: "intermediate finding worth recording".to_string(),
        payload: serde_json::Value::Null,
        created_at: Utc::now(),
        resume_policy: ParentResumePolicy::Never,
        input_request_id: None,
        input_audience: None,
    };

    // Same signal delivered twice — the second is a retried delivery.
    record_child_signal(&client, &session, &signal).await?;
    record_child_signal(&client, &session, &signal).await?;

    let signal_id_str = signal_id.to_string();
    let progress = await_progress_matching(&client, &session, Duration::from_secs(10), |p| {
        count_signal_events(p, &signal_id_str) >= 1
    })
    .await?;

    assert_eq!(
        count_signal_events(&progress, &signal_id_str),
        1,
        "duplicate signal delivery must append exactly one WorkerSignalReceived event"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn terminal_child_caches_and_consumes_result_service_e2e() -> Result<()> {
    // Pins: a child reaching terminal caches its result on the parent (the existing terminal
    // path), and the cached result is consumable exactly once before the child ref is dropped
    // from the active registry.
    //
    // NOTE: this asserts the durable parent-side cache/consume semantics. The accompanying
    // WorkerStatusChanged / WorkerNotificationDelivered events are emitted by the live
    // Worker terminal-delivery path (the child workflow), which the deterministic mock
    // provider cannot drive end-to-end (it never spawns a real child turn), so they are out
    // of reach for this harness and are not asserted here.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let child_id = unique_child_id();

    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: child_id.clone(),
            task_hash: "task-hash-terminal".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;
    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: child_id.clone(),
            terminal: completed_terminal(&child_id, "child completed the delegated work"),
        },
    )
    .await?;

    let first = consume_child_result(&client, &session, &child_id).await?;
    let terminal = first
        .terminal
        .context("first consume should return the cached terminal result")?;
    assert_eq!(terminal.state, WorkerState::Completed);
    assert_eq!(terminal.result.output, "child completed the delegated work");
    assert!(terminal.result.success);

    // Consuming a cached terminal is exactly-once: a second consume yields nothing.
    let second = consume_child_result(&client, &session, &child_id).await?;
    assert!(
        second.terminal.is_none(),
        "terminal result must be consumed exactly once: {second:?}"
    );

    // Consuming the terminal removes the child from the active registry.
    let refs = child_refs(&client, &session).await?;
    assert!(
        refs.iter().all(|child| child.id != child_id),
        "consumed terminal child must be dropped from child_refs: {refs:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn auto_delegation_fan_in_emits_single_ordered_bundle_service_e2e() -> Result<()> {
    // Pins (B1 slow-sibling fan-in + bundle dedupe): once every scheduled worker of an
    // auto-delegation run reaches terminal, the Session VO emits exactly one durable
    // WorkerResultBundle whose results follow the SCHEDULED order (not completion order), and a
    // handler replay (Restate re-invoking mark_child_terminal, plus the root turn re-polling
    // fan-in) never appends a second bundle. Driven directly through the durable VO handlers so no
    // synthesis turn runs (no owning identity is recorded without a real turn), which keeps the
    // event-log assertions deterministic under the mock provider.
    const FAN_IN_SEQ: u64 = 1;
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let worker_a = unique_child_id();
    let worker_b = unique_child_id();

    for (id, task) in [(&worker_a, "task-a"), (&worker_b, "task-b")] {
        register_child(
            &client,
            &session,
            &WorkerChildRef {
                id: id.clone(),
                task_hash: format!("hash-{task}"),
                budget_tokens: 4_096,
                terminal: None,
            },
        )
        .await?;
    }

    // Schedule order is [worker_b, worker_a]; workers finish in the OPPOSITE order below, so the
    // bundle order can only match if fan-in preserves the scheduled order rather than completion.
    register_auto_delegation_run(
        &client,
        &session,
        FAN_IN_SEQ,
        &[worker_b.clone(), worker_a.clone()],
    )
    .await?;

    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: worker_a.clone(),
            terminal: completed_terminal(&worker_a, "a finished first"),
        },
    )
    .await?;
    // Not complete yet: the slow sibling (worker_b) is still pending, so no bundle is emitted.
    let mid = session_progress(&client, &session).await?;
    assert!(
        bundle_events(&mid, FAN_IN_SEQ).is_empty(),
        "no bundle before every scheduled worker is terminal"
    );
    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: worker_b.clone(),
            terminal: completed_terminal(&worker_b, "b finished last"),
        },
    )
    .await?;

    let progress = await_progress_matching(&client, &session, Duration::from_secs(10), |p| {
        !bundle_events(p, FAN_IN_SEQ).is_empty()
    })
    .await?;
    let bundles = bundle_events(&progress, FAN_IN_SEQ);
    assert_eq!(
        bundles.len(),
        1,
        "fan-in emits exactly one WorkerResultBundle: {bundles:?}"
    );
    let results = bundles[0]["data"]["results"]
        .as_array()
        .context("bundle carries a results array")?;
    let order: Vec<&str> = results
        .iter()
        .map(|result| result["result"]["worker_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        order,
        vec![worker_b.as_str(), worker_a.as_str()],
        "bundle results follow scheduled order, not completion order"
    );

    // Handler replay 1: Restate may redeliver the terminal write; a replayed mark_child_terminal
    // must not re-emit (the run's bundle_emitted guard makes it a no-op).
    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: worker_b.clone(),
            terminal: completed_terminal(&worker_b, "redelivered"),
        },
    )
    .await?;
    // Handler replay 2: the active root turn re-polling fan-in after emission reports Ready and,
    // via the same auto_delegation_fan_in:{seq} dedupe key, appends no second bundle.
    let status =
        poll_auto_delegation_fan_in(&client, &session, FAN_IN_SEQ, "root-turn-replay", false)
            .await?;
    assert_eq!(
        status,
        serde_json::json!("Ready"),
        "re-poll after emission reports Ready without re-emitting"
    );

    let after_replay = session_progress(&client, &session).await?;
    assert_eq!(
        bundle_events(&after_replay, FAN_IN_SEQ).len(),
        1,
        "handler replay must not append a second WorkerResultBundle"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn unregistered_worker_signal_records_no_event_service_e2e() -> Result<()> {
    // Pins (signal-ownership negative): a control-plane signal whose parent_session matches this
    // session but whose worker is NOT a registered child appends ZERO WorkerSignalReceived events.
    // The handler treats it as a success no-op (a worker that raced its own remove_child/self-clean),
    // never a 403 that would permanently drop a legitimate late signal — but it injects nothing into
    // the durable log. A second, registered "control" signal delivered afterward proves the log was
    // observed, so the zero-count assertion is not merely a not-yet-landed event.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;

    // The session owns one child; the negative signal targets a DIFFERENT, unregistered worker.
    let registered = unique_child_id();
    register_child(
        &client,
        &session,
        &WorkerChildRef {
            id: registered.clone(),
            task_hash: "task-hash-owned".to_string(),
            budget_tokens: 4_096,
            terminal: None,
        },
    )
    .await?;

    let unregistered_signal_id = AgentSignalId::new();
    let unregistered_signal = WorkerSignal {
        signal_id: unregistered_signal_id,
        worker_id: unique_child_id(),
        parent_session: session.session_id,
        kind: ChildSignalKind::Blocked,
        severity: SignalSeverity::Warning,
        summary: "signal from a worker not registered on this session".to_string(),
        payload: serde_json::Value::Null,
        created_at: Utc::now(),
        resume_policy: ParentResumePolicy::Never,
        input_request_id: None,
        input_audience: None,
    };
    // The handler returns success (a no-op), not an error, for a parent-matching unregistered worker.
    record_child_signal(&client, &session, &unregistered_signal).await?;

    // Control signal for the registered child, delivered AFTER the unregistered one. Session VO
    // handlers run serially per key, so by the time this control event is durable the earlier
    // unregistered signal has already been fully processed — if it recorded an event, it would be
    // visible now too.
    let control_signal_id = AgentSignalId::new();
    let control_signal = WorkerSignal {
        signal_id: control_signal_id,
        worker_id: registered,
        parent_session: session.session_id,
        kind: ChildSignalKind::Finding,
        severity: SignalSeverity::Info,
        summary: "registered control finding".to_string(),
        payload: serde_json::Value::Null,
        created_at: Utc::now(),
        resume_policy: ParentResumePolicy::Never,
        input_request_id: None,
        input_audience: None,
    };
    record_child_signal(&client, &session, &control_signal).await?;

    let control_id_str = control_signal_id.to_string();
    let progress = await_progress_matching(&client, &session, Duration::from_secs(10), |p| {
        count_signal_events(p, &control_id_str) >= 1
    })
    .await?;

    let unregistered_id_str = unregistered_signal_id.to_string();
    assert_eq!(
        count_signal_events(&progress, &unregistered_id_str),
        0,
        "an unregistered worker's signal must append no WorkerSignalReceived event"
    );
    assert_eq!(
        count_signal_events(&progress, &control_id_str),
        1,
        "the registered control signal is recorded, proving the event log was observed"
    );
    Ok(())
}
