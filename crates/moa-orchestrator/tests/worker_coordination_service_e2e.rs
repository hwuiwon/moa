//! Service E2Es for event-driven conversational-worker coordination.
//!
//! Every scenario drives a real `Worker` virtual object and
//! `WorkerTurnExecution` workflow against the restartable local
//! Postgres/Restate/OpenFGA/Valkey fixture. Terminal delivery is intentionally
//! never injected through the Session handler: the Worker owns its waiters and
//! makes the joined parent call exactly as production does.

#![cfg(feature = "integration")]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use moa_core::events::{Event, TurnFailureActor};
use moa_core::types::action_policy::ActionPolicyEffect;
use moa_core::types::events_stream::{EventRange, EventRecord};
use moa_core::types::identifiers::{SessionId, UserId};
use moa_core::types::worker::signals::ChildSignalKind;
use moa_core::types::worker::state::{
    InputAudience, WorkerChildRef, WorkerInitialTask, WorkerMessage, WorkerState,
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::{
    FixtureCapabilityController, FixtureCapabilityOptions, FixtureCapabilityOutcome,
    FixtureCapabilityTool, OrchestratorTestFixture, TestApiClient,
};
use serde_json::{Value, json};

use crate::support::session_store_service::start_turn_request;

#[path = "support/mod.rs"]
mod support;

const CLASSIFIER_PROMPT: &str = "You classify one user turn into MOA's public execution decision.";
const PARENT_PRIME_REQUEST: &str = "Prime this session's authenticated coordinator owner.";
const PARENT_PRIME_RESPONSE: &str = "Parent coordinator ownership established.";
const FAN_IN_RESUME_RESPONSE: &str = "Fan-in resume observed.";
const MCP_SERVER: &str = "fixture-capability";
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const LONG_FAN_IN_WAVES: usize = 5;

const FAN_IN_WORKERS: [(&str, &str, &str); 3] = [
    (
        "FAN-IN-WORKER-A: run the held A effect.",
        "fan_in_probe_a",
        "FAN-IN-A-DONE",
    ),
    (
        "FAN-IN-WORKER-B: run the held B effect.",
        "fan_in_probe_b",
        "FAN-IN-B-DONE",
    ),
    (
        "FAN-IN-WORKER-C: run the held C effect.",
        "fan_in_probe_c",
        "FAN-IN-C-DONE",
    ),
];

const FAILED_WORKER_ID: &str = "coordination-failed-worker";
const FAILED_WORKER_TASK: &str = "FAILURE-REPLAY-WORKER: reach the terminal worker-turn catch-all.";

const INPUT_WORKER_ID: &str = "coordination-input-worker";
const INPUT_WORKER_TASK: &str = "INPUT-WORKER: request the exact human answer.";
const INPUT_QUESTION: &str = "Which ledger should I reconcile?";
const INPUT_ANSWER: &str = "Reconcile ledger-seven.";
const INPUT_COMPLETED: &str = "INPUT-WORKER-COMPLETED";

const WAIT_WORKER_ID: &str = "coordination-explicit-wait-worker";
const WAIT_WORKER_TASK: &str = "WAIT-WORKER: run the held wait effect.";
const WAIT_CAPABILITY: &str = "wait_worker_probe";
const WAIT_CAPABILITY_OUTPUT: &str = "WAIT-WORKER-DONE";
const WAIT_REQUEST: &str = "Wait explicitly for the registered coordination worker.";
const WAIT_WORKER_FINAL: &str = "The explicit worker completed its held wait effect.";
const WAIT_FINAL: &str = "The explicit worker wait returned exactly once.";

const LIVENESS_WORKER_ID: &str = "coordination-liveness-worker";
const LIVENESS_WORKER_TASK: &str = "LIVENESS-WORKER: remain held at the heartbeat probe.";
const LIVENESS_CAPABILITY: &str = "liveness_worker_probe";
const LIVENESS_CAPABILITY_OUTPUT: &str = "LIVENESS-WORKER-DONE";
const LIVENESS_STALE_MS: u64 = 2_000;
const HEALTHY_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const HEALTHY_HEARTBEATS: usize = 17;

fn tool_reference(tool_name: &str) -> String {
    moa_hands::mcp_tool_reference(MCP_SERVER, tool_name)
}

fn worker_id(scenario: &str, ordinal: usize) -> String {
    format!("coordination-{scenario}-{ordinal}")
}

fn success_tool(tool_name: &str, output: &str) -> FixtureCapabilityTool {
    FixtureCapabilityTool {
        name: tool_name.to_string(),
        description: format!("Hold and release the deterministic {tool_name} effect"),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["worker"],
            "properties": { "worker": { "type": "string" } }
        }),
        item_key_pointer: None,
        idempotent: true,
        outcomes: vec![FixtureCapabilityOutcome::Success {
            output: json!({ "result": output }),
        }],
    }
}

fn parent_prime_script_responses() -> [Value; 2] {
    [
        json!({
            "completion": {
                "content": r#"{"label":"respond","strategy":null,"rationale":"The priming turn needs one direct response.","confidence_bps":10000,"missing_inputs":[]}"#,
                "tool_calls": []
            }
        }),
        json!({
            "completion": { "content": PARENT_PRIME_RESPONSE, "tool_calls": [] }
        }),
    ]
}

fn fan_in_script() -> Value {
    let coordinator_resume = std::iter::once(json!({
        "match": "fan_in_settled",
        "completion": { "content": FAN_IN_RESUME_RESPONSE, "tool_calls": [] }
    }));
    let completions = FAN_IN_WORKERS.iter().map(|(_, _, output)| {
        json!({
            "match": output,
            "completion": { "content": format!("retained {output}"), "tool_calls": [] }
        })
    });
    let tool_calls = FAN_IN_WORKERS.iter().map(|(task, tool_name, _)| {
        json!({
            "match": task,
            "completion": {
                "content": "",
                "tool_calls": [{
                    "name": tool_reference(tool_name),
                    "id": format!("{tool_name}-call"),
                    "input": { "worker": tool_name }
                }]
            }
        })
    });
    json!({
        "default": {
            "completion": { "content": FAN_IN_RESUME_RESPONSE, "tool_calls": [] }
        },
        "responses": parent_prime_script_responses(),
        "keyed": coordinator_resume
            .chain(completions)
            .chain(tool_calls)
            .collect::<Vec<_>>()
    })
}

fn fan_in_options() -> FixtureCapabilityOptions {
    FixtureCapabilityOptions {
        tools: FAN_IN_WORKERS
            .iter()
            .map(|(_, tool_name, output)| success_tool(tool_name, output))
            .collect(),
        orchestrator_env: vec![("RUST_LOG".to_string(), "error".to_string())],
    }
}

fn failure_script() -> Value {
    json!({
        "default": {
            "completion": { "content": "Failure resume observed.", "tool_calls": [] }
        },
        "responses": parent_prime_script_responses(),
        "keyed": [
            {
                "match": "<child_signal kind=\"failed\"",
                "completion": { "content": "Failure resume observed.", "tool_calls": [] }
            },
            {
                "match": FAILED_WORKER_TASK,
                "completion": {
                    "content": "unreachable",
                    "tool_calls": [],
                    "fault": { "fail_first_n": 100, "status": 500 }
                }
            }
        ]
    })
}

fn input_script() -> Value {
    json!({
        "default": {
            "completion": { "content": "The worker needs a human answer.", "tool_calls": [] }
        },
        "responses": parent_prime_script_responses(),
        "keyed": [
            {
                "match": INPUT_ANSWER,
                "completion": { "content": INPUT_COMPLETED, "tool_calls": [] }
            },
            {
                "match": INPUT_WORKER_TASK,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "request_input",
                        "id": "coordination-input-request",
                        "input": { "question": INPUT_QUESTION, "audience": "user" }
                    }]
                }
            }
        ]
    })
}

fn wait_script() -> Value {
    json!({
        "default": { "completion": { "content": "unexpected wait fallback", "tool_calls": [] } },
        "keyed": [
            {
                "match": WAIT_WORKER_FINAL,
                "completion": { "content": WAIT_FINAL, "tool_calls": [] }
            },
            {
                "match": WAIT_CAPABILITY_OUTPUT,
                "completion": { "content": WAIT_WORKER_FINAL, "tool_calls": [] }
            },
            {
                "match": WAIT_WORKER_TASK,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": tool_reference(WAIT_CAPABILITY),
                        "id": "wait-worker-probe-call",
                        "input": { "worker": WAIT_WORKER_ID }
                    }]
                }
            },
            {
                "match": CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"inline","rationale":"The turn explicitly waits for an existing worker.","confidence_bps":10000,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": WAIT_REQUEST,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "wait_worker",
                        "id": "explicit-wait-call",
                        "input": { "worker_id": WAIT_WORKER_ID, "timeout_ms": 90000 }
                    }]
                }
            }
        ]
    })
}

fn wait_options() -> FixtureCapabilityOptions {
    FixtureCapabilityOptions {
        tools: vec![success_tool(WAIT_CAPABILITY, WAIT_CAPABILITY_OUTPUT)],
        orchestrator_env: vec![("RUST_LOG".to_string(), "error".to_string())],
    }
}

fn liveness_script() -> Value {
    json!({
        "default": {
            "completion": { "content": "Stale worker resume observed.", "tool_calls": [] }
        },
        "responses": parent_prime_script_responses(),
        "keyed": [{
            "match": LIVENESS_WORKER_TASK,
            "completion": {
                "content": "",
                "tool_calls": [{
                    "name": tool_reference(LIVENESS_CAPABILITY),
                    "id": "liveness-worker-probe-call",
                    "input": { "worker": LIVENESS_WORKER_ID }
                }]
            }
        }]
    })
}

fn liveness_options() -> FixtureCapabilityOptions {
    FixtureCapabilityOptions {
        tools: vec![success_tool(
            LIVENESS_CAPABILITY,
            LIVENESS_CAPABILITY_OUTPUT,
        )],
        orchestrator_env: vec![
            ("RUST_LOG".to_string(), "error".to_string()),
            (
                "MOA_SESSION_LIMITS_WORKER_HEARTBEAT_STALE_MS".to_string(),
                LIVENESS_STALE_MS.to_string(),
            ),
        ],
    }
}

async fn allow_tools(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    session_id: SessionId,
    tool_names: impl IntoIterator<Item = &'static str>,
) -> Result<()> {
    let meta = client
        .get_session(session_id)
        .await
        .context("load session before policy setup")?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant fixture identity tenant admin")?;
    for tool_name in tool_names {
        client
            .post_void(
                "/ActionPolicy/upsert_rule",
                &UpsertActionPolicyRuleRequest {
                    tenant_id: meta.tenant_id,
                    contact_id: None,
                    tool_name: tool_reference(tool_name),
                    pattern: "*".to_string(),
                    effect: ActionPolicyEffect::Allow,
                    reason: Some("deterministic worker-coordination fixture".to_string()),
                },
            )
            .await
            .with_context(|| format!("allow coordination fixture tool {tool_name}"))?;
    }
    Ok(())
}

/// Runs one authenticated production turn to leave the Session idle with an owning identity.
async fn establish_idle_parent(client: &TestApiClient, session_id: SessionId) -> Result<()> {
    let started = client
        .session(session_id.to_string())
        .start_turn(start_turn_request(PARENT_PRIME_REQUEST), None)
        .await
        .context("start parent ownership priming turn")?;
    let turn_id = started
        .turn_id
        .context("idle parent should admit its ownership priming turn")?;
    let outcome = client
        .session(session_id.to_string())
        .await_turn_outcome(&turn_id, SCENARIO_TIMEOUT, POLL_INTERVAL)
        .await
        .context("await parent ownership priming turn")?;
    ensure!(
        outcome.kind == moa_wire::turn::TurnOutcomeKind::Completed,
        "parent ownership priming turn did not complete: {:?}",
        outcome.kind
    );
    ensure!(
        outcome.message == PARENT_PRIME_RESPONSE,
        "parent ownership priming turn returned unexpected response: {}",
        outcome.message
    );
    Ok(())
}

/// Registers and starts one real worker with a caller-chosen stable object key.
async fn start_worker(
    client: &TestApiClient,
    session_id: SessionId,
    worker_id: &str,
    task: &str,
    tool_subset: Vec<String>,
) -> Result<()> {
    let meta = client
        .get_session(session_id)
        .await
        .context("load session before worker start")?;
    let identity = client
        .identity()
        .cloned()
        .context("fixture client must carry an identity")?;

    client
        .post_void(
            &format!("/Session/{session_id}/register_child"),
            &WorkerChildRef {
                id: worker_id.to_string(),
                task_hash: format!("coordination:{worker_id}"),
                budget_tokens: 4_096,
                terminal: None,
            },
        )
        .await
        .with_context(|| format!("register worker {worker_id}"))?;

    client
        .post_void(
            &format!("/Worker/{worker_id}/post_message"),
            &WorkerMessage::InitialTask(Box::new(WorkerInitialTask {
                task: task.to_string(),
                identity: identity.clone(),
                tool_subset,
                budget_tokens: 4_096,
                max_turns: Some(2),
                parent_session: session_id,
                depth: 1,
                tenant_id: meta.tenant_id,
                user_id: UserId::new(format!("identity:{}", identity.id)),
                model: meta.model,
                trusted_sandbox_manifest: None,
            })),
        )
        .await
        .with_context(|| format!("start real worker {worker_id}"))?;
    Ok(())
}

async fn session_events(client: &TestApiClient, session_id: SessionId) -> Result<Vec<EventRecord>> {
    client
        .get_events(session_id, EventRange::all())
        .await
        .context("load worker-coordination session events")
}

async fn await_events(
    client: &TestApiClient,
    session_id: SessionId,
    timeout: Duration,
    predicate: impl Fn(&[EventRecord]) -> bool,
) -> Result<Vec<EventRecord>> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = session_events(client, session_id).await?;
        if predicate(&events) {
            return Ok(events);
        }
        ensure!(
            Instant::now() < deadline,
            "session {session_id} did not reach the expected coordination boundary within {timeout:?}; events={events:#?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn worker_status_count(events: &[EventRecord], worker_id: &str, state: WorkerState) -> usize {
    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::WorkerStatusChanged {
                    worker_id: recorded,
                    to,
                    ..
                } if recorded == worker_id && *to == state
            )
        })
        .count()
}

fn worker_notification_count(events: &[EventRecord], worker_id: &str, state: WorkerState) -> usize {
    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::WorkerNotificationDelivered {
                    worker_id: recorded,
                    state: recorded_state,
                    ..
                } if recorded == worker_id && *recorded_state == state
            )
        })
        .count()
}

fn signal_count(events: &[EventRecord], worker_id: &str, kind: ChildSignalKind) -> usize {
    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::WorkerSignalReceived {
                    worker_id: recorded,
                    kind: recorded_kind,
                    ..
                } if recorded == worker_id && *recorded_kind == kind
            )
        })
        .count()
}

fn parent_resume_count(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::WorkerParentResumeRequested { .. }))
        .count()
}

fn event_sequence(events: &[EventRecord], predicate: impl Fn(&Event) -> bool) -> Option<u64> {
    events
        .iter()
        .find(|record| predicate(&record.event))
        .map(|record| record.sequence_num)
}

async fn child_refs(client: &TestApiClient, session_id: SessionId) -> Result<Vec<WorkerChildRef>> {
    client
        .post_empty_call(&format!("/Session/{session_id}/child_refs"))
        .await
        .context("load registered worker refs")
}

async fn await_exact_recovery_attempts(
    controller: &FixtureCapabilityController,
    completed_capability: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempts = controller.transport_attempts();
        let counts = FAN_IN_WORKERS
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
            .collect::<BTreeMap<_, _>>();
        let expected = |tool_name: &str| usize::from(tool_name != completed_capability) + 1;
        if counts
            .iter()
            .any(|(tool_name, count)| *count > expected(tool_name))
        {
            bail!("a fan-in capability replayed more than once: {counts:?}");
        }
        if counts
            .iter()
            .all(|(tool_name, count)| *count == expected(tool_name))
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "fan-in attempts did not reach the exact 1/2/2 recovery partition: {counts:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn restate_query_rows(fixture: &OrchestratorTestFixture, query: &str) -> Result<Vec<Value>> {
    let client = reqwest::Client::new();
    let url = format!("{}/query", fixture.admin_url.trim_end_matches('/'));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let last_error = match client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({ "query": query }))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) if status.is_success() => match serde_json::from_str::<Value>(&body) {
                        Ok(payload) => {
                            if let Some(rows) = payload.get("rows").and_then(Value::as_array) {
                                return Ok(rows.clone());
                            }
                            format!("response omitted rows: {payload}")
                        }
                        Err(error) => format!("decode JSON response: {error}; body={body:?}"),
                    },
                    Ok(body) => format!("status {status}; body={body:?}"),
                    Err(error) => format!("read response body: {error}"),
                }
            }
            Err(error) => format!("send query: {error}"),
        };
        ensure!(
            Instant::now() < deadline,
            "Restate introspection did not become ready: {last_error}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn invocation_count(
    fixture: &OrchestratorTestFixture,
    service: &str,
    key: Option<&str>,
    handler: Option<&str>,
) -> Result<usize> {
    let key_filter = key
        .map(|key| format!(" AND target_service_key = '{key}'"))
        .unwrap_or_default();
    let handler_filter = handler
        .map(|handler| format!(" AND target_handler_name = '{handler}'"))
        .unwrap_or_default();
    restate_query_rows(
        fixture,
        &format!(
            "SELECT id FROM sys_invocation WHERE target_service_name = '{service}'{key_filter}{handler_filter}"
        ),
    )
    .await
    .map(|rows| rows.len())
}

async fn await_invocation_count(
    fixture: &OrchestratorTestFixture,
    service: &str,
    key: Option<&str>,
    handler: Option<&str>,
    expected: usize,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let observed = invocation_count(fixture, service, key, handler).await?;
        if observed == expected {
            return Ok(());
        }
        ensure!(
            observed < expected,
            "expected {expected} {service}/{handler:?} invocations for {key:?}, observed {observed}"
        );
        ensure!(
            Instant::now() < deadline,
            "expected {expected} {service}/{handler:?} invocations for {key:?}, observed {observed}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
#[ignore = "requires Docker for the restartable Postgres/Restate/OpenFGA/Valkey fixture"]
async fn recovery_matrix_three_children_resume_once_after_all_settle_service_e2e() -> Result<()> {
    // Pins: three real children straddle a hard process restart; the idle parent records no
    // early success wake and resumes exactly once when the third terminal delivery settles the
    // current fan-in generation. Mutation pin: removing the all-settled guard makes the
    // pre-restart zero-resume assertion fail.
    let fixture =
        OrchestratorTestFixture::with_execution_fixture(fan_in_script(), fan_in_options())
            .await
            .context("boot fan-in recovery fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("three-child-fan-in").await?;
    establish_idle_parent(test.client(), session_id).await?;
    allow_tools(
        &fixture,
        test.client(),
        session_id,
        FAN_IN_WORKERS.iter().map(|(_, tool_name, _)| *tool_name),
    )
    .await?;

    let worker_ids = FAN_IN_WORKERS
        .iter()
        .enumerate()
        .map(|(index, _)| worker_id("fan-in", index))
        .collect::<Vec<_>>();
    for ((task, tool_name, _), worker_id) in FAN_IN_WORKERS.iter().zip(&worker_ids) {
        start_worker(
            test.client(),
            session_id,
            worker_id,
            task,
            vec![tool_reference(tool_name)],
        )
        .await?;
    }

    let controller = fixture
        .fixture_capability()
        .context("fan-in fixture omitted capability controller")?;
    let calls = controller
        .wait_for_calls(FAN_IN_WORKERS.len(), SCENARIO_TIMEOUT)
        .await
        .context("wait for all fan-in workers at their exact effect barriers")?;
    assert_eq!(calls.len(), FAN_IN_WORKERS.len());
    assert_eq!(
        calls
            .iter()
            .map(|call| call.capability.as_str())
            .collect::<BTreeSet<_>>(),
        FAN_IN_WORKERS
            .iter()
            .map(|(_, tool_name, _)| *tool_name)
            .collect::<BTreeSet<_>>()
    );

    let completed_capability = calls[0].capability.clone();
    controller.release(1);
    let before_restart = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
        worker_ids
            .iter()
            .map(|worker_id| worker_notification_count(events, worker_id, WorkerState::Completed))
            .sum::<usize>()
            == 1
    })
    .await?;
    assert_eq!(
        parent_resume_count(&before_restart),
        0,
        "N-1 settled children must not resume the idle parent"
    );
    assert_eq!(
        before_restart
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::WorkerSignalReceived {
                    kind: ChildSignalKind::FanInSettled,
                    ..
                }
            ))
            .count(),
        0,
        "fan-in settlement cannot be published before all registered children settle"
    );

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard restart at the one-complete/two-running fan-in partition")?;
    await_exact_recovery_attempts(controller, &completed_capability, SCENARIO_TIMEOUT).await?;
    controller.release(2);

    let events = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
        worker_ids.iter().all(|worker_id| {
            worker_notification_count(events, worker_id, WorkerState::Completed) == 1
        }) && parent_resume_count(events) == 1
            && events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::BrainResponse { text, .. } if text == FAN_IN_RESUME_RESPONSE
                )
            })
    })
    .await?;

    for worker_id in &worker_ids {
        assert_eq!(
            worker_status_count(&events, worker_id, WorkerState::Completed),
            1,
            "worker {worker_id} must record exactly one completed transition"
        );
        assert_eq!(
            worker_notification_count(&events, worker_id, WorkerState::Completed),
            1,
            "worker {worker_id} must deliver exactly one terminal notification"
        );
    }
    let fan_in_signals = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerSignalReceived {
                signal_id,
                kind: ChildSignalKind::FanInSettled,
                ..
            } => Some(*signal_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let resumes = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerParentResumeRequested { signal_id, .. } => Some(*signal_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(fan_in_signals.len(), 1, "fan-in emits one settled signal");
    assert_eq!(
        resumes, fan_in_signals,
        "the one resume consumes the exact fan-in signal"
    );
    let last_notification_seq = events
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
        .map(|record| record.sequence_num)
        .max()
        .context("completed fan-in omitted terminal notifications")?;
    let fan_in_signal_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerSignalReceived {
                kind: ChildSignalKind::FanInSettled,
                ..
            }
        )
    })
    .context("completed fan-in omitted its settled signal")?;
    let resume_seq = event_sequence(&events, |event| {
        matches!(event, Event::WorkerParentResumeRequested { .. })
    })
    .context("completed fan-in omitted its parent resume")?;
    let response_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::BrainResponse { text, .. } if text == FAN_IN_RESUME_RESPONSE
        )
    })
    .context("completed fan-in omitted its coordinator response")?;
    assert!(
        last_notification_seq < fan_in_signal_seq
            && fan_in_signal_seq < resume_seq
            && resume_seq < response_seq,
        "fan-in events must remain terminal facts -> settled signal -> resume -> response"
    );
    assert_eq!(controller.effect_count(), FAN_IN_WORKERS.len());
    assert_eq!(controller.request_count(), FAN_IN_WORKERS.len() + 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the restartable Postgres/Restate/OpenFGA/Valkey fixture"]
async fn long_successive_fan_in_generations_remain_exactly_once_service_e2e() -> Result<()> {
    // Pins: a long-lived idle parent can accept five successive three-child generations without
    // accumulating a stale settlement, duplicate terminal delivery, or duplicate parent resume.
    // Each generation must become visible in terminal facts before its one settled signal, and
    // the resumed coordinator turn must finish before the next generation is registered.
    let fixture =
        OrchestratorTestFixture::with_execution_fixture(fan_in_script(), fan_in_options())
            .await
            .context("boot long fan-in fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("long-successive-fan-in").await?;
    establish_idle_parent(test.client(), session_id).await?;
    allow_tools(
        &fixture,
        test.client(),
        session_id,
        FAN_IN_WORKERS.iter().map(|(_, tool_name, _)| *tool_name),
    )
    .await?;

    let controller = fixture
        .fixture_capability()
        .context("long fan-in fixture omitted capability controller")?;
    let mut wave_worker_ids = Vec::with_capacity(LONG_FAN_IN_WAVES);

    for wave in 0..LONG_FAN_IN_WAVES {
        let worker_ids = FAN_IN_WORKERS
            .iter()
            .enumerate()
            .map(|(ordinal, _)| worker_id(&format!("long-wave-{wave}"), ordinal))
            .collect::<Vec<_>>();
        for ((task, tool_name, _), worker_id) in FAN_IN_WORKERS.iter().zip(&worker_ids) {
            start_worker(
                test.client(),
                session_id,
                worker_id,
                task,
                vec![tool_reference(tool_name)],
            )
            .await?;
        }

        let expected_calls = (wave + 1) * FAN_IN_WORKERS.len();
        let calls = controller
            .wait_for_calls(expected_calls, SCENARIO_TIMEOUT)
            .await
            .with_context(|| format!("wait for long fan-in wave {wave} effects"))?;
        assert_eq!(
            calls.len(),
            expected_calls,
            "wave {wave} must add exactly three logical effects"
        );

        let before_release = session_events(test.client(), session_id).await?;
        assert_eq!(
            parent_resume_count(&before_release),
            wave,
            "wave {wave} cannot resume before all of its effects are released"
        );
        assert_eq!(
            before_release
                .iter()
                .filter(|record| matches!(
                    record.event,
                    Event::WorkerSignalReceived {
                        kind: ChildSignalKind::FanInSettled,
                        ..
                    }
                ))
                .count(),
            wave,
            "wave {wave} cannot settle before all three children finish"
        );

        controller.release(FAN_IN_WORKERS.len());
        let events = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
            worker_ids.iter().all(|worker_id| {
                worker_notification_count(events, worker_id, WorkerState::Completed) == 1
            }) && parent_resume_count(events) == wave + 1
                && events
                    .iter()
                    .filter(|record| {
                        matches!(
                            &record.event,
                            Event::BrainResponse { text, .. } if text == FAN_IN_RESUME_RESPONSE
                        )
                    })
                    .count()
                    == wave + 1
        })
        .await?;

        let resume_turn_id = events
            .iter()
            .rev()
            .find_map(|record| match &record.event {
                Event::WorkerParentResumeRequested { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .with_context(|| format!("wave {wave} omitted its parent resume turn"))?;
        let outcome = test
            .client()
            .session(session_id.to_string())
            .await_turn_outcome(&resume_turn_id, SCENARIO_TIMEOUT, POLL_INTERVAL)
            .await
            .with_context(|| format!("await long fan-in wave {wave} parent outcome"))?;
        ensure!(
            outcome.kind == moa_wire::turn::TurnOutcomeKind::Completed,
            "wave {wave} parent resume did not complete: {:?}",
            outcome.kind
        );
        ensure!(
            outcome.message == FAN_IN_RESUME_RESPONSE,
            "wave {wave} parent resume returned unexpected response: {}",
            outcome.message
        );
        wave_worker_ids.push(worker_ids);
    }

    let events = session_events(test.client(), session_id).await?;
    for worker_id in wave_worker_ids.iter().flatten() {
        assert_eq!(
            worker_status_count(&events, worker_id, WorkerState::Completed),
            1,
            "worker {worker_id} must record one terminal transition"
        );
        assert_eq!(
            worker_notification_count(&events, worker_id, WorkerState::Completed),
            1,
            "worker {worker_id} must deliver one terminal notification"
        );
    }

    let settled = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerSignalReceived {
                signal_id,
                kind: ChildSignalKind::FanInSettled,
                ..
            } => Some((record.sequence_num, *signal_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let resumes = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::WorkerParentResumeRequested { signal_id, .. } => {
                Some((record.sequence_num, *signal_id))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), LONG_FAN_IN_WAVES);
    assert_eq!(resumes.len(), LONG_FAN_IN_WAVES);
    for ((settled_seq, settled_id), (resume_seq, resume_id)) in settled.iter().zip(&resumes) {
        assert_eq!(
            resume_id, settled_id,
            "each resume must consume its settlement"
        );
        assert!(
            settled_seq < resume_seq,
            "the settlement fact must precede its parent resume"
        );
    }
    assert_eq!(
        controller.effect_count(),
        LONG_FAN_IN_WAVES * FAN_IN_WORKERS.len()
    );
    assert_eq!(
        controller.request_count(),
        LONG_FAN_IN_WAVES * FAN_IN_WORKERS.len(),
        "the no-restart long case must not replay a provider effect"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the restartable Postgres/Restate/OpenFGA/Valkey fixture"]
async fn recovery_matrix_failed_child_records_one_failure_and_resume_service_e2e() -> Result<()> {
    // Pins: a real worker turn that reaches its catch-all across a hard restart produces one
    // canonical failure, one failed terminal cache entry, one Failed attention signal, and one
    // guarded parent resume. Mutation pin: restoring the removed turn-level Failed writer makes
    // the exact signal/resume assertions fail.
    let fixture = OrchestratorTestFixture::with_script(failure_script())
        .await
        .context("boot failed-worker recovery fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("failed-child-replay").await?;
    establish_idle_parent(test.client(), session_id).await?;
    fixture.reset_scripted_requests()?;
    start_worker(
        test.client(),
        session_id,
        FAILED_WORKER_ID,
        FAILED_WORKER_TASK,
        Vec::new(),
    )
    .await?;

    fixture
        .wait_for_scripted_requests(1, SCENARIO_TIMEOUT)
        .await
        .context("wait for the failed worker's first provider attempt")?;
    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard restart while the failed worker turn is replayable")?;

    let events = await_events(
        test.client(),
        session_id,
        Duration::from_secs(240),
        |events| {
            worker_notification_count(events, FAILED_WORKER_ID, WorkerState::Failed) == 1
                && signal_count(events, FAILED_WORKER_ID, ChildSignalKind::Failed) == 1
                && parent_resume_count(events) == 1
                && events.iter().any(|record| {
                    matches!(
                        &record.event,
                        Event::BrainResponse { text, .. } if text == "Failure resume observed."
                    )
                })
        },
    )
    .await?;

    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::TurnFailed {
                    actor: TurnFailureActor::Worker { worker_id },
                    ..
                } if worker_id == FAILED_WORKER_ID
            ))
            .count(),
        1,
        "worker replay must retain one canonical failed-turn fact"
    );
    assert_eq!(
        worker_status_count(&events, FAILED_WORKER_ID, WorkerState::Failed),
        1
    );
    assert_eq!(
        worker_notification_count(&events, FAILED_WORKER_ID, WorkerState::Failed),
        1
    );
    assert_eq!(
        signal_count(&events, FAILED_WORKER_ID, ChildSignalKind::Failed),
        1
    );
    assert_eq!(parent_resume_count(&events), 1);
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == "Failure resume observed."
            ))
            .count(),
        1,
        "the one failure wake produces one coordinator response"
    );
    let turn_failed_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::TurnFailed {
                actor: TurnFailureActor::Worker { worker_id },
                ..
            } if worker_id == FAILED_WORKER_ID
        )
    })
    .context("failed worker omitted its canonical turn failure")?;
    let status_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerStatusChanged {
                worker_id,
                to: WorkerState::Failed,
                ..
            } if worker_id == FAILED_WORKER_ID
        )
    })
    .context("failed worker omitted its terminal status")?;
    let notification_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerNotificationDelivered {
                worker_id,
                state: WorkerState::Failed,
                ..
            } if worker_id == FAILED_WORKER_ID
        )
    })
    .context("failed worker omitted its terminal notification")?;
    let failed_signal_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerSignalReceived {
                worker_id,
                kind: ChildSignalKind::Failed,
                ..
            } if worker_id == FAILED_WORKER_ID
        )
    })
    .context("failed worker omitted its attention signal")?;
    let resume_seq = event_sequence(&events, |event| {
        matches!(event, Event::WorkerParentResumeRequested { .. })
    })
    .context("failed worker omitted its parent resume")?;
    assert!(
        turn_failed_seq < status_seq
            && status_seq < notification_seq
            && notification_seq < failed_signal_seq
            && failed_signal_seq < resume_seq,
        "failure events must remain canonical failure -> terminal facts -> signal -> resume"
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::WorkerSignalReceived {
                    kind: ChildSignalKind::FanInSettled,
                    ..
                }
            ))
            .count(),
        0,
        "a failed child suppresses success fan-in for its registration generation"
    );
    let refs = child_refs(test.client(), session_id).await?;
    let terminal = refs
        .iter()
        .find(|child| child.id == FAILED_WORKER_ID)
        .and_then(|child| child.terminal.as_ref())
        .context("failed worker terminal result must be cached on its parent")?;
    assert_eq!(terminal.state, WorkerState::Failed);
    assert!(!terminal.result.success);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Valkey fixture"]
async fn needs_input_wakes_parent_and_resolves_exact_awakeable_once_service_e2e() -> Result<()> {
    // Pins: a real child `request_input` wakes the idle Session immediately; one user reply is
    // routed to the exact Worker target, resumes the same WorkerTurnExecution awakeable, and an
    // admission replay resolves neither a second awakeable nor a second worker workflow.
    let fixture = OrchestratorTestFixture::with_script(input_script())
        .await
        .context("boot worker-input fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("worker-needs-input").await?;
    establish_idle_parent(test.client(), session_id).await?;
    start_worker(
        test.client(),
        session_id,
        INPUT_WORKER_ID,
        INPUT_WORKER_TASK,
        Vec::new(),
    )
    .await?;

    let input_events = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
        signal_count(events, INPUT_WORKER_ID, ChildSignalKind::NeedsInput) == 1
            && parent_resume_count(events) == 1
    })
    .await?;
    let input_request_id = input_events
        .iter()
        .find_map(|record| match &record.event {
            Event::WorkerSignalReceived {
                worker_id,
                kind: ChildSignalKind::NeedsInput,
                input_request_id: Some(input_request_id),
                input_audience: Some(InputAudience::User),
                ..
            } if worker_id == INPUT_WORKER_ID => Some(input_request_id.clone()),
            _ => None,
        })
        .context("NeedsInput event omitted its exact input request id")?;
    assert_eq!(
        invocation_count(&fixture, "WorkerTurnExecution", None, Some("run")).await?,
        1,
        "the input wait must be inside one worker workflow"
    );

    let reply = start_turn_request(INPUT_ANSWER);
    let first = test
        .client()
        .session(session_id.to_string())
        .start_turn(reply.clone(), None)
        .await
        .context("deliver exact user reply to the pending worker input")?;
    assert_eq!(
        first.turn_id, None,
        "a worker reply must not start a root turn"
    );
    assert!(!first.queued);
    let replay = test
        .client()
        .session(session_id.to_string())
        .start_turn(reply, None)
        .await
        .context("replay the same worker-input admission")?;
    assert_eq!(
        replay, first,
        "reply admission replay must return its stored response"
    );

    let events = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
        worker_notification_count(events, INPUT_WORKER_ID, WorkerState::Completed) == 1
    })
    .await?;
    assert_eq!(
        invocation_count(&fixture, "WorkerTurnExecution", None, Some("run")).await?,
        1,
        "answering input resumes the original worker workflow instead of starting another"
    );
    assert_eq!(
        invocation_count(
            &fixture,
            "Worker",
            Some(INPUT_WORKER_ID),
            Some("provide_input")
        )
        .await?,
        1,
        "the exact Worker awakeable target is resolved once despite admission replay"
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::WorkerMessageSent {
                    worker_id,
                    input_request_id: Some(recorded),
                    text,
                } if worker_id == INPUT_WORKER_ID
                    && recorded == &input_request_id
                    && text == INPUT_ANSWER
            ))
            .count(),
        1,
        "the durable reply must name the exact request and appear once"
    );
    assert_eq!(
        signal_count(&events, INPUT_WORKER_ID, ChildSignalKind::NeedsInput),
        1
    );
    assert_eq!(
        worker_status_count(&events, INPUT_WORKER_ID, WorkerState::Completed),
        1
    );
    assert_eq!(
        worker_notification_count(&events, INPUT_WORKER_ID, WorkerState::Completed),
        1
    );
    let needs_input_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerSignalReceived {
                worker_id,
                kind: ChildSignalKind::NeedsInput,
                ..
            } if worker_id == INPUT_WORKER_ID
        )
    })
    .context("input worker omitted NeedsInput signal")?;
    let input_resume_seq = event_sequence(&events, |event| {
        matches!(event, Event::WorkerParentResumeRequested { .. })
    })
    .context("input worker omitted immediate parent wake")?;
    let reply_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerMessageSent {
                worker_id,
                input_request_id: Some(recorded),
                ..
            } if worker_id == INPUT_WORKER_ID && recorded == &input_request_id
        )
    })
    .context("input worker omitted exact reply event")?;
    let completed_seq = event_sequence(&events, |event| {
        matches!(
            event,
            Event::WorkerStatusChanged {
                worker_id,
                to: WorkerState::Completed,
                ..
            } if worker_id == INPUT_WORKER_ID
        )
    })
    .context("input worker omitted completed status")?;
    assert!(
        needs_input_seq < input_resume_seq
            && needs_input_seq < reply_seq
            && reply_seq < completed_seq,
        "input events must wake immediately, then apply the exact reply before completion"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Valkey fixture"]
async fn explicit_wait_returns_without_queued_fan_in_resume_service_e2e() -> Result<()> {
    // Pins: `wait_worker` attaches to the real child before terminal delivery, returns the exact
    // result through the child-owned awakeable, and the Session records no success resume because
    // the coordinator is already active. This exercises waiter-before-parent-ack ordering.
    let fixture = OrchestratorTestFixture::with_execution_fixture(wait_script(), wait_options())
        .await
        .context("boot explicit-wait fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("explicit-worker-wait").await?;
    allow_tools(&fixture, test.client(), session_id, [WAIT_CAPABILITY]).await?;
    start_worker(
        test.client(),
        session_id,
        WAIT_WORKER_ID,
        WAIT_WORKER_TASK,
        vec![tool_reference(WAIT_CAPABILITY)],
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("explicit-wait fixture omitted capability controller")?;
    controller
        .wait_for_calls(1, SCENARIO_TIMEOUT)
        .await
        .context("wait for worker at its held effect")?;

    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(start_turn_request(WAIT_REQUEST), None)
        .await
        .context("start coordinator explicit wait turn")?;
    let turn_id = started
        .turn_id
        .context("idle session should start the explicit wait turn")?;
    await_invocation_count(
        &fixture,
        "Worker",
        Some(WAIT_WORKER_ID),
        Some("attach_result_waiter"),
        1,
        SCENARIO_TIMEOUT,
    )
    .await?;

    controller.release(1);
    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(&turn_id, SCENARIO_TIMEOUT, POLL_INTERVAL)
        .await
        .context("await explicit wait coordinator outcome")?;
    assert_eq!(outcome.kind, moa_wire::turn::TurnOutcomeKind::Completed);

    let events = await_events(test.client(), session_id, SCENARIO_TIMEOUT, |events| {
        worker_notification_count(events, WAIT_WORKER_ID, WorkerState::Completed) == 1
            && events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::BrainResponse { text, .. } if text == WAIT_FINAL
                )
            })
    })
    .await?;
    assert_eq!(
        invocation_count(
            &fixture,
            "Worker",
            Some(WAIT_WORKER_ID),
            Some("attach_result_waiter")
        )
        .await?,
        1,
        "the explicit waiter is attached once"
    );
    assert_eq!(
        parent_resume_count(&events),
        0,
        "explicit wait must not queue a second resume"
    );
    assert_eq!(
        signal_count(&events, WAIT_WORKER_ID, ChildSignalKind::FanInSettled),
        0,
        "an active explicit waiter suppresses automatic success fan-in wake"
    );
    assert_eq!(
        worker_status_count(&events, WAIT_WORKER_ID, WorkerState::Completed),
        1
    );
    assert_eq!(
        worker_notification_count(&events, WAIT_WORKER_ID, WorkerState::Completed),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == WAIT_FINAL
            ))
            .count(),
        1,
        "the one attached waiter returns one exact coordinator response"
    );
    assert_eq!(controller.effect_count(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Valkey fixture"]
async fn healthy_worker_never_polls_session_and_stale_deadline_emits_once_service_e2e() -> Result<()>
{
    // Pins: a held real worker stays healthy across multiple Worker-owned deadline firings
    // without invoking Session; after heartbeats stop, its latest exact deadline emits one
    // WorkerHeartbeatStale fact, one HeartbeatStale signal, and one parent resume, then stops.
    // Mutation pin: bypassing the liveness generation/outstanding fence makes the exact stale
    // counts fail after the additional deadline window.
    let fixture =
        OrchestratorTestFixture::with_execution_fixture(liveness_script(), liveness_options())
            .await
            .context("boot worker-owned liveness fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("worker-owned-liveness").await?;
    establish_idle_parent(test.client(), session_id).await?;
    allow_tools(&fixture, test.client(), session_id, [LIVENESS_CAPABILITY]).await?;
    start_worker(
        test.client(),
        session_id,
        LIVENESS_WORKER_ID,
        LIVENESS_WORKER_TASK,
        vec![tool_reference(LIVENESS_CAPABILITY)],
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("liveness fixture omitted capability controller")?;
    let barrier_deadline = Instant::now() + SCENARIO_TIMEOUT;
    while controller.calls().is_empty() {
        test.client()
            .post_void(
                &format!("/Worker/{LIVENESS_WORKER_ID}/record_heartbeat"),
                &Utc::now(),
            )
            .await
            .context("keep liveness worker healthy while it reaches the held effect")?;
        ensure!(
            Instant::now() < barrier_deadline,
            "liveness worker did not reach its held effect"
        );
        tokio::time::sleep(HEALTHY_HEARTBEAT_INTERVAL).await;
    }
    assert_eq!(
        controller.calls().len(),
        1,
        "the liveness worker reaches one exact held effect"
    );
    assert_eq!(
        session_events(test.client(), session_id)
            .await?
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::WorkerHeartbeatStale { worker_id, .. }
                    if worker_id == LIVENESS_WORKER_ID
            ))
            .count(),
        0,
        "a worker kept healthy while reaching its effect cannot already be stale"
    );

    let session_calls_before =
        invocation_count(&fixture, "Session", Some(&session_id.to_string()), None).await?;
    for _ in 0..HEALTHY_HEARTBEATS {
        test.client()
            .post_void(
                &format!("/Worker/{LIVENESS_WORKER_ID}/record_heartbeat"),
                &Utc::now(),
            )
            .await
            .context("refresh healthy worker heartbeat")?;
        tokio::time::sleep(HEALTHY_HEARTBEAT_INTERVAL).await;
        assert_eq!(
            invocation_count(&fixture, "Session", Some(&session_id.to_string()), None,).await?,
            session_calls_before,
            "fresh worker deadlines must reschedule on Worker without polling Session"
        );
    }
    let stale_events = await_events(
        test.client(),
        session_id,
        Duration::from_secs(10),
        |events| {
            events
                .iter()
                .filter(|record| {
                    matches!(
                        &record.event,
                        Event::WorkerHeartbeatStale { worker_id, .. }
                            if worker_id == LIVENESS_WORKER_ID
                    )
                })
                .count()
                == 1
                && signal_count(events, LIVENESS_WORKER_ID, ChildSignalKind::HeartbeatStale) == 1
                && parent_resume_count(events) == 1
                && events.iter().any(|record| {
                    matches!(
                        &record.event,
                        Event::BrainResponse { text, .. }
                            if text == "Stale worker resume observed."
                    )
                })
        },
    )
    .await?;
    let stale_seq = event_sequence(&stale_events, |event| {
        matches!(
            event,
            Event::WorkerHeartbeatStale { worker_id, .. }
                if worker_id == LIVENESS_WORKER_ID
        )
    })
    .context("stale worker omitted its stale fact")?;
    let signal_seq = event_sequence(&stale_events, |event| {
        matches!(
            event,
            Event::WorkerSignalReceived {
                worker_id,
                kind: ChildSignalKind::HeartbeatStale,
                ..
            } if worker_id == LIVENESS_WORKER_ID
        )
    })
    .context("stale worker omitted its attention signal")?;
    let resume_seq = event_sequence(&stale_events, |event| {
        matches!(event, Event::WorkerParentResumeRequested { .. })
    })
    .context("stale worker omitted its parent resume")?;
    assert!(
        stale_seq < signal_seq && signal_seq < resume_seq,
        "stale coordination must remain persisted fact -> joined signal -> resume"
    );
    assert_eq!(
        invocation_count(
            &fixture,
            "Session",
            Some(&session_id.to_string()),
            Some("record_child_signal")
        )
        .await?,
        1,
        "one stale transition makes one joined Session handoff"
    );
    let worker_status = test
        .client()
        .post_empty_call::<moa_core::types::worker::state::WorkerStatus>(&format!(
            "/Worker/{LIVENESS_WORKER_ID}/status"
        ))
        .await
        .context("read stale worker status")?;
    assert_eq!(
        worker_status.state,
        WorkerState::Running,
        "staleness is an attention transition, not a terminal worker state"
    );

    tokio::time::sleep(Duration::from_millis(LIVENESS_STALE_MS * 2 + 250)).await;
    let final_events = session_events(test.client(), session_id).await?;
    assert_eq!(
        final_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::WorkerHeartbeatStale { worker_id, .. }
                    if worker_id == LIVENESS_WORKER_ID
            ))
            .count(),
        1,
        "the stale deadline stops after its first accepted transition"
    );
    assert_eq!(
        signal_count(
            &final_events,
            LIVENESS_WORKER_ID,
            ChildSignalKind::HeartbeatStale,
        ),
        1
    );
    assert_eq!(parent_resume_count(&final_events), 1);
    assert_eq!(
        invocation_count(
            &fixture,
            "Session",
            Some(&session_id.to_string()),
            Some("record_child_signal")
        )
        .await?,
        1,
        "the Worker deadline cannot repeatedly wake Session after becoming stale"
    );
    Ok(())
}
