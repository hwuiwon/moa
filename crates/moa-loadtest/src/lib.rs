//! Custom load-test harness for realistic MOA multi-turn agent workloads.

pub mod scenarios;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::ValueEnum;
use moa_core::{
    ApprovalDecision, BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse,
    CompletionStream, DaemonCommand, DaemonReply, DaemonStreamEvent, Event, EventRecord,
    LLMProvider, MoaConfig, MoaError, ModelCapabilities, ModelId, ModelTask, Platform, Result,
    RuntimeEvent, SessionId, SessionMeta, SessionSignal, SessionStatus, SessionStore,
    StartSessionRequest, TokenPricing, TokenUsage, ToolCallFormat, UserId, UserMessage,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_orchestrator_local::LocalOrchestrator;
use moa_providers::{ModelRouter, ScriptedResponse, resolve_provider_selection};
use moa_session::{
    PostgresSessionStore,
    testing::{cleanup_test_schema, test_database_url},
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

/// Execution mode for the load harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Use the scripted mock provider and exercise only MOA infrastructure.
    Mock,
    /// Use the configured real provider stack.
    Live,
}

/// Backend target for the load harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoadTarget {
    /// Run an in-process local orchestrator.
    Local,
    /// Drive a running MOA daemon over its Unix socket.
    Daemon,
}

/// Session profile family for the generated workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SessionProfileKind {
    /// Five simple interactive turns.
    Short,
    /// Forty turns with deterministic read-only tool pressure in mock mode.
    Long,
    /// Stable mixed traffic with both short and long sessions.
    Mixed,
}

/// Output format for the final load-test report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable report text.
    Human,
    /// Structured JSON.
    Json,
}

/// User-configurable load-test options.
#[derive(Debug, Clone)]
pub struct LoadTestOptions {
    /// Execution mode.
    pub mode: LoadMode,
    /// Backend target.
    pub target: LoadTarget,
    /// Number of concurrent sessions to simulate.
    pub sessions: usize,
    /// Session profile family.
    pub profile: SessionProfileKind,
    /// Delay inserted between turns inside one session.
    pub inter_message_delay: Duration,
    /// Per-turn timeout.
    pub turn_timeout: Duration,
    /// Final output format.
    pub output: OutputFormat,
    /// Optional explicit model override for local live runs.
    pub model: Option<String>,
    /// Optional explicit config path.
    pub config_path: Option<PathBuf>,
    /// Optional explicit workspace root for local runs.
    pub workspace_root: Option<PathBuf>,
    /// Optional daemon socket path.
    pub daemon_socket: Option<PathBuf>,
}

impl LoadTestOptions {
    fn validate(&self) -> Result<()> {
        if !(1..=1_000).contains(&self.sessions) {
            return Err(MoaError::ValidationError(format!(
                "sessions must be between 1 and 1000; got {}",
                self.sessions
            )));
        }
        if matches!(self.mode, LoadMode::Mock) && matches!(self.target, LoadTarget::Daemon) {
            return Err(MoaError::ValidationError(
                "mock mode supports only the in-process local target".to_string(),
            ));
        }
        if self.turn_timeout.is_zero() {
            return Err(MoaError::ValidationError(
                "turn_timeout must be greater than zero".to_string(),
            ));
        }
        if self.model.is_some() && matches!(self.target, LoadTarget::Daemon) {
            return Err(MoaError::ValidationError(
                "model overrides are only supported for the local in-process target".to_string(),
            ));
        }
        Ok(())
    }
}

/// One completed session's measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    /// Session identifier.
    pub session_id: SessionId,
    /// Resolved profile for this session.
    pub profile: SessionProfileKind,
    /// Final session status.
    pub status: SessionStatus,
    /// Number of planned turns.
    pub planned_turns: usize,
    /// Number of completed turns observed by the harness.
    pub completed_turns: usize,
    /// Total session wall time in milliseconds.
    pub duration_ms: f64,
    /// Session-scoped cache hit rate.
    pub cache_hit_rate: f64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Total tool calls observed across the session.
    pub tool_calls: usize,
    /// Total error events observed across the session.
    pub error_count: usize,
    /// Count of approvals auto-denied by the harness.
    pub auto_denied_approvals: usize,
    /// Turn-by-turn latency samples in milliseconds.
    pub turn_latency_ms: Vec<f64>,
    /// Turn-by-turn TTFT samples in milliseconds.
    pub ttft_ms: Vec<f64>,
    /// Optional failure reason.
    pub failure_reason: Option<String>,
}

/// Percentile summary for one numeric metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PercentileSummary {
    /// Minimum sample value.
    pub min: f64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Maximum sample value.
    pub max: f64,
}

/// Aggregate load-test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestReport {
    /// Execution mode.
    pub mode: LoadMode,
    /// Backend target.
    pub target: LoadTarget,
    /// Requested profile family.
    pub profile: SessionProfileKind,
    /// Requested session count.
    pub sessions_requested: usize,
    /// Completed sessions.
    pub sessions_completed: usize,
    /// Failed sessions.
    pub sessions_failed: usize,
    /// Total observed error events.
    pub error_count: usize,
    /// Total observed tool calls.
    pub total_tool_calls: usize,
    /// Total auto-denied approvals.
    pub auto_denied_approvals: usize,
    /// Total run wall time in milliseconds.
    pub duration_ms: f64,
    /// Aggregate turn latency summary.
    pub latency_ms: PercentileSummary,
    /// Aggregate TTFT summary.
    pub ttft_ms: PercentileSummary,
    /// Aggregate cache-hit summary across sessions.
    pub cache_hit_rate: PercentileSummary,
    /// Total spend in cents.
    pub total_cost_cents: u64,
    /// Workspace root used for the run when known.
    pub workspace_root: Option<PathBuf>,
    /// Per-session results.
    pub sessions: Vec<SessionReport>,
}

/// Runs one load-test scenario and returns the final report.
pub async fn run_loadtest(options: LoadTestOptions) -> Result<LoadTestReport> {
    options.validate()?;
    let mut config = load_config(options.config_path.as_deref())?;
    config.observability.enabled = false;
    config.metrics.enabled = false;
    config.memory.auto_bootstrap = false;
    if matches!(options.mode, LoadMode::Mock) {
        config.compaction.enabled = false;
        config.session_limits.max_turns = 0;
        config.session_limits.loop_detection_threshold = 0;
    }

    let workspace_root = match options.target {
        LoadTarget::Local => Some(resolve_workspace_root(options.workspace_root.as_deref())?),
        LoadTarget::Daemon => None,
    };
    let inspection_files = inspectable_files(workspace_root.as_deref()).await?;
    let plans = build_session_plans(options.sessions, options.profile, &inspection_files);
    let backend = build_backend(&options, &mut config, workspace_root.clone()).await?;
    let started = Instant::now();
    let run_result = run_sessions(
        backend.clone(),
        &options,
        plans,
        workspace_root.clone(),
        started,
    )
    .await;
    let cleanup_result = backend.cleanup().await;

    match (run_result, cleanup_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}

/// Renders a human-readable load-test report.
pub fn render_human_report(report: &LoadTestReport) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "MOA Load Test Report");
    let _ = writeln!(&mut output, "====================");
    let _ = writeln!(
        &mut output,
        "Mode: {} | Target: {} | Sessions: {} | Profile: {}",
        report.mode.as_str(),
        report.target.as_str(),
        report.sessions_requested,
        report.profile.as_str()
    );
    if let Some(root) = &report.workspace_root {
        let _ = writeln!(&mut output, "Workspace: {}", root.display());
    }
    let _ = writeln!(
        &mut output,
        "Duration: {:.2}s",
        report.duration_ms / 1_000.0
    );
    let _ = writeln!(&mut output);
    let _ = writeln!(
        &mut output,
        "Turn Latency:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.latency_ms.p50),
        format_millis(report.latency_ms.p95),
        format_millis(report.latency_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "TTFT:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.ttft_ms.p50),
        format_millis(report.ttft_ms.p95),
        format_millis(report.ttft_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "Cache Hit Rate:\n  mean: {:.1}%  min: {:.1}%  max: {:.1}%",
        report.cache_hit_rate.mean * 100.0,
        report.cache_hit_rate.min * 100.0,
        report.cache_hit_rate.max * 100.0
    );
    let _ = writeln!(
        &mut output,
        "Sessions: {} completed, {} failed",
        report.sessions_completed, report.sessions_failed
    );
    let _ = writeln!(
        &mut output,
        "Total cost: {}",
        format_cost(report.total_cost_cents)
    );
    let _ = writeln!(
        &mut output,
        "Tool calls: {} | Errors: {} | Auto-denied approvals: {}",
        report.total_tool_calls, report.error_count, report.auto_denied_approvals
    );
    output
}

/// Serializes the report as pretty JSON.
pub fn render_json_report(report: &LoadTestReport) -> Result<String> {
    serde_json::to_string_pretty(report)
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

#[derive(Clone)]
struct InspectionFiles {
    summary_file: String,
    detail_file: String,
}

#[derive(Clone)]
struct SessionPlan {
    profile: SessionProfileKind,
    title: String,
    turns: Vec<TurnPlan>,
}

#[derive(Clone)]
struct TurnPlan {
    prompt: String,
    mock_behavior: MockTurnBehavior,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
enum MockTurnBehavior {
    Simple,
    FileRead {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    Bash {
        cmd: String,
    },
}

#[derive(Debug)]
struct TurnObservation {
    latency: Duration,
    ttft: Option<Duration>,
    auto_denied_approvals: usize,
}

#[derive(Clone)]
struct PerSessionScriptedProvider {
    capabilities: ModelCapabilities,
    responses: Arc<StdMutex<HashMap<String, VecDeque<ScriptedResponse>>>>,
    recorded_requests: Arc<StdMutex<Vec<CompletionRequest>>>,
}

impl PerSessionScriptedProvider {
    fn new(capabilities: ModelCapabilities) -> Self {
        Self {
            capabilities,
            responses: Arc::new(StdMutex::new(HashMap::new())),
            recorded_requests: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn register_session(&self, session_id: &SessionId, plan: &SessionPlan) -> Result<()> {
        let session_key = session_id.to_string();
        let responses = scripted_responses_for_plan(plan);
        self.responses
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider response registry poisoned: {error}"
                ))
            })?
            .insert(session_key, responses);
        Ok(())
    }
}

#[async_trait]
impl LLMProvider for PerSessionScriptedProvider {
    fn name(&self) -> &str {
        "scripted-per-session"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities.clone()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        self.recorded_requests
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider request log poisoned: {error}"
                ))
            })?
            .push(request.clone());
        let Some(session_key) = request
            .metadata
            .get("_moa.session_id")
            .and_then(serde_json::Value::as_str)
        else {
            return completion_stream_from_scripted_response(
                &self.capabilities,
                auxiliary_scripted_response(&request),
            );
        };
        let response = self
            .responses
            .lock()
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider response registry poisoned: {error}"
                ))
            })?
            .get_mut(session_key)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider has no script for session {session_key}"
                ))
            })?
            .pop_front()
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "per-session scripted provider ran out of queued responses for session {session_key}"
                ))
            })?;
        completion_stream_from_scripted_response(&self.capabilities, response)
    }
}

fn auxiliary_scripted_response(request: &CompletionRequest) -> ScriptedResponse {
    if request.messages.iter().any(|message| {
        message
            .content
            .contains("Distill the following successful MOA session into a reusable Agent Skill")
    }) {
        return ScriptedResponse::text(mock_skill_markdown()).with_usage(TokenUsage {
            input_tokens_uncached: 32,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 96,
        });
    }

    ScriptedResponse::text("mock auxiliary summary").with_usage(TokenUsage {
        input_tokens_uncached: 24,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 24,
    })
}

fn mock_skill_markdown() -> String {
    r#"---
name: mock-loadtest-skill
description: "Mock distilled skill for load-test auxiliary requests"
metadata:
  moa-version: "1.0"
  moa-one-liner: "Synthetic skill emitted by moa-loadtest mock mode"
  moa-estimated-tokens: "64"
---

# Mock loadtest skill

1. Reproduce the target workflow with deterministic mock inputs.
2. Measure turn latency, cache hit rate, and tool activity.
3. Verify that all sessions finish without unexpected pauses.
"#
    .to_string()
}

fn completion_stream_from_scripted_response(
    capabilities: &ModelCapabilities,
    response: ScriptedResponse,
) -> Result<CompletionStream> {
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            CompletionContent::Text(text) => Some(text.as_str()),
            CompletionContent::ToolCall(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect::<String>();
    let output_tokens = response
        .content
        .iter()
        .map(|block| match block {
            CompletionContent::Text(text) => text.chars().count().div_ceil(4),
            CompletionContent::ToolCall(call) => {
                8 + call
                    .invocation
                    .input
                    .to_string()
                    .chars()
                    .count()
                    .div_ceil(4)
            }
            CompletionContent::ProviderToolResult { summary, .. } => {
                summary.chars().count().div_ceil(4)
            }
        })
        .sum();

    Ok(CompletionStream::from_response(CompletionResponse {
        text,
        content: response.content,
        stop_reason: response.stop_reason,
        model: capabilities.model_id.clone(),
        usage: TokenUsage {
            input_tokens_uncached: response
                .input_tokens
                .saturating_sub(response.cached_input_tokens)
                .saturating_sub(response.cache_write_input_tokens),
            input_tokens_cache_write: response.cache_write_input_tokens,
            input_tokens_cache_read: response.cached_input_tokens,
            output_tokens,
        },
        duration_ms: response.duration_ms,
        thought_signature: None,
    }))
}

#[async_trait]
trait SessionTarget: Send + Sync {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId>;
    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation>;
    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta>;
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
    async fn cleanup(&self) -> Result<()>;
}

#[derive(Clone)]
struct LocalTarget {
    orchestrator: Arc<LocalOrchestrator>,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
    mock_provider: Option<Arc<PerSessionScriptedProvider>>,
    database_url: String,
    schema_name: String,
    _scratch_dir: Arc<TempDir>,
}

#[async_trait]
impl SessionTarget for LocalTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        let session_id = self
            .orchestrator
            .start_session(StartSessionRequest {
                workspace_id: self.workspace_id.clone(),
                user_id: self.user_id.clone(),
                platform: Platform::Cli,
                model: self.model.clone(),
                initial_message: None,
                title: Some(plan.title.clone()),
                parent_session_id: None,
            })
            .await?
            .session_id;
        if let Some(mock_provider) = &self.mock_provider {
            mock_provider.register_session(&session_id, plan)?;
        }
        Ok(session_id)
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let runtime_rx = match self.orchestrator.observe_runtime(session_id).await? {
            Some(runtime_rx) => runtime_rx,
            None => {
                self.orchestrator.resume_session(session_id).await?;
                self.orchestrator
                    .observe_runtime(session_id)
                    .await?
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "runtime observation unavailable for session {session_id}"
                        ))
                    })?
            }
        };
        let runtime_rx = Arc::new(Mutex::new(runtime_rx));
        self.orchestrator
            .signal(
                session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
        let orchestrator = self.orchestrator.clone();
        wait_for_turn_completion(
            timeout,
            move || {
                let runtime_rx = runtime_rx.clone();
                async move {
                    let mut runtime_rx = runtime_rx.lock().await;
                    runtime_rx.recv().await.map_err(map_broadcast_error)
                }
            },
            move |request_id| {
                let orchestrator = orchestrator.clone();
                async move {
                    orchestrator
                        .signal(
                            session_id,
                            SessionSignal::ApprovalDecided {
                                request_id,
                                decision: ApprovalDecision::Deny {
                                    reason: Some("auto-denied by moa-loadtest".to_string()),
                                },
                            },
                        )
                        .await
                }
            },
        )
        .await
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.orchestrator.get_session(session_id).await
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.orchestrator
            .session_store()
            .get_events(session_id, moa_core::EventRange::all())
            .await
    }

    async fn cleanup(&self) -> Result<()> {
        self.orchestrator.session_store().pool().close().await;
        cleanup_test_schema(&self.database_url, &self.schema_name).await
    }
}

#[derive(Clone)]
struct DaemonTarget {
    socket_path: PathBuf,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for DaemonTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::CreateSession {
                request: StartSessionRequest {
                    workspace_id: self.workspace_id.clone(),
                    user_id: self.user_id.clone(),
                    platform: Platform::Cli,
                    model: self.model.clone(),
                    initial_message: None,
                    title: Some(plan.title.clone()),
                    parent_session_id: None,
                },
            },
        )
        .await?
        {
            DaemonReply::SessionId(session_id) => Ok(session_id),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_id", &other)),
        }
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let reader = Arc::new(Mutex::new(
            daemon_open_stream(
                &self.socket_path,
                &DaemonCommand::ObserveSession { session_id },
            )
            .await?,
        ));
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::QueueMessage {
                session_id,
                prompt: prompt.to_string(),
            },
        )
        .await?;
        let socket_path = self.socket_path.clone();
        wait_for_turn_completion(
            timeout,
            move || {
                let reader = reader.clone();
                async move {
                    let mut reader = reader.lock().await;
                    daemon_recv_runtime_event(&mut reader).await
                }
            },
            move |request_id| {
                let socket_path = socket_path.clone();
                async move {
                    daemon_expect_ack(
                        &socket_path,
                        &DaemonCommand::RespondToApproval {
                            session_id,
                            request_id,
                            decision: ApprovalDecision::Deny {
                                reason: Some("auto-denied by moa-loadtest".to_string()),
                            },
                        },
                    )
                    .await
                }
            },
        )
        .await
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        match daemon_request(&self.socket_path, &DaemonCommand::GetSession { session_id }).await? {
            DaemonReply::Session(session) => Ok(session),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session", &other)),
        }
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::GetSessionEvents { session_id },
        )
        .await?
        {
            DaemonReply::SessionEvents(events) => Ok(events),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_events", &other)),
        }
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
}

async fn build_backend(
    options: &LoadTestOptions,
    config: &mut MoaConfig,
    workspace_root: Option<PathBuf>,
) -> Result<Arc<dyn SessionTarget>> {
    match options.target {
        LoadTarget::Local => build_local_target(options, config, workspace_root).await,
        LoadTarget::Daemon => build_daemon_target(options, config).await,
    }
}

async fn build_local_target(
    options: &LoadTestOptions,
    config: &mut MoaConfig,
    workspace_root: Option<PathBuf>,
) -> Result<Arc<dyn SessionTarget>> {
    let workspace_root = workspace_root.ok_or_else(|| {
        MoaError::ValidationError("local target requires a workspace root".to_string())
    })?;
    // The in-process loadtest target always uses an isolated Postgres schema so it
    // exercises the real session-store path without polluting a user's configured DB.
    config.database.url = test_database_url();
    let scratch_dir = tempfile::tempdir()
        .map_err(|error| MoaError::ProviderError(format!("failed to create tempdir: {error}")))?;
    config.local.memory_dir = scratch_dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = scratch_dir.path().join("sandbox").display().to_string();
    config.local.docker_enabled = false;
    let schema_name = format!("moa_loadtest_{}", Uuid::now_v7().simple());
    let session_store = Arc::new(
        PostgresSessionStore::new_in_schema(config.database.runtime_url(), &schema_name).await?,
    );
    let tool_router = Arc::new(
        ToolRouter::from_config(config)
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );

    let mock_provider = match options.mode {
        LoadMode::Mock => Some(Arc::new(PerSessionScriptedProvider::new(
            scripted_capabilities(),
        ))),
        LoadMode::Live => None,
    };
    let model_router = match options.mode {
        LoadMode::Mock => Arc::new(ModelRouter::new(
            mock_provider.clone().ok_or_else(|| {
                MoaError::ProviderError(
                    "mock mode requires a per-session scripted provider".to_string(),
                )
            })?,
            None,
        )),
        LoadMode::Live => {
            if let Some(model) = options.model.as_deref() {
                let selection = resolve_provider_selection(config, Some(model))?;
                config.set_main_model(selection.provider_name, selection.model_id);
            }
            Arc::new(ModelRouter::from_config(config)?)
        }
    };
    let model = model_router
        .provider_for(ModelTask::MainLoop)
        .capabilities()
        .model_id;
    let workspace_id = workspace_id_for_root(&workspace_root, "local");
    let orchestrator = Arc::new(
        LocalOrchestrator::new(config.clone(), session_store, model_router, tool_router).await?,
    );
    orchestrator
        .remember_workspace_root(workspace_id.clone(), workspace_root)
        .await;

    Ok(Arc::new(LocalTarget {
        orchestrator,
        workspace_id,
        user_id: UserId::new("loadtest"),
        model,
        mock_provider,
        database_url: config.database.runtime_url().to_string(),
        schema_name,
        _scratch_dir: Arc::new(scratch_dir),
    }))
}

async fn build_daemon_target(
    options: &LoadTestOptions,
    config: &MoaConfig,
) -> Result<Arc<dyn SessionTarget>> {
    #[cfg(not(unix))]
    {
        let _ = options;
        let _ = config;
        return Err(MoaError::Unsupported(
            "daemon load testing requires unix-domain sockets".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let socket_path = options
            .daemon_socket
            .clone()
            .unwrap_or_else(|| expand_local_path(&config.daemon.socket_path));
        let workspace_root = resolve_workspace_root(options.workspace_root.as_deref())?;
        Ok(Arc::new(DaemonTarget {
            socket_path,
            workspace_id: workspace_id_for_root(&workspace_root, "daemon"),
            user_id: UserId::new("loadtest"),
            model: ModelId::new(config.model_for_task(ModelTask::MainLoop)),
        }))
    }
}

async fn run_sessions(
    backend: Arc<dyn SessionTarget>,
    options: &LoadTestOptions,
    plans: Vec<SessionPlan>,
    workspace_root: Option<PathBuf>,
    started: Instant,
) -> Result<LoadTestReport> {
    let (results_tx, mut results_rx) = mpsc::channel(plans.len());
    for plan in plans {
        let backend = backend.clone();
        let results_tx = results_tx.clone();
        let inter_message_delay = options.inter_message_delay;
        let turn_timeout = options.turn_timeout;
        tokio::spawn(async move {
            let report = simulate_session(backend, plan, inter_message_delay, turn_timeout).await;
            let _ = results_tx.send(report).await;
        });
    }
    drop(results_tx);

    let mut sessions = Vec::new();
    while let Some(report) = results_rx.recv().await {
        sessions.push(report);
    }

    sessions.sort_by_key(|session| session.session_id.to_string());
    let sessions_completed = sessions
        .iter()
        .filter(|session| session.failure_reason.is_none())
        .count();
    let sessions_failed = sessions.len().saturating_sub(sessions_completed);
    let error_count = sessions.iter().map(|session| session.error_count).sum();
    let total_tool_calls = sessions.iter().map(|session| session.tool_calls).sum();
    let auto_denied_approvals = sessions
        .iter()
        .map(|session| session.auto_denied_approvals)
        .sum();
    let total_cost_cents = sessions
        .iter()
        .map(|session| session.total_cost_cents)
        .sum();
    let latency_samples = sessions
        .iter()
        .flat_map(|session| session.turn_latency_ms.iter().copied())
        .collect::<Vec<_>>();
    let ttft_samples = sessions
        .iter()
        .flat_map(|session| session.ttft_ms.iter().copied())
        .collect::<Vec<_>>();
    let cache_samples = sessions
        .iter()
        .map(|session| session.cache_hit_rate)
        .collect::<Vec<_>>();

    Ok(LoadTestReport {
        mode: options.mode,
        target: options.target,
        profile: options.profile,
        sessions_requested: options.sessions,
        sessions_completed,
        sessions_failed,
        error_count,
        total_tool_calls,
        auto_denied_approvals,
        duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
        latency_ms: summarize_percentiles(&latency_samples),
        ttft_ms: summarize_percentiles(&ttft_samples),
        cache_hit_rate: summarize_percentiles(&cache_samples),
        total_cost_cents,
        workspace_root,
        sessions,
    })
}

async fn simulate_session(
    backend: Arc<dyn SessionTarget>,
    plan: SessionPlan,
    inter_message_delay: Duration,
    turn_timeout: Duration,
) -> SessionReport {
    let started = Instant::now();
    let session_id = match backend.start_session(&plan).await {
        Ok(session_id) => session_id,
        Err(error) => {
            return SessionReport {
                session_id: SessionId::new(),
                profile: plan.profile,
                status: SessionStatus::Failed,
                planned_turns: plan.turns.len(),
                completed_turns: 0,
                duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
                cache_hit_rate: 0.0,
                total_cost_cents: 0,
                tool_calls: 0,
                error_count: 1,
                auto_denied_approvals: 0,
                turn_latency_ms: Vec::new(),
                ttft_ms: Vec::new(),
                failure_reason: Some(error.to_string()),
            };
        }
    };

    let mut completed_turns = 0usize;
    let mut turn_latency_ms = Vec::new();
    let mut ttft_ms = Vec::new();
    let mut tool_calls = 0usize;
    let mut error_count = 0usize;
    let mut auto_denied_approvals = 0usize;
    let mut last_sequence_num = 0u64;
    let mut failure_reason = None;

    for (turn_index, turn) in plan.turns.iter().enumerate() {
        match backend
            .run_turn(session_id, &turn.prompt, turn_timeout)
            .await
        {
            Ok(observation) => {
                completed_turns += 1;
                turn_latency_ms.push(observation.latency.as_secs_f64() * 1_000.0);
                if let Some(ttft) = observation.ttft {
                    ttft_ms.push(ttft.as_secs_f64() * 1_000.0);
                }
                auto_denied_approvals += observation.auto_denied_approvals;

                match backend.session_events(session_id).await {
                    Ok(events) => {
                        let previous_sequence_num = last_sequence_num;
                        let new_events = events
                            .into_iter()
                            .filter(|record| record.sequence_num > previous_sequence_num)
                            .collect::<Vec<_>>();
                        for record in new_events {
                            last_sequence_num = record.sequence_num;
                            match &record.event {
                                Event::ToolCall { .. } => tool_calls += 1,
                                Event::ToolError { error, .. }
                                    if !is_expected_harness_denial(error) =>
                                {
                                    error_count += 1;
                                }
                                Event::Error { .. } => error_count += 1,
                                _ => {}
                            }
                        }
                    }
                    Err(error) => {
                        failure_reason = Some(format!(
                            "turn {} completed but events could not be loaded: {error}",
                            turn_index + 1
                        ));
                        break;
                    }
                }

                if turn_index + 1 < plan.turns.len() && !inter_message_delay.is_zero() {
                    tokio::time::sleep(inter_message_delay).await;
                }
            }
            Err(error) => {
                failure_reason = Some(format!("turn {} failed: {error}", turn_index + 1));
                break;
            }
        }
    }

    let final_session_note = backend
        .session_events(session_id)
        .await
        .ok()
        .and_then(|events| latest_session_note(&events));

    match backend.session_meta(session_id).await {
        Ok(meta) => {
            let include_session_note = failure_reason.is_some()
                || matches!(
                    meta.status,
                    SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Paused
                );
            SessionReport {
                session_id,
                profile: plan.profile,
                status: meta.status.clone(),
                planned_turns: plan.turns.len(),
                completed_turns,
                duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
                cache_hit_rate: meta.cache_hit_rate(),
                total_cost_cents: meta.total_cost_cents as u64,
                tool_calls,
                error_count,
                auto_denied_approvals,
                turn_latency_ms,
                ttft_ms,
                failure_reason: merge_failure_reason(
                    failure_reason,
                    if matches!(
                        meta.status,
                        SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Paused
                    ) {
                        Some(format!("session ended in status {:?}", meta.status))
                    } else {
                        None
                    },
                    if include_session_note {
                        final_session_note
                    } else {
                        None
                    },
                ),
            }
        }
        Err(error) => SessionReport {
            session_id,
            profile: plan.profile,
            status: SessionStatus::Failed,
            planned_turns: plan.turns.len(),
            completed_turns,
            duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
            cache_hit_rate: 0.0,
            total_cost_cents: 0,
            tool_calls,
            error_count: error_count + 1,
            auto_denied_approvals,
            turn_latency_ms,
            ttft_ms,
            failure_reason: Some(
                merge_failure_reason(
                    failure_reason,
                    Some(format!("failed to load session metadata: {error}")),
                    final_session_note,
                )
                .unwrap_or_else(|| format!("failed to load session metadata: {error}")),
            ),
        },
    }
}

fn latest_session_note(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::Warning { message } => Some(message.clone()),
        Event::Error { message, .. } => Some(message.clone()),
        _ => None,
    })
}

fn is_expected_harness_denial(message: &str) -> bool {
    message.contains("auto-denied by moa-loadtest")
}

fn merge_failure_reason(
    primary: Option<String>,
    secondary: Option<String>,
    note: Option<String>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(primary) = primary {
        parts.push(primary);
    }
    if let Some(secondary) = secondary
        && !parts.iter().any(|existing| existing == &secondary)
    {
        parts.push(secondary);
    }
    if let Some(note) = note
        && !parts.iter().any(|existing| existing == &note)
    {
        parts.push(format!("session note: {note}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

async fn wait_for_turn_completion<Recv, RecvFuture, Approve, ApproveFuture>(
    timeout: Duration,
    mut recv_event: Recv,
    mut respond_to_approval: Approve,
) -> Result<TurnObservation>
where
    Recv: FnMut() -> RecvFuture + Send,
    RecvFuture: std::future::Future<Output = Result<RuntimeEvent>> + Send,
    Approve: FnMut(Uuid) -> ApproveFuture + Send,
    ApproveFuture: std::future::Future<Output = Result<()>> + Send,
{
    let started = Instant::now();
    let deadline = started + timeout;
    let mut ttft = None;
    let mut auto_denied_approvals = 0usize;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(MoaError::ProviderError(format!(
                "turn timed out after {:.2}s",
                timeout.as_secs_f64()
            )));
        }
        let remaining = deadline.saturating_duration_since(now);
        let event = tokio::time::timeout(remaining, recv_event())
            .await
            .map_err(|_| {
                MoaError::ProviderError(format!(
                    "turn timed out after {:.2}s",
                    timeout.as_secs_f64()
                ))
            })??;

        if matches!(
            event,
            RuntimeEvent::AssistantStarted
                | RuntimeEvent::AssistantDelta(_)
                | RuntimeEvent::AssistantFinished { .. }
                | RuntimeEvent::ToolUpdate(_)
                | RuntimeEvent::ApprovalRequested(_)
                | RuntimeEvent::Notice(_)
                | RuntimeEvent::Error(_)
        ) && ttft.is_none()
        {
            ttft = Some(started.elapsed());
        }

        match event {
            RuntimeEvent::ApprovalRequested(prompt) => {
                auto_denied_approvals += 1;
                respond_to_approval(prompt.request.request_id).await?;
            }
            RuntimeEvent::TurnCompleted => {
                return Ok(TurnObservation {
                    latency: started.elapsed(),
                    ttft,
                    auto_denied_approvals,
                });
            }
            RuntimeEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
            RuntimeEvent::AssistantStarted
            | RuntimeEvent::AssistantDelta(_)
            | RuntimeEvent::AssistantFinished { .. }
            | RuntimeEvent::ToolUpdate(_)
            | RuntimeEvent::UsageUpdated { .. }
            | RuntimeEvent::Notice(_) => {}
        }
    }
}

async fn daemon_request(socket_path: &Path, command: &DaemonCommand) -> Result<DaemonReply> {
    let mut reader = daemon_open_stream(socket_path, command).await?;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(MoaError::ProviderError(
            "daemon closed the control connection".to_string(),
        ));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

async fn daemon_expect_ack(socket_path: &Path, command: &DaemonCommand) -> Result<()> {
    match daemon_request(socket_path, command).await? {
        DaemonReply::Ack => Ok(()),
        DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
        other => Err(unexpected_daemon_reply("ack", &other)),
    }
}

async fn daemon_open_stream(
    socket_path: &Path,
    command: &DaemonCommand,
) -> Result<BufReader<UnixStream>> {
    #[cfg(not(unix))]
    {
        let _ = socket_path;
        let _ = command;
        return Err(MoaError::Unsupported(
            "daemon mode requires unix-domain sockets".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let mut socket = UnixStream::connect(socket_path).await?;
        let payload = serde_json::to_string(command)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        socket.write_all(payload.as_bytes()).await?;
        socket.write_all(b"\n").await?;
        Ok(BufReader::new(socket))
    }
}

async fn daemon_recv_runtime_event(reader: &mut BufReader<UnixStream>) -> Result<RuntimeEvent> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(MoaError::ProviderError(
                "daemon runtime stream closed unexpectedly".to_string(),
            ));
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(runtime) => return Ok(runtime),
            DaemonStreamEvent::Gap { count, channel } => {
                return Ok(RuntimeEvent::Notice(format!(
                    "missed {count} daemon runtime events on {}",
                    channel.as_str()
                )));
            }
            DaemonStreamEvent::Error(message) => return Err(MoaError::ProviderError(message)),
        }
    }
}

fn unexpected_daemon_reply(expected: &str, reply: &DaemonReply) -> MoaError {
    MoaError::ProviderError(format!(
        "expected daemon reply `{expected}`, received {reply:?}"
    ))
}

fn map_broadcast_error(error: broadcast::error::RecvError) -> MoaError {
    match error {
        broadcast::error::RecvError::Closed => {
            MoaError::ProviderError("runtime stream closed".to_string())
        }
        broadcast::error::RecvError::Lagged(skipped) => {
            MoaError::ProviderError(format!("runtime stream lagged by {skipped} events"))
        }
    }
}

fn load_config(path: Option<&Path>) -> Result<MoaConfig> {
    match path {
        Some(path) => MoaConfig::load_from_path(path),
        None => MoaConfig::load(),
    }
}

fn resolve_workspace_root(path: Option<&Path>) -> Result<PathBuf> {
    let root = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|error| {
            MoaError::ProviderError(format!("failed to resolve current directory: {error}"))
        })?,
    };
    root.canonicalize()
        .or(Ok(root))
        .map_err(|error: std::io::Error| {
            MoaError::ProviderError(format!("failed to canonicalize workspace root: {error}"))
        })
}

fn workspace_id_for_root(root: &Path, suffix: &str) -> WorkspaceId {
    let label = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_workspace_label)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    WorkspaceId::new(format!(
        "{label}-loadtest-{suffix}-{}",
        &Uuid::now_v7().simple().to_string()[..8]
    ))
}

fn sanitize_workspace_label(label: &str) -> String {
    let mut sanitized = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }
    sanitized.trim_matches('-').to_string()
}

fn expand_local_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }
    PathBuf::from(path)
}

async fn inspectable_files(workspace_root: Option<&Path>) -> Result<InspectionFiles> {
    if let Some(root) = workspace_root {
        let summary_candidates = [
            "Cargo.toml",
            "README.md",
            "docs/00-direction.md",
            "docs/02-brain-orchestration.md",
        ];
        let detail_candidates = [
            "docs/02-brain-orchestration.md",
            "moa-core/src/runtime_metrics.rs",
            "Cargo.toml",
            "README.md",
        ];
        let summary_file = first_existing_relative_path(root, &summary_candidates)
            .await?
            .unwrap_or_else(|| "Cargo.toml".to_string());
        let detail_file = first_existing_relative_path(root, &detail_candidates)
            .await?
            .unwrap_or_else(|| summary_file.clone());
        return Ok(InspectionFiles {
            summary_file,
            detail_file,
        });
    }

    Ok(InspectionFiles {
        summary_file: "Cargo.toml".to_string(),
        detail_file: "docs/02-brain-orchestration.md".to_string(),
    })
}

async fn first_existing_relative_path(root: &Path, candidates: &[&str]) -> Result<Option<String>> {
    for candidate in candidates {
        if tokio::fs::try_exists(root.join(candidate)).await? {
            return Ok(Some((*candidate).to_string()));
        }
    }
    Ok(None)
}

fn build_session_plans(
    sessions: usize,
    requested_profile: SessionProfileKind,
    inspection_files: &InspectionFiles,
) -> Vec<SessionPlan> {
    (0..sessions)
        .map(|index| {
            let profile = match requested_profile {
                SessionProfileKind::Short => SessionProfileKind::Short,
                SessionProfileKind::Long => SessionProfileKind::Long,
                SessionProfileKind::Mixed => {
                    if index % 4 == 0 {
                        SessionProfileKind::Long
                    } else {
                        SessionProfileKind::Short
                    }
                }
            };
            SessionPlan {
                profile,
                title: format!("loadtest-{profile:?}-{index:04}"),
                turns: match profile {
                    SessionProfileKind::Short => short_profile_turns(inspection_files),
                    SessionProfileKind::Long => long_profile_turns(inspection_files),
                    SessionProfileKind::Mixed => unreachable!("mixed is resolved above"),
                },
            }
        })
        .collect()
}

fn short_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    vec![
        TurnPlan {
            prompt: "Give a concise one-sentence summary of this workspace.".to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: format!(
                "List the two most important facts you can infer from {}.",
                inspection_files.summary_file
            ),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: "What operational metric would you inspect first for session latency spikes?"
                .to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: format!(
                "Briefly explain what {} is likely used for.",
                inspection_files.detail_file
            ),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: "End with a one-line readiness summary for a coding agent runtime.".to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
    ]
}

fn long_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    let prompts = [
        (
            format!(
                "Use tools if needed and summarize the role of {} using lines 1-30.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(1),
                end_line: Some(30),
            },
        ),
        (
            "Name one likely latency bottleneck in a multi-turn agent loop.".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Inspect {} lines 1-40 and report one implementation detail worth monitoring.",
                inspection_files.detail_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.detail_file.clone(),
                start_line: Some(1),
                end_line: Some(40),
            },
        ),
        (
            "What runtime signal would indicate cache warmth improving over time?".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Read {} lines 31-60 and state one concrete string you expect to find.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(31),
                end_line: Some(60),
            },
        ),
        (
            format!(
                "Inspect {} lines 41-80 and call out one detail that would affect monitoring.",
                inspection_files.detail_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.detail_file.clone(),
                start_line: Some(41),
                end_line: Some(80),
            },
        ),
        (
            "What metric would you correlate with TTFT in a staging load test?".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Read {} lines 61-90 and name one concrete token or key you expect.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(61),
                end_line: Some(90),
            },
        ),
    ];

    (0..40)
        .map(|index| {
            let (prompt, behavior) = prompts[index % prompts.len()].clone();
            TurnPlan {
                prompt,
                mock_behavior: behavior,
            }
        })
        .collect()
}

fn scripted_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("scripted-loadtest"),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: Some(Duration::from_secs(300)),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: Some(0.0),
        },
        native_tools: Vec::new(),
    }
}

fn scripted_responses_for_plan(plan: &SessionPlan) -> VecDeque<ScriptedResponse> {
    let mut responses = VecDeque::new();

    for (turn_index, turn) in plan.turns.iter().enumerate() {
        match &turn.mock_behavior {
            MockTurnBehavior::Simple => {
                responses.push_back(scripted_text_response(
                    format!("mock turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
            MockTurnBehavior::FileRead {
                path,
                start_line,
                end_line,
            } => {
                let tool_id = Uuid::now_v7().to_string();
                responses.push_back(
                    ScriptedResponse::tool_call(
                        "file_read",
                        serde_json::json!({
                            "path": path,
                            "start_line": start_line,
                            "end_line": end_line,
                        }),
                        tool_id,
                    )
                    .with_usage(TokenUsage {
                        input_tokens_uncached: 20,
                        input_tokens_cache_write: 0,
                        input_tokens_cache_read: if turn_index == 0 { 0 } else { 48 },
                        output_tokens: 0,
                    }),
                );
                responses.push_back(scripted_text_response(
                    format!("mock tool turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
            MockTurnBehavior::Bash { cmd } => {
                let tool_id = Uuid::now_v7().to_string();
                responses.push_back(
                    ScriptedResponse::tool_call(
                        "bash",
                        serde_json::json!({
                            "cmd": cmd,
                        }),
                        tool_id,
                    )
                    .with_usage(TokenUsage {
                        input_tokens_uncached: 24,
                        input_tokens_cache_write: 0,
                        input_tokens_cache_read: if turn_index == 0 { 0 } else { 52 },
                        output_tokens: 0,
                    }),
                );
                responses.push_back(scripted_text_response(
                    format!("mock approval turn {} complete", turn_index + 1),
                    turn_index,
                ));
            }
        }
    }

    responses
}

fn scripted_text_response(text: String, turn_index: usize) -> ScriptedResponse {
    ScriptedResponse::text(text).with_usage(TokenUsage {
        input_tokens_uncached: if turn_index == 0 { 64 } else { 20 },
        input_tokens_cache_write: 0,
        input_tokens_cache_read: if turn_index == 0 { 0 } else { 44 },
        output_tokens: 24,
    })
}

fn summarize_percentiles(samples: &[f64]) -> PercentileSummary {
    if samples.is_empty() {
        return PercentileSummary {
            min: 0.0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }

    let mut sorted = samples.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let sum: f64 = sorted.iter().sum();
    PercentileSummary {
        min: *sorted.first().unwrap_or(&0.0),
        mean: sum / sorted.len() as f64,
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        p99: percentile(&sorted, 0.99),
        max: *sorted.last().unwrap_or(&0.0),
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn format_millis(value: f64) -> String {
    if value >= 1_000.0 {
        format!("{:.2}s", value / 1_000.0)
    } else {
        format!("{value:.0}ms")
    }
}

fn format_cost(cost_cents: u64) -> String {
    format!("${:.2}", cost_cents as f64 / 100.0)
}

impl LoadMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Live => "live",
        }
    }
}

impl LoadTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Daemon => "daemon",
        }
    }
}

impl SessionProfileKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
            Self::Mixed => "mixed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from("/Users/hwuiwon/Github/moa")
    }

    fn inspection_files() -> InspectionFiles {
        InspectionFiles {
            summary_file: "Cargo.toml".to_string(),
            detail_file: "docs/02-brain-orchestration.md".to_string(),
        }
    }

    fn test_options(profile: SessionProfileKind) -> LoadTestOptions {
        LoadTestOptions {
            mode: LoadMode::Mock,
            target: LoadTarget::Local,
            sessions: 4,
            profile,
            inter_message_delay: Duration::from_millis(1),
            turn_timeout: Duration::from_secs(15),
            output: OutputFormat::Json,
            model: None,
            config_path: None,
            workspace_root: Some(repo_root()),
            daemon_socket: None,
        }
    }

    async fn run_custom_mock_loadtest(
        options: LoadTestOptions,
        plans: Vec<SessionPlan>,
    ) -> Result<LoadTestReport> {
        let mut config = load_config(options.config_path.as_deref())?;
        config.observability.enabled = false;
        config.metrics.enabled = false;
        config.memory.auto_bootstrap = false;
        config.compaction.enabled = false;
        config.session_limits.max_turns = 0;
        config.session_limits.loop_detection_threshold = 0;

        let workspace_root = Some(resolve_workspace_root(options.workspace_root.as_deref())?);
        let backend = build_backend(&options, &mut config, workspace_root.clone()).await?;
        let started = Instant::now();
        let run_result = run_sessions(
            backend.clone(),
            &options,
            plans,
            workspace_root.clone(),
            started,
        )
        .await;
        let cleanup_result = backend.cleanup().await;

        match (run_result, cleanup_result) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
            (Err(error), Err(_cleanup_error)) => Err(error),
        }
    }

    fn approval_heavy_plans(sessions: usize) -> Vec<SessionPlan> {
        (0..sessions)
            .map(|index| SessionPlan {
                profile: SessionProfileKind::Long,
                title: format!("approval-heavy-{index:04}"),
                turns: vec![
                    TurnPlan {
                        prompt: "Summarize the active workspace in one sentence.".to_string(),
                        mock_behavior: MockTurnBehavior::Simple,
                    },
                    TurnPlan {
                        prompt: "Use bash to print an approval marker before answering."
                            .to_string(),
                        mock_behavior: MockTurnBehavior::Bash {
                            cmd: format!("printf 'approval-{index}-1\\n'"),
                        },
                    },
                    TurnPlan {
                        prompt: "Report one likely runtime bottleneck.".to_string(),
                        mock_behavior: MockTurnBehavior::Simple,
                    },
                    TurnPlan {
                        prompt: "Use bash to print a second approval marker.".to_string(),
                        mock_behavior: MockTurnBehavior::Bash {
                            cmd: format!("printf 'approval-{index}-2\\n'"),
                        },
                    },
                    TurnPlan {
                        prompt: "Give a short readiness recommendation.".to_string(),
                        mock_behavior: MockTurnBehavior::Simple,
                    },
                ],
            })
            .collect()
    }

    #[tokio::test]
    async fn mock_short_profile_produces_parseable_report() {
        let report = run_loadtest(test_options(SessionProfileKind::Short))
            .await
            .expect("loadtest report");

        assert_eq!(report.sessions_requested, 4);
        assert_eq!(report.sessions_completed, 4);
        assert_eq!(report.sessions_failed, 0);
        assert!(report.latency_ms.p95 >= report.latency_ms.p50);
        assert_eq!(report.total_cost_cents, 0);

        let json = render_json_report(&report).expect("json report");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert!(parsed.get("latency_ms").is_some());
        assert!(parsed.get("sessions_completed").is_some());
    }

    #[test]
    fn mixed_profile_includes_long_and_short_plans_with_tool_turns() {
        let plans = build_session_plans(4, SessionProfileKind::Mixed, &inspection_files());

        assert_eq!(plans.len(), 4);
        assert_eq!(
            plans
                .iter()
                .filter(|plan| plan.profile == SessionProfileKind::Long)
                .count(),
            1
        );
        assert_eq!(
            plans
                .iter()
                .filter(|plan| plan.profile == SessionProfileKind::Short)
                .count(),
            3
        );
        assert!(
            plans
                .iter()
                .find(|plan| plan.profile == SessionProfileKind::Long)
                .expect("mixed profile includes one long session")
                .turns
                .iter()
                .any(|turn| matches!(turn.mock_behavior, MockTurnBehavior::FileRead { .. }))
        );
    }

    #[tokio::test]
    async fn approval_heavy_sessions_auto_deny_cleanly_under_concurrency() {
        let session_count = 24;
        let options = LoadTestOptions {
            mode: LoadMode::Mock,
            target: LoadTarget::Local,
            sessions: session_count,
            profile: SessionProfileKind::Long,
            inter_message_delay: Duration::ZERO,
            turn_timeout: Duration::from_secs(20),
            output: OutputFormat::Json,
            model: None,
            config_path: None,
            workspace_root: Some(repo_root()),
            daemon_socket: None,
        };

        let report = run_custom_mock_loadtest(options, approval_heavy_plans(session_count))
            .await
            .expect("approval-heavy loadtest report");

        assert_eq!(report.sessions_requested, session_count);
        assert_eq!(report.sessions_completed, session_count);
        assert_eq!(report.sessions_failed, 0);
        assert_eq!(report.auto_denied_approvals, session_count * 2);
        assert_eq!(report.error_count, 0);
        assert!(
            report.sessions.iter().all(|session| {
                session.failure_reason.is_none() && session.auto_denied_approvals == 2
            }),
            "approval-heavy sessions should complete after automatic denials"
        );
    }

    #[tokio::test]
    #[ignore = "stress validation for realistic mock traffic"]
    async fn mock_short_profile_handles_hundred_sessions_within_throughput_budget() {
        let mut options = test_options(SessionProfileKind::Short);
        options.sessions = 100;
        options.inter_message_delay = Duration::ZERO;
        options.turn_timeout = Duration::from_secs(20);

        let started = Instant::now();
        let report = run_loadtest(options).await.expect("loadtest report");

        assert_eq!(report.sessions_requested, 100);
        assert_eq!(report.sessions_completed, 100);
        assert_eq!(report.sessions_failed, 0);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "mock mixed profile exceeded 30s: {:?}",
            started.elapsed()
        );
    }
}
