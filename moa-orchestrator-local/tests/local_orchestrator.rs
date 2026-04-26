mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ConfidenceLevel, ContextMessage, Event, EventRange, EventType, LLMProvider, LiveEvent,
    MemoryPath, MemoryScope, MemoryStore, MessageRole, MoaConfig, MoaError, PageType, Platform,
    Result, RuntimeEvent, SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal,
    SessionStatus, SessionStore, StartSessionRequest, TokenPricing, TokenUsage, ToolCallFormat,
    ToolOutput, UserId, UserMessage, WikiPage, WorkspaceId,
};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_memory::FileMemoryStore;
use moa_orchestrator_local::LocalOrchestrator;
use moa_providers::ModelRouter;
use moa_session::{PostgresSessionStore, testing};
use support::orchestrator_contract::{
    OrchestratorContractHarness, assert_blank_session_waits_for_first_message,
    assert_processes_multiple_queued_messages_fifo, assert_processes_two_sessions_independently,
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn,
    assert_soft_cancel_waiting_for_approval_cancels_cleanly,
};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Instant, sleep, timeout};
const ASYNC_TEST_DEADLINE: Duration = Duration::from_secs(6);

fn disable_query_rewrite(config: &mut MoaConfig) {
    config.query_rewrite.enabled = false;
}

struct LocalContractHarness<'a> {
    orchestrator: &'a LocalOrchestrator,
    requests: Option<Arc<Mutex<Vec<CompletionRequest>>>>,
}

impl<'a> LocalContractHarness<'a> {
    fn new(
        orchestrator: &'a LocalOrchestrator,
        requests: Option<Arc<Mutex<Vec<CompletionRequest>>>>,
    ) -> Self {
        Self {
            orchestrator,
            requests,
        }
    }
}

#[async_trait]
impl OrchestratorContractHarness for LocalContractHarness<'_> {
    fn harness_name(&self) -> &'static str {
        "local"
    }

    fn default_model(&self) -> moa_core::ModelId {
        moa_core::ModelId::new(self.orchestrator.model())
    }

    fn platform(&self) -> Platform {
        Platform::Desktop
    }

    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle> {
        self.orchestrator.start_session(req).await
    }

    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.orchestrator.signal(session_id, signal).await
    }

    async fn session_status(&self, session_id: SessionId) -> Result<Option<SessionStatus>> {
        self.orchestrator
            .get_session(session_id)
            .await
            .map(|session| Some(session.status))
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<moa_core::EventRecord>> {
        self.orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await
    }

    fn recorded_requests(&self) -> Option<Vec<CompletionRequest>> {
        self.requests
            .as_ref()
            .map(|requests| requests.lock().expect("request log lock poisoned").clone())
    }
}

#[derive(Clone)]
struct MockProvider {
    model: String,
    first_turn_delay: Duration,
}

#[derive(Clone)]
struct SlowStreamingProvider {
    model: String,
    text: String,
    delay: Duration,
}

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}

#[async_trait]
impl LLMProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: false,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let prompt = last_user_message(&request.messages).unwrap_or_default();
        let delay = if prompt.contains("first") {
            self.first_turn_delay
        } else {
            Duration::from_millis(5)
        };
        let model = self.model.clone();
        let prompt_text = prompt.to_string();
        let response = CompletionResponse {
            text: format!("assistant:{prompt_text}"),
            content: vec![CompletionContent::Text(format!("assistant:{prompt_text}"))],
            stop_reason: moa_core::StopReason::EndTurn,
            model: model.into(),
            usage: token_usage(4, 2),
            duration_ms: delay.as_millis() as u64,
            thought_signature: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let completion = tokio::spawn(async move {
            sleep(delay).await;
            let _ = tx
                .send(Ok(CompletionContent::Text(response.text.clone())))
                .await;
            Ok(response)
        });
        Ok(CompletionStream::new(rx, completion))
    }
}

#[async_trait]
impl LLMProvider for SlowStreamingProvider {
    fn name(&self) -> &str {
        "slow-stream"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: false,
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

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        let text = self.text.clone();
        let model = self.model.clone();
        let delay = self.delay;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let completion = tokio::spawn(async move {
            for ch in text.chars() {
                sleep(delay).await;
                if tx
                    .send(Ok(CompletionContent::Text(ch.to_string())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(CompletionResponse {
                text: text.clone(),
                content: text
                    .chars()
                    .map(|ch| CompletionContent::Text(ch.to_string()))
                    .collect(),
                stop_reason: moa_core::StopReason::EndTurn,
                model: model.into(),
                usage: token_usage(4, text.len()),
                duration_ms: (delay.as_millis() as usize * text.len()) as u64,
                thought_signature: None,
            })
        });
        Ok(CompletionStream::new(rx, completion))
    }
}

fn last_user_message(messages: &[ContextMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| {
            message.role == moa_core::MessageRole::User
                && !message.content.starts_with("<system-reminder>")
                && !message.content.starts_with("<memory-reminder>")
        })
        .or_else(|| {
            messages
                .iter()
                .rev()
                .find(|message| message.role == moa_core::MessageRole::User)
        })
        .map(|message| message.content.as_str())
}

async fn test_orchestrator() -> Result<(TempDir, LocalOrchestrator)> {
    test_orchestrator_with_delay(Duration::from_millis(200)).await
}

async fn test_orchestrator_with_delay(delay: Duration) -> Result<(TempDir, LocalOrchestrator)> {
    let provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: MoaConfig::default().general.default_model,
        first_turn_delay: delay,
    });
    test_orchestrator_with_provider(provider).await
}

async fn test_orchestrator_with_provider(
    provider: Arc<dyn LLMProvider>,
) -> Result<(TempDir, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    disable_query_rewrite(&mut config);
    config.memory.auto_bootstrap = false;
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let session_store = timed_test_stage("local:create_test_store", create_test_store()).await?;
    let memory_store = Arc::new(
        timed_test_stage(
            "local:create_memory_store",
            FileMemoryStore::from_config_with_pool(
                &config,
                Arc::new(session_store.pool().clone()),
                session_store.schema_name(),
            ),
        )
        .await?,
    );
    let tool_router = Arc::new(
        timed_test_stage(
            "local:create_tool_router",
            ToolRouter::from_config(&config, memory_store.clone()),
        )
        .await?
        .with_rule_store(session_store.clone())
        .with_session_store(session_store.clone()),
    );
    let orchestrator = timed_test_stage(
        "local:create_orchestrator",
        LocalOrchestrator::new(
            config,
            session_store,
            memory_store,
            Arc::new(ModelRouter::new(provider, None)),
            tool_router,
        ),
    )
    .await?;

    Ok((dir, orchestrator))
}

async fn test_orchestrator_with_config_and_provider(
    config: MoaConfig,
    provider: Arc<dyn LLMProvider>,
) -> Result<LocalOrchestrator> {
    let session_store = create_test_store().await?;
    test_orchestrator_with_config_router_and_store(
        config,
        Arc::new(ModelRouter::new(provider, None)),
        session_store,
    )
    .await
}

async fn test_orchestrator_with_config_provider_and_store(
    config: MoaConfig,
    provider: Arc<dyn LLMProvider>,
    session_store: Arc<PostgresSessionStore>,
) -> Result<LocalOrchestrator> {
    test_orchestrator_with_config_router_and_store(
        config,
        Arc::new(ModelRouter::new(provider, None)),
        session_store,
    )
    .await
}

async fn test_orchestrator_with_config_router_and_store(
    mut config: MoaConfig,
    model_router: Arc<ModelRouter>,
    session_store: Arc<PostgresSessionStore>,
) -> Result<LocalOrchestrator> {
    disable_query_rewrite(&mut config);
    let memory_store = Arc::new(
        timed_test_stage(
            "local:create_memory_store",
            FileMemoryStore::from_config_with_pool(
                &config,
                Arc::new(session_store.pool().clone()),
                session_store.schema_name(),
            ),
        )
        .await?,
    );
    let tool_router = Arc::new(
        timed_test_stage(
            "local:create_tool_router",
            ToolRouter::from_config(&config, memory_store.clone()),
        )
        .await?
        .with_rule_store(session_store.clone())
        .with_session_store(session_store.clone()),
    );
    timed_test_stage(
        "local:create_orchestrator",
        LocalOrchestrator::new(
            config,
            session_store,
            memory_store,
            model_router,
            tool_router,
        ),
    )
    .await
}

fn cwd_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

async fn create_test_store() -> Result<Arc<PostgresSessionStore>> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    Ok(Arc::new(store))
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

struct CurrentDirGuard {
    previous: std::path::PathBuf,
}

impl CurrentDirGuard {
    fn set(path: &std::path::Path) -> Result<Self> {
        let previous =
            std::env::current_dir().map_err(|error| MoaError::ProviderError(error.to_string()))?;
        std::env::set_current_dir(path)
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(Self { previous })
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[derive(Clone)]
struct RequestGuardProvider {
    model: String,
    first_turn_delay: Duration,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for RequestGuardProvider {
    fn name(&self) -> &str {
        "request-guard"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: false,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let last_role = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role != MessageRole::System)
            .map(|message| message.role.clone());

        self.requests
            .lock()
            .expect("request log lock poisoned")
            .push(request.clone());

        if !matches!(last_role, Some(MessageRole::User)) {
            return Err(MoaError::ProviderError(
                "request must end with a user message".to_string(),
            ));
        }

        let prompt = last_user_message(&request.messages).unwrap_or_default();
        let delay = if prompt.contains("first") {
            self.first_turn_delay
        } else {
            Duration::from_millis(5)
        };
        let model = self.model.clone();
        let prompt_text = prompt.to_string();
        let response = CompletionResponse {
            text: format!("assistant:{prompt_text}"),
            content: vec![CompletionContent::Text(format!("assistant:{prompt_text}"))],
            stop_reason: moa_core::StopReason::EndTurn,
            model: model.into(),
            usage: token_usage(4, 2),
            duration_ms: delay.as_millis() as u64,
            thought_signature: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let completion = tokio::spawn(async move {
            sleep(delay).await;
            let _ = tx
                .send(Ok(CompletionContent::Text(response.text.clone())))
                .await;
            Ok(response)
        });
        Ok(CompletionStream::new(rx, completion))
    }
}

#[derive(Clone)]
struct ToolCancelProvider {
    model: String,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for ToolCancelProvider {
    fn name(&self) -> &str {
        "tool-cancel"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: moa_core::ToolInvocation {
                        id: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()),
                        name: "bash".to_string(),
                        input: serde_json::json!({
                            "cmd": "sleep 0.35 && printf 'cancelled-tool\\n'"
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            CompletionResponse {
                text: "should-not-run".to_string(),
                content: vec![CompletionContent::Text("should-not-run".to_string())],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Clone)]
struct ToolThenEchoProvider {
    model: String,
    first_tool_cmd: String,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for ToolThenEchoProvider {
    fn name(&self) -> &str {
        "tool-then-echo"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: moa_core::ToolInvocation {
                        id: Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()),
                        name: "bash".to_string(),
                        input: serde_json::json!({
                            "cmd": self.first_tool_cmd,
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            let prompt = last_user_message(&request.messages).unwrap_or_default();
            CompletionResponse {
                text: format!("assistant:{prompt}"),
                content: vec![CompletionContent::Text(format!("assistant:{prompt}"))],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Clone)]
struct RepeatingToolTurnProvider {
    model: String,
    search_pattern: String,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for RepeatingToolTurnProvider {
    fn name(&self) -> &str {
        "repeating-tool-turn"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.len().is_multiple_of(2) {
            let tool_call_id = format!("00000000-0000-0000-0000-{:012}", requests.len() + 1);
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: moa_core::ToolInvocation {
                        id: Some(tool_call_id),
                        name: "file_search".to_string(),
                        input: serde_json::json!({
                            "pattern": self.search_pattern,
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            let prompt = last_user_message(&request.messages).unwrap_or_default();
            CompletionResponse {
                text: format!("assistant:{prompt}"),
                content: vec![CompletionContent::Text(format!("assistant:{prompt}"))],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Clone)]
struct FileWriteApprovalProvider {
    model: String,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl LLMProvider for FileWriteApprovalProvider {
    fn name(&self) -> &str {
        "file-write-approval"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
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

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: moa_core::ToolInvocation {
                        id: Some("cccccccc-cccc-cccc-cccc-cccccccccccc".to_string()),
                        name: "file_write".to_string(),
                        input: serde_json::json!({
                            "path": "docs/approval-check.md",
                            "content": "approved via orchestrator\n",
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        } else {
            CompletionResponse {
                text: "done".to_string(),
                content: vec![CompletionContent::Text("done".to_string())],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone().into(),
                usage: token_usage(8, 4),
                duration_ms: 10,
                thought_signature: None,
            }
        };
        requests.push(request);
        Ok(CompletionStream::from_response(response))
    }
}

async fn start_session(orchestrator: &LocalOrchestrator) -> Result<SessionHandle> {
    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await
}

async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<()> {
    wait_for_status_with_timeout(orchestrator, session_id, expected, ASYNC_TEST_DEADLINE).await
}

async fn wait_for_status_with_timeout(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let session = orchestrator.get_session(session_id).await?;
        if session.status == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(moa_core::MoaError::ProviderError(format!(
                "timed out waiting for status {:?}",
                expected
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_brain_response_count_with_timeout(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: usize,
    timeout: Duration,
) -> Result<Vec<moa_core::EventRecord>> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        let brain_response_count = events
            .iter()
            .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
            .count();
        if brain_response_count == expected {
            return Ok(events);
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for {expected} brain responses"
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_approval_request(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
) -> Result<uuid::Uuid> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        if let Some(request_id) = events.iter().find_map(|record| match record.event {
            Event::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        }) {
            return Ok(request_id);
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(
                "timed out waiting for approval request".to_string(),
            ));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_approval_event(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
) -> Result<Event> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        if let Some(event) = events.iter().find_map(|record| match &record.event {
            Event::ApprovalRequested { .. } => Some(record.event.clone()),
            _ => None,
        }) {
            return Ok(event);
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(
                "timed out waiting for approval event".to_string(),
            ));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn collect_runtime_events_until<P>(
    runtime_rx: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    predicate: P,
) -> Result<Vec<RuntimeEvent>>
where
    P: Fn(&RuntimeEvent) -> bool,
{
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MoaError::ProviderError(
                "timed out waiting for runtime events".to_string(),
            ));
        }

        let event = tokio::time::timeout(remaining, runtime_rx.recv())
            .await
            .map_err(|_| {
                MoaError::ProviderError("timed out waiting for runtime event".to_string())
            })?
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let should_stop = predicate(&event);
        events.push(event);
        if should_stop {
            return Ok(events);
        }
    }
}

async fn wait_for_pending_signal_count(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: usize,
) -> Result<()> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let pending = orchestrator
            .session_store()
            .get_pending_signals(session_id)
            .await?;
        if pending.len() == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for {expected} pending signals"
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_tool_result_count(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: usize,
) -> Result<()> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        if tool_result_texts(&events).len() == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for {expected} tool results"
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_tool_call_count(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: usize,
) -> Result<()> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        let tool_call_count = events
            .iter()
            .filter(|record| matches!(record.event, Event::ToolCall { .. }))
            .count();
        if tool_call_count == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for {expected} tool calls"
            )));
        }
        sleep(Duration::from_millis(20)).await;
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

fn tool_result_texts(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolResult { output, .. } => Some(output.to_text()),
            _ => None,
        })
        .collect()
}

fn warning_messages(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn event_labels(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .map(|record| match &record.event {
            Event::SessionCreated { .. } => "SessionCreated".to_string(),
            Event::SessionStatusChanged { from, to } => {
                format!("SessionStatusChanged({from:?}->{to:?})")
            }
            Event::SessionCompleted { .. } => "SessionCompleted".to_string(),
            Event::SegmentStarted { segment_index, .. } => {
                format!("SegmentStarted({segment_index})")
            }
            Event::SegmentCompleted { segment_index, .. } => {
                format!("SegmentCompleted({segment_index})")
            }
            Event::UserMessage { text, .. } => format!("UserMessage({text})"),
            Event::QueuedMessage { text, .. } => format!("QueuedMessage({text})"),
            Event::BrainThinking { .. } => "BrainThinking".to_string(),
            Event::BrainResponse { text, .. } => format!("BrainResponse({text})"),
            Event::ToolCall { tool_name, .. } => format!("ToolCall({tool_name})"),
            Event::ToolResult { .. } => "ToolResult".to_string(),
            Event::ToolError {
                tool_name, error, ..
            } => {
                format!("ToolError({tool_name}: {error})")
            }
            Event::ApprovalRequested { tool_name, .. } => {
                format!("ApprovalRequested({tool_name})")
            }
            Event::ApprovalDecided { .. } => "ApprovalDecided".to_string(),
            Event::MemoryRead { path, .. } => format!("MemoryRead({path})"),
            Event::MemoryWrite { path, .. } => format!("MemoryWrite({path})"),
            Event::HandProvisioned { hand_id, .. } => format!("HandProvisioned({hand_id})"),
            Event::HandDestroyed { hand_id, .. } => format!("HandDestroyed({hand_id})"),
            Event::HandError { hand_id, error } => format!("HandError({hand_id}: {error})"),
            Event::Checkpoint { .. } => "Checkpoint".to_string(),
            Event::Error { message, .. } => format!("Error({message})"),
            Event::Warning { message } => format!("Warning({message})"),
            Event::MemoryIngest { source_path, .. } => format!("MemoryIngest({source_path})"),
            Event::CacheReport { .. } => "CacheReport".to_string(),
        })
        .collect()
}

#[derive(Clone)]
struct PanicProvider {
    model: String,
}

#[async_trait]
impl LLMProvider for PanicProvider {
    fn name(&self) -> &str {
        "panic-provider"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: false,
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

    async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
        panic!("panic-provider boom");
    }
}

#[derive(Clone)]
struct DestroyTrackingHandProvider {
    provisioned: Arc<AtomicUsize>,
    destroyed: Arc<AtomicUsize>,
}

#[async_trait]
impl moa_core::HandProvider for DestroyTrackingHandProvider {
    fn provider_name(&self) -> &str {
        "tracked"
    }

    async fn provision(&self, _spec: moa_core::HandSpec) -> Result<moa_core::HandHandle> {
        let id = self.provisioned.fetch_add(1, Ordering::SeqCst);
        Ok(moa_core::HandHandle::local(std::path::PathBuf::from(
            format!("/tmp/tracked-hand-{id}"),
        )))
    }

    async fn execute(
        &self,
        _handle: &moa_core::HandHandle,
        _tool: &str,
        _input: &str,
    ) -> Result<ToolOutput> {
        Ok(ToolOutput::text(
            "tracked-hand-output",
            Duration::from_millis(5),
        ))
    }

    async fn status(&self, _handle: &moa_core::HandHandle) -> Result<moa_core::HandStatus> {
        Ok(moa_core::HandStatus::Running)
    }

    async fn pause(&self, _handle: &moa_core::HandHandle) -> Result<()> {
        Ok(())
    }

    async fn resume(&self, _handle: &moa_core::HandHandle) -> Result<()> {
        Ok(())
    }

    async fn destroy(&self, _handle: &moa_core::HandHandle) -> Result<()> {
        self.destroyed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn starts_two_sessions_and_processes_both() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let harness = LocalContractHarness::new(&orchestrator, None);
    assert_processes_two_sessions_independently(&harness, "left", "right").await
}

#[tokio::test]
async fn blank_session_waits_for_first_message() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: MoaConfig::default().general.default_model,
        first_turn_delay: Duration::from_millis(50),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, Some(requests));
    assert_blank_session_waits_for_first_message(
        &harness,
        "ws-blank-local",
        "u-blank-local",
        "first real message",
    )
    .await
}

#[tokio::test]
async fn soft_cancel_marks_session_cancelled() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(250)).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::SessionStatusChanged {
            to: SessionStatus::Cancelled,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::Error { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn hard_cancel_aborts_stream_and_emits_cancelled_status() -> Result<()> {
    let provider: Arc<dyn LLMProvider> = Arc::new(SlowStreamingProvider {
        model: MoaConfig::default().general.default_model,
        text: "streaming response that should be interrupted".to_string(),
        delay: Duration::from_millis(40),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local runtime stream should exist");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "interrupt me".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let mut delta_text = String::new();
    let cancel_deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    while delta_text.len() < 3 && Instant::now() < cancel_deadline {
        if let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(250), runtime.recv()).await
            && let RuntimeEvent::AssistantDelta(ch) = event
        {
            delta_text.push(ch);
        }
    }
    assert!(
        delta_text.len() >= 3,
        "expected to receive streamed deltas before cancelling"
    );

    orchestrator
        .signal(session.session_id, SessionSignal::HardCancel)
        .await?;

    let finish_deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    let mut saw_turn_completed = false;
    while Instant::now() < finish_deadline {
        match tokio::time::timeout(Duration::from_millis(250), runtime.recv()).await {
            Ok(Ok(RuntimeEvent::AssistantDelta(ch))) => delta_text.push(ch),
            Ok(Ok(RuntimeEvent::TurnCompleted)) => {
                saw_turn_completed = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    assert!(saw_turn_completed);
    assert!(delta_text.len() < "streaming response that should be interrupted".len());

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::SessionStatusChanged {
            from: SessionStatus::Running,
            to: SessionStatus::Cancelled,
        }
    )));
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::Error { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn soft_cancel_stops_after_current_tool_call() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolCancelProvider {
        model,
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "cancel during tool".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(
        events.iter().any(|record| matches!(
            record.event,
            Event::ToolResult { .. } | Event::ToolError { .. }
        )),
        "expected ToolResult or ToolError in events: {:?}",
        event_labels(&events)
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
    );
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 1);
    Ok(())
}

#[tokio::test]
async fn queued_message_is_processed_after_current_turn() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(200)).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_pending_signal_count(&orchestrator, session.session_id, 1).await?;
    let pending = orchestrator
        .session_store()
        .get_pending_signals(session.session_id)
        .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].user_message()?.text, "second");

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    wait_for_pending_signal_count(&orchestrator, session.session_id, 0).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;

    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::QueuedMessage { ref text, .. } if text == "second"
    )));
    let responses = events
        .iter()
        .filter(|record| record.event_type == EventType::BrainResponse)
        .count();
    assert!(responses >= 2);
    Ok(())
}

#[tokio::test]
async fn resume_session_recovers_unresolved_pending_prompt() -> Result<()> {
    let (dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "initial".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let pending = moa_core::PendingSignal::queue_message(
        session.session_id,
        UserMessage {
            text: "recovered follow-up".to_string(),
            attachments: Vec::new(),
        },
    )?;
    orchestrator
        .session_store()
        .store_pending_signal(session.session_id, pending)
        .await?;

    let reopened_store = orchestrator.session_store();
    drop(orchestrator);

    let mut reopened_config = MoaConfig::default();
    disable_query_rewrite(&mut reopened_config);
    reopened_config.local.memory_dir = dir.path().join("memory").display().to_string();
    reopened_config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    let reopened_memory = Arc::new(
        FileMemoryStore::from_config_with_pool(
            &reopened_config,
            Arc::new(reopened_store.pool().clone()),
            reopened_store.schema_name(),
        )
        .await?,
    );
    let reopened_provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: reopened_config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
    });
    let reopened_router = Arc::new(
        ToolRouter::from_config(&reopened_config, reopened_memory.clone())
            .await?
            .with_rule_store(reopened_store.clone())
            .with_session_store(reopened_store.clone()),
    );
    let reopened = LocalOrchestrator::new(
        reopened_config,
        reopened_store,
        reopened_memory,
        Arc::new(ModelRouter::new(reopened_provider, None)),
        reopened_router,
    )
    .await?;

    reopened.resume_session(session.session_id).await?;
    wait_for_status(&reopened, session.session_id, SessionStatus::Completed).await?;
    wait_for_pending_signal_count(&reopened, session.session_id, 0).await?;

    let events = reopened
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::QueuedMessage { ref text, .. } if text == "recovered follow-up"
    )));
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text.contains("recovered follow-up")
    )));
    Ok(())
}

#[tokio::test]
async fn resume_session_processes_user_message_before_trailing_status_event() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(10)).await?;
    let session_id = SessionId::new();
    let now = chrono::Utc::now();
    orchestrator
        .session_store()
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: moa_core::ModelId::new(orchestrator.model()),
            status: SessionStatus::Running,
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionCreated {
                workspace_id: moa_core::WorkspaceId::new("workspace"),
                user_id: moa_core::UserId::new("user"),
                model: moa_core::ModelId::new(orchestrator.model()),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "recover trailing status".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionStatusChanged {
                from: SessionStatus::Created,
                to: SessionStatus::Running,
            },
        )
        .await?;

    orchestrator.resume_session(session_id).await?;
    wait_for_status(&orchestrator, session_id, SessionStatus::Completed).await?;

    let events = orchestrator
        .session_store()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text == "assistant:recover trailing status"
    )));
    Ok(())
}

#[tokio::test]
async fn approval_requested_event_persists_full_prompt_details() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(FileWriteApprovalProvider { model, requests });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "write approval test".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let event = wait_for_approval_event(&orchestrator, session.session_id).await?;
    match event {
        Event::ApprovalRequested {
            tool_name,
            input_summary,
            risk_level,
            prompt,
            ..
        } => {
            assert_eq!(tool_name, "file_write");
            assert!(input_summary.contains("docs/approval-check.md"));
            assert_eq!(risk_level, moa_core::RiskLevel::Medium);
            assert_eq!(prompt.request.tool_name, "file_write");
            assert_eq!(prompt.parameters.len(), 2);
            assert_eq!(prompt.file_diffs.len(), 1);
            assert_eq!(prompt.file_diffs[0].path, "docs/approval-check.md");
            assert!(
                prompt.file_diffs[0].before.is_empty()
                    || prompt.file_diffs[0].before == "approved via orchestrator\n"
            );
            assert_eq!(
                prompt.file_diffs[0].after,
                "approved via orchestrator\n".to_string()
            );
            assert!(prompt.pattern.contains("docs/approval-check.md"));
        }
        other => panic!("expected approval requested event, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn observe_runtime_streams_assistant_text_and_turn_completion() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(40)).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "stream this".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let runtime_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;

    let delta_text = runtime_events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantDelta(ch) => Some(*ch),
            _ => None,
        })
        .collect::<String>();
    let finished_text = runtime_events.iter().find_map(|event| match event {
        RuntimeEvent::AssistantFinished { text, .. } => Some(text.clone()),
        _ => None,
    });

    assert!(
        runtime_events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::AssistantStarted))
    );
    assert_eq!(delta_text, "assistant:stream this");
    assert_eq!(finished_text, Some("assistant:stream this".to_string()));
    assert!(matches!(
        runtime_events.last(),
        Some(RuntimeEvent::TurnCompleted)
    ));
    Ok(())
}

#[tokio::test]
async fn observe_runtime_reports_tool_updates_and_approval_flow() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(FileWriteApprovalProvider { model, requests });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "write approval test".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let pre_approval_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::ApprovalRequested(_))
    })
    .await?;
    let approval_prompt = pre_approval_events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ApprovalRequested(prompt) => Some(prompt.clone()),
            _ => None,
        })
        .expect("approval prompt missing from runtime stream");

    assert!(pre_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolUpdate(update)
            if update.tool_name == "file_write"
                && matches!(update.status, moa_core::ToolCardStatus::WaitingApproval)
    )));

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id: approval_prompt.request.request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    let post_approval_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;

    assert!(post_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolUpdate(update)
            if update.tool_name == "file_write"
                && matches!(update.status, moa_core::ToolCardStatus::Succeeded)
    )));
    assert!(post_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantFinished { text, .. } if text == "done"
    )));
    assert!(matches!(
        post_approval_events.last(),
        Some(RuntimeEvent::TurnCompleted)
    ));
    Ok(())
}

#[tokio::test]
async fn resumed_session_observe_runtime_streams_from_persisted_events() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(150)).await?;
    let session_id = SessionId::new();
    let now = chrono::Utc::now();
    orchestrator
        .session_store()
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: moa_core::ModelId::new(orchestrator.model()),
            status: SessionStatus::Created,
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionCreated {
                workspace_id: moa_core::WorkspaceId::new("workspace"),
                user_id: moa_core::UserId::new("user"),
                model: moa_core::ModelId::new(orchestrator.model()),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "resume me".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    orchestrator.resume_session(session_id).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

    let runtime_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;

    let delta_text = runtime_events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantDelta(ch) => Some(*ch),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(delta_text, "assistant:resume me");
    assert!(runtime_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantFinished { text, .. } if text == "assistant:resume me"
    )));
    Ok(())
}

#[tokio::test]
async fn queued_follow_up_request_ends_with_user_message() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(200),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let logged = requests.lock().expect("request log lock poisoned").clone();
    assert!(logged.len() >= 2);
    assert_eq!(
        logged[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role != MessageRole::System)
            .expect("second request should contain a non-system message")
            .role,
        MessageRole::User
    );
    assert_eq!(last_user_message(&logged[1].messages), Some("second"));

    Ok(())
}

#[tokio::test]
async fn compaction_uses_auxiliary_model_router_tier() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.general.default_model = "claude-sonnet-4-6".to_string();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.models.auxiliary = Some("claude-haiku-4-5".to_string());
    config.compaction.event_threshold = 1;
    config.compaction.token_ratio_threshold = 0.0;
    config.compaction.recent_turns_verbatim = 1;

    let main_provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
    });
    let auxiliary_provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: config
            .models
            .auxiliary
            .clone()
            .expect("auxiliary model configured"),
        first_turn_delay: Duration::from_millis(5),
    });
    let store = create_test_store().await?;
    let orchestrator = test_orchestrator_with_config_router_and_store(
        config,
        Arc::new(ModelRouter::new(main_provider, Some(auxiliary_provider))),
        store,
    )
    .await?;
    let session = start_session(&orchestrator).await?;

    for text in ["first", "second", "third"] {
        orchestrator
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: text.to_string(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
        wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    }

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    let main_models: Vec<_> = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse {
                model, model_tier, ..
            } => Some((model.as_str().to_string(), *model_tier)),
            _ => None,
        })
        .collect();
    let checkpoint_models: Vec<_> = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::Checkpoint {
                model, model_tier, ..
            } => Some((model.as_str().to_string(), *model_tier)),
            _ => None,
        })
        .collect();

    assert!(!main_models.is_empty());
    assert!(main_models.iter().all(|(model, tier)| {
        model == "claude-sonnet-4-6" && *tier == moa_core::ModelTier::Main
    }));
    assert!(!checkpoint_models.is_empty());
    assert!(checkpoint_models.iter().all(|(model, tier)| {
        model == "claude-haiku-4-5" && *tier == moa_core::ModelTier::Auxiliary
    }));

    Ok(())
}

#[tokio::test]
async fn multiple_queued_messages_are_processed_fifo_one_turn_at_a_time() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(200),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, Some(requests));
    assert_processes_multiple_queued_messages_fifo(&harness, "first", &["second", "third"]).await
}

#[tokio::test]
async fn burst_of_queued_messages_preserves_fifo_under_hot_session_pressure() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(150),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let queued = (0..10)
        .map(|index| format!("burst-{index:02}"))
        .collect::<Vec<_>>();
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(40)).await;
    for message in &queued {
        orchestrator
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: message.clone(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
    }

    wait_for_status_with_timeout(
        &orchestrator,
        session.session_id,
        SessionStatus::Completed,
        Duration::from_secs(60),
    )
    .await?;
    let events = wait_for_brain_response_count_with_timeout(
        &orchestrator,
        session.session_id,
        queued.len() + 1,
        Duration::from_secs(60),
    )
    .await?;
    let expected = std::iter::once("first".to_string())
        .chain(queued.iter().cloned())
        .map(|prompt| format!("assistant:{prompt}"))
        .collect::<Vec<_>>();
    assert_eq!(brain_response_texts(&events), expected);

    let requests = requests
        .lock()
        .expect("request log mutex should not be poisoned")
        .clone();
    let ordered_prompts = requests
        .iter()
        .filter_map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == moa_core::MessageRole::User)
                .map(|message| message.content.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_prompts,
        expected
            .iter()
            .map(|text| text.trim_start_matches("assistant:").to_string())
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[tokio::test]
async fn queued_message_waiting_for_approval_runs_after_allowed_turn() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "python3 -c 'print(\"tool-complete\")'".to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, None);
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn(&harness, "first", "queued")
        .await
}

#[tokio::test]
async fn soft_cancel_waiting_for_approval_cancels_cleanly() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: MoaConfig::default().general.default_model,
        first_tool_cmd: "printf 'awaiting approval'".to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, None);
    assert_soft_cancel_waiting_for_approval_cancels_cleanly(&harness, "first").await
}

#[tokio::test]
async fn denied_tool_preserves_queued_follow_up() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "python3 -c 'print(\"tool-complete\")'".to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "after-deny".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::Deny { reason: None },
            },
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;

    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, Event::ToolError { .. }))
    );
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:after-deny"]
    );

    Ok(())
}

#[tokio::test]
async fn resume_cancelled_session_waits_for_new_input() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().general.default_model;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "sleep 0.35 && printf 'tool-finished\\n'".to_string(),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "cancel during tool".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;
    wait_for_tool_call_count(&orchestrator, session.session_id, 1).await?;
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;

    orchestrator.resume_session(session.session_id).await?;
    sleep(Duration::from_millis(450)).await;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(brain_response_texts(&events).is_empty());
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 1);

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "after resume".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:after resume"]
    );
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 2);

    Ok(())
}

#[tokio::test]
async fn panicking_provider_marks_session_failed() -> Result<()> {
    let provider: Arc<dyn LLMProvider> = Arc::new(PanicProvider {
        model: MoaConfig::default().general.default_model,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "panic please".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Failed).await?;

    Ok(())
}

#[tokio::test]
async fn completed_tool_turn_destroys_cached_hand() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    disable_query_rewrite(&mut config);
    config.memory.auto_bootstrap = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let session_store = create_test_store().await?;
    let memory_store = Arc::new(
        FileMemoryStore::from_config_with_pool(
            &config,
            Arc::new(session_store.pool().clone()),
            session_store.schema_name(),
        )
        .await?,
    );
    let provider = Arc::new(DestroyTrackingHandProvider {
        provisioned: Arc::new(AtomicUsize::new(0)),
        destroyed: Arc::new(AtomicUsize::new(0)),
    });
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "tracked".to_string(),
        provider.clone() as Arc<dyn moa_core::HandProvider>,
    );
    let mut registry = ToolRegistry::default_local();
    registry.retarget_hand_tools("tracked", moa_core::SandboxTier::Local);
    let tool_router = Arc::new(
        ToolRouter::new(registry, memory_store.clone(), providers)
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let llm_provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: config.general.default_model.clone(),
        first_tool_cmd: "echo tracked".to_string(),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store,
        memory_store,
        Arc::new(ModelRouter::new(llm_provider, None)),
        tool_router,
    )
    .await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "run tracked tool".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    assert_eq!(provider.provisioned.load(Ordering::SeqCst), 1);
    assert_eq!(provider.destroyed.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn observe_stream_receives_events_in_order() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;
    let mut stream = orchestrator
        .observe(session.session_id, moa_core::ObserveLevel::Normal)
        .await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "observe".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let first = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing first observed event".to_string())
    })?;
    let second = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing second observed event".to_string())
    })?;
    let third = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing third observed event".to_string())
    })?;
    let fourth = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing fourth observed event".to_string())
    })?;
    let fifth = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing fifth observed event".to_string())
    })?;

    let LiveEvent::Event(first) = first else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for first observed event".to_string(),
        ));
    };
    let LiveEvent::Event(second) = second else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for second observed event".to_string(),
        ));
    };
    let LiveEvent::Event(third) = third else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for third observed event".to_string(),
        ));
    };
    let LiveEvent::Event(fourth) = fourth else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for fourth observed event".to_string(),
        ));
    };
    let LiveEvent::Event(fifth) = fifth else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for fifth observed event".to_string(),
        ));
    };

    assert_eq!(first.sequence_num, 0);
    assert_eq!(first.event_type, EventType::SessionCreated);
    assert_eq!(second.sequence_num, 1);
    assert_eq!(second.event_type, EventType::UserMessage);
    assert_eq!(third.sequence_num, 2);
    assert_eq!(third.event_type, EventType::SessionStatusChanged);
    assert_eq!(fourth.sequence_num, 3);
    assert_eq!(fourth.event_type, EventType::CacheReport);
    assert_eq!(fifth.sequence_num, 4);
    assert_eq!(fifth.event_type, EventType::BrainResponse);
    Ok(())
}

#[tokio::test]
async fn observe_uses_postgres_listener_for_remote_active_sessions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(20),
    });
    let session_store = create_test_store().await?;
    let observer = test_orchestrator_with_config_provider_and_store(
        config.clone(),
        provider,
        session_store.clone(),
    )
    .await?;

    let now = Utc::now();
    let session_id = SessionId::new();
    session_store
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            status: SessionStatus::Running,
            platform: Platform::Desktop,
            model: moa_core::ModelId::new(observer.model()),
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;

    let mut stream = observer
        .observe(session_id, moa_core::ObserveLevel::Normal)
        .await?;

    session_store
        .emit_event(
            session_id,
            Event::Warning {
                message: "remote-listener".to_string(),
            },
        )
        .await?;

    let observed = tokio::time::timeout(ASYNC_TEST_DEADLINE, stream.next())
        .await
        .map_err(|_| {
            MoaError::ProviderError(
                "timed out waiting for Postgres LISTEN-backed observation".to_string(),
            )
        })?
        .transpose()?
        .ok_or_else(|| {
            MoaError::ProviderError("missing observed event from Postgres listener".to_string())
        })?;

    let LiveEvent::Event(record) = observed else {
        return Err(MoaError::ProviderError(
            "expected concrete event from Postgres listener path".to_string(),
        ));
    };
    assert!(matches!(
        record.event,
        Event::Warning { ref message } if message == "remote-listener"
    ));

    Ok(())
}

#[tokio::test]
async fn list_sessions_includes_active_session() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;
    let sessions = orchestrator.list_sessions(SessionFilter::default()).await?;
    assert!(
        sessions
            .iter()
            .any(|summary| summary.session_id == session.session_id)
    );
    Ok(())
}

#[tokio::test]
async fn memory_maintenance_runs_due_workspace_consolidation() -> Result<()> {
    let (dir, orchestrator) = test_orchestrator().await?;
    let memory_store = FileMemoryStore::new(dir.path()).await?;
    let session_store = orchestrator.session_store();
    let workspace_id = WorkspaceId::new("ws1");
    let user_id = UserId::new("u1");
    let now = chrono::Utc::now();

    memory_store
        .write_page(
            &MemoryScope::Workspace {
                workspace_id: workspace_id.clone(),
            },
            &"topics/architecture.md".into(),
            WikiPage {
                path: None,
                title: "Architecture".to_string(),
                page_type: PageType::Topic,
                content: "# Architecture\n\nRefresh tokens rotate today.\n".to_string(),
                created: now,
                updated: now - chrono::Duration::days(40),
                confidence: ConfidenceLevel::High,
                related: Vec::new(),
                sources: Vec::new(),
                tags: Vec::new(),
                auto_generated: false,
                last_referenced: now - chrono::Duration::days(40),
                reference_count: 0,
                metadata: std::collections::HashMap::new(),
            },
        )
        .await?;

    for index in 0..3 {
        session_store
            .create_session(SessionMeta {
                id: SessionId::new(),
                workspace_id: workspace_id.clone(),
                user_id: user_id.clone(),
                title: Some(format!("finished-{index}")),
                status: SessionStatus::Completed,
                platform: Platform::Cli,
                model: moa_core::ModelId::new("test-model"),
                created_at: now,
                updated_at: now,
                completed_at: Some(now),
                ..SessionMeta::default()
            })
            .await?;
    }

    let reports = orchestrator.run_memory_maintenance_once().await?;

    assert_eq!(reports.len(), 1);
    assert!(reports[0].relative_dates_normalized >= 1);
    let architecture = memory_store
        .read_page(
            &MemoryScope::Workspace { workspace_id },
            &"topics/architecture.md".into(),
        )
        .await?;
    assert!(architecture.content.contains("20"));

    Ok(())
}

#[tokio::test]
async fn memory_maintenance_skips_when_threshold_or_cooldown_not_met() -> Result<()> {
    let (dir, orchestrator) = test_orchestrator().await?;
    let memory_store = FileMemoryStore::new(dir.path()).await?;
    let session_store = orchestrator.session_store();
    let workspace_id = WorkspaceId::new("ws1");
    let user_id = UserId::new("u1");
    let now = chrono::Utc::now();
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };

    memory_store
        .write_page(
            &scope,
            &"topics/architecture.md".into(),
            WikiPage {
                path: None,
                title: "Architecture".to_string(),
                page_type: PageType::Topic,
                content: "# Architecture\n\nRefresh tokens rotate today.\n".to_string(),
                created: now,
                updated: now - chrono::Duration::days(40),
                confidence: ConfidenceLevel::High,
                related: Vec::new(),
                sources: Vec::new(),
                tags: Vec::new(),
                auto_generated: false,
                last_referenced: now - chrono::Duration::days(40),
                reference_count: 0,
                metadata: std::collections::HashMap::new(),
            },
        )
        .await?;

    for index in 0..2 {
        session_store
            .create_session(SessionMeta {
                id: SessionId::new(),
                workspace_id: workspace_id.clone(),
                user_id: user_id.clone(),
                title: Some(format!("finished-{index}")),
                status: SessionStatus::Completed,
                platform: Platform::Cli,
                model: moa_core::ModelId::new("test-model"),
                created_at: now,
                updated_at: now,
                completed_at: Some(now),
                ..SessionMeta::default()
            })
            .await?;
    }

    let first = orchestrator.run_memory_maintenance_once().await?;
    assert!(first.is_empty());

    session_store
        .create_session(SessionMeta {
            id: SessionId::new(),
            workspace_id: workspace_id.clone(),
            user_id,
            title: Some("finished-2".to_string()),
            status: SessionStatus::Completed,
            platform: Platform::Cli,
            model: moa_core::ModelId::new("test-model"),
            created_at: now,
            updated_at: now,
            completed_at: Some(now),
            ..SessionMeta::default()
        })
        .await?;

    let second = orchestrator.run_memory_maintenance_once().await?;
    assert_eq!(second.len(), 1);

    let third = orchestrator.run_memory_maintenance_once().await?;
    assert!(third.is_empty());

    Ok(())
}

#[tokio::test]
async fn workspace_memory_bootstrap_copies_contributing_file_without_provider_call() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("CONTRIBUTING.md"),
        "# Project Agent Instructions\n\nUse bootmarkeralpha when describing this project.\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = true;
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;

    let index = orchestrator
        .memory_store()
        .get_index(&MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace"),
        })
        .await?;
    assert!(index.contains("Project instructions loaded from `CONTRIBUTING.md`"));
    let project = orchestrator
        .memory_store()
        .read_page(
            &MemoryScope::Workspace {
                workspace_id: WorkspaceId::new("workspace"),
            },
            &"topics/project.md".into(),
        )
        .await?;
    assert!(project.content.contains("Use bootmarkeralpha"));
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);

    let sentinel = base
        .path()
        .join("workspaces")
        .join("workspace")
        .join("memory")
        .join("_bootstrap.json");
    assert!(tokio::fs::try_exists(&sentinel).await?);
    Ok(())
}

#[tokio::test]
async fn workspace_memory_bootstrap_informs_first_turn_from_instruction_file() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("CONTRIBUTING.md"),
        "# Project Agent Instructions\n\nbootmarkeralpha is the canonical bootstrap marker.\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = true;
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    let session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: Some(UserMessage {
                text: "What is bootmarkeralpha?".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let requests = requests.lock().expect("request log lock poisoned");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| { message.content.contains("bootmarkeralpha") })
    );
    Ok(())
}

#[tokio::test]
async fn workspace_memory_bootstrap_sentinel_prevents_rerun_until_deleted() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("CONTRIBUTING.md"),
        "# Project Agent Instructions\n\nversion-one\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    config.memory.auto_bootstrap = true;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;
    let project_path: MemoryPath = "topics/project.md".into();
    let project = orchestrator
        .memory_store()
        .read_page(
            &MemoryScope::Workspace {
                workspace_id: WorkspaceId::new("workspace"),
            },
            &project_path,
        )
        .await?;
    assert!(project.content.contains("version-one"));

    let sentinel = base
        .path()
        .join("workspaces")
        .join("workspace")
        .join("memory")
        .join("_bootstrap.json");
    assert!(tokio::fs::try_exists(&sentinel).await?);

    tokio::fs::write(
        workspace.path().join("CONTRIBUTING.md"),
        "# Project Agent Instructions\n\nversion-two\n",
    )
    .await?;
    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;
    let project = orchestrator
        .memory_store()
        .read_page(
            &MemoryScope::Workspace {
                workspace_id: WorkspaceId::new("workspace"),
            },
            &project_path,
        )
        .await?;
    assert!(project.content.contains("version-one"));
    assert!(!project.content.contains("version-two"));

    tokio::fs::remove_file(&sentinel).await?;
    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;
    let project = orchestrator
        .memory_store()
        .read_page(
            &MemoryScope::Workspace {
                workspace_id: WorkspaceId::new("workspace"),
            },
            &project_path,
        )
        .await?;
    assert!(project.content.contains("version-two"));
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);

    Ok(())
}

#[tokio::test]
async fn workspace_memory_bootstrap_can_be_disabled() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("CONTRIBUTING.md"),
        "# Project Agent Instructions\n\nversion-one\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;

    let index = orchestrator
        .memory_store()
        .get_index(&MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace"),
        })
        .await?;
    assert!(index.trim().is_empty());

    let sentinel = base
        .path()
        .join("workspaces")
        .join("workspace")
        .join("memory")
        .join("_bootstrap.json");
    assert!(!tokio::fs::try_exists(&sentinel).await?);
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);
    Ok(())
}

#[tokio::test]
async fn workspace_instruction_file_is_injected_into_prompt_with_config_instructions() -> Result<()>
{
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Project Instructions\n\nUse pytest for testing.\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    config.general.workspace_instructions = Some("Config workspace guidance.".to_string());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    let session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: Some(UserMessage {
                text: "How should I run tests?".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let requests = requests.lock().expect("request log lock poisoned");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message.role == MessageRole::System
            && message.content.contains("<workspace_instructions>")
            && message.content.contains("Config workspace guidance.")
            && message.content.contains("# Project Instructions")
            && message.content.contains("Use pytest for testing.")
    }));
    Ok(())
}

#[tokio::test]
async fn workspace_instruction_file_is_reloaded_for_each_new_session() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Project Instructions\n\nversion-one\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let base = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.path().join("memory").display().to_string();
    config.local.sandbox_dir = base.path().join("sandbox").display().to_string();
    config.memory.auto_bootstrap = false;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;

    let first_session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: Some(UserMessage {
                text: "first session".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await?;
    wait_for_status(
        &orchestrator,
        first_session.session_id,
        SessionStatus::Completed,
    )
    .await?;

    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Project Instructions\n\nversion-two\n",
    )
    .await?;

    let second_session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: Some(UserMessage {
                text: "second session".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await?;
    wait_for_status(
        &orchestrator,
        second_session.session_id,
        SessionStatus::Completed,
    )
    .await?;

    let requests = requests.lock().expect("request log lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages.iter().any(|message| {
        message.role == MessageRole::System && message.content.contains("version-one")
    }));
    assert!(requests[1].messages.iter().any(|message| {
        message.role == MessageRole::System && message.content.contains("version-two")
    }));
    Ok(())
}

#[tokio::test]
async fn local_bash_tools_run_in_detected_workspace_root() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("repo-marker.txt"),
        "workspace-visible\n",
    )
    .await?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;

    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: MoaConfig::default().general.default_model,
        first_tool_cmd: "printf 'PWD: '; pwd; echo; printf 'marker: '; cat repo-marker.txt"
            .to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "inspect workspace".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    let tool_outputs = tool_result_texts(&events);
    let workspace_display = workspace.path().display().to_string();

    assert!(
        tool_outputs
            .iter()
            .any(|output| output.contains("workspace-visible")),
        "expected tool output to include workspace marker, got: {tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .any(|output| output.contains(&workspace_display)),
        "expected tool output to include workspace path {workspace_display}, got: {tool_outputs:?}"
    );

    Ok(())
}

#[tokio::test]
async fn local_bash_tools_prefer_git_root_over_nested_cwd() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::create_dir_all(workspace.path().join(".git")).await?;
    tokio::fs::create_dir_all(workspace.path().join("src-tauri")).await?;
    tokio::fs::write(workspace.path().join("repo-marker.txt"), "workspace-root\n").await?;
    let nested = workspace.path().join("src-tauri");
    let _dir_guard = CurrentDirGuard::set(&nested)?;

    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: MoaConfig::default().general.default_model,
        first_tool_cmd: "printf 'PWD: '; pwd; echo; printf 'marker: '; cat repo-marker.txt"
            .to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "inspect git root".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    let tool_outputs = tool_result_texts(&events);
    let workspace_display = workspace.path().display().to_string();
    let nested_display = nested.display().to_string();

    assert!(
        tool_outputs
            .iter()
            .any(|output| output.contains("workspace-root")),
        "expected tool output to include repo marker, got: {tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .any(|output| output.contains(&workspace_display)),
        "expected tool output to include git root {workspace_display}, got: {tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .all(|output| !output.contains(&nested_display)),
        "expected tool output to avoid nested cwd {nested_display}, got: {tool_outputs:?}"
    );

    Ok(())
}

#[tokio::test]
async fn session_pauses_after_max_turns_and_resume_processes_pending_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.database.url = testing::test_database_url();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.session_limits.max_turns = 1;

    let provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: config.general.default_model.clone(),
        first_turn_delay: Duration::from_millis(5),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("runtime receiver should exist for active session");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    let _ = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    let pause_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Paused).await?;
    assert!(pause_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Notice(message)
            if message.contains("Session paused after 1 turn. Use /resume to continue.")
    )));

    let paused_events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert_eq!(
        brain_response_texts(&paused_events),
        vec!["assistant:first"]
    );
    assert!(warning_messages(&paused_events).iter().any(|message| {
        message.contains("Session paused after 1 turn. Use /resume to continue.")
    }));

    orchestrator.resume_session(session.session_id).await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let resumed_events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert_eq!(
        brain_response_texts(&resumed_events),
        vec!["assistant:first", "assistant:second"]
    );

    Ok(())
}

#[tokio::test]
async fn session_pauses_on_loop_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.database.url = testing::test_database_url();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.session_limits.max_turns = 0;
    config.session_limits.loop_detection_threshold = 3;

    let provider: Arc<dyn LLMProvider> = Arc::new(RepeatingToolTurnProvider {
        model: config.general.default_model.clone(),
        search_pattern: "moa-brain/Cargo.toml".to_string(),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let session = start_session(&orchestrator).await?;

    for prompt in ["first", "second"] {
        orchestrator
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
        let expected_tool_turns = if prompt == "first" { 1 } else { 2 };
        wait_for_tool_result_count(&orchestrator, session.session_id, expected_tool_turns).await?;
    }

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "third".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Paused).await?;

    let paused_events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    let tool_outputs = tool_result_texts(&paused_events);
    assert_eq!(tool_outputs.len(), 3);
    assert!(tool_outputs.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(warning_messages(&paused_events).iter().any(|message| {
        message.contains("Loop detected after 3 consecutive turns with identical tool call patterns. Session paused. Use /resume to continue.")
    }));

    Ok(())
}
