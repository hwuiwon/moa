//! Provider E2E coverage for bounded Act behavior, cancellation recovery, and planning checks.
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
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionRequirement, GeneratedExecutionCandidate, RetryPolicy,
};
use moa_brain::execution_planning::EXECUTION_PLANNER_PROMPT_VERSION;
use moa_core::canonical_json::canonical_json_bytes;
use moa_core::traits::Identity;
use moa_core::{
    events::Event,
    types::completion::CompletionRequest,
    types::contact::SessionActorRef,
    types::context::MessageRole,
    types::events_stream::EventRange,
    types::events_stream::EventRecord,
    types::execution_planning::{
        ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope,
        ExecutionPlanningAuditPayload, ExecutionRouteKind, ExecutionRouteStage,
        ExecutionRunAdmissionStatus, ExecutionRunStarted, ExecutionSourceProvenance,
        ExecutionStrategy, GeneratedPlanPlannerProvenance, execution_planning_hash,
        validate_planning_audit_envelope,
    },
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::ToolCallId,
    types::session::{CancelScope, SessionStatus},
    types::tools::ToolContent,
    types::worker::state::{WorkerState, WorkerStatus},
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
use moa_wire::turn::{StartTurnRequest, StartTurnResponse, TurnOutcomeKind};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;

use crate::support::restate_runtime::{
    OrchestratorPorts, RESTATE_E2E_LOCK, deployment_endpoint_url, grant_session_participant,
    grant_tenant_operator, register_deployment, reserve_orchestrator_ports, restate_ingress_url,
    restate_test_admin_url, test_user_identity, with_identity,
};
use crate::support::session_store_service::{
    get_events_request, init_session_vo_request, storage_partition_id_from_meta, test_session_meta,
};
use moa_test_support::OrchestratorTestFixture;
use moa_test_support::execution_audits::load_execution_planning_audits;
use moa_test_support::fixtures::fresh_client_message_id;
use moa_test_support::postgres::test_database_url;
use moa_test_support::process::TestChildGuard;

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
    min_spawns_before_first_result: usize,
    requires_spawn_after_first_result: bool,
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
    repair_attempts: u8,
}

#[derive(Serialize)]
struct SupplementaryInitialCompileCandidate<'a> {
    kind: &'static str,
    source: ExecutionCompileSource,
    goal: &'a moa_artifacts::execution_plan::ExecutionGoalContract,
    plan: &'a moa_artifacts::execution_plan::ExecutionPlanDefinition,
    run_input: &'a Value,
}

fn supplementary_compile_candidate_hash(candidate: &GeneratedExecutionCandidate) -> Result<String> {
    let preimage = SupplementaryInitialCompileCandidate {
        kind: "initial",
        source: ExecutionCompileSource::GeneratedPlan,
        goal: &candidate.goal,
        plan: &candidate.plan,
        run_input: &candidate.run_input,
    };
    Ok(execution_planning_hash(
        "moa.execution.compile-candidate",
        &canonical_json_bytes(&preimage).context("canonicalize supplementary compiler preimage")?,
    ))
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
        .arg("--credential-port")
        .arg(ports.credential.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
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
    meta.contact = None;
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
            client_message_id: fresh_client_message_id(),
            reply_to: None,
            stream_cursor: None,
            user_message: case.prompt.to_string(),
            attachments: Vec::new(),
            model: None,
            contact: None,
            max_turns: Some(12),
            resource_budget: Default::default(),
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
    let _orchestrator = TestChildGuard::new(spawn_orchestrator(
        ports,
        &memory_dir,
        &sandbox_dir,
        &orchestrator_log,
    )?);

    async {
        wait_for_orchestrator_health(&client, ports.health, Duration::from_secs(60))
            .await
            .with_context(|| {
                format!(
                    "spawned orchestrator for case {} did not become healthy; log follows:\n{}",
                    case.name,
                    read_log(&orchestrator_log)
                )
            })?;
        register_deployment(&restate_test_admin_url(), endpoint_url.as_str()).await?;
        let session = create_initialized_session(&client, &ingress, model, case.name).await?;
        let turn_id = start_turn(&client, &ingress, &session, case).await?;
        wait_for_status(
            &client,
            &ingress,
            &session.identity,
            session.session_id,
            SessionStatus::Idle,
            Duration::from_secs(180),
        )
        .await
        .with_context(|| format!("case {} turn {turn_id} should complete", case.name))?;
        let events = session_events(&client, &ingress, &session.identity, session.session_id)
            .await
            .with_context(|| format!("fetch events for case {}", case.name))?;
        if let Err(error) = verify_case(case, &events) {
            let audits = load_execution_planning_audits(&test_database_url(), session.session_id)
                .await
                .with_context(|| format!("load route diagnostics for case {}", case.name))?;
            bail!(
                "case {} failed delegation verification: {error:#}\nplanning audits: {audits:#?}\norchestrator log:\n{}",
                case.name,
                read_log(&orchestrator_log)
            );
        }
        Ok(())
    }
    .await
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

    ensure!(
        observation.spawned.len() >= case.min_spawned,
        "case {} should spawn at least {} workers, got {}\n{}",
        case.name,
        case.min_spawned,
        observation.spawned.len(),
        describe_events(events)
    );
    ensure!(
        observation.worker_notifications.len() == observation.spawned.len(),
        "case {} should receive exactly one ordinary WorkerNotificationDelivered lifecycle report for each spawned worker; got {} notifications for {} workers\n{}",
        case.name,
        observation.worker_notifications.len(),
        observation.spawned.len(),
        describe_events(events)
    );

    let first_result_seq = observation
        .worker_notifications
        .iter()
        .map(|(seq, _)| *seq)
        .min()
        .with_context(|| format!("case {} should receive a worker result", case.name))?;
    let spawns_before_first_result = observation
        .spawn_calls
        .iter()
        .filter(|(seq, _)| *seq < first_result_seq)
        .count();
    ensure!(
        spawns_before_first_result >= case.min_spawns_before_first_result,
        "case {} should spawn at least {} independent conversational workers before the first result, got {}\n{}",
        case.name,
        case.min_spawns_before_first_result,
        spawns_before_first_result,
        describe_events(events)
    );

    if case.requires_spawn_after_first_result {
        ensure!(
            observation
                .spawn_calls
                .iter()
                .any(|(seq, _)| *seq > first_result_seq),
            "case {} should spawn a follow-up conversational worker after the earlier result\n{}",
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
        ensure!(
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
        self.worker_notifications
            .iter()
            .map(|(seq, _)| *seq)
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
        .map(ToolContent::rendered_text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact(text: &str) -> String {
    let mut value = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.len() > 320 {
        let mut end = 320;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str("...");
    }
    value
}

#[test]
fn compact_diagnostic_truncates_on_utf8_boundary() {
    // Pins: timeout diagnostics may contain provider text whose multibyte code point crosses the
    // byte cap; rendering the diagnostic must not replace the underlying failure with a panic.
    let prefix = "a".repeat(319);

    assert_eq!(compact(&format!("{prefix}é")), format!("{prefix}..."));
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
        min_spawns_before_first_result: 2,
        requires_spawn_after_first_result: false,
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
        min_spawns_before_first_result: 3,
        requires_spawn_after_first_result: false,
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
        min_spawns_before_first_result: 1,
        requires_spawn_after_first_result: false,
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
        min_spawns_before_first_result: 1,
        requires_spawn_after_first_result: true,
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
        min_spawns_before_first_result: 2,
        requires_spawn_after_first_result: true,
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
        min_spawns_before_first_result: 2,
        requires_spawn_after_first_result: false,
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
        min_spawns_before_first_result: 2,
        requires_spawn_after_first_result: false,
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
        min_spawns_before_first_result: 1,
        requires_spawn_after_first_result: true,
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
        min_spawns_before_first_result: 4,
        requires_spawn_after_first_result: false,
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

const REASONING_ONLY_SELECTION_PROMPT: &str = concat!(
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
    _orchestrator: TestChildGuard,
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
        let orchestrator = TestChildGuard::new(spawn_supplementary_orchestrator(
            ports,
            &memory_dir,
            &sandbox_dir,
            &orchestrator_log,
            model,
        )?);

        let setup = async {
            wait_for_orchestrator_health(&client, ports.health, Duration::from_secs(60))
                .await
                .with_context(|| {
                    format!(
                        "supplementary orchestrator for {name} did not become healthy; log follows:\n{}",
                        read_log(&orchestrator_log)
                    )
                })?;
            register_deployment(&restate_test_admin_url(), endpoint_url.as_str()).await?;
            create_initialized_session(&client, &ingress, model, name).await
        }
        .await;

        match setup {
            Ok(session) => Ok(Self {
                client,
                ingress,
                session,
                _orchestrator: orchestrator,
                orchestrator_log,
                _memory_dir: memory_dir,
                _sandbox_dir: sandbox_dir,
                _restate_guard: restate_guard,
            }),
            Err(error) => Err(error),
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
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: prompt.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: Some(max_turns),
                resource_budget: Default::default(),
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
            let terminal_schema_response = events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::BrainResponse { text, .. }
                        if text == "planner response failed the strict response schema"
                )
            });
            if terminal_schema_response {
                let audits = supplementary_planning_audits(self)
                    .await
                    .context("load planning audits after terminal schema rejection")?;
                if audits.iter().rev().any(|audit| {
                    matches!(
                        &audit.payload,
                        ExecutionPlanningAuditPayload::PlannerCall {
                            outcome: ExecutionPlannerOutcome::SchemaRejected,
                            ..
                        }
                    )
                }) {
                    bail!(
                        "supplementary planner response failed strict schema validation; planning audits: {audits:#?}; log follows:\n{}\n{}",
                        read_log(&self.orchestrator_log),
                        describe_events(&events)
                    );
                }
            }
            if Instant::now() >= deadline {
                let audits = supplementary_planning_audits(self)
                    .await
                    .context("load planning audits after supplementary admission timeout")?;
                bail!(
                    "supplementary run was not admitted within {timeout:?}; planning audits: {audits:#?}; log follows:\n{}\n{}",
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
        .arg("--credential-port")
        .arg(ports.credential.to_string())
        .env("MOA_DATABASE_URL", test_database_url())
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
) -> Result<Vec<ExecutionPlanningAuditEnvelope>> {
    load_execution_planning_audits(&test_database_url(), harness.session.session_id).await
}

fn assert_audit_scope(
    audit: &ExecutionPlanningAuditEnvelope,
    harness: &SupplementaryLiveHarness,
    originating_sequence: u64,
) -> Result<()> {
    validate_planning_audit_envelope(audit).context("persisted planning audit must be strict")?;
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

async fn assert_reasoning_only_selection(
    harness: &SupplementaryLiveHarness,
    events: &[EventRecord],
) -> Result<()> {
    let audits = supplementary_planning_audits(harness).await?;
    assert_eq!(
        audits.len(),
        1,
        "reasoning-only request must emit exactly one route audit\n{}",
        describe_events(events)
    );
    let originating_sequence = audits[0]
        .originating_sequence
        .expect("route audit must retain its user-message origin");
    assert_audit_scope(&audits[0], harness, originating_sequence)
        .expect("reasoning-only route audit must retain its exact scope");
    ensure!(
        matches!(
            &audits[0].payload,
            ExecutionPlanningAuditPayload::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteKind::Respond,
                strategy: None,
                ..
            }
        ),
        "reasoning-only route audit drifted: {:#?}",
        audits[0].payload
    );

    let responses = events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        responses.len(),
        1,
        "reasoning-only case should produce one bounded response\n{}",
        describe_events(events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionRunStarted(_)))
            .count(),
        0,
        "reasoning-only work must not admit an ExecutionRun\n{}",
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
    compiler_report_hash(report_json, label, true)
}

fn rejected_compiler_report_hash(report_json: &str, label: &str) -> Result<String> {
    compiler_report_hash(report_json, label, false)
}

fn compiler_report_hash(report_json: &str, label: &str, expect_empty: bool) -> Result<String> {
    let report: ExecutionAuditReport = serde_json::from_str(report_json)
        .with_context(|| format!("{label} must deserialize as a strict audit report"))?;
    let ExecutionAuditReport::Compiler {
        violations,
        omitted_violations,
        full_report_hash,
    } = report
    else {
        bail!("{label} must be a compiler report");
    };
    if expect_empty {
        ensure!(violations.is_empty(), "{label} retained violations");
        ensure!(omitted_violations == 0, "{label} omitted violations");
    } else {
        ensure!(
            !violations.is_empty() || omitted_violations > 0,
            "{label} must retain compiler rejection evidence"
        );
    }
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
    provenance: &ExecutionSourceProvenance,
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
    let ExecutionSourceProvenance::GeneratedPlan {
        planner:
            GeneratedPlanPlannerProvenance {
                candidate_hash,
                compiler_report_hash,
                final_plan_hash,
                repair_attempts,
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
    ensure!(
        repair_attempts == &evidence.repair_attempts,
        "persisted provenance repair count must equal the accepted planner path"
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
        repair_attempts: 0,
    };
    let provenance = ExecutionSourceProvenance::GeneratedPlan {
        planner: GeneratedPlanPlannerProvenance {
            model: "fixture-model".to_string(),
            prompt_version: "execution-planner".to_string(),
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
        matches!(audits.len(), 3..=5),
        "accepted generated plan must emit one exact direct, schema-repair, or compiler-repair audit path\n{}",
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
            ExecutionPlanningAuditPayload::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteKind::Execute,
                strategy: Some(ExecutionStrategy::Durable),
                ..
            }
        ),
        "generated-plan route audit drifted"
    );

    let (planner_index, compile_index, repair_attempts, immutable_repair_goal) = match audits.len()
    {
        3 => (1, 2, 0, None),
        4 => {
            match &audits[1].payload {
                ExecutionPlanningAuditPayload::PlannerCall {
                    call_kind: ExecutionPlannerCallKind::InitialPlan,
                    call_ordinal: 0,
                    run_uid: None,
                    plan_revision: None,
                    outcome: ExecutionPlannerOutcome::SchemaRejected,
                    provider_model,
                    prompt_version,
                    candidate_hash: Some(raw_hash),
                    candidate_json: None,
                    compiler_report: Some(report),
                    duration_micros,
                    ..
                } => {
                    ensure!(
                        !provider_model.trim().is_empty(),
                        "planner model must be set"
                    );
                    ensure!(
                        prompt_version == EXECUTION_PLANNER_PROMPT_VERSION,
                        "planner prompt version drifted"
                    );
                    ensure!(
                        raw_hash.len() == 64,
                        "rejected raw response hash must be BLAKE3 hex"
                    );
                    ensure!(
                        !report.is_empty(),
                        "schema rejection must retain a bounded report"
                    );
                    ensure!(
                        *duration_micros > 0,
                        "rejected planner duration must be positive"
                    );
                }
                other => bail!("unexpected initial schema-rejection audit: {other:#?}"),
            }
            (2, 3, 1, None)
        }
        5 => {
            let (initial_candidate_json, initial_candidate_hash, initial_report) = match &audits[1]
                .payload
            {
                ExecutionPlanningAuditPayload::PlannerCall {
                    call_kind: ExecutionPlannerCallKind::InitialPlan,
                    call_ordinal: 0,
                    run_uid: None,
                    plan_revision: None,
                    outcome: ExecutionPlannerOutcome::CompilerRejected,
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
                        "initial rejected planner model must be set"
                    );
                    ensure!(
                        prompt_version == EXECUTION_PLANNER_PROMPT_VERSION,
                        "initial rejected planner prompt version drifted"
                    );
                    ensure!(
                        *duration_micros > 0,
                        "initial rejected planner duration must be positive"
                    );
                    (candidate_json, candidate_hash, compiler_report)
                }
                other => bail!("unexpected initial compiler-rejected planner audit: {other:#?}"),
            };
            let initial_candidate: GeneratedExecutionCandidate =
                serde_json::from_str(initial_candidate_json)
                    .context("deserialize initial compiler-rejected candidate")?;
            ensure!(
                initial_candidate_hash
                    == &execution_planning_hash(
                        "moa.execution.planner-candidate",
                        initial_candidate_json.as_bytes(),
                    ),
                "initial rejected planner candidate hash drifted"
            );
            let initial_compile_hash = supplementary_compile_candidate_hash(&initial_candidate)?;
            let initial_compile_report = match &audits[2].payload {
                ExecutionPlanningAuditPayload::Compile {
                    source: ExecutionCompileSource::GeneratedPlan,
                    operation_key,
                    run_uid: None,
                    plan_revision: None,
                    outcome: ExecutionCompileOutcome::Rejected,
                    candidate_hash,
                    final_plan_hash: None,
                    validation_report,
                    duration_micros,
                    ..
                } => {
                    ensure!(
                        operation_key
                            == &format!(
                                "session:{}:{}:generated:0",
                                harness.session.session_id, originating_sequence,
                            ),
                        "initial rejected compile operation key drifted"
                    );
                    ensure!(
                        candidate_hash == &initial_compile_hash,
                        "initial rejected compiler candidate hash drifted"
                    );
                    ensure!(
                        *duration_micros > 0,
                        "initial rejected compiler duration must be positive"
                    );
                    validation_report
                }
                other => bail!("unexpected initial rejected compiler audit: {other:#?}"),
            };
            ensure!(
                initial_compile_report == initial_report,
                "initial rejected planner and compiler reports must be byte-identical"
            );
            ensure!(
                rejected_compiler_report_hash(
                    initial_report,
                    "initial rejected planner compiler report",
                )? == rejected_compiler_report_hash(
                    initial_compile_report,
                    "initial rejected compile validation report",
                )?,
                "initial rejected planner and compiler report hashes must match"
            );
            (3, 4, 1, Some(initial_candidate.goal))
        }
        _ => unreachable!("audit count was validated above"),
    };

    let (planner_candidate_json, planner_report, planner_candidate_hash, planner_duration_micros) =
        match &audits[planner_index].payload {
            ExecutionPlanningAuditPayload::PlannerCall {
                call_kind,
                call_ordinal,
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
                    (*call_kind, *call_ordinal)
                        == if repair_attempts == 0 {
                            (ExecutionPlannerCallKind::InitialPlan, 0)
                        } else {
                            (ExecutionPlannerCallKind::InitialRepair, 1)
                        },
                    "accepted planner call identity drifted"
                );
                ensure!(
                    !provider_model.trim().is_empty(),
                    "planner model must be set"
                );
                ensure!(
                    prompt_version == EXECUTION_PLANNER_PROMPT_VERSION,
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
        match &audits[compile_index].payload {
            ExecutionPlanningAuditPayload::Compile {
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
                            "session:{}:{}:generated:{repair_attempts}",
                            harness.session.session_id, originating_sequence,
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
    if let Some(immutable_goal) = immutable_repair_goal {
        ensure!(
            candidate.goal == immutable_goal,
            "compiler repair must preserve the complete immutable goal contract"
        );
    }
    validate_generated_candidate_quality(&candidate, GENERATED_PLAN_QUALITY_PROMPT)?;
    let expected_planner_candidate_hash = execution_planning_hash(
        "moa.execution.planner-candidate",
        planner_candidate_json.as_bytes(),
    );
    let expected_compiler_candidate_hash = supplementary_compile_candidate_hash(&candidate)?;

    let planning_context: ExecutionPlanningContextResponse = harness
        .execution_call(
            "planning_context",
            &ExecutionPlanningContextRequest {
                tenant_id: harness.session.identity.tenant_id,
                contact_id: None,
                session_id: harness.session.session_id,
                originating_user_sequence_num: originating_sequence,
                deadline_at: chrono::Utc::now() + chrono::TimeDelta::days(1),
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
            repair_attempts,
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

const RECOVERY_MATRIX_CLASSIFIER_PROMPT: &str =
    "You classify one user turn into MOA's public execution decision.";
const RECOVERY_MATRIX_BLOCKED_ROOT: &str =
    "RECOVERY-MATRIX-BLOCKED-ROOT: answer only after the cancellation probe.";
const RECOVERY_MATRIX_LATE_ROOT_RESULT: &str = "RECOVERY-MATRIX-LATE-ROOT-RESULT";
const RECOVERY_MATRIX_REPLACEMENT_ROOT: &str =
    "RECOVERY-MATRIX-REPLACEMENT-ROOT: prove the admission fence was released.";
const RECOVERY_MATRIX_REPLACEMENT_RESULT: &str = "RECOVERY-MATRIX-REPLACEMENT-COMPLETED";
const RECOVERY_MATRIX_CANCELLED_INPUT_TOKENS: usize = 777_777;
const DUPLICATE_SPAWN_ROOT: &str =
    "DUPLICATE-SPAWN-ROOT: exercise recoverable duplicate worker rejection.";
const DUPLICATE_SPAWN_TASK: &str =
    "DUPLICATE-SPAWN-WORKER: remain active while the parent repeats this task.";
const DUPLICATE_SPAWN_RECOVERED: &str = "DUPLICATE-SPAWN-RECOVERED";

fn duplicate_spawn_recovery_script() -> Value {
    json!({
        "default": {
            "completion": {
                "content": "unexpected duplicate-spawn fallback",
                "tool_calls": []
            }
        },
        "keyed": [
            {
                "match": RECOVERY_MATRIX_CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"inline","rationale":"The request is a bounded delegation probe.","confidence_bps":10000,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": "duplicate worker task detected",
                "completion": {
                    "content": DUPLICATE_SPAWN_RECOVERED,
                    "tool_calls": []
                }
            },
            {
                "match": "Spawned worker",
                "completion": {
                    "content": "Repeating the same child request to exercise loop prevention.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "id": "duplicate-spawn-second",
                        "input": {
                            "task": DUPLICATE_SPAWN_TASK,
                            "tool_subset": [],
                            "budget_tokens": 1200,
                            "max_turns": 1
                        }
                    }]
                }
            },
            {
                "match": DUPLICATE_SPAWN_ROOT,
                "completion": {
                    "content": "Spawning the first child.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "id": "duplicate-spawn-first",
                        "input": {
                            "task": DUPLICATE_SPAWN_TASK,
                            "tool_subset": [],
                            "budget_tokens": 1200,
                            "max_turns": 1
                        }
                    }]
                }
            },
            {
                "match": DUPLICATE_SPAWN_TASK,
                "completion": {
                    "content": "worker should remain active during parent validation",
                    "tool_calls": [],
                    "latency_ms": 120000
                }
            }
        ]
    })
}

fn recovery_matrix_blocked_root_script() -> Value {
    json!({
        "default": {
            "completion": {
                "content": "unexpected recovery-matrix fallback",
                "tool_calls": []
            }
        },
        "keyed": [
            {
                "match": RECOVERY_MATRIX_CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"inline","rationale":"The request needs one bounded model turn.","confidence_bps":10000,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_REPLACEMENT_ROOT,
                "completion": {
                    "content": RECOVERY_MATRIX_REPLACEMENT_RESULT,
                    "tool_calls": [],
                    "input_tokens": 73
                }
            },
            {
                "match": RECOVERY_MATRIX_BLOCKED_ROOT,
                "completion": {
                    "content": RECOVERY_MATRIX_LATE_ROOT_RESULT,
                    "tool_calls": [],
                    "latency_ms": 120000,
                    "input_tokens": RECOVERY_MATRIX_CANCELLED_INPUT_TOKENS
                }
            }
        ]
    })
}

fn recovery_matrix_turn_request(message: &str) -> StartTurnRequest {
    StartTurnRequest {
        client_message_id: fresh_client_message_id(),
        reply_to: None,
        stream_cursor: None,
        user_message: message.to_string(),
        attachments: Vec::new(),
        model: None,
        contact: None,
        max_turns: Some(2),
        resource_budget: Default::default(),
        execution_template: None,
    }
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Redis scripted-provider fixture"]
async fn duplicate_worker_rejection_persists_tool_result_and_turn_continues_service_e2e()
-> Result<()> {
    // Pins: a repeated active-child request is a recoverable model-authored tool error. The
    // provider history must contain the failed ToolResult paired with the duplicate ToolCall,
    // and the coordinator must be able to produce a final response without a TurnFailed event.
    let fixture = OrchestratorTestFixture::with_script(duplicate_spawn_recovery_script())
        .await
        .context("boot duplicate-spawn scripted orchestrator fixture")?;
    fixture.reset_scripted_requests()?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("duplicate-spawn-recovery").await?;
    let session = test.client().session(session_id.to_string());
    let mut request = recovery_matrix_turn_request(DUPLICATE_SPAWN_ROOT);
    request.max_turns = Some(4);

    let started = session.start_turn(request, None).await?;
    let turn_id = started.turn_id.context("duplicate-spawn turn omitted id")?;
    let outcome = session
        .await_turn_outcome(&turn_id, Duration::from_secs(60), Duration::from_millis(25))
        .await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, DUPLICATE_SPAWN_RECOVERED);

    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    let spawn_tool_ids = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                tool_id, tool_name, ..
            } if tool_name == "spawn_worker" => Some(*tool_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(spawn_tool_ids.len(), 2, "{}", describe_events(&events));

    let spawn_results = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolResult {
                tool_id,
                success,
                output,
                ..
            } if spawn_tool_ids.contains(tool_id) => {
                Some((*tool_id, *success, tool_output_text(output)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(spawn_results.len(), 2, "{}", describe_events(&events));
    assert_eq!(
        spawn_results
            .iter()
            .filter(|(_, success, _)| *success)
            .count(),
        1
    );
    assert_eq!(
        spawn_results
            .iter()
            .filter(|(_, success, _)| !*success)
            .filter(|(_, _, text)| text.contains("duplicate worker task"))
            .count(),
        1,
        "{}",
        describe_events(&events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::WorkerSpawned { .. }))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|record| !matches!(record.event, Event::TurnFailed { .. })),
        "{}",
        describe_events(&events)
    );
    Ok(())
}

fn recovery_matrix_request_contains(request: &CompletionRequest, marker: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| message.content.contains(marker))
}

fn recovery_matrix_current_user_turn_is(request: &CompletionRequest, expected: &str) -> bool {
    request
        .metadata
        .get("_moa.user_turn")
        .and_then(Value::as_str)
        == Some(expected)
}

fn recovery_matrix_latest_user_message_contains(request: &CompletionRequest, marker: &str) -> bool {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .is_some_and(|message| message.content.contains(marker))
}

async fn recovery_matrix_wait_for_request(
    fixture: &OrchestratorTestFixture,
    marker: &str,
    exclude_marker: &str,
    timeout: Duration,
) -> Result<(CompletionRequest, Vec<Value>)> {
    let deadline = Instant::now() + timeout;
    let mut next_count = 1;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "scripted provider did not record request marker {marker:?} within {timeout:?}"
        );
        let rows = fixture
            .wait_for_scripted_requests(next_count, remaining)
            .await?;
        let requests = rows
            .iter()
            .cloned()
            .map(serde_json::from_value::<CompletionRequest>)
            .collect::<serde_json::Result<Vec<_>>>()
            .context("decode recovery-matrix scripted-provider journal")?;
        if let Some(request) = requests.iter().find(|request| {
            recovery_matrix_request_contains(request, marker)
                && !recovery_matrix_request_contains(request, exclude_marker)
        }) {
            return Ok((request.clone(), rows));
        }
        next_count = rows.len() + 1;
    }
}

async fn recovery_matrix_restate_rows(
    fixture: &OrchestratorTestFixture,
    query: impl AsRef<str>,
) -> Result<Vec<Value>> {
    let client = reqwest::Client::new();
    let url = format!("{}/query", fixture.admin_url.trim_end_matches('/'));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let last_error = match client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({ "query": query.as_ref() }))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) if status.is_success() => match serde_json::from_str::<Value>(&body) {
                        Ok(response) => {
                            if let Some(rows) = response.get("rows").and_then(Value::as_array) {
                                return Ok(rows.clone());
                            }
                            format!("response omitted rows: {response}")
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
            tokio::time::Instant::now() < deadline,
            "Restate recovery-matrix introspection did not become ready: {last_error}"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn recovery_matrix_blocked_llm_invocation(
    fixture: &OrchestratorTestFixture,
    workflow_service: &str,
    workflow_key: Option<&str>,
) -> Result<(String, String)> {
    let key_filter = workflow_key
        .map(|key| format!(" AND target_service_key = '{key}'"))
        .unwrap_or_default();
    let parents = recovery_matrix_restate_rows(
        fixture,
        format!(
            "SELECT id FROM sys_invocation WHERE target_service_name = '{workflow_service}'\
             {key_filter}"
        ),
    )
    .await?;
    ensure!(
        !parents.is_empty(),
        "expected a {workflow_service} invocation, got {parents:?}"
    );
    for parent in &parents {
        let parent_id = parent
            .get("id")
            .and_then(Value::as_str)
            .context("workflow introspection row omitted id")?;
        let children = recovery_matrix_restate_rows(
            fixture,
            format!(
                "SELECT id, status FROM sys_invocation WHERE invoked_by_id = '{parent_id}' \
                 AND target_service_name = 'LLMGateway' AND status != 'completed' ORDER BY id"
            ),
        )
        .await?;
        for child in &children {
            let Some(invoked_id) = child.get("id").and_then(Value::as_str) else {
                continue;
            };
            return Ok((parent_id.to_string(), invoked_id.to_string()));
        }
    }
    bail!("{workflow_service} has no incomplete LLMGateway child: {parents:?}")
}

async fn recovery_matrix_execution_task_attempt_key(
    fixture: &OrchestratorTestFixture,
    run_uid: uuid::Uuid,
) -> Result<String> {
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect recovery-matrix execution database")?;
    let dispatch_uids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT dispatch_uid FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'task_attempt' \
         ORDER BY created_at, dispatch_uid",
    )
    .bind(run_uid)
    .fetch_all(&pool)
    .await
    .context("load recovery-matrix task-attempt dispatch identity")?;
    let [dispatch_uid] = dispatch_uids.as_slice() else {
        bail!(
            "expected exactly one task-attempt dispatch for run {run_uid}, got {dispatch_uids:?}"
        );
    };
    Ok(dispatch_uid.to_string())
}

async fn recovery_matrix_assert_child_joined(
    fixture: &OrchestratorTestFixture,
    parent_id: &str,
    child_id: &str,
) -> Result<()> {
    let rows = recovery_matrix_restate_rows(
        fixture,
        format!("SELECT id, invoked_by_id, status FROM sys_invocation WHERE id = '{child_id}'"),
    )
    .await?;
    ensure!(
        rows.len() == 1
            && rows[0].get("invoked_by_id").and_then(Value::as_str) == Some(parent_id)
            && rows[0].get("status").and_then(Value::as_str) == Some("completed"),
        "cancelled LLM child {child_id} was not joined in parent {parent_id}: {rows:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Redis scripted-provider fixture"]
async fn recovery_matrix_root_llm_cancel_crash_restart_releases_replacement_service_e2e()
-> Result<()> {
    // Pins: cancellation wins while the root's exact LLM child is blocked, joins that child
    // across a hard orchestrator crash, publishes one normal cancelled outcome, and releases
    // the session admission fence so one replacement turn can run without old result or usage.
    let fixture = OrchestratorTestFixture::with_script(recovery_matrix_blocked_root_script())
        .await
        .context("boot recovery-matrix scripted orchestrator fixture")?;
    fixture.reset_scripted_requests()?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("recovery-matrix-root-cancellation")
        .await?;
    let session = test.client().session(session_id.to_string());

    let first = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_BLOCKED_ROOT),
            None,
        )
        .await?;
    ensure!(!first.queued, "fresh blocked turn must start immediately");
    let first_turn_id = first.turn_id.context("blocked turn omitted turn id")?;
    let (_, requests_at_barrier) = recovery_matrix_wait_for_request(
        &fixture,
        RECOVERY_MATRIX_BLOCKED_ROOT,
        RECOVERY_MATRIX_CLASSIFIER_PROMPT,
        Duration::from_secs(30),
    )
    .await?;
    let (parent_invocation_id, child_invocation_id) =
        recovery_matrix_blocked_llm_invocation(&fixture, "TurnExecution", Some(&first_turn_id))
            .await?;
    let old_main_requests_at_barrier = requests_at_barrier
        .iter()
        .cloned()
        .map(serde_json::from_value::<CompletionRequest>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .iter()
        .filter(|request| {
            recovery_matrix_current_user_turn_is(request, RECOVERY_MATRIX_BLOCKED_ROOT)
                && !recovery_matrix_request_contains(request, RECOVERY_MATRIX_CLASSIFIER_PROMPT)
        })
        .count();
    ensure!(
        old_main_requests_at_barrier == 1,
        "blocked root must cross the provider boundary exactly once before cancellation"
    );

    test.client()
        .post_void(
            &format!("/Session/{session_id}/cancel"),
            &CancelScope::CoordinatorOnly,
        )
        .await?;
    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard-crash and restart recovery-matrix orchestrator")?;

    let cancelled = session
        .await_turn_outcome(
            &first_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    assert_eq!(cancelled.kind, TurnOutcomeKind::Cancelled);
    recovery_matrix_assert_child_joined(&fixture, &parent_invocation_id, &child_invocation_id)
        .await?;

    let replacement = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_REPLACEMENT_ROOT),
            None,
        )
        .await?;
    assert!(
        !replacement.queued,
        "the exact cancelled outcome must release the admission fence"
    );
    let replacement_turn_id = replacement
        .turn_id
        .context("replacement turn omitted turn id")?;
    let completed = session
        .await_turn_outcome(
            &replacement_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    assert_eq!(completed.kind, TurnOutcomeKind::Completed);
    assert_eq!(completed.message, RECOVERY_MATRIX_REPLACEMENT_RESULT);

    let requests = fixture.scripted_requests()?;
    let decoded = requests
        .into_iter()
        .map(serde_json::from_value::<CompletionRequest>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let old_main_requests = decoded
        .iter()
        .filter(|request| {
            recovery_matrix_current_user_turn_is(request, RECOVERY_MATRIX_BLOCKED_ROOT)
                && !recovery_matrix_request_contains(request, RECOVERY_MATRIX_CLASSIFIER_PROMPT)
        })
        .count();
    let replacement_main_requests = decoded
        .iter()
        .filter(|request| {
            recovery_matrix_current_user_turn_is(request, RECOVERY_MATRIX_REPLACEMENT_ROOT)
                && !recovery_matrix_request_contains(request, RECOVERY_MATRIX_CLASSIFIER_PROMPT)
        })
        .count();
    assert_eq!(old_main_requests, 1, "cancelled root LLM must not replay");
    assert_eq!(
        replacement_main_requests, 1,
        "replacement root LLM must execute exactly once"
    );

    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    let old_results = events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == RECOVERY_MATRIX_LATE_ROOT_RESULT
            )
        })
        .count();
    let replacement_results = events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::BrainResponse { text, .. } if text == RECOVERY_MATRIX_REPLACEMENT_RESULT
            )
        })
        .count();
    let cancelled_usage_events = events
        .iter()
        .filter(|record| record.event.input_tokens() == RECOVERY_MATRIX_CANCELLED_INPUT_TOKENS)
        .count();
    assert_eq!(
        old_results, 0,
        "cancelled root produced a late visible result"
    );
    assert_eq!(
        cancelled_usage_events, 0,
        "cancelled root produced late token usage or budget evidence"
    );
    assert_eq!(
        replacement_results, 1,
        "replacement turn must produce exactly one user-visible result"
    );
    Ok(())
}

const RECOVERY_MATRIX_WORKER_ROOT: &str =
    "RECOVERY-MATRIX-WORKER-ROOT: delegate the blocked worker probe.";
const RECOVERY_MATRIX_BLOCKED_WORKER: &str =
    "RECOVERY-MATRIX-BLOCKED-WORKER: wait for cancellation before answering.";
const RECOVERY_MATRIX_LATE_WORKER_RESULT: &str = "RECOVERY-MATRIX-LATE-WORKER-RESULT";
const RECOVERY_MATRIX_WORKER_ROOT_DONE: &str = "RECOVERY-MATRIX-WORKER-DISPATCHED";
const RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER: &str = "RECOVERY-MATRIX-REPLACEMENT-AFTER-WORKER";
const RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER_RESULT: &str =
    "RECOVERY-MATRIX-REPLACEMENT-AFTER-WORKER-COMPLETED";
const RECOVERY_MATRIX_CANCELLED_WORKER_INPUT_TOKENS: usize = 666_666;

fn recovery_matrix_blocked_worker_script() -> Value {
    json!({
        "default": {
            "completion": {
                "content": "unexpected worker recovery-matrix fallback",
                "tool_calls": []
            }
        },
        "keyed": [
            {
                "match": RECOVERY_MATRIX_CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"inline","rationale":"The request delegates one bounded child.","confidence_bps":10000,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER,
                "completion": {
                    "content": RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER_RESULT,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_BLOCKED_WORKER,
                "completion": {
                    "content": RECOVERY_MATRIX_LATE_WORKER_RESULT,
                    "tool_calls": [],
                    "latency_ms": 120000,
                    "input_tokens": RECOVERY_MATRIX_CANCELLED_WORKER_INPUT_TOKENS
                }
            },
            {
                "match": "Spawned worker",
                "completion": {
                    "content": RECOVERY_MATRIX_WORKER_ROOT_DONE,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_WORKER_ROOT,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "id": "recovery-matrix-spawn-blocked-worker",
                        "input": {
                            "task": RECOVERY_MATRIX_BLOCKED_WORKER,
                            "tool_subset": [],
                            "budget_tokens": 1200,
                            "max_turns": 1
                        }
                    }]
                }
            }
        ]
    })
}

async fn recovery_matrix_wait_for_events(
    client: &moa_test_support::TestApiClient,
    session_id: SessionId,
    timeout: Duration,
    predicate: impl Fn(&[EventRecord]) -> bool,
) -> Result<Vec<EventRecord>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        let events = client.get_events(session_id, EventRange::all()).await?;
        if predicate(&events) {
            return Ok(events);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "session {session_id} did not reach the expected recovery-matrix event boundary"
        );
    }
}

async fn recovery_matrix_wait_for_worker_state(
    client: &moa_test_support::TestApiClient,
    worker_id: &str,
    expected: WorkerState,
    timeout: Duration,
) -> Result<WorkerStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        let status = client
            .post_empty_call::<WorkerStatus>(&format!("/Worker/{worker_id}/status"))
            .await?;
        if status.state == expected {
            return Ok(status);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "worker {worker_id} did not reach {expected:?}; last status: {status:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Redis scripted-provider fixture"]
async fn recovery_matrix_worker_llm_cancel_crash_restart_fences_late_delivery_service_e2e()
-> Result<()> {
    // Pins: TaskTree cancellation joins a blocked WorkerTurnExecution LLM child across a hard
    // process crash, records one cancelled worker terminal, drops its result and usage, and
    // leaves the owning session able to execute one fresh replacement turn.
    let fixture = OrchestratorTestFixture::with_script(recovery_matrix_blocked_worker_script())
        .await
        .context("boot blocked-worker recovery-matrix fixture")?;
    fixture.reset_scripted_requests()?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("recovery-matrix-worker-cancel").await?;
    let session = test.client().session(session_id.to_string());
    let started = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_WORKER_ROOT),
            None,
        )
        .await?;
    ensure!(!started.queued, "worker probe root must start immediately");
    let root_turn_id = started
        .turn_id
        .context("worker probe root omitted turn id")?;

    recovery_matrix_wait_for_request(
        &fixture,
        RECOVERY_MATRIX_BLOCKED_WORKER,
        RECOVERY_MATRIX_CLASSIFIER_PROMPT,
        Duration::from_secs(30),
    )
    .await?;
    let spawned_events = recovery_matrix_wait_for_events(
        test.client(),
        session_id,
        Duration::from_secs(30),
        |events| {
            events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::WorkerSpawned { task, .. } if task == RECOVERY_MATRIX_BLOCKED_WORKER
                )
            })
        },
    )
    .await?;
    let worker_id = spawned_events
        .iter()
        .find_map(|record| match &record.event {
            Event::WorkerSpawned {
                worker_id, task, ..
            } if task == RECOVERY_MATRIX_BLOCKED_WORKER => Some(worker_id.clone()),
            _ => None,
        })
        .context("blocked worker spawn event omitted worker id")?;
    let (parent_invocation_id, child_invocation_id) =
        recovery_matrix_blocked_llm_invocation(&fixture, "WorkerTurnExecution", None).await?;

    test.client()
        .post_void(
            &format!("/Session/{session_id}/cancel"),
            &CancelScope::TaskTree,
        )
        .await?;
    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard-crash and restart blocked-worker fixture")?;

    let worker = recovery_matrix_wait_for_worker_state(
        test.client(),
        &worker_id,
        WorkerState::Cancelled,
        Duration::from_secs(60),
    )
    .await?;
    assert_eq!(worker.tokens_used, 0, "cancelled worker consumed tokens");
    recovery_matrix_assert_child_joined(&fixture, &parent_invocation_id, &child_invocation_id)
        .await?;
    let terminal_events = recovery_matrix_wait_for_events(
        test.client(),
        session_id,
        Duration::from_secs(60),
        |events| {
            events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::WorkerNotificationDelivered {
                        worker_id: delivered,
                        state: WorkerState::Cancelled,
                        ..
                    } if delivered == &worker_id
                )
            })
        },
    )
    .await?;
    assert_eq!(
        terminal_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::WorkerStatusChanged {
                        worker_id: changed,
                        to: WorkerState::Cancelled,
                        ..
                    } if changed == &worker_id
                )
            })
            .count(),
        1,
        "blocked worker must publish exactly one cancelled status"
    );
    assert_eq!(
        terminal_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::WorkerNotificationDelivered {
                        worker_id: delivered,
                        state: WorkerState::Cancelled,
                        ..
                    } if delivered == &worker_id
                )
            })
            .count(),
        1,
        "blocked worker must deliver exactly one cancelled terminal"
    );

    let root_outcome = session
        .await_turn_outcome(
            &root_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    assert!(
        matches!(
            root_outcome.kind,
            TurnOutcomeKind::Completed | TurnOutcomeKind::Cancelled
        ),
        "task-tree cancellation must settle the owning coordinator before admitting replacement work: {root_outcome:?}"
    );

    let settled_snapshot = session.snapshot().await?;
    ensure!(
        settled_snapshot.active_turn_id.is_none(),
        "worker cancellation left an active turn after the root outcome: {settled_snapshot:?}"
    );

    let replacement = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER),
            None,
        )
        .await?;
    assert!(
        !replacement.queued,
        "worker cancellation left a session fence"
    );
    let replacement_turn_id = replacement.turn_id.context("replacement omitted turn id")?;
    let replacement_outcome = session
        .await_turn_outcome(
            &replacement_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    assert_eq!(replacement_outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(
        replacement_outcome.message,
        RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER_RESULT
    );

    let requests = fixture
        .scripted_requests()?
        .into_iter()
        .map(serde_json::from_value::<CompletionRequest>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                recovery_matrix_latest_user_message_contains(
                    request,
                    RECOVERY_MATRIX_BLOCKED_WORKER,
                ) && !recovery_matrix_request_contains(request, RECOVERY_MATRIX_CLASSIFIER_PROMPT)
            })
            .count(),
        1,
        "cancelled worker LLM must not replay"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                recovery_matrix_latest_user_message_contains(
                    request,
                    RECOVERY_MATRIX_REPLACEMENT_AFTER_WORKER,
                ) && !recovery_matrix_request_contains(request, RECOVERY_MATRIX_CLASSIFIER_PROMPT)
            })
            .count(),
        1,
        "replacement after worker cancellation must execute once"
    );
    assert!(terminal_events.iter().all(|record| {
        !matches!(
            &record.event,
            Event::BrainResponse { text, .. } if text == RECOVERY_MATRIX_LATE_WORKER_RESULT
        ) && record.event.input_tokens() != RECOVERY_MATRIX_CANCELLED_WORKER_INPUT_TOKENS
    }));
    Ok(())
}

const RECOVERY_MATRIX_EXECUTION_ROOT: &str =
    "RECOVERY-MATRIX-EXECUTION-ROOT: run the cancellable durable agent.";
const RECOVERY_MATRIX_EXECUTION_TASK: &str =
    "RECOVERY-MATRIX-BLOCKED-EXECUTION-TASK: wait for cancellation.";
const RECOVERY_MATRIX_LATE_EXECUTION_RESULT: &str = "RECOVERY-MATRIX-LATE-EXECUTION-RESULT";
const RECOVERY_MATRIX_REPLACEMENT_EXECUTION_ROOT: &str =
    "RECOVERY-MATRIX-REPLACEMENT-EXECUTION-ROOT: run the replacement durable agent.";
const RECOVERY_MATRIX_REPLACEMENT_EXECUTION_TASK: &str =
    "RECOVERY-MATRIX-REPLACEMENT-EXECUTION-TASK: return the replacement result.";
const RECOVERY_MATRIX_REPLACEMENT_EXECUTION_RESULT: &str =
    "RECOVERY-MATRIX-REPLACEMENT-EXECUTION-COMPLETED";
const RECOVERY_MATRIX_CANCELLED_EXECUTION_INPUT_TOKENS: usize = 555_555;

fn recovery_matrix_execution_candidate(
    objective: &str,
    instructions: &str,
) -> GeneratedExecutionCandidate {
    let output_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer"],
        "properties": {"answer": {"type": "string"}}
    });
    let retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    };
    GeneratedExecutionCandidate {
        goal: ExecutionGoalContract {
            objective: objective.to_string(),
            requirements: vec![ExecutionRequirement {
                id: "answer".to_string(),
                description: "produce the deterministic recovery-matrix answer".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "terminal output satisfies its declared schema".to_string(),
                requirement_ids: vec!["answer".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::After {
                    delay_seconds: 86_400,
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailRun,
            },
            input_schema: json!({"type": "object", "additionalProperties": false}),
            output_schema: output_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: "agent".to_string(),
                    requirement_ids: vec!["answer".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: output_schema.clone(),
                    operation: ExecutionOperation::Agent {
                        instructions: instructions.to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    compensation: None,
                    retry: retry.clone(),
                    budget: None,
                },
                ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["answer".to_string()],
                    depends_on: vec!["agent".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"$ref": "$.nodes.agent.output"}),
                    },
                    compensation: None,
                    retry,
                    budget: None,
                },
            ],
        },
        run_input: json!({}),
    }
}

fn recovery_matrix_blocked_execution_script() -> Result<Value> {
    let cancelled_candidate = recovery_matrix_execution_candidate(
        RECOVERY_MATRIX_EXECUTION_ROOT,
        RECOVERY_MATRIX_EXECUTION_TASK,
    );
    let replacement_candidate = recovery_matrix_execution_candidate(
        RECOVERY_MATRIX_REPLACEMENT_EXECUTION_ROOT,
        RECOVERY_MATRIX_REPLACEMENT_EXECUTION_TASK,
    );
    Ok(json!({
        "default": {
            "completion": {
                "content": "replacement execution synthesis complete",
                "tool_calls": []
            }
        },
        "keyed": [
            {
                "match": RECOVERY_MATRIX_CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"durable","rationale":"The request requires recoverable durable execution.","confidence_bps":10000,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_EXECUTION_TASK,
                "completion": {
                    "content": serde_json::to_string(&json!({
                        "answer": RECOVERY_MATRIX_LATE_EXECUTION_RESULT
                    }))?,
                    "tool_calls": [],
                    "latency_ms": 120000,
                    "input_tokens": RECOVERY_MATRIX_CANCELLED_EXECUTION_INPUT_TOKENS
                }
            },
            {
                "match": RECOVERY_MATRIX_REPLACEMENT_EXECUTION_TASK,
                "completion": {
                    "content": serde_json::to_string(&json!({
                        "answer": RECOVERY_MATRIX_REPLACEMENT_EXECUTION_RESULT
                    }))?,
                    "tool_calls": [],
                    "input_tokens": 47
                }
            },
            {
                "match": RECOVERY_MATRIX_EXECUTION_ROOT,
                "completion": {
                    "content": serde_json::to_string(&cancelled_candidate)?,
                    "tool_calls": []
                }
            },
            {
                "match": RECOVERY_MATRIX_REPLACEMENT_EXECUTION_ROOT,
                "completion": {
                    "content": serde_json::to_string(&replacement_candidate)?,
                    "tool_calls": []
                }
            }
        ]
    }))
}

async fn recovery_matrix_wait_for_execution_status(
    fixture: &OrchestratorTestFixture,
    client: &moa_test_support::TestApiClient,
    request: &ExecutionRunRequest,
    expected: ExecutionRunStatus,
    timeout: Duration,
) -> Result<ExecutionStatusResponse> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        let status: ExecutionStatusResponse = client
            .post_call("/Execution/status", request)
            .await
            .context("poll recovery-matrix Execution/status")?;
        if status.run.status == expected {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            let invocations = recovery_matrix_restate_rows(
                fixture,
                "SELECT id, invoked_by_id, target_service_name, target_service_key, status \
                 FROM sys_invocation WHERE target_service_name IN \
                 ('ExecutionRunController', 'ExecutionTaskAttempt', 'ExecutionDispatcher', 'LLMGateway') \
                 ORDER BY target_service_name, id",
            )
            .await
            .unwrap_or_else(|error| vec![json!({"introspection_error": error.to_string()})]);
            let orchestrator_exit = fixture
                .unexpected_orchestrator_exit()
                .await
                .unwrap_or_else(|error| Some(format!("exit inspection failed: {error}")));
            bail!(
                "execution run {} did not reach {expected:?}; last status: {:?}; orchestrator_exit={orchestrator_exit:?}; invocations={invocations:?}",
                request.run_uid,
                status.run.status
            );
        }
    }
}

async fn recovery_matrix_wait_for_session_unfenced(
    session: &moa_test_support::TestSessionHandle<'_>,
    run_uid: uuid::Uuid,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        poll.tick().await;
        let snapshot = session.snapshot().await?;
        if snapshot.active_turn_id.is_none()
            && !snapshot.active_execution_run_uids.contains(&run_uid)
        {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "cancelled execution {run_uid} did not release its session-owned run and synthesis fences; last snapshot: {snapshot:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA/Redis scripted-provider fixture"]
async fn recovery_matrix_execution_task_llm_cancel_crash_restart_fences_budget_service_e2e()
-> Result<()> {
    // Pins: cancelling a durable run joins its blocked ExecutionTaskAttempt LLM child across a
    // hard orchestrator crash, records one cancelled run/task with zero actual usage, and permits
    // a fresh replacement durable run to execute its task exactly once.
    let fixture = OrchestratorTestFixture::with_script(recovery_matrix_blocked_execution_script()?)
        .await
        .context("boot execution-task recovery-matrix fixture")?;
    fixture.reset_scripted_requests()?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("recovery-matrix-execution-task-cancel")
        .await?;
    let meta = test.client().get_session(session_id).await?;
    let session = test.client().session(session_id.to_string());
    let admitted = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_EXECUTION_ROOT),
            None,
        )
        .await?;
    let admitted_turn_id = admitted.turn_id.context("durable turn omitted turn id")?;
    let admitted_outcome = session
        .await_turn_outcome(
            &admitted_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = admitted_outcome.kind else {
        bail!("durable recovery-matrix turn was not accepted: {admitted_outcome:?}");
    };
    let cancelled_run = ExecutionRunRequest {
        tenant_id: meta.tenant_id,
        contact_id: None,
        session_id,
        run_uid: execution_run_uid,
    };
    recovery_matrix_wait_for_request(
        &fixture,
        RECOVERY_MATRIX_EXECUTION_TASK,
        RECOVERY_MATRIX_CLASSIFIER_PROMPT,
        Duration::from_secs(60),
    )
    .await?;
    let task_attempt_key =
        recovery_matrix_execution_task_attempt_key(&fixture, execution_run_uid).await?;
    let (parent_invocation_id, child_invocation_id) = recovery_matrix_blocked_llm_invocation(
        &fixture,
        "ExecutionTaskAttempt",
        Some(&task_attempt_key),
    )
    .await?;

    let session_snapshot = session.snapshot().await?;
    ensure!(
        session_snapshot
            .active_execution_run_uids
            .contains(&execution_run_uid),
        "session omitted blocked execution run before cancellation: {session_snapshot:?}"
    );

    test.client()
        .post_void(
            &format!("/Session/{session_id}/cancel"),
            &CancelScope::TaskTree,
        )
        .await?;
    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard-crash and restart blocked execution-task fixture")?;
    let cancelled = recovery_matrix_wait_for_execution_status(
        &fixture,
        test.client(),
        &cancelled_run,
        ExecutionRunStatus::Cancelled,
        Duration::from_secs(60),
    )
    .await?;
    assert_eq!(cancelled.run.budget_ledger.consumed.tokens, 0);
    recovery_matrix_assert_child_joined(&fixture, &parent_invocation_id, &child_invocation_id)
        .await?;
    let cancelled_tasks: ExecutionTaskListResponse = test
        .client()
        .post_call(
            "/Execution/list_tasks",
            &ExecutionTaskListRequest {
                run: cancelled_run.clone(),
                limit: Some(100),
                cursor: None,
            },
        )
        .await?;
    let cancelled_agent = cancelled_tasks
        .tasks
        .iter()
        .find(|task| task.node_id == "agent")
        .context("cancelled run omitted its agent task")?;
    assert_eq!(cancelled_agent.status, ExecutionTaskStatus::Cancelled);
    let cancelled_usage = cancelled_agent
        .outcome
        .as_ref()
        .map(|outcome| outcome.usage.clone())
        .unwrap_or(moa_artifacts::execution_plan::ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        });
    assert_eq!(cancelled_usage.tokens, 0);
    assert_eq!(cancelled_usage.cost_microusd, 0);
    let cancelled_events = recovery_matrix_wait_for_events(
        test.client(),
        session_id,
        Duration::from_secs(60),
        |events| {
            events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::ExecutionCancelled(summary) if summary.run_uid == execution_run_uid
                )
            })
        },
    )
    .await?;
    assert_eq!(
        cancelled_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ExecutionCancelled(summary) if summary.run_uid == execution_run_uid
                )
            })
            .count(),
        1,
        "cancelled durable run must publish exactly one terminal event"
    );
    recovery_matrix_wait_for_session_unfenced(&session, execution_run_uid, Duration::from_secs(60))
        .await?;

    let replacement = session
        .start_turn(
            recovery_matrix_turn_request(RECOVERY_MATRIX_REPLACEMENT_EXECUTION_ROOT),
            None,
        )
        .await?;
    assert!(
        !replacement.queued,
        "cancelled execution left a session fence"
    );
    let replacement_turn_id = replacement.turn_id.context("replacement omitted turn id")?;
    let replacement_outcome = session
        .await_turn_outcome(
            &replacement_turn_id,
            Duration::from_secs(60),
            Duration::from_millis(25),
        )
        .await?;
    let TurnOutcomeKind::Accepted {
        execution_run_uid: replacement_run_uid,
    } = replacement_outcome.kind
    else {
        bail!("replacement durable turn was not accepted: {replacement_outcome:?}");
    };
    let replacement_run = ExecutionRunRequest {
        run_uid: replacement_run_uid,
        ..cancelled_run.clone()
    };
    recovery_matrix_wait_for_execution_status(
        &fixture,
        test.client(),
        &replacement_run,
        ExecutionRunStatus::Completed,
        Duration::from_secs(60),
    )
    .await?;

    let requests = fixture
        .scripted_requests()?
        .into_iter()
        .map(serde_json::from_value::<CompletionRequest>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                recovery_matrix_request_contains(request, RECOVERY_MATRIX_EXECUTION_TASK)
            })
            .count(),
        1,
        "cancelled execution-task LLM must not replay"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                recovery_matrix_request_contains(
                    request,
                    RECOVERY_MATRIX_REPLACEMENT_EXECUTION_TASK,
                )
            })
            .count(),
        1,
        "replacement execution task must execute exactly once"
    );
    assert!(cancelled_events.iter().all(|record| {
        !matches!(
            &record.event,
            Event::BrainResponse { text, .. } if text.contains(RECOVERY_MATRIX_LATE_EXECUTION_RESULT)
        ) && record.event.input_tokens() != RECOVERY_MATRIX_CANCELLED_EXECUTION_INPUT_TOKENS
    }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, local Restate/Postgres, and a supported provider API key"]
async fn coordinator_reasoning_only_selection_pins_respond_route_provider_e2e() -> Result<()> {
    // Pins: direct reasoning-only work remains Respond with its typed reason and no planning.
    let harness = SupplementaryLiveHarness::start("reasoning-only-selection").await?;
    let turn_id = harness
        .start_turn(REASONING_ONLY_SELECTION_PROMPT, 3)
        .await?;
    wait_for_status(
        &harness.client,
        &harness.ingress,
        &harness.session.identity,
        harness.session.session_id,
        SessionStatus::Idle,
        Duration::from_secs(120),
    )
    .await
    .with_context(|| format!("reasoning-only selection turn {turn_id} should complete"))?;
    let events = harness.events().await?;
    assert_reasoning_only_selection(&harness, &events).await?;
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
