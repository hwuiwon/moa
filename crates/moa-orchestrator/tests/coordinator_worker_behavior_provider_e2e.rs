//! Live provider E2E coverage for bounded Act behavior and supplementary planning checks.
//!
//! These tests intentionally validate observable behavior instead of prompt or
//! schema structure: a real interactive turn delegates conversational subtasks,
//! waits for worker results, and produces the expected final outcome. Durable
//! dependency graphs and bulk execution belong to `ExecutionRun` and
//! `ExecutionTask`, not this worker-delegation lane.

#![cfg(feature = "integration")]

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use moa_artifacts::canonical::canonical_json_bytes as artifact_canonical_json_bytes;
use moa_artifacts::execution_plan::{
    CompletionCheckKind, ExecutionOperation, GeneratedExecutionCandidate,
};
use moa_core::traits::Identity;
use moa_core::wire::turn::{StartTurnRequest, StartTurnResponse};
use moa_core::{
    events::Event,
    types::contact::SessionActorRef,
    types::events_stream::EventRange,
    types::events_stream::EventRecord,
    types::execution_planning::{
        ExecutionAuditReportV1, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelopeV1,
        ExecutionPlanningAuditPayloadV1, ExecutionRouteKind, ExecutionRouteReason,
        ExecutionRouteStage, ExecutionRunAdmissionStatus, ExecutionRunStarted,
        ExecutionSourceProvenanceV1, ExecutionStrategy, GeneratedPlanPlannerProvenanceV1,
        execution_planning_hash, validate_planning_audit_envelope,
    },
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::ToolCallId,
    types::session::SessionStatus,
    types::tools::ToolContent,
};
use moa_execution::repository::{ExecutionRepository, ExecutionScope};
use moa_execution::state::{
    ExecutionRunStatus, ExecutionTaskStatus, ExecutionTerminalCause, ExecutionTerminalEvidence,
};
use moa_execution::wire::{
    ExecutionCancelRequest, ExecutionConflictReason, ExecutionMutationResponse,
    ExecutionPlanningContextRequest, ExecutionPlanningContextResponse, ExecutionRunRequest,
    ExecutionStatusResponse, ExecutionTaskListRequest, ExecutionTaskListResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
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
use moa_test_support::execution_audits::load_execution_planning_audits;
use moa_test_support::postgres::test_database_url;

#[path = "support/mod.rs"]
mod support;

struct InitializedSession {
    session_id: SessionId,
    identity: Identity,
}

#[derive(Clone, Copy)]
struct LiveActDelegationCase {
    name: &'static str,
    prompt: &'static str,
    expected_markers: &'static [&'static str],
    min_spawned: usize,
    min_wait_calls: usize,
    min_spawns_before_first_wait: usize,
    requires_spawn_after_first_wait: bool,
}

#[derive(Default)]
struct CaseObservation {
    spawned: Vec<(u64, String)>,
    spawn_calls: Vec<(u64, ToolCallId)>,
    wait_calls: Vec<(u64, ToolCallId)>,
    worker_notifications: Vec<(u64, String)>,
    tool_results: HashMap<ToolCallId, bool>,
    final_text_after_result: Option<String>,
}

struct GeneratedPlanAuditEvidence {
    candidate: GeneratedExecutionCandidate,
    originating_sequence: u64,
    hashes: GeneratedPlanHashEvidence,
}

#[derive(Clone)]
struct GeneratedPlanHashEvidence {
    planner_candidate_hash: String,
    compiler_candidate_hash: String,
    expected_planner_candidate_hash: String,
    expected_compiler_candidate_hash: String,
    planner_report_hash: String,
    compiler_report_hash: String,
    final_plan_hash: String,
}

#[derive(Serialize)]
struct SupplementaryInitialCompileCandidate<'a> {
    kind: &'static str,
    schema_version: u8,
    source: ExecutionCompileSource,
    goal: &'a moa_artifacts::execution_plan::ExecutionGoalContract,
    plan: &'a moa_artifacts::execution_plan::ExecutionPlanDefinition,
    run_input: &'a Value,
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
    case: LiveActDelegationCase,
) -> Result<String> {
    let request = client.post(session_url(ingress, session.session_id, "start_turn"));
    let response = with_identity(request, &session.identity)
        .json(&StartTurnRequest {
            user_message: case.prompt.to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: Some(12),
            execution_template: None,
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

async fn run_case(case: LiveActDelegationCase) -> Result<()> {
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

fn verify_case(case: LiveActDelegationCase, events: &[EventRecord]) -> Result<()> {
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
        observation.wait_calls.len() >= case.min_wait_calls,
        "case {} should call wait_worker at least {} times, got {}\n{}",
        case.name,
        case.min_wait_calls,
        observation.wait_calls.len(),
        describe_events(events)
    );
    assert!(
        observation.worker_notifications.len() == observation.spawned.len(),
        "case {} should receive exactly one ordinary WorkerNotificationDelivered lifecycle report for each spawned worker; got {} notifications for {} workers\n{}",
        case.name,
        observation.worker_notifications.len(),
        observation.spawned.len(),
        describe_events(events)
    );

    let first_wait_seq = observation
        .wait_calls
        .iter()
        .map(|(seq, _)| *seq)
        .min()
        .with_context(|| format!("case {} should wait for a worker result", case.name))?;
    let spawns_before_first_wait = observation
        .spawn_calls
        .iter()
        .filter(|(seq, _)| *seq < first_wait_seq)
        .count();
    assert!(
        spawns_before_first_wait >= case.min_spawns_before_first_wait,
        "case {} should spawn at least {} independent conversational workers before the first wait, got {}\n{}",
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
            "case {} should spawn a follow-up conversational worker after the earlier wait\n{}",
            case.name,
            describe_events(events)
        );
    }

    let final_text = observation.final_text_after_result.with_context(|| {
        format!(
            "case {} should produce a final BrainResponse after observing worker results containing {:?}\n{}",
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

fn observe_case(case: LiveActDelegationCase, events: &[EventRecord]) -> CaseObservation {
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
            Event::WorkerNotificationDelivered { worker_id, .. } => {
                observation
                    .worker_notifications
                    .push((record.sequence_num, worker_id.clone()));
            }
            Event::ToolResult {
                tool_id, success, ..
            } => {
                observation.tool_results.insert(*tool_id, *success);
            }
            Event::BrainResponse { text, .. } => {
                let last_result_seq = observation
                    .worker_result_sequences()
                    .into_iter()
                    .max()
                    .unwrap_or(0);
                if record.sequence_num > last_result_seq
                    && case
                        .expected_markers
                        .iter()
                        .all(|marker| text.contains(marker))
                {
                    observation.final_text_after_result = Some(text.clone());
                }
            }
            _ => {}
        }
    }
    observation
}

impl CaseObservation {
    fn worker_result_sequences(&self) -> Vec<u64> {
        self.wait_calls
            .iter()
            .map(|(seq, _)| *seq)
            .chain(self.worker_notifications.iter().map(|(seq, _)| *seq))
            .collect()
    }
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

fn tool_output_text(output: &moa_core::types::tools::ToolOutput) -> String {
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

fn case_act_parallel_two() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_parallel_two_independent_delegations",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "A and B are independent subtasks. Spawn A and B before observing either result. ",
            "Use spawn_worker for each with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A returns exactly PAL_LEVEL=YES after checking whether 'level' is a palindrome. ",
            "B returns exactly SUM_13_29=42 after computing 13+29. ",
            "Wait for both workers. Then answer exactly: FINAL CASE-01 PAL_LEVEL=YES SUM_13_29=42"
        ),
        expected_markers: &["FINAL CASE-01", "PAL_LEVEL=YES", "SUM_13_29=42"],
        min_spawned: 2,
        min_wait_calls: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_act_parallel_three() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_parallel_three_independent_delegations",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "A, B, and C are independent subtasks. Spawn all three before observing any result. ",
            "Use spawn_worker with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A uppercases 'moa' and returns UPPER=MOA. ",
            "B alphabetically sorts the letters in 'cab' and returns SORTED=abc. ",
            "C counts vowels in 'education' and returns VOWELS=5. ",
            "Wait for all three workers. Then answer exactly: ",
            "FINAL CASE-02 UPPER=MOA SORTED=abc VOWELS=5"
        ),
        expected_markers: &["FINAL CASE-02", "UPPER=MOA", "SORTED=abc", "VOWELS=5"],
        min_spawned: 3,
        min_wait_calls: 3,
        min_spawns_before_first_wait: 3,
        requires_spawn_after_first_wait: false,
    }
}

fn case_act_single_worker() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_single_bounded_delegation",
        prompt: concat!(
            "Bounded interactive delegation check. This request has one conversational subtask. ",
            "Spawn exactly one worker with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "The worker computes 21*2 and returns PRODUCT=42. ",
            "Wait for that worker. Then answer exactly: FINAL CASE-03 PRODUCT=42"
        ),
        expected_markers: &["FINAL CASE-03", "PRODUCT=42"],
        min_spawned: 1,
        min_wait_calls: 1,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: false,
    }
}

fn case_act_follow_up_uses_prior_result() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_follow_up_delegation_uses_prior_result",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "B needs A's result. Spawn A first with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A extracts the numbers from 'red=6 blue=7' and returns FACTORS=6,7. ",
            "Observe A's result before spawning B. Then brief B with A's result; B returns PRODUCT=42. ",
            "Wait for B. Then answer exactly: FINAL CASE-04 FACTORS=6,7 PRODUCT=42"
        ),
        expected_markers: &["FINAL CASE-04", "FACTORS=6,7", "PRODUCT=42"],
        min_spawned: 2,
        min_wait_calls: 2,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: true,
    }
}

fn case_act_parallel_then_follow_up() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_parallel_then_follow_up_delegation",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "A and B are independent subtasks; C needs both of their results. ",
            "Spawn A and B before observing either result, with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A reads 'alpha:4; beta:8' and returns SUM_AB=12. ",
            "B counts characters in 'test' and returns LEN_TEST=4. ",
            "Observe A and B before briefing and spawning C. C computes 12+4+26 and returns TOTAL=42. ",
            "Wait for C. Then answer exactly: FINAL CASE-05 SUM_AB=12 LEN_TEST=4 TOTAL=42"
        ),
        expected_markers: &["TOTAL=42", "12", "4"],
        min_spawned: 3,
        min_wait_calls: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: true,
    }
}

fn case_act_delegated_instructions() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_workers_follow_delegated_normalization_instructions",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "The delegated normalization instructions are: lowercase text, remove spaces, count characters. ",
            "A and B are independent subtasks. Spawn both before observing either result. ",
            "Include the normalization instructions in each worker task. ",
            "A applies the instructions to 'MO A' and returns A_VALUE=moa A_LEN=3. ",
            "B applies the instructions to 'D AG' and returns B_VALUE=dag B_LEN=3. ",
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
        min_wait_calls: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_act_independent_cross_checks() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_parallel_independent_cross_checks",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "A and B are independent cross-checks. Spawn both before observing either result. ",
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
        min_wait_calls: 2,
        min_spawns_before_first_wait: 2,
        requires_spawn_after_first_wait: false,
    }
}

fn case_act_follow_up_quality_check() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_follow_up_quality_check_uses_worker_result",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "B checks A's result. Spawn A first with max_turns 1, budget_tokens 1200, and tool_subset []. ",
            "A returns DRAFT=DELTA. Observe A's result. ",
            "Then spawn B to check A's output contains DELTA; B returns QA=PASS. ",
            "Wait for B. Then answer exactly: FINAL CASE-09 DRAFT=DELTA QA=PASS"
        ),
        expected_markers: &["FINAL CASE-09", "DRAFT=DELTA", "QA=PASS"],
        min_spawned: 2,
        min_wait_calls: 2,
        min_spawns_before_first_wait: 1,
        requires_spawn_after_first_wait: true,
    }
}

fn case_act_parallel_four() -> LiveActDelegationCase {
    LiveActDelegationCase {
        name: "act_parallel_four_independent_delegations",
        prompt: concat!(
            "Bounded interactive delegation check. Use conversational workers; do not solve directly. ",
            "A, B, C, and D are independent subtasks. Spawn all four before observing any result. ",
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
        min_wait_calls: 4,
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
    coordinator_act_parallel_two_delegations_provider_e2e,
    case_act_parallel_two
);
live_case_test!(
    coordinator_act_parallel_three_delegations_provider_e2e,
    case_act_parallel_three
);
live_case_test!(
    coordinator_act_single_bounded_delegation_provider_e2e,
    case_act_single_worker
);
live_case_test!(
    coordinator_act_follow_up_uses_prior_result_provider_e2e,
    case_act_follow_up_uses_prior_result
);
live_case_test!(
    coordinator_act_parallel_then_follow_up_provider_e2e,
    case_act_parallel_then_follow_up
);
live_case_test!(
    coordinator_act_delegated_instructions_provider_e2e,
    case_act_delegated_instructions
);
live_case_test!(
    coordinator_act_parallel_cross_checks_provider_e2e,
    case_act_independent_cross_checks
);
live_case_test!(
    coordinator_act_follow_up_quality_check_provider_e2e,
    case_act_follow_up_quality_check
);
live_case_test!(
    coordinator_act_parallel_four_delegations_provider_e2e,
    case_act_parallel_four
);

const AMBIGUOUS_MODE_SELECTION_PROMPT: &str = concat!(
    "A production total sometimes appears inconsistent. Deeply investigate whether 19 + 23 ",
    "equals 42, explain why the check is bounded, and recommend one next check. ",
    "Answer from reasoning only; do not call tools, delegate, or start durable work."
);

const GENERATED_PLAN_QUALITY_PROMPT: &str = concat!(
    "Start an execution run to independently verify that 19 + 23 equals 42 and return a ",
    "structured JSON report with integer field sum and boolean field verified. Generate exactly ",
    "one bounded Agent node with at most two turns and no skills or capabilities, followed by ",
    "exactly one Output node that depends on the Agent and returns the Agent output through a ",
    "direct execution reference. Preserve explicit immutable requirements and gate completion ",
    "with an output-schema check. Do not use capability, map, reduce, review, wait-signal, or ",
    "agent-verifier operations."
);

struct SupplementaryLiveHarness {
    client: reqwest::Client,
    ingress: String,
    session: InitializedSession,
    orchestrator: Child,
    orchestrator_log: std::path::PathBuf,
    _memory_dir: TempDir,
    _sandbox_dir: TempDir,
    _restate_guard: tokio::sync::MutexGuard<'static, ()>,
}

impl SupplementaryLiveHarness {
    async fn start(name: &str) -> Result<Self> {
        if !truthy_env("MOA_RUN_LIVE_PROVIDER_TESTS") {
            bail!(
                "set MOA_RUN_LIVE_PROVIDER_TESTS=1 to run supplementary live provider planning E2E"
            );
        }
        let Some(model) = live_model() else {
            bail!(
                "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY"
            );
        };

        let restate_guard = RESTATE_E2E_LOCK.lock().await;
        let memory_dir =
            tempfile::tempdir().context("create supplementary temporary memory root")?;
        let sandbox_dir =
            tempfile::tempdir().context("create supplementary temporary sandbox root")?;
        let ports = reserve_orchestrator_ports()?;
        let endpoint_url = deployment_endpoint_url(ports.restate);
        let ingress = restate_ingress_url();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build bounded supplementary live-provider HTTP client")?;
        let orchestrator_log = memory_dir.path().join(format!("orchestrator-{name}.log"));
        let mut orchestrator = spawn_supplementary_orchestrator(
            ports,
            &memory_dir,
            &sandbox_dir,
            &orchestrator_log,
            model,
        )?;

        let setup = async {
            wait_for_orchestrator_health(&client, ports.health, Duration::from_secs(60))
                .await
                .with_context(|| {
                    format!(
                        "supplementary orchestrator for {name} did not become healthy; log follows:\n{}",
                        read_log(&orchestrator_log)
                    )
                })?;
            register_deployment(&restate_admin_url(), endpoint_url.as_str()).await?;
            create_initialized_session(&client, &ingress, model, name).await
        }
        .await;

        match setup {
            Ok(session) => Ok(Self {
                client,
                ingress,
                session,
                orchestrator,
                orchestrator_log,
                _memory_dir: memory_dir,
                _sandbox_dir: sandbox_dir,
                _restate_guard: restate_guard,
            }),
            Err(error) => {
                let _ = orchestrator.kill();
                let _ = orchestrator.wait();
                Err(error)
            }
        }
    }

    async fn start_turn(&self, prompt: &str, max_turns: u32) -> Result<String> {
        let request = self.client.post(session_url(
            &self.ingress,
            self.session.session_id,
            "start_turn",
        ));
        let response = with_identity(request, &self.session.identity)
            .json(&StartTurnRequest {
                user_message: prompt.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: Some(max_turns),
                execution_template: None,
            })
            .send()
            .await
            .context("send supplementary Session/start_turn")?
            .error_for_status()
            .context("supplementary Session/start_turn should succeed")?
            .json::<StartTurnResponse>()
            .await
            .context("deserialize supplementary Session/start_turn response")?;
        assert!(
            !response.queued,
            "fresh supplementary turn should not queue"
        );
        response
            .turn_id
            .context("supplementary turn should return a turn id")
    }

    async fn events(&self) -> Result<Vec<EventRecord>> {
        session_events(
            &self.client,
            &self.ingress,
            &self.session.identity,
            self.session.session_id,
        )
        .await
    }

    async fn execution_call<Request, Response>(
        &self,
        handler: &str,
        request_body: &Request,
    ) -> Result<Response>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let request = self.client.post(format!(
            "{}/restate/call/Execution/{handler}",
            self.ingress.trim_end_matches('/')
        ));
        with_identity(request, &self.session.identity)
            .json(request_body)
            .send()
            .await
            .with_context(|| format!("call Execution/{handler}"))?
            .error_for_status()
            .with_context(|| format!("Execution/{handler} should succeed"))?
            .json::<Response>()
            .await
            .with_context(|| format!("deserialize Execution/{handler} response"))
    }

    async fn wait_for_run_started(
        &self,
        timeout: Duration,
    ) -> Result<(ExecutionRunStarted, Vec<EventRecord>)> {
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events().await?;
            let started = events.iter().find_map(|record| match &record.event {
                Event::ExecutionRunStarted(started) => Some(started.clone()),
                _ => None,
            });
            if let Some(started) = started {
                return Ok((started, events));
            }
            if Instant::now() >= deadline {
                bail!(
                    "supplementary run was not admitted within {timeout:?}; log follows:\n{}\n{}",
                    read_log(&self.orchestrator_log),
                    describe_events(&events)
                );
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn wait_for_terminal_status(
        &self,
        request: &ExecutionRunRequest,
        timeout: Duration,
    ) -> Result<ExecutionStatusResponse> {
        let deadline = Instant::now() + timeout;
        loop {
            let status: ExecutionStatusResponse = self.execution_call("status", request).await?;
            if status.run.status.is_terminal() {
                return Ok(status);
            }
            if status.run.status == ExecutionRunStatus::AwaitingConfirmation {
                bail!(
                    "small supplementary generated plan unexpectedly required confirmation: {status:?}"
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "supplementary run {} did not become terminal within {timeout:?}; last={:?}; log follows:\n{}",
                    request.run_uid,
                    status.run.status,
                    read_log(&self.orchestrator_log)
                );
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn cancel_run_and_verify(
        &self,
        request: &ExecutionRunRequest,
        reason: &str,
        timeout: Duration,
    ) -> Result<()> {
        let response: ExecutionMutationResponse = self
            .execution_call(
                "cancel",
                &ExecutionCancelRequest {
                    run: request.clone(),
                    reason: reason.to_string(),
                },
            )
            .await?;
        ensure!(
            matches!(
                response,
                ExecutionMutationResponse::Applied { ref run }
                    | ExecutionMutationResponse::Replayed { ref run }
                    if run.status == ExecutionRunStatus::Cancelled
            ) || matches!(
                response,
                ExecutionMutationResponse::Conflict {
                    reason: ExecutionConflictReason::AlreadyTerminal
                }
            ),
            "supplementary cleanup must cancel or observe an already-terminal run: {response:?}"
        );

        let deadline = Instant::now() + timeout;
        loop {
            let status: ExecutionStatusResponse = self.execution_call("status", request).await?;
            let events = self.events().await?;
            let has_cancelled_event = events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::ExecutionCancelled(summary) if summary.run_uid == request.run_uid
                )
            });
            if status.run.status == ExecutionRunStatus::Cancelled && has_cancelled_event {
                return Ok(());
            }
            if status.run.status.is_terminal() && status.run.status != ExecutionRunStatus::Cancelled
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "cancelled run {} did not converge to Cancelled plus ExecutionCancelled within {timeout:?}; status={:?}; log follows:\n{}\n{}",
                    request.run_uid,
                    status.run.status,
                    read_log(&self.orchestrator_log),
                    describe_events(&events)
                );
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn cleanup_admitted_run_after_error(
        &self,
        request: &ExecutionRunRequest,
        reason: &str,
    ) -> Result<()> {
        let status = self
            .execution_call::<_, ExecutionStatusResponse>("status", request)
            .await;
        if matches!(&status, Ok(status) if status.run.status.is_terminal()) {
            return Ok(());
        }
        self.cancel_run_and_verify(request, reason, Duration::from_secs(30))
            .await
            .with_context(|| match status {
                Ok(status) => format!(
                    "cancel admitted run after verification failure from status {:?}",
                    status.run.status
                ),
                Err(error) => format!(
                    "cancel admitted run after status read failed during cleanup: {error:#}"
                ),
            })
    }

    async fn wait_for_completed_event(
        &self,
        run_uid: uuid::Uuid,
        timeout: Duration,
    ) -> Result<Vec<EventRecord>> {
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events().await?;
            if events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::ExecutionCompleted(summary) if summary.run_uid == run_uid
                )
            }) {
                return Ok(events);
            }
            if Instant::now() >= deadline {
                bail!(
                    "completed run {run_uid} did not publish ExecutionCompleted within {timeout:?}; log follows:\n{}\n{}",
                    read_log(&self.orchestrator_log),
                    describe_events(&events)
                );
            }
            sleep(Duration::from_millis(500)).await;
        }
    }
}

impl Drop for SupplementaryLiveHarness {
    fn drop(&mut self) {
        let _ = self.orchestrator.kill();
        let _ = self.orchestrator.wait();
    }
}

fn spawn_supplementary_orchestrator(
    ports: OrchestratorPorts,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    log_path: &Path,
    model: &str,
) -> Result<Child> {
    let log = File::create(log_path).context("create supplementary orchestrator log file")?;
    let stderr = log
        .try_clone()
        .context("clone supplementary orchestrator log file")?;
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
        .env("MOA_MODELS_MAIN", model)
        .env("MOA_MODELS_AUXILIARY", model)
        .env("MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD", "100000000")
        .env("RUST_LOG", "info")
        .env_remove("MOA_COHERE_API_KEY")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn supplementary live-provider orchestrator")
}

async fn supplementary_planning_audits(
    harness: &SupplementaryLiveHarness,
) -> Result<Vec<ExecutionPlanningAuditEnvelopeV1>> {
    load_execution_planning_audits(&test_database_url(), harness.session.session_id).await
}

fn assert_audit_scope(
    audit: &ExecutionPlanningAuditEnvelopeV1,
    harness: &SupplementaryLiveHarness,
    originating_sequence: u64,
) -> Result<()> {
    validate_planning_audit_envelope(audit)
        .context("persisted planning audit must be strict v1")?;
    ensure!(audit.schema_version == 1, "planning audit schema drifted");
    ensure!(
        audit.tenant_id == harness.session.identity.tenant_id,
        "planning audit tenant drifted"
    );
    ensure!(audit.contact_id.is_none(), "planning audit contact drifted");
    ensure!(
        audit.session_id == Some(harness.session.session_id),
        "planning audit session drifted"
    );
    ensure!(
        audit.originating_sequence == Some(originating_sequence),
        "planning audit origin drifted"
    );
    Ok(())
}

async fn assert_ambiguous_mode_selection(
    harness: &SupplementaryLiveHarness,
    events: &[EventRecord],
) -> Result<()> {
    let audits = supplementary_planning_audits(harness).await?;
    assert_eq!(
        audits.len(),
        1,
        "ambiguous bounded investigation must emit exactly one route audit\n{}",
        describe_events(events)
    );
    let originating_sequence = audits[0]
        .originating_sequence
        .expect("route audit must retain its user-message origin");
    assert_audit_scope(&audits[0], harness, originating_sequence)
        .expect("ambiguous route audit must retain its exact scope");
    assert!(matches!(
        &audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Inline),
            reason: ExecutionRouteReason::BoundedInteractiveWork,
            ..
        }
    ));

    let responses = events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        responses.len(),
        1,
        "ambiguous mode-selection case should produce one bounded response\n{}",
        describe_events(events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionRunStarted(_)))
            .count(),
        0,
        "ambiguous difficulty must not admit an ExecutionRun\n{}",
        describe_events(events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| {
                matches!(
                    record.event,
                    Event::ExecutionCompleted(_)
                        | Event::ExecutionFailed { .. }
                        | Event::ExecutionCancelled(_)
                )
            })
            .count(),
        0,
        "Act selection must not emit execution terminal data\n{}",
        describe_events(events)
    );
    Ok(())
}

fn empty_compiler_report_hash(report_json: &str, label: &str) -> Result<String> {
    let report: ExecutionAuditReportV1 = serde_json::from_str(report_json)
        .with_context(|| format!("{label} must deserialize as a strict audit report"))?;
    let ExecutionAuditReportV1::Compiler {
        violations,
        omitted_violations,
        full_report_hash,
    } = report
    else {
        bail!("{label} must be a compiler report");
    };
    ensure!(violations.is_empty(), "{label} retained violations");
    ensure!(omitted_violations == 0, "{label} omitted violations");
    ensure!(
        full_report_hash.len() == 64,
        "{label} hash must be BLAKE3 hex"
    );
    Ok(full_report_hash)
}

fn required_schema_fields(schema: &Value) -> HashSet<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn validate_generated_candidate_quality(
    candidate: &GeneratedExecutionCandidate,
    objective: &str,
) -> Result<()> {
    ensure!(
        candidate.goal.objective == objective,
        "generated objective drifted"
    );
    ensure!(
        candidate.plan.schema_version == 1,
        "generated plan schema drifted"
    );
    ensure!(
        candidate.run_input == json!({}),
        "generated input must be empty"
    );
    ensure!(
        candidate.plan.nodes.len() == 2,
        "quality plan must contain exactly Agent then Output"
    );

    let mut agent = None;
    let mut output = None;
    for node in &candidate.plan.nodes {
        match &node.operation {
            ExecutionOperation::Agent {
                instructions,
                skill_refs,
                capability_refs,
                max_turns,
            } => {
                ensure!(agent.is_none(), "quality plan must contain one Agent node");
                ensure!(
                    !instructions.trim().is_empty(),
                    "Agent instructions must be non-empty"
                );
                ensure!(
                    skill_refs.is_empty(),
                    "Agent must not invent skill authority"
                );
                ensure!(
                    capability_refs.is_empty(),
                    "Agent must not invent capability authority"
                );
                ensure!(
                    (1..=2).contains(max_turns),
                    "Agent max_turns must be bounded"
                );
                ensure!(node.depends_on.is_empty(), "Agent must be the root node");
                agent = Some(node);
            }
            ExecutionOperation::Output { value } => {
                ensure!(
                    output.is_none(),
                    "quality plan must contain one Output node"
                );
                output = Some((node, value));
            }
            other => bail!("quality plan admitted an unexpected operation: {other:?}"),
        }
    }
    let agent = agent.context("quality plan must contain an Agent node")?;
    let (output, output_value) = output.context("quality plan must contain an Output node")?;
    ensure!(
        output.depends_on == vec![agent.id.clone()],
        "Output must depend on the Agent"
    );
    ensure!(
        output_value == &json!({"$ref": format!("$.nodes.{}.output", agent.id)}),
        "Output must forward the Agent result"
    );
    ensure!(
        agent.output_schema == candidate.plan.output_schema,
        "Agent schema must equal plan schema"
    );
    ensure!(
        output.output_schema == candidate.plan.output_schema,
        "Output schema must equal plan schema"
    );

    let required = required_schema_fields(&candidate.plan.output_schema);
    ensure!(
        required == HashSet::from(["sum", "verified"]),
        "required fields drifted"
    );
    ensure!(
        candidate.plan.output_schema.pointer("/properties/sum/type")
            == Some(&Value::String("integer".to_string())),
        "sum schema must be integer"
    );
    ensure!(
        candidate
            .plan
            .output_schema
            .pointer("/properties/verified/type")
            == Some(&Value::String("boolean".to_string())),
        "verified schema must be boolean"
    );

    let requirement_ids = candidate
        .goal
        .requirements
        .iter()
        .map(|requirement| requirement.id.as_str())
        .collect::<HashSet<_>>();
    ensure!(
        !requirement_ids.is_empty(),
        "goal requirements must be non-empty"
    );
    ensure!(
        requirement_ids.len() == candidate.goal.requirements.len(),
        "goal requirement IDs must be unique"
    );
    let served_requirement_ids = candidate
        .plan
        .nodes
        .iter()
        .flat_map(|node| node.requirement_ids.iter().map(String::as_str))
        .collect::<HashSet<_>>();
    ensure!(
        served_requirement_ids == requirement_ids,
        "plan nodes must serve every requirement exactly by ID"
    );
    ensure!(
        candidate.plan.nodes.iter().all(|node| {
            !node.requirement_ids.is_empty()
                && node
                    .requirement_ids
                    .iter()
                    .all(|requirement_id| requirement_ids.contains(requirement_id.as_str()))
        }),
        "every node must serve only declared requirements"
    );

    let output_schema_checks = candidate
        .goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::OutputSchema))
        .collect::<Vec<_>>();
    ensure!(
        output_schema_checks.len() == 1,
        "quality goal must have exactly one output-schema completion gate"
    );
    ensure!(
        output_schema_checks[0]
            .requirement_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>()
            == requirement_ids,
        "output-schema check must cover every requirement"
    );
    ensure!(
        candidate
            .goal
            .completion_checks
            .iter()
            .all(|check| { !matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }) }),
        "supplementary quality plan must not add an Agent verifier"
    );
    Ok(())
}

fn validate_generated_plan_hash_chain(
    evidence: &GeneratedPlanHashEvidence,
    initial_plan_hash: &str,
    active_plan_hash: &str,
    provenance: &ExecutionSourceProvenanceV1,
) -> Result<()> {
    ensure!(
        evidence.planner_candidate_hash == evidence.expected_planner_candidate_hash,
        "planner candidate hash must bind the canonical accepted candidate"
    );
    ensure!(
        evidence.compiler_candidate_hash == evidence.expected_compiler_candidate_hash,
        "compiler candidate hash must bind the canonical compile input"
    );
    ensure!(
        evidence.planner_report_hash == evidence.compiler_report_hash,
        "planner and compiler full-report hashes must match"
    );
    ensure!(
        evidence.final_plan_hash == initial_plan_hash,
        "compiler final plan hash must equal persisted initial plan hash"
    );
    ensure!(
        evidence.final_plan_hash == active_plan_hash,
        "compiler final plan hash must equal persisted active plan hash"
    );
    let ExecutionSourceProvenanceV1::GeneratedPlan {
        route_reason: ExecutionRouteReason::ExplicitDurableExecution,
        planner:
            GeneratedPlanPlannerProvenanceV1 {
                candidate_hash,
                compiler_report_hash,
                final_plan_hash,
                ..
            },
    } = provenance
    else {
        bail!("persisted run must retain explicit generated-plan provenance");
    };
    ensure!(
        candidate_hash == &evidence.planner_candidate_hash,
        "persisted planner candidate hash must equal the accepted planner audit"
    );
    ensure!(
        compiler_report_hash == &evidence.planner_report_hash,
        "persisted compiler report hash must equal the accepted compiler report"
    );
    ensure!(
        final_plan_hash == &evidence.final_plan_hash,
        "persisted provenance final plan hash must equal the accepted compiler plan hash"
    );
    Ok(())
}

#[test]
fn generated_plan_hash_chain_rejects_cross_surface_drift() {
    // Pins: accepted planning audits, persisted plan hashes, and generated provenance are one chain.
    let evidence = GeneratedPlanHashEvidence {
        planner_candidate_hash: "a".repeat(64),
        compiler_candidate_hash: "d".repeat(64),
        expected_planner_candidate_hash: "a".repeat(64),
        expected_compiler_candidate_hash: "d".repeat(64),
        planner_report_hash: "b".repeat(64),
        compiler_report_hash: "b".repeat(64),
        final_plan_hash: "c".repeat(64),
    };
    let provenance = ExecutionSourceProvenanceV1::GeneratedPlan {
        route_reason: ExecutionRouteReason::ExplicitDurableExecution,
        planner: GeneratedPlanPlannerProvenanceV1 {
            model: "fixture-model".to_string(),
            prompt_version: "execution-planner-v1".to_string(),
            candidate_hash: evidence.planner_candidate_hash.clone(),
            compiler_report_hash: evidence.planner_report_hash.clone(),
            final_plan_hash: evidence.final_plan_hash.clone(),
            repair_attempts: 0,
        },
    };
    validate_generated_plan_hash_chain(
        &evidence,
        &evidence.final_plan_hash,
        &evidence.final_plan_hash,
        &provenance,
    )
    .expect("matching hash chain must validate");

    let mut drifted = evidence.clone();
    drifted.compiler_candidate_hash = "e".repeat(64);
    let error = validate_generated_plan_hash_chain(
        &drifted,
        &evidence.final_plan_hash,
        &evidence.final_plan_hash,
        &provenance,
    )
    .expect_err("compiler candidate drift must be rejected");
    assert!(
        error
            .to_string()
            .contains("compiler candidate hash must bind the canonical compile input")
    );
}

async fn assert_generated_plan_audits_and_authorization(
    harness: &SupplementaryLiveHarness,
    events: &[EventRecord],
) -> Result<GeneratedPlanAuditEvidence> {
    let audits = supplementary_planning_audits(harness).await?;
    ensure!(
        audits.len() == 3,
        "accepted generated plan must emit route, planner, and compiler audits\n{}",
        describe_events(events)
    );
    let originating_sequence = audits[0]
        .originating_sequence
        .context("generated route audit must retain its user-message origin")?;
    for audit in &audits {
        assert_audit_scope(audit, harness, originating_sequence)?;
    }
    ensure!(
        matches!(
            &audits[0].payload,
            ExecutionPlanningAuditPayloadV1::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteKind::Execute,
                strategy: Some(ExecutionStrategy::Durable),
                reason: ExecutionRouteReason::ExplicitDurableExecution,
                ..
            }
        ),
        "generated-plan route audit drifted"
    );

    let (planner_candidate_json, planner_report, planner_candidate_hash, planner_duration_micros) =
        match &audits[1].payload {
            ExecutionPlanningAuditPayloadV1::PlannerCall {
                call_kind: ExecutionPlannerCallKind::InitialPlan,
                call_ordinal: 0,
                run_uid: None,
                plan_revision: None,
                outcome: ExecutionPlannerOutcome::Accepted,
                provider_model,
                prompt_version,
                candidate_hash: Some(candidate_hash),
                candidate_json: Some(candidate_json),
                compiler_report: Some(compiler_report),
                duration_micros,
                ..
            } => {
                ensure!(
                    !provider_model.trim().is_empty(),
                    "planner model must be set"
                );
                ensure!(
                    prompt_version == "execution-planner-v1",
                    "planner prompt version drifted"
                );
                (
                    candidate_json,
                    compiler_report,
                    candidate_hash,
                    *duration_micros,
                )
            }
            other => bail!("unexpected accepted planner audit: {other:#?}"),
        };
    ensure!(
        planner_duration_micros > 0,
        "planner duration must be positive"
    );
    ensure!(
        planner_candidate_hash.len() == 64,
        "planner candidate hash must be BLAKE3 hex"
    );
    let planner_report_hash =
        empty_compiler_report_hash(planner_report, "planner compiler report")?;

    let (compile_candidate_hash, compile_report, final_plan_hash, compile_duration_micros) =
        match &audits[2].payload {
            ExecutionPlanningAuditPayloadV1::Compile {
                source: ExecutionCompileSource::GeneratedPlan,
                operation_key,
                run_uid: None,
                plan_revision: None,
                outcome: ExecutionCompileOutcome::Accepted,
                candidate_hash,
                final_plan_hash: Some(final_plan_hash),
                validation_report,
                duration_micros,
                ..
            } => {
                ensure!(
                    operation_key
                        == &format!(
                            "session:{}:{}:generated:0",
                            harness.session.session_id, originating_sequence
                        ),
                    "generated compile operation key drifted"
                );
                ensure!(
                    candidate_hash.len() == 64,
                    "compiler candidate hash must be BLAKE3 hex"
                );
                (
                    candidate_hash,
                    validation_report,
                    final_plan_hash,
                    *duration_micros,
                )
            }
            other => bail!("unexpected accepted compiler audit: {other:#?}"),
        };
    ensure!(
        compile_duration_micros > 0,
        "compiler duration must be positive"
    );
    ensure!(
        final_plan_hash.len() == 64,
        "final plan hash must be BLAKE3 hex"
    );
    ensure!(
        compile_report == planner_report,
        "planner and compiler reports must be byte-identical"
    );
    let compile_report_hash =
        empty_compiler_report_hash(compile_report, "compile validation report")?;

    let candidate: GeneratedExecutionCandidate = serde_json::from_str(planner_candidate_json)
        .context("deserialize canonical generated execution candidate")?;
    validate_generated_candidate_quality(&candidate, GENERATED_PLAN_QUALITY_PROMPT)?;
    let expected_planner_candidate_hash = execution_planning_hash(
        "moa.execution.planner-candidate.v1",
        planner_candidate_json.as_bytes(),
    );
    let compile_preimage = SupplementaryInitialCompileCandidate {
        kind: "initial",
        schema_version: 1,
        source: ExecutionCompileSource::GeneratedPlan,
        goal: &candidate.goal,
        plan: &candidate.plan,
        run_input: &candidate.run_input,
    };
    let expected_compiler_candidate_hash = execution_planning_hash(
        "moa.execution.compile-candidate.v1",
        &artifact_canonical_json_bytes(&compile_preimage)
            .context("canonicalize supplementary compiler preimage")?,
    );

    let planning_context: ExecutionPlanningContextResponse = harness
        .execution_call(
            "planning_context",
            &ExecutionPlanningContextRequest {
                tenant_id: harness.session.identity.tenant_id,
                contact_id: None,
                session_id: harness.session.session_id,
                originating_user_sequence_num: originating_sequence,
                requested_template: None,
            },
        )
        .await?;
    ensure!(
        !planning_context.created,
        "post-admission planning context read must replay the frozen authority snapshot"
    );
    planning_context
        .snapshot
        .validate()
        .context("frozen planning context must validate")?;
    ensure!(
        planning_context.snapshot.authorization.capability_refs
            == planning_context
                .snapshot
                .catalog
                .capabilities
                .iter()
                .map(|capability| capability.reference.clone())
                .collect::<Vec<_>>(),
        "planning-context capability authorization drifted"
    );
    ensure!(
        candidate
            .plan
            .nodes
            .iter()
            .all(|node| match &node.operation {
                ExecutionOperation::Agent {
                    skill_refs,
                    capability_refs,
                    ..
                } =>
                    skill_refs.iter().all(|reference| {
                        planning_context
                            .snapshot
                            .authorization
                            .skill_refs
                            .contains(reference)
                    }) && capability_refs.iter().all(|reference| {
                        planning_context
                            .snapshot
                            .authorization
                            .capability_refs
                            .contains(reference)
                    }),
                ExecutionOperation::Output { .. } => true,
                _ => false,
            }),
        "generated plan references authority outside the frozen planning context"
    );

    Ok(GeneratedPlanAuditEvidence {
        candidate,
        originating_sequence,
        hashes: GeneratedPlanHashEvidence {
            planner_candidate_hash: planner_candidate_hash.clone(),
            compiler_candidate_hash: compile_candidate_hash.clone(),
            expected_planner_candidate_hash,
            expected_compiler_candidate_hash,
            planner_report_hash,
            compiler_report_hash: compile_report_hash,
            final_plan_hash: final_plan_hash.clone(),
        },
    })
}

fn validate_generated_plan_event_order(events: &[EventRecord]) -> Result<()> {
    let sequences = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::UserMessage { .. } => Some((0_u8, record.sequence_num)),
            Event::ExecutionRunStarted(_) => Some((1_u8, record.sequence_num)),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(
        sequences.iter().map(|(kind, _)| *kind).collect::<Vec<_>>() == vec![0, 1],
        "generated planning lifecycle must persist the objective before admission"
    );
    ensure!(
        sequences.windows(2).all(|pair| pair[0].1 < pair[1].1),
        "objective and admission events must be strictly ordered\n{}",
        describe_events(events)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, local Restate/Postgres, and a supported provider API key"]
async fn coordinator_ambiguous_selection_pins_execute_inline_route_provider_e2e() -> Result<()> {
    // Pins: difficult but bounded work remains Execute/Inline with its typed reason and no planning.
    let harness = SupplementaryLiveHarness::start("ambiguous-mode-selection").await?;
    let turn_id = harness
        .start_turn(AMBIGUOUS_MODE_SELECTION_PROMPT, 3)
        .await?;
    wait_for_status(
        &harness.client,
        &harness.ingress,
        &harness.session.identity,
        harness.session.session_id,
        SessionStatus::Paused,
        Duration::from_secs(120),
    )
    .await
    .with_context(|| format!("ambiguous mode-selection turn {turn_id} should complete"))?;
    let events = harness.events().await?;
    assert_ambiguous_mode_selection(&harness, &events).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, local Restate/Postgres, and a supported provider API key"]
async fn coordinator_generated_plan_is_strict_authorized_and_terminal_provider_e2e() -> Result<()> {
    // Pins: real provider planning must admit a strict authorized Agent-to-Output plan that serves
    // every immutable requirement and produces typed completed run/task/session evidence.
    let harness = SupplementaryLiveHarness::start("generated-plan-quality").await?;
    let _turn_id = harness.start_turn(GENERATED_PLAN_QUALITY_PROMPT, 4).await?;
    let (started, admission_events) = harness
        .wait_for_run_started(Duration::from_secs(180))
        .await?;
    let run_request = ExecutionRunRequest {
        tenant_id: harness.session.identity.tenant_id,
        contact_id: None,
        session_id: harness.session.session_id,
        run_uid: started.run_uid,
    };
    let verification: Result<()> = async {
        started
            .validate()
            .context("ExecutionRunStarted must satisfy the strict admission contract")?;
        let audit_evidence =
            assert_generated_plan_audits_and_authorization(&harness, &admission_events).await?;
        ensure!(
            started.originating_user_sequence_num == audit_evidence.originating_sequence,
            "admission origin must equal planning-audit origin"
        );
        ensure!(
            started.plan_revision == 1,
            "admission must start at revision one"
        );
        ensure!(
            started.status == ExecutionRunAdmissionStatus::Queued,
            "small generated plan must queue without confirmation"
        );
        ensure!(
            started.confirmation.is_none(),
            "queued admission cannot carry confirmation"
        );
        validate_generated_plan_event_order(&admission_events)?;

        let repository = ExecutionRepository::new(
            sqlx::PgPool::connect(&test_database_url())
                .await
                .context("connect supplementary generated-plan repository")?,
        );
        let persisted = repository
            .load_run(
                ExecutionScope::Tenant {
                    tenant_id: harness.session.identity.tenant_id,
                },
                started.run_uid,
            )
            .await?
            .context("admitted generated run must be persisted")?;
        ensure!(
            persisted.run_uid == started.run_uid,
            "event run UID must equal persisted run UID"
        );
        ensure!(
            persisted.originating_user_sequence_num == started.originating_user_sequence_num,
            "event origin must equal persisted run origin"
        );
        ensure!(
            persisted.plan_revision == started.plan_revision,
            "event revision must equal persisted run revision"
        );
        validate_generated_plan_hash_chain(
            &audit_evidence.hashes,
            &persisted.initial_plan_hash.to_string(),
            &persisted.active_plan_hash.to_string(),
            &persisted.source_provenance,
        )?;

        let terminal = harness
            .wait_for_terminal_status(&run_request, Duration::from_secs(180))
            .await?;
        ensure!(
            terminal.run.status == ExecutionRunStatus::Completed,
            "quality run must complete"
        );
        ensure!(
            terminal.run.plan_revision == 1,
            "quality run must remain revision one"
        );
        ensure!(
            terminal.run.total_tasks == 2,
            "quality run must materialize two tasks"
        );
        ensure!(
            terminal.run.completed_tasks == 2,
            "quality run must complete both tasks"
        );
        ensure!(
            terminal.run.failed_tasks == 0,
            "quality run must have no failed tasks"
        );
        ensure!(
            terminal.waiting.is_empty(),
            "completed quality run cannot retain waits"
        );
        ensure!(
            terminal.gaps.is_empty(),
            "completed quality run cannot retain gaps"
        );
        ensure!(
            terminal.run.terminal_evidence
                == Some(ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::Completion { limit_stop: None },
                    satisfied_requirement_count: audit_evidence.candidate.goal.requirements.len()
                        as u64,
                    requirement_count: audit_evidence.candidate.goal.requirements.len() as u64,
                }),
            "quality run terminal evidence must cover every requirement"
        );
        let output = terminal
            .output
            .as_ref()
            .context("completed quality run must retain structured terminal output")?;
        ensure!(
            output.get("sum").and_then(Value::as_i64) == Some(42),
            "sum must equal 42"
        );
        ensure!(
            output.get("verified").and_then(Value::as_bool) == Some(true),
            "verified must be true"
        );

        let tasks: ExecutionTaskListResponse = harness
            .execution_call(
                "list_tasks",
                &ExecutionTaskListRequest {
                    run: run_request.clone(),
                    limit: Some(10),
                    cursor: None,
                },
            )
            .await?;
        ensure!(
            tasks.next_cursor.is_none(),
            "two tasks must fit on one page"
        );
        ensure!(
            tasks.tasks.len() == 2,
            "quality run must expose exactly two tasks"
        );
        let mut expected_node_ids = audit_evidence
            .candidate
            .plan
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();
        expected_node_ids.sort_unstable();
        ensure!(
            tasks
                .tasks
                .iter()
                .map(|task| task.node_id.as_str())
                .collect::<Vec<_>>()
                == expected_node_ids,
            "persisted task nodes must equal the generated plan nodes"
        );
        ensure!(
            tasks.tasks.iter().all(|task| {
                task.status == ExecutionTaskStatus::Completed
                    && task.attempt == 1
                    && task.generation == 1
                    && task.outcome.is_some()
            }),
            "quality tasks must complete once with persisted outcomes"
        );

        let terminal_events = harness
            .wait_for_completed_event(started.run_uid, Duration::from_secs(60))
            .await?;
        let completed = terminal_events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionCompleted(summary) if summary.run_uid == started.run_uid => {
                    Some(summary)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        ensure!(
            completed.len() == 1,
            "quality run must publish one completion event"
        );
        let completed = completed[0];
        ensure!(
            completed.originating_user_sequence_num == audit_evidence.originating_sequence,
            "completion event must retain planning origin"
        );
        ensure!(
            completed.output.as_ref() == terminal.output.as_ref(),
            "completion event output must equal persisted terminal output"
        );
        ensure!(
            completed.failures.is_empty(),
            "completion event must have no failures"
        );
        ensure!(
            completed.gaps.is_empty(),
            "completion event must have no gaps"
        );
        ensure!(
            terminal_events
                .iter()
                .filter(|record| {
                    matches!(
                        &record.event,
                        Event::ExecutionFailed { summary, .. }
                            | Event::ExecutionCancelled(summary)
                            if summary.run_uid == started.run_uid
                    )
                })
                .count()
                == 0,
            "completed quality run cannot publish failed or cancelled terminal events"
        );
        Ok(())
    }
    .await;

    if let Err(error) = verification {
        if let Err(cleanup_error) = harness
            .cleanup_admitted_run_after_error(
                &run_request,
                "supplementary generated-plan verification failed",
            )
            .await
        {
            return Err(error.context(format!(
                "failed to clean up admitted run {} after verification error: {cleanup_error:#}",
                started.run_uid
            )));
        }
        return Err(error);
    }
    Ok(())
}
