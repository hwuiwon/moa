#![cfg(feature = "temporal")]

mod support;

use std::fs::OpenOptions;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ContextMessage, Event, EventRange, LLMProvider, MessageRole, MoaConfig, Result, SessionFilter,
    SessionId, SessionSignal, SessionStatus, SessionStore, StopReason, TokenPricing, TokenUsage,
    ToolCallFormat, ToolInvocation, UserMessage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::TemporalOrchestrator;
use moa_providers::{AnthropicProvider, GeminiProvider, ModelRouter, OpenAIProvider};
use moa_session::{PostgresSessionStore, create_session_store, testing};
use serde_json::json;
use support::orchestrator_contract::{
    OrchestratorContractHarness, assert_blank_session_waits_for_first_message,
    assert_processes_multiple_queued_messages_fifo, assert_processes_two_sessions_independently,
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn,
    assert_soft_cancel_waiting_for_approval_cancels_cleanly,
};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};

#[derive(Clone)]
struct TemporalEchoProvider {
    model: String,
    delay: Duration,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for TemporalEchoProvider {
    fn name(&self) -> &str {
        "temporal-echo"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        mock_capabilities(&self.model, false)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        self.requests
            .lock()
            .expect("request log lock poisoned")
            .push(request.clone());
        let prompt = last_user_message(&request.messages).unwrap_or_default();
        Ok(delayed_text_stream(
            &self.model,
            format!("assistant:{prompt}"),
            self.delay,
        ))
    }
}

#[derive(Clone)]
struct TemporalToolThenEchoProvider {
    model: String,
    tool_call: ToolInvocation,
    final_text: Option<String>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for TemporalToolThenEchoProvider {
    fn name(&self) -> &str {
        "temporal-tool-then-echo"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        mock_capabilities(&self.model, true)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: self.tool_call.clone(),
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: self.model.clone().into(),
                input_tokens: 12,
                output_tokens: 3,
                cached_input_tokens: 0,
                usage: usage(12, 0, 0, 3),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            let prompt = last_user_message(&request.messages).unwrap_or_default();
            let text = self
                .final_text
                .clone()
                .unwrap_or_else(|| format!("assistant:{prompt}"));
            CompletionResponse {
                text: text.clone(),
                content: vec![CompletionContent::Text(text)],
                stop_reason: StopReason::EndTurn,
                model: self.model.clone().into(),
                input_tokens: 12,
                output_tokens: 4,
                cached_input_tokens: 0,
                usage: usage(12, 0, 0, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

fn mock_capabilities(model: &str, supports_tools: bool) -> moa_core::ModelCapabilities {
    moa_core::ModelCapabilities {
        model_id: model.to_string().into(),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools,
        supports_vision: false,
        supports_prefix_caching: false,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

fn usage(
    input_tokens_uncached: usize,
    input_tokens_cache_write: usize,
    input_tokens_cache_read: usize,
    output_tokens: usize,
) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached,
        input_tokens_cache_write,
        input_tokens_cache_read,
        output_tokens,
    }
}

fn delayed_text_stream(model: &str, text: String, delay: Duration) -> CompletionStream {
    let response = CompletionResponse {
        text: text.clone(),
        content: vec![CompletionContent::Text(text)],
        stop_reason: StopReason::EndTurn,
        model: model.to_string().into(),
        input_tokens: 4,
        output_tokens: 2,
        cached_input_tokens: 0,
        usage: usage(4, 0, 0, 2),
        duration_ms: delay.as_millis() as u64,
        thought_signature: None,
    };
    if delay.is_zero() {
        return CompletionStream::from_response(response);
    }
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let completion = tokio::spawn(async move {
        sleep(delay).await;
        let _ = tx
            .send(Ok(CompletionContent::Text(response.text.clone())))
            .await;
        Ok(response)
    });
    CompletionStream::new(rx, completion)
}

fn last_user_message(messages: &[ContextMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| {
            message.role == MessageRole::User
                && !message.content.starts_with("<system-reminder>")
                && !message.content.starts_with("<memory-reminder>")
        })
        .or_else(|| {
            messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
        })
        .map(|message| message.content.as_str())
}

struct TemporalContractHarness<'a> {
    orchestrator: &'a TemporalOrchestrator,
    model: String,
    requests: Option<Arc<Mutex<Vec<CompletionRequest>>>>,
}

struct LiveProvider {
    label: &'static str,
    model: String,
    provider: Arc<dyn LLMProvider>,
}

fn available_live_providers() -> Vec<LiveProvider> {
    let mut providers = Vec::new();
    if let Ok(provider) = OpenAIProvider::from_env("gpt-5.4") {
        providers.push(LiveProvider {
            label: "openai",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    if let Ok(provider) = AnthropicProvider::from_env("claude-sonnet-4-6") {
        providers.push(LiveProvider {
            label: "anthropic",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    if let Ok(provider) = GeminiProvider::from_env("gemini-3.1-pro-preview") {
        providers.push(LiveProvider {
            label: "google",
            model: provider.capabilities().model_id.to_string(),
            provider: Arc::new(provider),
        });
    }
    providers
}

impl<'a> TemporalContractHarness<'a> {
    fn new(
        orchestrator: &'a TemporalOrchestrator,
        model: String,
        requests: Option<Arc<Mutex<Vec<CompletionRequest>>>>,
    ) -> Self {
        Self {
            orchestrator,
            model,
            requests,
        }
    }
}

#[async_trait]
impl OrchestratorContractHarness for TemporalContractHarness<'_> {
    fn harness_name(&self) -> &'static str {
        "temporal"
    }

    fn default_model(&self) -> moa_core::ModelId {
        self.model.clone().into()
    }

    fn platform(&self) -> moa_core::Platform {
        moa_core::Platform::Cli
    }

    async fn start_session(
        &self,
        req: moa_core::StartSessionRequest,
    ) -> Result<moa_core::SessionHandle> {
        self.orchestrator.start_session(req).await
    }

    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.orchestrator.signal(session_id, signal).await
    }

    async fn session_status(&self, session_id: SessionId) -> Result<Option<SessionStatus>> {
        Ok(self
            .orchestrator
            .list_sessions(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| session.session_id == session_id)
            .map(|session| session.status))
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<moa_core::EventRecord>> {
        Ok(self
            .orchestrator
            .observe(session_id, moa_core::ObserveLevel::Normal)
            .await?
            .events)
    }

    fn recorded_requests(&self) -> Option<Vec<CompletionRequest>> {
        self.requests
            .as_ref()
            .map(|requests| requests.lock().expect("request log lock poisoned").clone())
    }
}

struct TemporalDevServer {
    child: Child,
    port: u16,
}

impl TemporalDevServer {
    fn start(tempdir: &TempDir) -> Self {
        let port = unused_port();
        let child = Command::new("temporal")
            .args([
                "server",
                "start-dev",
                "--headless",
                "--ip",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--db-filename",
                &tempdir.path().join("temporal.db").display().to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn temporal dev server");

        Self { child, port }
    }

    async fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "temporal dev server did not start listening in time"
            );
            sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for TemporalDevServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn temporal_test_orchestrator_with_provider(
    provider: Arc<dyn LLMProvider>,
) -> (TempDir, TemporalDevServer, TemporalOrchestrator) {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = TemporalDevServer::start(&dir);
    timed_test_stage("temporal:wait_dev_server_ready", server.wait_ready()).await;

    let mut config = MoaConfig::default();
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.cloud.enabled = true;
    if let Some(hands) = config.cloud.hands.as_mut() {
        hands.default_provider = Some("local".to_string());
    }
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .address = Some(format!("127.0.0.1:{}", server.port));
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .namespace = Some("default".to_string());
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .task_queue = format!("moa-test-{}", uuid::Uuid::now_v7());
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .api_key_env = None;

    let (session_store, _database_url, schema_name) = timed_test_stage(
        "temporal:create_test_store",
        testing::create_isolated_test_store(),
    )
    .await
    .expect("session store");
    let session_store = Arc::new(session_store);
    let memory_store = Arc::new(
        timed_test_stage(
            "temporal:create_memory_store",
            FileMemoryStore::from_config_with_pool(
                &config,
                Arc::new(session_store.pool().clone()),
                Some(&schema_name),
            ),
        )
        .await
        .expect("memory store"),
    );
    let tool_router = Arc::new(
        timed_test_stage(
            "temporal:create_tool_router",
            ToolRouter::from_config(&config, memory_store.clone()),
        )
        .await
        .expect("tool router")
        .with_rule_store(session_store.clone())
        .with_session_store(session_store.clone()),
    );
    let orchestrator = timed_test_stage(
        "temporal:create_orchestrator",
        TemporalOrchestrator::new(
            config,
            session_store,
            memory_store,
            Arc::new(ModelRouter::new(provider, None)),
            tool_router,
        ),
    )
    .await
    .expect("temporal orchestrator");
    (dir, server, orchestrator)
}

async fn timed_test_stage<F, T>(stage: &'static str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match timeout(Duration::from_secs(20), future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out waiting for test stage `{stage}`"),
    }
}

async fn temporal_test_orchestrator() -> (TempDir, TemporalDevServer, TemporalOrchestrator) {
    let model = MoaConfig::default().general.default_model;
    temporal_test_orchestrator_with_provider(Arc::new(TemporalToolThenEchoProvider {
        model,
        tool_call: ToolInvocation {
            id: Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()),
            name: "file_write".to_string(),
            input: json!({
                "path": "approval/temporal.txt",
                "content": "written by temporal approval test"
            }),
        },
        final_text: Some("temporal-complete".to_string()),
        requests: Arc::new(Mutex::new(Vec::new())),
    }))
    .await
}

fn temporal_helper_binary() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/examples/temporal_worker_helper")
}

fn build_temporal_helper_binary() {
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "moa-orchestrator",
            "--features",
            "temporal",
            "--example",
            "temporal_worker_helper",
        ])
        .status()
        .expect("failed to invoke cargo build for temporal helper");
    assert!(status.success(), "failed to build temporal helper example");
}

fn spawn_temporal_helper(
    mode: &str,
    root: &Path,
    port: u16,
    task_queue: &str,
    delay_ms: u64,
) -> Child {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(format!("helper-{mode}.stdout.log")))
        .expect("open helper stdout log");
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(format!("helper-{mode}.stderr.log")))
        .expect("open helper stderr log");
    Command::new(temporal_helper_binary())
        .args([
            mode,
            &root.display().to_string(),
            &port.to_string(),
            task_queue,
            &delay_ms.to_string(),
        ])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("failed to spawn temporal helper")
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind free port")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn wait_for_status(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
) {
    wait_for_status_with_timeout(orchestrator, session_id, expected, Duration::from_secs(20)).await;
}

async fn wait_for_status_with_timeout(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
    timeout_window: Duration,
) {
    let deadline = Instant::now() + timeout_window;
    loop {
        let sessions = orchestrator
            .list_sessions(SessionFilter::default())
            .await
            .expect("list sessions");
        if sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .is_some_and(|session| session.status == expected)
        {
            return;
        }

        if Instant::now() >= deadline {
            let current = sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| format!("{:?}", session.status))
                .unwrap_or_else(|| "missing".to_string());
            let events = orchestrator
                .observe(session_id, moa_core::ObserveLevel::Normal)
                .await
                .expect("observe")
                .events;
            panic!(
                "session never reached {expected:?}; current status={current}; events={events:?}"
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_event_text(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    expected_text: &str,
) {
    wait_for_event_text_with_timeout(
        orchestrator,
        session_id,
        expected_text,
        Duration::from_secs(20),
    )
    .await;
}

async fn wait_for_event_text_with_timeout(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    expected_text: &str,
    timeout_window: Duration,
) {
    let deadline = Instant::now() + timeout_window;
    loop {
        let events = orchestrator
            .observe(session_id, moa_core::ObserveLevel::Normal)
            .await
            .expect("observe")
            .events;
        if events.iter().any(|record| match &record.event {
            Event::BrainResponse { text, .. } => text.contains(expected_text),
            _ => false,
        }) {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "brain response containing {expected_text:?} was never observed"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_store_status(
    session_store: &PostgresSessionStore,
    session_id: SessionId,
    expected: SessionStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let session = session_store
            .get_session(session_id)
            .await
            .expect("get session");
        if session.status == expected {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "session never reached {expected:?}; current status={:?}",
            session.status
        );
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_store_event_text(
    session_store: &PostgresSessionStore,
    session_id: SessionId,
    expected_text: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let events = session_store
            .get_events(session_id, EventRange::all())
            .await
            .expect("get events");
        if events.iter().any(|record| match &record.event {
            Event::BrainResponse { text, .. } => text.contains(expected_text),
            _ => false,
        }) {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "brain response containing {expected_text:?} was never persisted"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_tool_result(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    tool_id: uuid::Uuid,
) -> Vec<moa_core::EventRecord> {
    wait_for_tool_result_with_timeout(orchestrator, session_id, tool_id, Duration::from_secs(20))
        .await
}

async fn wait_for_tool_result_with_timeout(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    tool_id: uuid::Uuid,
    timeout_window: Duration,
) -> Vec<moa_core::EventRecord> {
    let deadline = Instant::now() + timeout_window;
    loop {
        let events = orchestrator
            .observe(session_id, moa_core::ObserveLevel::Normal)
            .await
            .expect("observe")
            .events;
        if events.iter().any(|record| match &record.event {
            Event::ToolResult {
                tool_id: event_tool_id,
                ..
            } => event_tool_id.0 == tool_id,
            _ => false,
        }) {
            return events;
        }

        assert!(
            Instant::now() < deadline,
            "tool result for {tool_id} was never observed"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_approval_request_id_with_timeout(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    timeout_window: Duration,
) -> uuid::Uuid {
    let deadline = Instant::now() + timeout_window;
    loop {
        let events = orchestrator
            .observe(session_id, moa_core::ObserveLevel::Normal)
            .await
            .expect("observe")
            .events;
        if let Some(request_id) = events.iter().find_map(|record| match record.event {
            Event::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        }) {
            return request_id;
        }

        assert!(
            Instant::now() < deadline,
            "approval request was never observed for session {session_id}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn run_live_temporal_provider_tool_approval_roundtrip(provider: LiveProvider) {
    let label = provider.label.to_string();
    let model = provider.model.clone();
    let token = format!("LIVE-E2E-{}", label.to_uppercase());
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(provider.provider).await;

    let relative_path = format!("live/{label}.txt");
    let prompt = format!(
        "Use the file_write tool exactly once to write \"{token}\" to \"{relative_path}\". \
         After the tool succeeds, answer with exactly {token}."
    );
    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new(format!("ws-live-{label}")),
            user_id: moa_core::UserId::new(format!("u-live-{label}")),
            platform: moa_core::Platform::Cli,
            model: model.into(),
            initial_message: Some(UserMessage {
                text: prompt,
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .unwrap_or_else(|error| panic!("{label} start session failed: {error}"));

    wait_for_status_with_timeout(
        &orchestrator,
        session.session_id,
        SessionStatus::WaitingApproval,
        Duration::from_secs(120),
    )
    .await;
    let request_id = wait_for_approval_request_id_with_timeout(
        &orchestrator,
        session.session_id,
        Duration::from_secs(120),
    )
    .await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{label} approval signal failed: {error}"));

    let events = wait_for_tool_result_with_timeout(
        &orchestrator,
        session.session_id,
        request_id,
        Duration::from_secs(120),
    )
    .await;
    let tool_output = events.iter().find_map(|record| match &record.event {
        Event::ToolResult {
            tool_id,
            success: true,
            output,
            ..
        } if tool_id.0 == request_id => Some(output.to_text()),
        _ => None,
    });
    assert!(
        tool_output
            .as_deref()
            .is_some_and(|output| output.contains(&format!("wrote {relative_path}"))),
        "{label} wrote an unexpected path: {tool_output:?}"
    );

    wait_for_status_with_timeout(
        &orchestrator,
        session.session_id,
        SessionStatus::Completed,
        Duration::from_secs(120),
    )
    .await;
    wait_for_event_text_with_timeout(
        &orchestrator,
        session.session_id,
        &token,
        Duration::from_secs(120),
    )
    .await;
}

async fn wait_for_session_id_file(root: &Path) -> SessionId {
    let deadline = Instant::now() + Duration::from_secs(30);
    let path = root.join("session_id.txt");
    loop {
        if let Ok(contents) = tokio::fs::read_to_string(&path).await {
            let parsed = uuid::Uuid::parse_str(contents.trim()).expect("valid session id");
            return SessionId(parsed);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for helper to write {}.\nstart stdout:\n{}\nstart stderr:\n{}",
            path.display(),
            tokio::fs::read_to_string(root.join("helper-start.stdout.log"))
                .await
                .unwrap_or_default(),
            tokio::fs::read_to_string(root.join("helper-start.stderr.log"))
                .await
                .unwrap_or_default(),
        );
        sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
async fn temporal_orchestrator_runs_workflow_and_unblocks_on_approval() {
    let (_dir, _server, orchestrator) = temporal_test_orchestrator().await;
    let model = MoaConfig::default().general.default_model;
    let tool_id = uuid::Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("uuid");
    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-temporal"),
            user_id: moa_core::UserId::new("u-temporal"),
            platform: moa_core::Platform::Cli,
            model: model.into(),
            initial_message: Some(UserMessage {
                text: "write the file".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    wait_for_status(
        &orchestrator,
        session.session_id,
        SessionStatus::WaitingApproval,
    )
    .await;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id: tool_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .expect("approval signal");

    let events = wait_for_tool_result(&orchestrator, session.session_id, tool_id).await;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await;
    wait_for_event_text(&orchestrator, session.session_id, "temporal-complete").await;
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::ApprovalDecided {
            request_id,
            decision: moa_core::ApprovalDecision::AllowOnce,
            ..
        } if *request_id == tool_id
    )));
    assert!(
        events
            .iter()
            .any(|record| matches!(&record.event, Event::ToolResult { success: true, .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_orchestrator_waits_for_first_message_on_blank_session() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(Arc::new(TemporalEchoProvider {
            model: model.clone(),
            delay: Duration::from_millis(50),
            requests: requests.clone(),
        }))
        .await;
    let harness = TemporalContractHarness::new(&orchestrator, model, Some(requests));
    assert_blank_session_waits_for_first_message(
        &harness,
        "ws-temporal",
        "u-temporal",
        "first real message",
    )
    .await
    .expect("blank session contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_orchestrator_processes_two_sessions_independently() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(Arc::new(TemporalEchoProvider {
            model: model.clone(),
            delay: Duration::from_millis(100),
            requests,
        }))
        .await;
    let harness = TemporalContractHarness::new(&orchestrator, model, None);
    assert_processes_two_sessions_independently(&harness, "left", "right")
        .await
        .expect("two-session contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_orchestrator_processes_multiple_queued_messages_fifo() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(Arc::new(TemporalEchoProvider {
            model: model.clone(),
            delay: Duration::from_millis(200),
            requests: requests.clone(),
        }))
        .await;
    let harness = TemporalContractHarness::new(&orchestrator, model, Some(requests));
    assert_processes_multiple_queued_messages_fifo(&harness, "first", &["second", "third"])
        .await
        .expect("fifo contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_orchestrator_queues_message_while_waiting_for_approval() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let tool_id = uuid::Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("uuid");
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(Arc::new(TemporalToolThenEchoProvider {
            model: model.clone(),
            tool_call: ToolInvocation {
                id: Some(tool_id.to_string()),
                name: "file_write".to_string(),
                input: json!({
                    "path": "approval/queued.txt",
                    "content": "written before queued follow-up"
                }),
            },
            final_text: None,
            requests,
        }))
        .await;
    let harness = TemporalContractHarness::new(&orchestrator, model, None);
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn(&harness, "first", "queued")
        .await
        .expect("approval queue contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
async fn temporal_orchestrator_soft_cancel_stops_after_current_tool_call() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let tool_id = uuid::Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").expect("uuid");
    let (_dir, _server, orchestrator) =
        temporal_test_orchestrator_with_provider(Arc::new(TemporalToolThenEchoProvider {
            model: model.clone(),
            tool_call: ToolInvocation {
                id: Some(tool_id.to_string()),
                name: "bash".to_string(),
                input: json!({
                    "cmd": "sleep 0.35 && printf 'temporal-tool\\n'"
                }),
            },
            final_text: Some("should-not-run".to_string()),
            requests,
        }))
        .await;

    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-cancel"),
            user_id: moa_core::UserId::new("u-cancel"),
            platform: moa_core::Platform::Cli,
            model: model.into(),
            initial_message: Some(UserMessage {
                text: "cancel during tool".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    wait_for_status(
        &orchestrator,
        session.session_id,
        SessionStatus::WaitingApproval,
    )
    .await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id: tool_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .expect("approve");
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await
        .expect("soft cancel");

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await;
    let events = wait_for_tool_result(&orchestrator, session.session_id, tool_id).await;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::ToolResult { .. } | Event::ToolError { .. }
    )));
    assert!(!events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "should-not-run"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_orchestrator_soft_cancel_waiting_for_approval() {
    let model = MoaConfig::default().general.default_model;
    let (_dir, _server, orchestrator) = temporal_test_orchestrator().await;
    let harness = TemporalContractHarness::new(&orchestrator, model, None);
    assert_soft_cancel_waiting_for_approval_cancels_cleanly(&harness, "write the file")
        .await
        .expect("soft cancel contract");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "set MOA_RUN_LIVE_PROVIDER_TESTS=1 to run the live-provider Temporal smoke test"]
async fn temporal_orchestrator_live_anthropic_smoke() {
    if std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let server = TemporalDevServer::start(&dir);
    server.wait_ready().await;

    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.cloud.enabled = true;
    if let Some(hands) = config.cloud.hands.as_mut() {
        hands.default_provider = Some("local".to_string());
    }
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .address = Some(format!("127.0.0.1:{}", server.port));
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .namespace = Some("default".to_string());
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .task_queue = format!("moa-live-{}", uuid::Uuid::now_v7());
    config
        .cloud
        .temporal
        .as_mut()
        .expect("temporal config")
        .api_key_env = Some("ANTHROPIC_API_KEY".to_string());

    let orchestrator = TemporalOrchestrator::from_config(config.clone())
        .await
        .expect("temporal orchestrator");
    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-live"),
            user_id: moa_core::UserId::new("u-live"),
            platform: moa_core::Platform::Cli,
            model: config.general.default_model.into(),
            initial_message: Some(UserMessage {
                text: "What is 2+2? Respond with just the answer.".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await;
    wait_for_event_text(&orchestrator, session.session_id, "4").await;

    let _ = server;
    let _ = dir;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual live provider Temporal orchestrator test"]
async fn temporal_live_providers_complete_tool_approval_roundtrip_when_available() {
    if std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let providers = available_live_providers();
    if providers.is_empty() {
        return;
    }

    for provider in providers {
        run_live_temporal_provider_tool_approval_roundtrip(provider).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal worker restart recovery test"]
async fn temporal_orchestrator_recovers_after_worker_process_restart() {
    build_temporal_helper_binary();

    let dir = tempfile::tempdir().expect("tempdir");
    let server = TemporalDevServer::start(&dir);
    server.wait_ready().await;
    let task_queue = format!("moa-restart-{}", uuid::Uuid::now_v7());

    let mut starter = spawn_temporal_helper("start", dir.path(), server.port, &task_queue, 3000);
    let session_id = wait_for_session_id_file(dir.path()).await;
    sleep(Duration::from_millis(500)).await;
    let _ = starter.kill();
    let _ = starter.wait();

    let mut restarted = spawn_temporal_helper("worker", dir.path(), server.port, &task_queue, 200);
    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    let session_store = create_session_store(&config).await.expect("session store");
    wait_for_store_status(&session_store, session_id, SessionStatus::Completed).await;
    wait_for_store_event_text(&session_store, session_id, "assistant:recover me").await;

    let _ = restarted.kill();
    let _ = restarted.wait();
}
