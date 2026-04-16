#![cfg(feature = "temporal")]

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
    SessionId, SessionSignal, SessionStatus, SessionStore, StopReason, TokenPricing,
    ToolCallFormat, ToolInvocation, UserMessage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::TemporalOrchestrator;
use moa_session::{PostgresSessionStore, testing};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep};

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
                model: self.model.clone(),
                input_tokens: 12,
                output_tokens: 3,
                cached_input_tokens: 0,
                duration_ms: 10,
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
                model: self.model.clone(),
                input_tokens: 12,
                output_tokens: 4,
                cached_input_tokens: 0,
                duration_ms: 10,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

fn mock_capabilities(model: &str, supports_tools: bool) -> moa_core::ModelCapabilities {
    moa_core::ModelCapabilities {
        model_id: model.to_string(),
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
    }
}

fn delayed_text_stream(model: &str, text: String, delay: Duration) -> CompletionStream {
    let response = CompletionResponse {
        text: text.clone(),
        content: vec![CompletionContent::Text(text)],
        stop_reason: StopReason::EndTurn,
        model: model.to_string(),
        input_tokens: 4,
        output_tokens: 2,
        cached_input_tokens: 0,
        duration_ms: delay.as_millis() as u64,
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
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.as_str())
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
    server.wait_ready().await;

    let mut config = MoaConfig::default();
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

    let (session_store, _database_url, _schema_name) = testing::create_isolated_test_store()
        .await
        .expect("session store");
    let session_store = Arc::new(session_store);
    let memory_store = Arc::new(
        FileMemoryStore::from_config(&config)
            .await
            .expect("memory store"),
    );
    let tool_router = Arc::new(
        ToolRouter::from_config(&config, memory_store.clone())
            .await
            .expect("tool router")
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let orchestrator =
        TemporalOrchestrator::new(config, session_store, memory_store, provider, tool_router)
            .await
            .expect("temporal orchestrator");
    (dir, server, orchestrator)
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
    let deadline = Instant::now() + Duration::from_secs(20);
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
                .observe(session_id.clone(), moa_core::ObserveLevel::Normal)
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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let events = orchestrator
            .observe(session_id.clone(), moa_core::ObserveLevel::Normal)
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
            .get_session(session_id.clone())
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
            .get_events(session_id.clone(), EventRange::all())
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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let events = orchestrator
            .observe(session_id.clone(), moa_core::ObserveLevel::Normal)
            .await
            .expect("observe")
            .events;
        if events.iter().any(|record| match &record.event {
            Event::ToolResult {
                tool_id: event_tool_id,
                ..
            } => *event_tool_id == tool_id,
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

async fn wait_for_brain_response_count(
    orchestrator: &TemporalOrchestrator,
    session_id: SessionId,
    expected: usize,
) -> Vec<moa_core::EventRecord> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let events = orchestrator
            .observe(session_id.clone(), moa_core::ObserveLevel::Normal)
            .await
            .expect("observe")
            .events;
        let count = events
            .iter()
            .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
            .count();
        if count >= expected {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "brain response count never reached {expected}"
        );
        sleep(Duration::from_millis(200)).await;
    }
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

fn brain_response_texts(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
async fn temporal_orchestrator_runs_workflow_and_unblocks_on_approval() {
    let (dir, _server, orchestrator) = temporal_test_orchestrator().await;
    let model = MoaConfig::default().general.default_model;
    let tool_id = uuid::Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("uuid");
    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-temporal"),
            user_id: moa_core::UserId::new("u-temporal"),
            platform: moa_core::Platform::Cli,
            model,
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
        session.session_id.clone(),
        SessionStatus::WaitingApproval,
    )
    .await;

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id: tool_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .expect("approval signal");

    let events = wait_for_tool_result(&orchestrator, session.session_id.clone(), tool_id).await;
    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    wait_for_event_text(
        &orchestrator,
        session.session_id.clone(),
        "temporal-complete",
    )
    .await;
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::ApprovalDecided {
            request_id,
            decision: moa_core::ApprovalDecision::AllowOnce,
            ..
        } if *request_id == tool_id
    )));
    let written = std::fs::read_dir(dir.path().join("sandbox"))
        .expect("sandbox root")
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.path().join("approval").join("temporal.txt"))
        })
        .find(|candidate| candidate.exists())
        .expect("written file inside a session sandbox");
    let contents = tokio::fs::read_to_string(&written)
        .await
        .expect("written file should exist");
    assert_eq!(contents, "written by temporal approval test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
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

    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-temporal"),
            user_id: moa_core::UserId::new("u-temporal"),
            platform: moa_core::Platform::Cli,
            model,
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    sleep(Duration::from_millis(400)).await;
    let events = orchestrator
        .observe(session.session_id.clone(), moa_core::ObserveLevel::Normal)
        .await
        .expect("observe")
        .events;
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
    );
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first real message".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
        .expect("queue first message");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    let events = wait_for_brain_response_count(&orchestrator, session.session_id.clone(), 1).await;
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first real message"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
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

    let left = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-left"),
            user_id: moa_core::UserId::new("u-left"),
            platform: moa_core::Platform::Cli,
            model: model.clone(),
            initial_message: Some(UserMessage {
                text: "left".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("left session");
    let right = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-right"),
            user_id: moa_core::UserId::new("u-right"),
            platform: moa_core::Platform::Cli,
            model,
            initial_message: Some(UserMessage {
                text: "right".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("right session");

    wait_for_status(
        &orchestrator,
        left.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    wait_for_status(
        &orchestrator,
        right.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;

    let left_events = wait_for_brain_response_count(&orchestrator, left.session_id, 1).await;
    let right_events = wait_for_brain_response_count(&orchestrator, right.session_id, 1).await;
    assert_eq!(brain_response_texts(&left_events), vec!["assistant:left"]);
    assert_eq!(brain_response_texts(&right_events), vec!["assistant:right"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
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

    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-fifo"),
            user_id: moa_core::UserId::new("u-fifo"),
            platform: moa_core::Platform::Cli,
            model,
            initial_message: Some(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    sleep(Duration::from_millis(40)).await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
        .expect("queue second");
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "third".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
        .expect("queue third");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    let events = wait_for_brain_response_count(&orchestrator, session.session_id, 3).await;
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:second", "assistant:third"]
    );

    let logged = requests.lock().expect("request log lock poisoned").clone();
    assert_eq!(logged.len(), 3);
    assert_eq!(last_user_message(&logged[0].messages), Some("first"));
    assert_eq!(last_user_message(&logged[1].messages), Some("second"));
    assert_eq!(last_user_message(&logged[2].messages), Some("third"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
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

    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-temporal"),
            user_id: moa_core::UserId::new("u-temporal"),
            platform: moa_core::Platform::Cli,
            model,
            initial_message: Some(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::WaitingApproval,
    )
    .await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "queued".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
        .expect("queue follow-up");
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id: tool_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .expect("approve");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    let events = wait_for_brain_response_count(&orchestrator, session.session_id.clone(), 2).await;
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::QueuedMessage { text, .. } if text == "queued"
    )));
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:queued"]
    );
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
                    "cmd": "python3 -c 'import time; time.sleep(0.35); print(\"temporal-tool\")'"
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
            model,
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
        session.session_id.clone(),
        SessionStatus::WaitingApproval,
    )
    .await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id: tool_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await
        .expect("approve");
    sleep(Duration::from_millis(50)).await;
    orchestrator
        .signal(session.session_id.clone(), SessionSignal::SoftCancel)
        .await
        .expect("soft cancel");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Cancelled,
    )
    .await;
    let events = wait_for_tool_result(&orchestrator, session.session_id.clone(), tool_id).await;
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, Event::ToolResult { .. }))
    );
    assert!(!events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "should-not-run"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server integration test"]
async fn temporal_orchestrator_soft_cancel_waiting_for_approval() {
    let model = MoaConfig::default().general.default_model;
    let (_dir, _server, orchestrator) = temporal_test_orchestrator().await;
    let session = orchestrator
        .start_session(moa_core::StartSessionRequest {
            workspace_id: WorkspaceId::new("ws-temporal"),
            user_id: moa_core::UserId::new("u-temporal"),
            platform: moa_core::Platform::Cli,
            model,
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
        session.session_id.clone(),
        SessionStatus::WaitingApproval,
    )
    .await;
    orchestrator
        .signal(session.session_id.clone(), SessionSignal::SoftCancel)
        .await
        .expect("soft cancel");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Cancelled,
    )
    .await;
    let events = orchestrator
        .observe(session.session_id, moa_core::ObserveLevel::Normal)
        .await
        .expect("observe")
        .events;
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::ApprovalDecided { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::ToolResult { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server + Anthropic integration test"]
async fn temporal_orchestrator_live_anthropic_smoke() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let server = TemporalDevServer::start(&dir);
    server.wait_ready().await;

    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
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
            model: config.general.default_model,
            initial_message: Some(UserMessage {
                text: "What is 2+2? Respond with just the answer.".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await
        .expect("start session");

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await;
    wait_for_event_text(&orchestrator, session.session_id, "4").await;

    let _ = server;
    let _ = dir;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual Temporal dev-server restart recovery test"]
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
    let (session_store, _database_url, _schema_name) = testing::create_isolated_test_store()
        .await
        .expect("session store");
    wait_for_store_status(&session_store, session_id.clone(), SessionStatus::Completed).await;
    wait_for_store_event_text(&session_store, session_id, "assistant:recover me").await;

    let _ = restarted.kill();
    let _ = restarted.wait();
}
