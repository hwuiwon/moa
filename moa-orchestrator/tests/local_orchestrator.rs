use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ContextMessage, Event, EventRange, EventType, LLMProvider, MessageRole, MoaConfig, MoaError,
    Platform, Result, SessionFilter, SessionHandle, SessionId, SessionSignal, SessionStatus,
    SessionStore, StartSessionRequest, TokenPricing, ToolCallFormat, UserId, UserMessage,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_session::TursoSessionStore;
use tempfile::TempDir;
use tokio::time::{Instant, sleep};

#[derive(Clone)]
struct MockProvider {
    model: String,
    first_turn_delay: Duration,
}

#[async_trait]
impl LLMProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.clone(),
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
            model,
            input_tokens: 4,
            output_tokens: 2,
            cached_input_tokens: 0,
            duration_ms: delay.as_millis() as u64,
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

fn last_user_message(messages: &[ContextMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == moa_core::MessageRole::User)
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
    config.local.session_db = dir.path().join("sessions.db").display().to_string();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let session_store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
    let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
    let tool_router = Arc::new(
        ToolRouter::from_config(&config, memory_store.clone())
            .await?
            .with_rule_store(session_store.clone()),
    );
    let orchestrator =
        LocalOrchestrator::new(config, session_store, memory_store, provider, tool_router).await?;

    Ok((dir, orchestrator))
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
            model_id: self.model.clone(),
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
            model,
            input_tokens: 4,
            output_tokens: 2,
            cached_input_tokens: 0,
            duration_ms: delay.as_millis() as u64,
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
            model_id: self.model.clone(),
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
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolInvocation {
                    id: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()),
                    name: "bash".to_string(),
                    input: serde_json::json!({
                        "cmd": "python3 -c 'import time; time.sleep(0.35); print(\"cancelled-tool\")'"
                    }),
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone(),
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: 0,
                duration_ms: 10,
            }
        } else {
            CompletionResponse {
                text: "should-not-run".to_string(),
                content: vec![CompletionContent::Text("should-not-run".to_string())],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone(),
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: 0,
                duration_ms: 10,
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
            model_id: self.model.clone(),
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
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let mut requests = self.requests.lock().expect("request log lock poisoned");
        let response = if requests.is_empty() {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(moa_core::ToolInvocation {
                    id: Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()),
                    name: "bash".to_string(),
                    input: serde_json::json!({
                        "cmd": self.first_tool_cmd,
                    }),
                })],
                stop_reason: moa_core::StopReason::ToolUse,
                model: self.model.clone(),
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: 0,
                duration_ms: 10,
            }
        } else {
            let prompt = last_user_message(&request.messages).unwrap_or_default();
            CompletionResponse {
                text: format!("assistant:{prompt}"),
                content: vec![CompletionContent::Text(format!("assistant:{prompt}"))],
                stop_reason: moa_core::StopReason::EndTurn,
                model: self.model.clone(),
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: 0,
                duration_ms: 10,
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
            platform: Platform::Tui,
            model: orchestrator.model().to_string(),
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
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let session = orchestrator.get_session(session_id.clone()).await?;
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

async fn wait_for_approval_request(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
) -> Result<uuid::Uuid> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let events = orchestrator
            .session_store()
            .get_events(session_id.clone(), EventRange::all())
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

fn brain_response_texts(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn starts_two_sessions_and_processes_both() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let left = start_session(&orchestrator).await?;
    let right = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            left.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "left".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            right.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "right".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(
        &orchestrator,
        left.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;
    wait_for_status(
        &orchestrator,
        right.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;

    let left_events = orchestrator
        .session_store()
        .get_events(left.session_id, EventRange::all())
        .await?;
    let right_events = orchestrator
        .session_store()
        .get_events(right.session_id, EventRange::all())
        .await?;

    assert!(left_events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text.contains("left")
    )));
    assert!(right_events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text.contains("right")
    )));
    Ok(())
}

#[tokio::test]
async fn soft_cancel_marks_session_cancelled() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(250)).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(session.session_id.clone(), SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
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
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "cancel during tool".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id.clone()).await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;
    sleep(Duration::from_millis(50)).await;
    orchestrator
        .signal(session.session_id.clone(), SessionSignal::SoftCancel)
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Cancelled,
    )
    .await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, Event::ToolResult { .. }))
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
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;
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
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;

    let logged = requests.lock().expect("request log lock poisoned").clone();
    assert!(logged.len() >= 2);
    assert_eq!(
        logged[1]
            .messages
            .last()
            .expect("second request should have messages")
            .role,
        MessageRole::User
    );
    assert_eq!(last_user_message(&logged[1].messages), Some("second"));

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
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "third".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:second", "assistant:third"]
    );

    let logged = requests.lock().expect("request log lock poisoned").clone();
    assert_eq!(logged.len(), 3);
    assert_eq!(last_user_message(&logged[0].messages), Some("first"));
    assert_eq!(last_user_message(&logged[1].messages), Some("second"));
    assert_eq!(last_user_message(&logged[2].messages), Some("third"));

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
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id.clone()).await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "queued".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;

    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:queued"]
    );

    let first_response_index = events
        .iter()
        .position(|record| matches!(record.event, Event::BrainResponse { ref text, .. } if text == "assistant:first"))
        .expect("missing first response");
    let queued_index = events
        .iter()
        .position(|record| matches!(record.event, Event::QueuedMessage { ref text, .. } if text == "queued"))
        .expect("missing queued message");
    assert!(queued_index > first_response_index);

    Ok(())
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
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id.clone()).await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "after-deny".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::Deny { reason: None },
            },
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;
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
        first_tool_cmd: "python3 -c 'import time; time.sleep(0.35); print(\"tool-finished\")'"
            .to_string(),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "cancel during tool".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let request_id = wait_for_approval_request(&orchestrator, session.session_id.clone()).await?;
    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;
    sleep(Duration::from_millis(50)).await;
    orchestrator
        .signal(session.session_id.clone(), SessionSignal::SoftCancel)
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Cancelled,
    )
    .await?;

    orchestrator
        .resume_session(session.session_id.clone())
        .await?;
    sleep(Duration::from_millis(450)).await;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id.clone(), EventRange::all())
        .await?;
    assert!(brain_response_texts(&events).is_empty());
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 1);

    orchestrator
        .signal(
            session.session_id.clone(),
            SessionSignal::QueueMessage(UserMessage {
                text: "after resume".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(
        &orchestrator,
        session.session_id.clone(),
        SessionStatus::Completed,
    )
    .await?;
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
async fn observe_stream_receives_events_in_order() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;
    let mut stream = orchestrator
        .observe(session.session_id.clone(), moa_core::ObserveLevel::Normal)
        .await?;

    orchestrator
        .signal(
            session.session_id.clone(),
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

    assert_eq!(first.sequence_num, 0);
    assert_eq!(first.event_type, EventType::SessionCreated);
    assert_eq!(second.sequence_num, 1);
    assert_eq!(second.event_type, EventType::UserMessage);
    assert_eq!(third.sequence_num, 2);
    assert_eq!(third.event_type, EventType::BrainResponse);
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
