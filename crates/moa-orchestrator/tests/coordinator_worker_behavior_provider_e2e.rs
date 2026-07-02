//! Live provider E2E coverage for coordinator-to-worker behavior.
//!
//! These tests intentionally validate observable behavior instead of prompt or
//! schema structure: a real coordinator turn must delegate ready DAG nodes to
//! workers, wait for their results, and synthesize the expected final outcome.

#![cfg(feature = "integration")]

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_core::traits::Identity;
use moa_core::wire::turn::{StartTurnRequest, StartTurnResponse};
use moa_core::{
    Event, EventRange, EventRecord, ModelId, SessionActorRef, SessionId, SessionStatus, ToolCallId,
    ToolContent,
};
use tempfile::TempDir;
use tokio::time::sleep;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_operator, register_deployment, reserve_orchestrator_ports, restate_admin_url,
    restate_ingress_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, storage_partition_id_from_meta, test_session_meta,
};
use moa_test_support::postgres::test_database_url;

#[path = "support/mod.rs"]
mod support;

struct InitializedSession {
    session_id: SessionId,
    identity: Identity,
}

#[derive(Clone, Copy)]
struct LiveDelegationCase {
    name: &'static str,
    prompt: &'static str,
    expected_markers: &'static [&'static str],
    min_spawned: usize,
    min_spawns_before_first_wait: usize,
    requires_spawn_after_first_wait: bool,
}

#[derive(Default)]
struct CaseObservation {
    spawned: Vec<(u64, String)>,
    spawn_calls: Vec<(u64, ToolCallId)>,
    wait_calls: Vec<(u64, ToolCallId)>,
    tool_results: HashMap<ToolCallId, bool>,
    final_text_after_wait: Option<String>,
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    log_path: &Path,
) -> Result<Child> {
    let log = File::create(log_path).context("create orchestrator log file")?;
    let stderr = log.try_clone().context("clone orchestrator log file")?;
    Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"))
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
        .env("MOA_RESTATE_ADMIN_URL", restate_admin_url())
        .env("MOA_RESTATE_INGRESS_URL", restate_ingress_url())
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env("RUST_LOG", "info")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn moa-orchestrator binary for live provider behavior E2E")
}

fn truthy_env(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn configured_env(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| !value.trim().is_empty())
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

fn session_url(ingress: &str, session_id: SessionId, handler: &str) -> String {
    format!(
        "{}/restate/call/Session/{session_id}/{handler}",
        ingress.trim_end_matches('/')
    )
}

async fn create_initialized_session(
    client: &reqwest::Client,
    ingress: &str,
    model: &str,
    label: &str,
) -> Result<InitializedSession> {
    let mut meta = test_session_meta(label);
    meta.model = ModelId::new(model);
    meta.created_by = None;
    let storage_partition_id = storage_partition_id_from_meta(&meta);
    let mut identity = test_user_identity();
    identity.tenant_id = meta.tenant_id;
    meta.created_by = Some(SessionActorRef::Identity { id: identity.id });
    grant_tenant_operator(&identity, &storage_partition_id).await?;

    let create_request = client.post(format!(
        "{}/restate/call/SessionStore/create_session",
        ingress.trim_end_matches('/')
    ));
    let session_id = with_identity(create_request, &identity)
        .json(&meta)
        .send()
        .await
        .context("create session via restate ingress")?
        .error_for_status()
        .context("SessionStore create_session should succeed")?
        .json::<SessionId>()
        .await
        .context("deserialize create_session response")?;
    grant_session_participant(&identity, session_id).await?;

    client
        .post(format!(
            "{}/restate/call/SessionStore/init_session_vo",
            ingress.trim_end_matches('/')
        ))
        .json(&init_session_vo_request(session_id, meta))
        .send()
        .await
        .context("initialize session VO state")?
        .error_for_status()
        .context("SessionStore init_session_vo should succeed")?;

    Ok(InitializedSession {
        session_id,
        identity,
    })
}

async fn start_turn(
    client: &reqwest::Client,
    ingress: &str,
    session: &InitializedSession,
    case: LiveDelegationCase,
) -> Result<String> {
    let request = client.post(session_url(ingress, session.session_id, "start_turn"));
    let response = with_identity(request, &session.identity)
        .json(&StartTurnRequest {
            user_message: case.prompt.to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: Some(12),
        })
        .send()
        .await
        .with_context(|| format!("send Session/start_turn for {}", case.name))?
        .error_for_status()
        .context("Session/start_turn should succeed")?
        .json::<StartTurnResponse>()
        .await
        .context("deserialize Session/start_turn response")?;

    assert!(
        !response.queued,
        "fresh case {} should start immediately",
        case.name
    );
    response
        .turn_id
        .with_context(|| format!("case {} should return a turn id", case.name))
}

async fn wait_for_status(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
    expected: SessionStatus,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        let request = client.post(session_url(ingress, session_id, "status"));
        let status = with_identity(request, identity)
            .send()
            .await
            .context("call Session/status")?
            .error_for_status()
            .context("Session/status should succeed")?
            .json::<SessionStatus>()
            .await
            .context("deserialize Session/status")?;
        if status == expected {
            return Ok(());
        }
        last = Some(status);
        sleep(Duration::from_secs(1)).await;
    }

    bail!("timed out waiting for session {session_id} status {expected:?}; last={last:?}")
}

async fn session_events(
    client: &reqwest::Client,
    ingress: &str,
    identity: &Identity,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    let request = client.post(format!(
        "{}/restate/call/SessionStore/get_events",
        ingress.trim_end_matches('/')
    ));
    with_identity(request, identity)
        .json(&get_events_request(session_id, EventRange::all()))
        .send()
        .await
        .context("fetch events via restate ingress")?
        .error_for_status()
        .context("SessionStore/get_events should succeed")?
        .json::<Vec<EventRecord>>()
        .await
        .context("deserialize session events")
}

async fn run_case(case: LiveDelegationCase) -> Result<()> {
    if !truthy_env("MOA_RUN_LIVE_PROVIDER_TESTS") {
        bail!(
            "set MOA_RUN_LIVE_PROVIDER_TESTS=1 to run live provider coordinator-worker behavior E2E"
        );
    }
    let Some(model) = live_model() else {
        bail!(
            "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY"
        );
    };

    let _guard = RESTATE_E2E_LOCK.lock().await;
    let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
    let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
    let ports = reserve_orchestrator_ports()?;
    let endpoint_url = deployment_endpoint_url(ports.restate);
    let ingress = restate_ingress_url();
    let client = reqwest::Client::new();
    let orchestrator_log = memory_dir
        .path()
        .join(format!("orchestrator-{}.log", case.name));
    let mut orchestrator = spawn_orchestrator(ports, &memory_dir, &sandbox_dir, &orchestrator_log)?;

    let result = async {
        wait_for_orchestrator_health(&client, ports.health, Duration::from_secs(60))
            .await
            .with_context(|| {
                format!(
                    "spawned orchestrator for case {} did not become healthy; log follows:\n{}",
                    case.name,
                    read_log(&orchestrator_log)
                )
            })?;
        register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
        let session = create_initialized_session(&client, &ingress, model, case.name).await?;
        let turn_id = start_turn(&client, &ingress, &session, case).await?;
        wait_for_status(
            &client,
            &ingress,
            &session.identity,
            session.session_id,
            SessionStatus::Paused,
            Duration::from_secs(180),
        )
        .await
        .with_context(|| format!("case {} turn {turn_id} should complete", case.name))?;
        let events = session_events(&client, &ingress, &session.identity, session.session_id)
            .await
            .with_context(|| format!("fetch events for case {}", case.name))?;
        verify_case(case, &events)
    }
    .await;

    let _ = orchestrator.kill();
    let _ = orchestrator.wait();

    result
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("failed to read log: {error}"))
}

async fn wait_for_orchestrator_health(
    client: &reqwest::Client,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/_health/live");
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => last_error = Some(format!("status {}", response.status())),
            Err(error) => last_error = Some(error.to_string()),
        }
        sleep(Duration::from_millis(200)).await;
    }

    bail!("orchestrator health endpoint {url} was not ready within {timeout:?}: {last_error:?}")
}

fn verify_case(case: LiveDelegationCase, events: &[EventRecord]) -> Result<()> {
    let observation = observe_case(case, events);
    ensure_successful_delegation_tool_results(&observation).with_context(|| {
        format!(
            "case {} had failed or missing delegation tool results",
            case.name
        )
    })?;

    assert!(
        observation.spawned.len() >= case.min_spawned,
        "case {} should spawn at least {} workers, got {}\n{}",
        case.name,
        case.min_spawned,
        observation.spawned.len(),
        describe_events(events)
    );
    assert!(
        observation.wait_calls.len() >= case.min_spawned,
        "case {} should wait for at least {} workers, got {}\n{}",
        case.name,
        case.min_spawned,
        observation.wait_calls.len(),
        describe_events(events)
    );

    let first_wait_seq = observation
        .wait_calls
        .iter()
        .map(|(seq, _)| *seq)
        .min()
        .with_context(|| format!("case {} should call wait_worker", case.name))?;
    let spawns_before_first_wait = observation
        .spawn_calls
        .iter()
        .filter(|(seq, _)| *seq < first_wait_seq)
        .count();
    assert!(
        spawns_before_first_wait >= case.min_spawns_before_first_wait,
        "case {} should spawn at least {} ready workers before first wait, got {}\n{}",
        case.name,
        case.min_spawns_before_first_wait,
        spawns_before_first_wait,
        describe_events(events)
    );

    if case.requires_spawn_after_first_wait {
        assert!(
            observation
                .spawn_calls
                .iter()
                .any(|(seq, _)| *seq > first_wait_seq),
            "case {} should spawn a dependent worker after first wait\n{}",
            case.name,
            describe_events(events)
        );
    }

    let final_text = observation.final_text_after_wait.with_context(|| {
        format!(
            "case {} should produce a final BrainResponse after wait_worker containing {:?}\n{}",
            case.name,
            case.expected_markers,
            describe_events(events)
        )
    })?;
    for marker in case.expected_markers {
        assert!(
            final_text.contains(marker),
            "case {} final text should contain marker {marker:?}; final text: {final_text}",
            case.name
        );
    }

    Ok(())
}

fn ensure_successful_delegation_tool_results(observation: &CaseObservation) -> Result<()> {
    let mut ids = HashSet::new();
    for (_, tool_id) in observation
        .spawn_calls
        .iter()
        .chain(observation.wait_calls.iter())
    {
        ids.insert(*tool_id);
    }
    for tool_id in ids {
        match observation.tool_results.get(&tool_id).copied() {
            Some(true) => {}
            Some(false) => bail!("delegation tool {tool_id} returned success=false"),
            None => bail!("delegation tool {tool_id} did not persist a ToolResult"),
        }
    }
    Ok(())
}

fn observe_case(case: LiveDelegationCase, events: &[EventRecord]) -> CaseObservation {
    let mut observation = CaseObservation::default();
    for record in events {
        match &record.event {
            Event::WorkerSpawned { task, .. } => {
                observation
                    .spawned
                    .push((record.sequence_num, task.clone()));
            }
            Event::ToolCall {
                tool_id, tool_name, ..
            } if tool_name == "spawn_worker" => {
                observation
                    .spawn_calls
                    .push((record.sequence_num, *tool_id));
            }
            Event::ToolCall {
                tool_id, tool_name, ..
            } if tool_name == "wait_worker" => {
                observation.wait_calls.push((record.sequence_num, *tool_id));
            }
            Event::ToolResult {
                tool_id, success, ..
            } => {
                observation.tool_results.insert(*tool_id, *success);
            }
            Event::BrainResponse { text, .. } => {
                let last_wait_seq = observation
                    .wait_calls
                    .iter()
                    .map(|(seq, _)| *seq)
                    .max()
                    .unwrap_or(0);
                if record.sequence_num > last_wait_seq
                    && case
                        .expected_markers
                        .iter()
                        .all(|marker| text.contains(marker))
                {
                    observation.final_text_after_wait = Some(text.clone());
                }
            }
            _ => {}
        }
    }
    observation
}

fn describe_events(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| match &record.event {
            Event::UserMessage { text, .. } => {
                format!("#{} UserMessage {}", record.sequence_num, compact(text))
            }
            Event::BrainResponse { text, .. } => {
                format!("#{} BrainResponse {}", record.sequence_num, compact(text))
            }
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                format!(
                    "#{} ToolCall {} {} {}",
                    record.sequence_num,
                    tool_name,
                    tool_id,
                    compact(&input.to_string())
                )
            }
            Event::ToolResult {
                tool_id,
                success,
                output,
                ..
            } => {
                format!(
                    "#{} ToolResult {} success={} {}",
                    record.sequence_num,
                    tool_id,
                    success,
                    compact(&tool_output_text(output))
                )
            }
            Event::WorkerSpawned {
                worker_id, task, ..
            } => {
                format!(
                    "#{} WorkerSpawned {} {}",
                    record.sequence_num,
                    worker_id,
                    compact(task)
                )
            }
            Event::WorkerStatusChanged {
                worker_id,
                to,
                summary,
                ..
            } => {
                format!(
                    "#{} WorkerStatusChanged {} {:?} {}",
                    record.sequence_num,
                    worker_id,
                    to,
                    compact(summary.as_deref().unwrap_or(""))
                )
            }
            Event::WorkerNotificationDelivered {
                worker_id,
                state,
                summary,
            } => {
                format!(
                    "#{} WorkerNotificationDelivered {} {:?} {}",
                    record.sequence_num,
                    worker_id,
                    state,
                    compact(summary)
                )
            }
            other => format!("#{} {:?}", record.sequence_num, other),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_output_text(output: &moa_core::ToolOutput) -> String {
    output
        .content
        .iter()
        .map(|content| match content {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Json { data } => data.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact(text: &str) -> String {
    let mut value = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.len() > 320 {
        value.truncate(320);
        value.push_str("...");
    }
    value
}

fn case_parallel_two() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "parallel_two_ready_nodes",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A and B are independent ready nodes. Spawn A and B before any wait. ",
            "Use spawn_worker for each with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A returns exactly PAL_LEVEL=YES after checking whether 'level' is a palindrome. ",
            "B returns exactly SUM_13_29=42 after computing 13+29. ",
            "Wait for both workers. Then answer exactly: FINAL CASE-01 PAL_LEVEL=YES SUM_13_29=42"
        ),
        expected_markers: &["FINAL CASE-01", "PAL_LEVEL=YES", "SUM_13_29=42"],
        min_spawned: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_parallel_three() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "parallel_three_ready_nodes",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A, B, and C are independent ready nodes. Spawn all three before any wait. ",
            "Use spawn_worker with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A uppercases 'moa' and returns UPPER=MOA. ",
            "B alphabetically sorts the letters in 'cab' and returns SORTED=abc. ",
            "C counts vowels in 'education' and returns VOWELS=5. ",
            "Wait for all three workers. Then answer exactly: ",
            "FINAL CASE-02 UPPER=MOA SORTED=abc VOWELS=5"
        ),
        expected_markers: &["FINAL CASE-02", "UPPER=MOA", "SORTED=abc", "VOWELS=5"],
        min_spawned: 3,
        min_spawns_before_first_wait: 3,
        requires_spawn_after_first_wait: false,
    }
}

fn case_single_worker() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "single_worker_when_task_is_atomic",
        prompt: concat!(
            "Coordinator-worker behavior check. This task has one DAG node only. ",
            "Spawn exactly one worker with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "The worker computes 21*2 and returns PRODUCT=42. ",
            "Wait for that worker. Then answer exactly: FINAL CASE-03 PRODUCT=42"
        ),
        expected_markers: &["FINAL CASE-03", "PRODUCT=42"],
        min_spawned: 1,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: false,
    }
}

fn case_sequential_dependency() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "sequential_dependency_waits_before_dependent_spawn",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: B depends on A. Spawn A first with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A extracts the numbers from 'red=6 blue=7' and returns FACTORS=6,7. ",
            "Wait for A before spawning B. Then spawn B with A's result; B returns PRODUCT=42. ",
            "Wait for B. Then answer exactly: FINAL CASE-04 FACTORS=6,7 PRODUCT=42"
        ),
        expected_markers: &["FINAL CASE-04", "FACTORS=6,7", "PRODUCT=42"],
        min_spawned: 2,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: true,
    }
}

fn case_mixed_dag() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "mixed_parallel_then_dependent_node",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A and B are independent; C depends on A and B. ",
            "Spawn A and B before any wait, with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A reads 'alpha:4; beta:8' and returns SUM_AB=12. ",
            "B counts characters in 'test' and returns LEN_TEST=4. ",
            "Wait for A and B before spawning C. C computes 12+4+26 and returns TOTAL=42. ",
            "Wait for C. Then answer exactly: FINAL CASE-05 SUM_AB=12 LEN_TEST=4 TOTAL=42"
        ),
        expected_markers: &["FINAL CASE-05", "SUM_AB=12", "LEN_TEST=4", "TOTAL=42"],
        min_spawned: 3,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: true,
    }
}

fn case_fan_out_fan_in() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "fan_out_fan_in_total",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A, B, and C are independent ready nodes. Spawn all three before any wait. ",
            "Use max_turns 1, budget_tokens 1200, and tool_subset [] for each worker. ",
            "A returns N1=21 from 20+1. B returns N2=20 from 10*2. C returns N3=1 from 3-2. ",
            "Wait for all workers, add their values yourself, and answer exactly: ",
            "FINAL CASE-06 N1=21 N2=20 N3=1 TOTAL=42"
        ),
        expected_markers: &["FINAL CASE-06", "N1=21", "N2=20", "N3=1", "TOTAL=42"],
        min_spawned: 3,
        min_spawns_before_first_wait: 3,
        requires_spawn_after_first_wait: false,
    }
}

fn case_skill_steps() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "worker_receives_applicable_skill_steps",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "Applicable skill mini-normalize-v1 has steps: lowercase text, remove spaces, count characters. ",
            "DAG: A and B are independent ready nodes. Spawn both before any wait. ",
            "Each worker task must include the mini-normalize-v1 steps. ",
            "A applies the skill to 'MO A' and returns A_VALUE=moa A_LEN=3. ",
            "B applies the skill to 'D AG' and returns B_VALUE=dag B_LEN=3. ",
            "Wait for both workers. Then answer exactly: ",
            "FINAL CASE-07 A_VALUE=moa A_LEN=3 B_VALUE=dag B_LEN=3 SKILL_TOTAL_LEN=6"
        ),
        expected_markers: &[
            "FINAL CASE-07",
            "A_VALUE=moa",
            "A_LEN=3",
            "B_VALUE=dag",
            "B_LEN=3",
            "SKILL_TOTAL_LEN=6",
        ],
        min_spawned: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_independent_validation() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "parallel_validation_workers",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A and B are independent validation nodes. Spawn both before any wait. ",
            "Use max_turns 1, budget_tokens 1200, and tool_subset [] for each worker. ",
            "A validates '18+24=42' and returns VALID_A=true. ",
            "B validates '50-8=42' and returns VALID_B=true. ",
            "Wait for both workers. Then answer exactly: FINAL CASE-08 VALID_A=true VALID_B=true BOTH_VALID=true"
        ),
        expected_markers: &[
            "FINAL CASE-08",
            "VALID_A=true",
            "VALID_B=true",
            "BOTH_VALID=true",
        ],
        min_spawned: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_quality_gate_chain() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "quality_gate_depends_on_worker_result",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: B depends on A. Spawn A first with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A returns DRAFT=DELTA. Wait for A. ",
            "Then spawn B to check A's output contains DELTA; B returns QA=PASS. ",
            "Wait for B. Then answer exactly: FINAL CASE-09 DRAFT=DELTA QA=PASS"
        ),
        expected_markers: &["FINAL CASE-09", "DRAFT=DELTA", "QA=PASS"],
        min_spawned: 2,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: true,
    }
}

fn case_parallel_four() -> LiveDelegationCase {
    LiveDelegationCase {
        name: "parallel_four_ready_nodes",
        prompt: concat!(
            "Coordinator-worker behavior check. Use workers; do not solve directly. ",
            "DAG: A, B, C, and D are independent ready nodes. Spawn all four before any wait. ",
            "Use max_turns 1, budget_tokens 1200, and tool_subset [] for each worker. ",
            "A returns NORTH_FIRST=N. B returns EAST_LAST=t. C returns POW=32 from 2^5. ",
            "D returns TEN=10. Wait for all workers. Then answer exactly: ",
            "FINAL CASE-10 NORTH_FIRST=N EAST_LAST=t POW=32 TEN=10 TOTAL=42"
        ),
        expected_markers: &[
            "FINAL CASE-10",
            "NORTH_FIRST=N",
            "EAST_LAST=t",
            "POW=32",
            "TEN=10",
            "TOTAL=42",
        ],
        min_spawned: 4,
        min_spawns_before_first_wait: 4,
        requires_spawn_after_first_wait: false,
    }
}

macro_rules! live_case_test {
    ($test_name:ident, $case_fn:ident) => {
        #[tokio::test]
        #[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, local Restate/Postgres, and a provider API key"]
        async fn $test_name() -> Result<()> {
            run_case($case_fn()).await
        }
    };
}

live_case_test!(
    coordinator_parallel_two_ready_nodes_provider_e2e,
    case_parallel_two
);
live_case_test!(
    coordinator_parallel_three_ready_nodes_provider_e2e,
    case_parallel_three
);
live_case_test!(
    coordinator_single_worker_atomic_task_provider_e2e,
    case_single_worker
);
live_case_test!(
    coordinator_sequential_dependency_provider_e2e,
    case_sequential_dependency
);
live_case_test!(coordinator_mixed_dag_provider_e2e, case_mixed_dag);
live_case_test!(coordinator_fan_out_fan_in_provider_e2e, case_fan_out_fan_in);
live_case_test!(coordinator_skill_steps_provider_e2e, case_skill_steps);
live_case_test!(
    coordinator_parallel_validation_provider_e2e,
    case_independent_validation
);
live_case_test!(
    coordinator_quality_gate_chain_provider_e2e,
    case_quality_gate_chain
);
live_case_test!(
    coordinator_parallel_four_ready_nodes_provider_e2e,
    case_parallel_four
);
