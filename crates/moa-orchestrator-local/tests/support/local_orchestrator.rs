//! Shared local-orchestrator integration-test fixtures.

#![allow(dead_code)]
#![allow(unused_imports)]

pub(crate) use std::sync::Arc;
pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};
pub(crate) use std::sync::{Mutex, OnceLock};
pub(crate) use std::time::Duration;

pub(crate) use super::orchestrator_contract::{
    OrchestratorContractHarness, assert_blank_session_waits_for_first_message,
    assert_processes_multiple_queued_messages_fifo, assert_processes_two_sessions_independently,
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn,
    assert_soft_cancel_waiting_for_approval_cancels_cleanly,
};
use async_trait::async_trait;
pub(crate) use chrono::Utc;
pub(crate) use moa_core::{
    BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ContextMessage, Event, EventRange, EventRecord, EventType, LLMProvider, LiveEvent, MessageRole,
    MoaConfig, MoaError, Platform, Result, RuntimeEvent, SessionFilter, SessionHandle, SessionId,
    SessionMeta, SessionSignal, SessionStatus, SessionStore, StartSessionRequest, TokenPricing,
    TokenUsage, ToolCallFormat, ToolOutput, UserId, UserMessage, WorkspaceId,
};
pub(crate) use moa_hands::{ToolRegistry, ToolRouter};
pub(crate) use moa_orchestrator_local::LocalOrchestrator;
pub(crate) use moa_providers::ModelRouter;
pub(crate) use moa_session::{PostgresSessionStore, testing};
pub(crate) use tempfile::TempDir;
pub(crate) use tokio::sync::Mutex as AsyncMutex;
pub(crate) use tokio::time::{Instant, sleep, timeout};
pub(crate) const ASYNC_TEST_DEADLINE: Duration = Duration::from_secs(6);

pub(crate) fn disable_query_rewrite(config: &mut MoaConfig) {
    config.query_rewrite.enabled = false;
}

pub(crate) struct LocalContractHarness<'a> {
    pub(crate) orchestrator: &'a LocalOrchestrator,
    pub(crate) requests: Option<Arc<Mutex<Vec<CompletionRequest>>>>,
}

impl<'a> LocalContractHarness<'a> {
    pub(crate) fn new(
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
        Platform::Cli
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
pub(crate) struct MockProvider {
    pub(crate) model: String,
    pub(crate) first_turn_delay: Duration,
}

#[derive(Clone)]
pub(crate) struct SlowStreamingProvider {
    pub(crate) model: String,
    pub(crate) text: String,
    pub(crate) delay: Duration,
}

pub(crate) fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
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

pub(crate) fn last_user_message(messages: &[ContextMessage]) -> Option<&str> {
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

pub(crate) async fn test_orchestrator() -> Result<(TempDir, LocalOrchestrator)> {
    test_orchestrator_with_delay(Duration::from_millis(200)).await
}

pub(crate) async fn test_orchestrator_with_delay(
    delay: Duration,
) -> Result<(TempDir, LocalOrchestrator)> {
    let provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: MoaConfig::default().models.main,
        first_turn_delay: delay,
    });
    test_orchestrator_with_provider(provider).await
}

pub(crate) async fn test_orchestrator_with_provider(
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
    let tool_router = Arc::new(
        timed_test_stage("local:create_tool_router", ToolRouter::from_config(&config))
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let orchestrator = timed_test_stage(
        "local:create_orchestrator",
        LocalOrchestrator::new(
            config,
            session_store,
            Arc::new(ModelRouter::new(provider, None)),
            tool_router,
        ),
    )
    .await?;

    Ok((dir, orchestrator))
}

pub(crate) async fn test_orchestrator_with_config_and_provider(
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

pub(crate) async fn test_orchestrator_with_config_provider_and_store(
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

pub(crate) async fn test_orchestrator_with_config_router_and_store(
    mut config: MoaConfig,
    model_router: Arc<ModelRouter>,
    session_store: Arc<PostgresSessionStore>,
) -> Result<LocalOrchestrator> {
    disable_query_rewrite(&mut config);
    let tool_router = Arc::new(
        timed_test_stage("local:create_tool_router", ToolRouter::from_config(&config))
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    timed_test_stage(
        "local:create_orchestrator",
        LocalOrchestrator::new(config, session_store, model_router, tool_router),
    )
    .await
}

pub(crate) fn cwd_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

pub(crate) async fn create_test_store() -> Result<Arc<PostgresSessionStore>> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    Ok(Arc::new(store))
}

pub(crate) async fn graph_node_count(
    store: &PostgresSessionStore,
    workspace_id: &WorkspaceId,
) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(workspace_id.as_str())
    .fetch_one(store.pool())
    .await
    .map_err(|error| MoaError::StorageError(error.to_string()))?;
    Ok(count)
}

pub(crate) async fn timed_test_stage<F, T>(stage: &'static str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match timeout(Duration::from_secs(20), future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out waiting for test stage `{stage}`"),
    }
}

pub(crate) struct CurrentDirGuard {
    pub(crate) previous: std::path::PathBuf,
}

impl CurrentDirGuard {
    pub(crate) fn set(path: &std::path::Path) -> Result<Self> {
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
pub(crate) struct RequestGuardProvider {
    pub(crate) model: String,
    pub(crate) first_turn_delay: Duration,
    pub(crate) requests: Arc<Mutex<Vec<CompletionRequest>>>,
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
pub(crate) struct ToolCancelProvider {
    pub(crate) model: String,
    pub(crate) requests: Arc<Mutex<Vec<CompletionRequest>>>,
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
pub(crate) struct ToolThenEchoProvider {
    pub(crate) model: String,
    pub(crate) first_tool_cmd: String,
    pub(crate) requests: Arc<Mutex<Vec<CompletionRequest>>>,
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
pub(crate) struct RepeatingToolTurnProvider {
    pub(crate) model: String,
    pub(crate) search_pattern: String,
    pub(crate) requests: Arc<Mutex<Vec<CompletionRequest>>>,
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
pub(crate) struct FileWriteApprovalProvider {
    pub(crate) model: String,
    pub(crate) requests: Arc<Mutex<Vec<CompletionRequest>>>,
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

pub(crate) async fn start_session(orchestrator: &LocalOrchestrator) -> Result<SessionHandle> {
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
        .await
}

pub(crate) async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<()> {
    wait_for_status_with_timeout(orchestrator, session_id, expected, ASYNC_TEST_DEADLINE).await
}

pub(crate) async fn wait_for_status_with_timeout(
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

pub(crate) async fn wait_for_status_event(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<Vec<EventRecord>> {
    let deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await?;
        if events.iter().any(|record| {
            matches!(
                &record.event,
                Event::SessionStatusChanged { to, .. } if *to == expected
            )
        }) {
            return Ok(events);
        }
        if Instant::now() >= deadline {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for status event {:?}",
                expected
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

pub(crate) async fn wait_for_brain_response_count_with_timeout(
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

pub(crate) async fn wait_for_approval_request(
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

pub(crate) async fn wait_for_approval_event(
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

pub(crate) async fn collect_runtime_events_until<P>(
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

pub(crate) async fn wait_for_pending_signal_count(
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

pub(crate) async fn wait_for_tool_result_count(
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

pub(crate) async fn wait_for_tool_call_count(
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

pub(crate) fn brain_response_texts(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn tool_result_texts(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolResult { output, .. } => Some(output.to_text()),
            _ => None,
        })
        .collect()
}

pub(crate) fn warning_messages(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn event_labels(events: &[moa_core::EventRecord]) -> Vec<String> {
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
pub(crate) struct PanicProvider {
    pub(crate) model: String,
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
pub(crate) struct DestroyTrackingHandProvider {
    pub(crate) provisioned: Arc<AtomicUsize>,
    pub(crate) destroyed: Arc<AtomicUsize>,
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
