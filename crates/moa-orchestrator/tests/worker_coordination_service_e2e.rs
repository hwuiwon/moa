//! End-to-end coverage for the durable worker coordination wiring on the
//! Session VO (child registry, control-plane signal dedupe, scoped cancellation,
//! and terminal-result caching) driven against a real Restate ingress + Postgres.
//!
//! Most tests intentionally exercise the coordination control plane directly
//! through the `Session` (and `Worker`) virtual-object handlers. The recovery
//! matrix scenario instead boots the dedicated scripted-provider/MCP fixture so
//! three real conversational workers can be held at an exact external-effect
//! barrier across a hard orchestrator restart.

#![cfg(feature = "integration")]

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::{
    events::Event, types::action_policy::ActionPolicyEffect, types::contact::SessionActorRef,
    types::events_stream::EventRange, types::identifiers::AgentSignalId,
    types::identifiers::ModelId, types::identifiers::SessionId, types::identifiers::TenantId,
    types::session::CancelScope, types::session::SessionMeta,
    types::worker::commands::ConsumeWorkerChildResultInput,
    types::worker::commands::ConsumeWorkerChildResultOutput,
    types::worker::commands::MarkWorkerChildTerminalInput, types::worker::state::ChildSignalKind,
    types::worker::state::ParentResumePolicy, types::worker::state::SignalSeverity,
    types::worker::state::WorkerChildRef, types::worker::state::WorkerProgressSummary,
    types::worker::state::WorkerResult, types::worker::state::WorkerSignal,
    types::worker::state::WorkerState, types::worker::state::WorkerStatus,
    types::worker::state::WorkerTerminalResult,
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    OrchestratorTestFixture, TestApiClient,
};
use moa_wire::turn::{SessionProgress, SessionProgressRequest, TurnOutcomeKind};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::support::restate_runtime::{grant_tenant_operator, test_user_identity, with_identity};
use crate::support::session_store_service::start_turn_request;

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

const RECOVERY_REQUEST: &str =
    "Run the RECOVERY-MATRIX fan-in check with three independent workers.";
const RECOVERY_CLASSIFIER_PROMPT: &str =
    "You classify one user turn into MOA's public execution decision.";
const RECOVERY_MCP_SERVER: &str = "fixture-capability";
const RECOVERY_WORKERS: [(&str, &str, &str); 3] = [
    (
        "RECOVERY-WORKER-A: execute the durable A probe.",
        "recovery_probe_a",
        "RECOVERY-A-DONE",
    ),
    (
        "RECOVERY-WORKER-B: execute the durable B probe.",
        "recovery_probe_b",
        "RECOVERY-B-DONE",
    ),
    (
        "RECOVERY-WORKER-C: execute the durable C probe.",
        "recovery_probe_c",
        "RECOVERY-C-DONE",
    ),
];

fn recovery_tool_reference(tool_name: &str) -> String {
    moa_hands::mcp_tool_reference(RECOVERY_MCP_SERVER, tool_name)
}

fn recovery_script() -> serde_json::Value {
    let worker_completions = RECOVERY_WORKERS.iter().map(|(_, _, result)| {
        json!({
            "match": result,
            "completion": { "content": format!("Worker retained {result}."), "tool_calls": [] }
        })
    });
    let worker_calls = RECOVERY_WORKERS.iter().map(|(task, tool_name, _)| {
        json!({
            "match": task,
            "completion": {
                "content": "",
                "tool_calls": [{
                    "name": recovery_tool_reference(tool_name),
                    "id": format!("{tool_name}-call"),
                    "input": { "worker": tool_name }
                }]
            }
        })
    });
    let spawn_calls = RECOVERY_WORKERS
        .iter()
        .map(|(task, tool_name, _)| {
            json!({
                "name": "spawn_worker",
                "id": format!("spawn-{tool_name}"),
                "input": {
                    "task": task,
                    "tool_subset": [recovery_tool_reference(tool_name)],
                    "budget_tokens": 1_200,
                    "max_turns": 2
                }
            })
        })
        .collect::<Vec<_>>();
    let mut keyed = worker_completions.chain(worker_calls).collect::<Vec<_>>();
    keyed.extend([
        json!({
            "match": "Spawned worker",
            "completion": { "content": "All recovery workers were dispatched.", "tool_calls": [] }
        }),
        json!({
            "match": RECOVERY_CLASSIFIER_PROMPT,
            "completion": {
                "content": r#"{"label":"execute","strategy":"inline","rationale":"The bounded work should run in parallel conversational workers.","confidence_bps":10000,"missing_inputs":[]}"#,
                "tool_calls": []
            }
        }),
        json!({
            "match": RECOVERY_REQUEST,
            "completion": { "content": "", "tool_calls": spawn_calls }
        }),
    ]);
    json!({
        "default": { "completion": { "content": "unexpected recovery fallback", "tool_calls": [] } },
        "keyed": keyed
    })
}

fn recovery_capability_options() -> FixtureCapabilityOptions {
    FixtureCapabilityOptions {
        tools: RECOVERY_WORKERS
            .iter()
            .map(|(_, tool_name, result)| FixtureCapabilityTool {
                name: (*tool_name).to_string(),
                description: format!("Block and release the deterministic {tool_name} effect"),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["worker"],
                    "properties": { "worker": { "type": "string" } }
                }),
                item_key_pointer: None,
                idempotent: true,
                outcomes: vec![FixtureCapabilityOutcome::Success {
                    output: json!({ "result": result }),
                }],
            })
            .collect(),
        orchestrator_env: vec![("RUST_LOG".to_string(), "error".to_string())],
    }
}

async fn seed_recovery_allow_rules(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    tenant_id: TenantId,
) -> Result<()> {
    fixture
        .grant_default_tenant_admin(tenant_id)
        .await
        .context("grant tenant admin before recovery policy setup")?;
    for (_, tool_name, _) in RECOVERY_WORKERS {
        client
            .post_void(
                "/ActionPolicy/upsert_rule",
                &UpsertActionPolicyRuleRequest {
                    tenant_id,
                    contact_id: None,
                    tool_name: recovery_tool_reference(tool_name),
                    pattern: "*".to_string(),
                    effect: ActionPolicyEffect::Allow,
                    reason: Some("deterministic recovery matrix fixture".to_string()),
                },
            )
            .await
            .with_context(|| format!("allow recovery fixture tool {tool_name}"))?;
    }
    Ok(())
}

async fn fixture_progress(
    client: &TestApiClient,
    session_id: SessionId,
) -> Result<SessionProgress> {
    client
        .post_call(
            &format!("/Session/{session_id}/progress"),
            &SessionProgressRequest {
                event_range: EventRange::all(),
            },
        )
        .await
        .context("read recovery session progress")
}

async fn await_worker_partition(
    client: &TestApiClient,
    session_id: SessionId,
    completed: usize,
    running: usize,
    timeout: Duration,
) -> Result<SessionProgress> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let progress = fixture_progress(client, session_id).await?;
        let completed_count = progress
            .child_progress
            .iter()
            .filter(|worker| worker.state == WorkerState::Completed)
            .count();
        let running_count = progress
            .child_progress
            .iter()
            .filter(|worker| worker.state == WorkerState::Running)
            .count();
        if progress.child_progress.len() == RECOVERY_WORKERS.len()
            && completed_count == completed
            && running_count == running
        {
            return Ok(progress);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "worker partition did not reach completed={completed}, running={running} within {timeout:?}; last summaries: {:?}",
                progress.child_progress
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn await_exact_recovery_attempts(
    controller: &moa_test_support::FixtureCapabilityController,
    completed_capability: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let attempts = controller.transport_attempts();
        let counts = RECOVERY_WORKERS
            .iter()
            .map(|(_, tool_name, _)| {
                (
                    *tool_name,
                    attempts
                        .iter()
                        .filter(|attempt| attempt.capability == *tool_name)
                        .count(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let exact = counts.iter().all(|(tool_name, count)| {
            let expected = if *tool_name == completed_capability {
                1
            } else {
                2
            };
            *count == expected
        });
        let exceeded = counts.iter().any(|(tool_name, count)| {
            let expected = if *tool_name == completed_capability {
                1
            } else {
                2
            };
            *count > expected
        });
        if exceeded {
            bail!("a recovery capability replayed more than once: {counts:?}");
        }
        if exact {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("recovery attempts did not reach the exact 1/2/2 partition: {counts:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn await_recovery_terminal_events(
    client: &TestApiClient,
    session_id: SessionId,
    timeout: Duration,
) -> Result<Vec<moa_core::types::events_stream::EventRecord>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let events = client
            .get_events(session_id, EventRange::all())
            .await
            .context("load durable worker recovery events")?;
        let completed_changes = events
            .iter()
            .filter(|record| {
                matches!(
                    record.event,
                    Event::WorkerStatusChanged {
                        to: WorkerState::Completed,
                        ..
                    }
                )
            })
            .count();
        let completed_notifications = events
            .iter()
            .filter(|record| {
                matches!(
                    record.event,
                    Event::WorkerNotificationDelivered {
                        state: WorkerState::Completed,
                        ..
                    }
                )
            })
            .count();
        if completed_changes > RECOVERY_WORKERS.len()
            || completed_notifications > RECOVERY_WORKERS.len()
        {
            bail!(
                "worker recovery emitted duplicate terminal events: changes={completed_changes}, notifications={completed_notifications}"
            );
        }
        if completed_changes == RECOVERY_WORKERS.len()
            && completed_notifications == RECOVERY_WORKERS.len()
        {
            return Ok(events);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "worker terminal events did not settle within {timeout:?}: changes={completed_changes}, notifications={completed_notifications}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires a running Restate ingress and moa-orchestrator deployment"]
async fn session_progress_includes_child_progress_service_e2e() -> Result<()> {
    // Pins: Session/progress collects active child summaries around an immediate cached terminal
    // summary, then restores registration order regardless of completion order.
    let client = reqwest::Client::new();
    let session = create_initialized_session(&client).await?;
    let active_before = unique_child_id();
    let terminal_child = unique_child_id();
    let active_after = unique_child_id();

    for (index, child_id) in [&active_before, &terminal_child, &active_after]
        .into_iter()
        .enumerate()
    {
        register_child(
            &client,
            &session,
            &WorkerChildRef {
                id: child_id.clone(),
                task_hash: format!("task-hash-progress-{index}"),
                budget_tokens: 4_096,
                terminal: None,
            },
        )
        .await?;
    }
    mark_child_terminal(
        &client,
        &session,
        &MarkWorkerChildTerminalInput {
            worker_id: terminal_child.clone(),
            terminal: completed_terminal(&terminal_child, "child finished the research"),
        },
    )
    .await?;

    let progress = await_progress_matching(&client, &session, Duration::from_secs(10), |p| {
        p.child_progress.len() == 3
    })
    .await?;

    let ordered_worker_ids: Vec<&str> = progress
        .child_progress
        .iter()
        .map(|summary| summary.worker_id.as_str())
        .collect();
    assert_eq!(
        ordered_worker_ids,
        vec![
            active_before.as_str(),
            terminal_child.as_str(),
            active_after.as_str(),
        ],
        "concurrent child-progress completion must be restored to registration order"
    );
    let summary = progress
        .child_progress
        .iter()
        .find(|summary| summary.worker_id == terminal_child)
        .with_context(|| format!("child_progress should include {terminal_child}"))?;
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
        created_at: moa_test_support::fixtures::pg_now(),
        resume_policy: ParentResumePolicy::Never,
        input_request: None,
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
        created_at: moa_test_support::fixtures::pg_now(),
        resume_policy: ParentResumePolicy::Never,
        input_request: None,
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
        created_at: moa_test_support::fixtures::pg_now(),
        resume_policy: ParentResumePolicy::Never,
        input_request: None,
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

#[tokio::test]
#[ignore = "requires Docker for the restartable Restate/Postgres/OpenFGA/Valkey fixture"]
async fn recovery_matrix_three_workers_retain_and_resume_once_service_e2e() -> Result<()> {
    // Pins: after one of three conversational workers commits its terminal result and two
    // siblings remain blocked on exact MCP effects, a hard orchestrator restart retains the
    // completed child and resumes each unfinished child exactly once without duplicating any
    // upstream effect, tool result, or terminal notification.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        recovery_script(),
        recovery_capability_options(),
    )
    .await
    .context("boot restartable worker recovery fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("worker-fan-in-recovery").await?;
    let session = test.client().get_session(session_id).await?;
    seed_recovery_allow_rules(&fixture, test.client(), session.tenant_id).await?;

    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(start_turn_request(RECOVERY_REQUEST), None)
        .await
        .context("start three-worker recovery turn")?;
    let turn_id = started
        .turn_id
        .context("idle recovery session should start its turn immediately")?;
    let controller = fixture
        .fixture_capability()
        .context("worker recovery fixture omitted its capability controller")?;
    let calls = controller
        .wait_for_calls(RECOVERY_WORKERS.len(), Duration::from_secs(90))
        .await
        .context("wait for all three workers at their MCP barriers")?;
    assert_eq!(
        calls.len(),
        RECOVERY_WORKERS.len(),
        "every conversational worker must reach one unique logical effect"
    );
    let observed_capabilities = calls
        .iter()
        .map(|call| call.capability.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_capabilities = RECOVERY_WORKERS
        .iter()
        .map(|(_, tool_name, _)| *tool_name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(observed_capabilities, expected_capabilities);

    let completed_capability = calls[0].capability.clone();
    controller.release(1);
    let before_restart =
        await_worker_partition(test.client(), session_id, 1, 2, Duration::from_secs(90)).await?;
    let completed_worker_id = before_restart
        .child_progress
        .iter()
        .find(|worker| worker.state == WorkerState::Completed)
        .map(|worker| worker.worker_id.clone())
        .context("one worker should be durably completed before restart")?;

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard crash orchestrator at the one-completed/two-blocked partition")?;

    let after_restart =
        await_worker_partition(test.client(), session_id, 1, 2, Duration::from_secs(90)).await?;
    assert!(
        after_restart.child_progress.iter().any(|worker| {
            worker.worker_id == completed_worker_id && worker.state == WorkerState::Completed
        }),
        "the exact completed worker must remain completed after the hard restart: {:?}",
        after_restart.child_progress
    );

    await_exact_recovery_attempts(controller, &completed_capability, Duration::from_secs(90))
        .await?;
    controller.release(2);

    let terminal = await_worker_partition(
        test.client(),
        session_id,
        RECOVERY_WORKERS.len(),
        0,
        Duration::from_secs(90),
    )
    .await?;
    assert_eq!(
        terminal
            .child_progress
            .iter()
            .filter(|worker| worker.state == WorkerState::Completed)
            .count(),
        RECOVERY_WORKERS.len()
    );

    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(90),
            Duration::from_millis(100),
        )
        .await
        .context("await retained coordinator turn outcome")?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let events =
        await_recovery_terminal_events(test.client(), session_id, Duration::from_secs(90)).await?;
    let spawned = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerSpawned {
                worker_id, task, ..
            } => Some((worker_id.clone(), task.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spawned.len(),
        RECOVERY_WORKERS.len(),
        "the coordinator must spawn exactly three workers"
    );
    let completed_task = RECOVERY_WORKERS
        .iter()
        .find(|(_, tool_name, _)| *tool_name == completed_capability)
        .map(|(task, _, _)| *task)
        .context("completed capability must belong to one scripted worker")?;
    assert!(
        spawned
            .iter()
            .any(|(worker_id, task)| worker_id == &completed_worker_id && *task == completed_task),
        "the retained completed worker must be the child whose effect was released first"
    );

    let terminal_changes = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerStatusChanged {
                worker_id,
                to: WorkerState::Completed,
                ..
            } => Some(worker_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let terminal_notifications = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerNotificationDelivered {
                worker_id,
                state: WorkerState::Completed,
                ..
            } => Some(worker_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_changes.len(),
        RECOVERY_WORKERS.len(),
        "each worker must record one completed state transition"
    );
    assert_eq!(
        terminal_changes
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        RECOVERY_WORKERS.len(),
        "no worker may record duplicate completed transitions"
    );
    assert_eq!(
        terminal_notifications.len(),
        RECOVERY_WORKERS.len(),
        "each worker must deliver one terminal notification"
    );
    assert_eq!(
        terminal_notifications
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        RECOVERY_WORKERS.len(),
        "no worker may deliver a duplicate terminal notification"
    );
    assert_eq!(
        controller.effect_count(),
        RECOVERY_WORKERS.len(),
        "restart must not duplicate any logical upstream effect"
    );
    assert_eq!(
        controller.request_count(),
        RECOVERY_WORKERS.len() + 2,
        "the completed effect stays at one transport attempt and each blocked effect resumes once"
    );
    Ok(())
}
