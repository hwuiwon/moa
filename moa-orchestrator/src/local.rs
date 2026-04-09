//! Tokio-task local orchestrator for multi-session MOA execution.

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::build_default_pipeline_with_tools;
use moa_core::{
    ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest,
    BrainOrchestrator, CompletionContent, CronHandle, CronSpec, Event, EventRange, EventRecord,
    EventStream, LLMProvider, MoaConfig, MoaError, ObserveLevel, PolicyAction, Result,
    RuntimeEvent, SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal,
    SessionStatus, SessionStore, SessionSummary, StartSessionRequest, StopReason, ToolCardStatus,
    ToolInvocation, ToolUpdate, UserId, UserMessage,
};
use moa_hands::ToolRouter;
use moa_hands::tools::file_read::resolve_sandbox_path;
use moa_memory::FileMemoryStore;
use moa_providers::AnthropicProvider;
use moa_session::TursoSessionStore;
use serde_json::Value;
use tokio::fs;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::task::AbortHandle;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

/// Local orchestrator backed by Tokio tasks and broadcast channels.
#[derive(Clone)]
pub struct LocalOrchestrator {
    config: MoaConfig,
    session_store: Arc<TursoSessionStore>,
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
    scheduler: Arc<JobScheduler>,
    sessions: Arc<RwLock<HashMap<SessionId, LocalBrainHandle>>>,
}

struct LocalBrainHandle {
    signal_tx: mpsc::Sender<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    abort_handle: AbortHandle,
    status: Arc<RwLock<SessionStatus>>,
    finished: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SessionTaskContext {
    config: MoaConfig,
    session_store: Arc<TursoSessionStore>,
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
    session_id: SessionId,
}

enum TurnDisposition {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone)]
struct PendingToolApproval {
    tool_id: Uuid,
    tool_name: String,
    input: Value,
    decision: StoredApprovalDecision,
    sequence_num: u64,
}

#[derive(Debug, Clone)]
enum StoredApprovalDecision {
    AllowOnce,
    AlwaysAllow { pattern: String, decided_by: String },
    Deny { reason: Option<String> },
}

impl LocalOrchestrator {
    /// Creates a local orchestrator from explicit component instances.
    pub async fn new(
        config: MoaConfig,
        session_store: Arc<TursoSessionStore>,
        memory_store: Arc<FileMemoryStore>,
        llm_provider: Arc<dyn LLMProvider>,
        tool_router: Arc<ToolRouter>,
    ) -> Result<Self> {
        let scheduler = JobScheduler::new()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        scheduler
            .start()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;

        Ok(Self {
            config,
            session_store,
            memory_store,
            llm_provider,
            tool_router,
            scheduler: Arc::new(scheduler),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Creates a fully local orchestrator from the loaded MOA config.
    pub async fn from_config(config: MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, None).await
    }

    /// Creates a fully local orchestrator from config with an optional model override.
    pub async fn from_config_with_model(
        mut config: MoaConfig,
        model_override: Option<String>,
    ) -> Result<Self> {
        if let Some(model) = model_override {
            config.general.default_model = model;
        }

        let session_store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone()),
        );
        let llm_provider: Arc<dyn LLMProvider> = Arc::new(AnthropicProvider::from_config(&config)?);
        Self::new(
            config,
            session_store,
            memory_store,
            llm_provider,
            tool_router,
        )
        .await
    }

    /// Returns the underlying local session store.
    pub fn session_store(&self) -> Arc<TursoSessionStore> {
        self.session_store.clone()
    }

    /// Returns the configured default model identifier.
    pub fn model(&self) -> &str {
        &self.config.general.default_model
    }

    /// Subscribes to live runtime updates for a running session.
    pub async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> Result<broadcast::Receiver<RuntimeEvent>> {
        self.ensure_session_running(session_id.clone()).await?;
        let sessions = self.sessions.read().await;
        let handle = sessions
            .get(&session_id)
            .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;
        Ok(handle.runtime_tx.subscribe())
    }

    /// Returns the current persisted session snapshot.
    pub async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.session_store.get_session(session_id).await
    }

    /// Ensures a persisted session has an active background task.
    pub async fn ensure_session_running(&self, session_id: SessionId) -> Result<()> {
        if self.handle_is_active(&session_id).await {
            return Ok(());
        }

        self.resume_session(session_id).await.map(|_| ())
    }

    async fn handle_is_active(&self, session_id: &SessionId) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|handle| !handle.finished.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    async fn spawn_session(
        &self,
        session_id: SessionId,
        initial_turn_requested: bool,
    ) -> Result<()> {
        let (signal_tx, signal_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(256);
        let (runtime_tx, _) = broadcast::channel(512);
        let status = Arc::new(RwLock::new(
            self.session_store
                .get_session(session_id.clone())
                .await?
                .status
                .clone(),
        ));
        let finished = Arc::new(AtomicBool::new(false));
        let context = SessionTaskContext {
            config: self.config.clone(),
            session_store: self.session_store.clone(),
            memory_store: self.memory_store.clone(),
            llm_provider: self.llm_provider.clone(),
            tool_router: self.tool_router.clone(),
            session_id: session_id.clone(),
        };
        let task_status = status.clone();
        let task_finished = finished.clone();
        let task_event_tx = event_tx.clone();
        let task_runtime_tx = runtime_tx.clone();
        let task = tokio::spawn(async move {
            let result = run_session_task(
                context,
                signal_rx,
                task_event_tx,
                task_runtime_tx,
                task_status,
                initial_turn_requested,
            )
            .await;
            task_finished.store(true, Ordering::SeqCst);
            result
        });

        let handle = LocalBrainHandle {
            signal_tx,
            event_tx,
            runtime_tx,
            abort_handle: task.abort_handle(),
            status,
            finished,
        };
        self.sessions.write().await.insert(session_id, handle);
        Ok(())
    }
}

#[async_trait]
impl BrainOrchestrator for LocalOrchestrator {
    /// Starts a new session task and returns its handle.
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle> {
        let initial_message = req.initial_message.clone();
        let session_id = SessionId::new();
        let now = Utc::now();
        let meta = SessionMeta {
            id: session_id.clone(),
            workspace_id: req.workspace_id.clone(),
            user_id: req.user_id.clone(),
            title: req.title.clone(),
            status: SessionStatus::Created,
            platform: req.platform.clone(),
            model: req.model.clone(),
            created_at: now,
            updated_at: now,
            parent_session_id: req.parent_session_id.clone(),
            ..SessionMeta::default()
        };
        self.session_store.create_session(meta).await?;
        append_event(
            &self.session_store,
            &broadcast::channel(1).0,
            session_id.clone(),
            Event::SessionCreated {
                workspace_id: req.workspace_id.to_string(),
                user_id: req.user_id.to_string(),
                model: req.model.clone(),
            },
        )
        .await?;
        if let Some(message) = initial_message {
            append_event(
                &self.session_store,
                &broadcast::channel(1).0,
                session_id.clone(),
                Event::UserMessage {
                    text: message.text,
                    attachments: message.attachments,
                },
            )
            .await?;
        }
        self.spawn_session(session_id.clone(), req.initial_message.is_some())
            .await?;
        Ok(SessionHandle { session_id })
    }

    /// Resumes an existing persisted session by spawning a new background task if needed.
    async fn resume_session(&self, session_id: SessionId) -> Result<SessionHandle> {
        if self.handle_is_active(&session_id).await {
            return Ok(SessionHandle { session_id });
        }

        let wake = self.session_store.wake(session_id.clone()).await?;
        let initial_turn_requested =
            session_requires_processing(&wake.session, &wake.recent_events);
        self.spawn_session(session_id.clone(), initial_turn_requested)
            .await?;
        Ok(SessionHandle { session_id })
    }

    /// Sends a signal to a running local session.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.ensure_session_running(session_id.clone()).await?;
        let sessions = self.sessions.read().await;
        let handle = sessions
            .get(&session_id)
            .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;

        if matches!(signal, SessionSignal::HardCancel) {
            self.session_store
                .update_status(session_id, SessionStatus::Cancelled)
                .await?;
            *handle.status.write().await = SessionStatus::Cancelled;
            let _ = handle.runtime_tx.send(RuntimeEvent::Notice(
                "Cancelled current generation.".to_string(),
            ));
            let _ = handle.runtime_tx.send(RuntimeEvent::TurnCompleted);
            handle.finished.store(true, Ordering::SeqCst);
            handle.abort_handle.abort();
            return Ok(());
        }

        handle
            .signal_tx
            .send(signal)
            .await
            .map_err(|_| MoaError::ProviderError("session signal channel closed".to_string()))
    }

    /// Lists persisted sessions matching the provided filter.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        self.session_store.list_sessions(filter).await
    }

    /// Returns buffered history plus optional live event updates for a session.
    async fn observe(&self, session_id: SessionId, _level: ObserveLevel) -> Result<EventStream> {
        let history = self
            .session_store
            .get_events(session_id.clone(), EventRange::all())
            .await?;
        let sessions = self.sessions.read().await;
        if let Some(handle) = sessions.get(&session_id)
            && !handle.finished.load(Ordering::SeqCst)
        {
            return Ok(EventStream::from_history_and_broadcast(
                history,
                handle.event_tx.subscribe(),
            ));
        }

        Ok(EventStream::from_events(history))
    }

    /// Registers a local cron job backed by `tokio-cron-scheduler`.
    async fn schedule_cron(&self, spec: CronSpec) -> Result<CronHandle> {
        let job_name = spec.name.clone();
        let task_name = spec.task.clone();
        let job = Job::new_async(spec.schedule.as_str(), move |_id, _lock| {
            let job_name = job_name.clone();
            let task_name = task_name.clone();
            Box::pin(async move {
                tracing::info!(job = %job_name, task = %task_name, "running scheduled local job");
            })
        })
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let job_id = job.guid().to_string();
        self.scheduler
            .add(job)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(CronHandle::Local { id: job_id })
    }
}

async fn run_session_task(
    context: SessionTaskContext,
    mut signal_rx: mpsc::Receiver<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    status: Arc<RwLock<SessionStatus>>,
    mut turn_requested: bool,
) -> Result<()> {
    let pipeline = build_default_pipeline_with_tools(
        &context.config,
        context.session_store.clone(),
        context.memory_store.clone(),
        context.tool_router.tool_schemas(),
    );
    let mut queued_messages = Vec::new();

    loop {
        if !turn_requested {
            match signal_rx.recv().await {
                Some(SessionSignal::QueueMessage(message)) => {
                    accept_user_message(
                        &context.session_store,
                        &event_tx,
                        &context.session_id,
                        message,
                        false,
                    )
                    .await?;
                    update_status(
                        &context.session_store,
                        &status,
                        context.session_id.clone(),
                        SessionStatus::Running,
                    )
                    .await?;
                    turn_requested = true;
                }
                Some(SessionSignal::SoftCancel) | Some(SessionSignal::HardCancel) => {
                    update_status(
                        &context.session_store,
                        &status,
                        context.session_id.clone(),
                        SessionStatus::Cancelled,
                    )
                    .await?;
                    let _ = runtime_tx.send(RuntimeEvent::Notice(
                        "Cancelled current generation.".to_string(),
                    ));
                    let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
                    return Ok(());
                }
                Some(SessionSignal::ApprovalDecided {
                    request_id,
                    decision,
                }) => {
                    append_event(
                        &context.session_store,
                        &event_tx,
                        context.session_id.clone(),
                        Event::ApprovalDecided {
                            request_id,
                            decision,
                            decided_by: "orchestrator".to_string(),
                            decided_at: Utc::now(),
                        },
                    )
                    .await?;
                    turn_requested = true;
                }
                None => return Ok(()),
            }
            continue;
        }

        turn_requested = false;
        let disposition = drive_turn(
            &context,
            &pipeline,
            &event_tx,
            &runtime_tx,
            &mut signal_rx,
            &mut turn_requested,
            &mut queued_messages,
        )
        .await;

        match disposition {
            Ok(TurnDisposition::Completed) => {
                if flush_next_queued_message(
                    &context.session_store,
                    &event_tx,
                    &context.session_id,
                    &mut queued_messages,
                )
                .await?
                {
                    turn_requested = true;
                }
                if turn_requested {
                    continue;
                }
                update_status(
                    &context.session_store,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Completed,
                )
                .await?;
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
            }
            Ok(TurnDisposition::Cancelled) => {
                flush_queued_messages(
                    &context.session_store,
                    &event_tx,
                    &context.session_id,
                    &mut queued_messages,
                )
                .await?;
                update_status(
                    &context.session_store,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Cancelled,
                )
                .await?;
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
                return Ok(());
            }
            Err(error) => {
                append_event(
                    &context.session_store,
                    &event_tx,
                    context.session_id.clone(),
                    Event::Error {
                        message: error.to_string(),
                        recoverable: false,
                    },
                )
                .await?;
                flush_queued_messages(
                    &context.session_store,
                    &event_tx,
                    &context.session_id,
                    &mut queued_messages,
                )
                .await?;
                update_status(
                    &context.session_store,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Failed,
                )
                .await?;
                let _ = runtime_tx.send(RuntimeEvent::Error(error.to_string()));
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
                return Err(error);
            }
        }
    }
}

async fn drive_turn(
    context: &SessionTaskContext,
    pipeline: &moa_brain::ContextPipeline,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
) -> Result<TurnDisposition> {
    let mut soft_cancel_requested = false;

    loop {
        let session = context
            .session_store
            .get_session(context.session_id.clone())
            .await?;
        let events = context
            .session_store
            .get_events(context.session_id.clone(), EventRange::all())
            .await?;

        if process_resolved_approval(
            context,
            &session,
            event_tx,
            runtime_tx,
            signal_rx,
            turn_requested,
            queued_messages,
            &events,
            &mut soft_cancel_requested,
        )
        .await?
        {
            if soft_cancel_requested {
                return Ok(TurnDisposition::Cancelled);
            }
            continue;
        }

        if let Some(pending) = find_pending_tool_approval(&events)
            && wait_for_approval(
                context,
                &session,
                event_tx,
                runtime_tx,
                signal_rx,
                turn_requested,
                queued_messages,
                pending,
                &mut soft_cancel_requested,
            )
            .await?
        {
            if soft_cancel_requested {
                return Ok(TurnDisposition::Cancelled);
            }
            continue;
        }

        let mut ctx = moa_core::WorkingContext::new(&session, context.llm_provider.capabilities());
        let _stage_reports = pipeline.run(&mut ctx).await?;
        let mut stream = context.llm_provider.complete(ctx.into_request()).await?;
        let mut streamed_text = String::new();
        let mut started_assistant = false;

        loop {
            tokio::select! {
                block = stream.next() => {
                    let Some(block) = block else {
                        break;
                    };
                    match block? {
                        CompletionContent::Text(delta) => {
                            if !started_assistant {
                                let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);
                                started_assistant = true;
                            }
                            streamed_text.push_str(&delta);
                            for ch in delta.chars() {
                                let _ = runtime_tx.send(RuntimeEvent::AssistantDelta(ch));
                            }
                        }
                        CompletionContent::ToolCall(_) => {}
                    }
                }
                signal = signal_rx.recv() => {
                    match signal {
                        Some(SessionSignal::QueueMessage(message)) => {
                            buffer_queued_message(queued_messages, message);
                            *turn_requested = true;
                            let _ = runtime_tx.send(RuntimeEvent::Notice(
                                "Message queued. Will process after current turn.".to_string(),
                            ));
                        }
                        Some(SessionSignal::SoftCancel) => {
                            soft_cancel_requested = true;
                            let _ = runtime_tx.send(RuntimeEvent::Notice(
                                "Stop requested. MOA will stop after the current step.".to_string(),
                            ));
                        }
                        Some(SessionSignal::HardCancel) => {
                            return Ok(TurnDisposition::Cancelled);
                        }
                        Some(SessionSignal::ApprovalDecided { .. }) => {}
                        None => return Ok(TurnDisposition::Cancelled),
                    }
                }
            }
        }

        let response = stream.into_response().await?;
        if !streamed_text.trim().is_empty() {
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::BrainResponse {
                    text: streamed_text.clone(),
                    model: response.model.clone(),
                    input_tokens: response.input_tokens,
                    output_tokens: response.output_tokens,
                    cost_cents: 0,
                    duration_ms: response.duration_ms,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::AssistantFinished {
                text: streamed_text,
            });
        }

        let mut saw_tool_request = false;
        let mut executed_tool = false;
        for block in &response.content {
            if let CompletionContent::ToolCall(call) = block {
                saw_tool_request = true;
                let outcome = handle_tool_call(
                    context,
                    &session,
                    call,
                    event_tx,
                    runtime_tx,
                    signal_rx,
                    turn_requested,
                    queued_messages,
                    &mut soft_cancel_requested,
                )
                .await?;
                drain_signal_queue(
                    context,
                    event_tx,
                    runtime_tx,
                    signal_rx,
                    turn_requested,
                    queued_messages,
                    &mut soft_cancel_requested,
                )
                .await?;
                match outcome {
                    ToolCallOutcome::Executed => executed_tool = true,
                    ToolCallOutcome::Skipped => {}
                    ToolCallOutcome::Cancelled => return Ok(TurnDisposition::Cancelled),
                }
                if soft_cancel_requested {
                    return Ok(TurnDisposition::Cancelled);
                }
            }
        }

        let session = context
            .session_store
            .get_session(context.session_id.clone())
            .await?;
        let _ = runtime_tx.send(RuntimeEvent::UsageUpdated {
            total_tokens: session.total_input_tokens + session.total_output_tokens,
        });

        if soft_cancel_requested {
            return Ok(TurnDisposition::Cancelled);
        }
        if executed_tool || saw_tool_request || response.stop_reason == StopReason::ToolUse {
            continue;
        }

        return Ok(TurnDisposition::Completed);
    }
}

enum ToolCallOutcome {
    Executed,
    Skipped,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn handle_tool_call(
    context: &SessionTaskContext,
    session: &SessionMeta,
    call: &ToolInvocation,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<ToolCallOutcome> {
    let tool_id = parse_tool_id(call);
    let policy = context.tool_router.check_policy(session, call).await?;
    let summary = policy.input_summary.clone();
    let pattern = always_allow_pattern(&call.name, &policy.normalized_input);

    match policy.action {
        PolicyAction::Allow => {
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Running,
                summary,
                detail: None,
            }));
            execute_tool(context, session, call, tool_id, true, event_tx, runtime_tx)
                .await
                .map(|executed| {
                    if executed {
                        ToolCallOutcome::Executed
                    } else {
                        ToolCallOutcome::Skipped
                    }
                })
        }
        PolicyAction::Deny => {
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolCall {
                    tool_id,
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let message = format!("tool {} denied by policy", call.name);
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolError {
                    tool_id,
                    error: message.clone(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary,
                detail: Some(message),
            }));
            Ok(ToolCallOutcome::Skipped)
        }
        PolicyAction::RequireApproval => {
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolCall {
                    tool_id,
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    hand_id: None,
                },
            )
            .await?;
            let request = ApprovalRequest {
                request_id: tool_id,
                tool_name: call.name.clone(),
                input_summary: summary.clone(),
                risk_level: policy.risk_level,
            };
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ApprovalRequested {
                    request_id: request.request_id,
                    tool_name: request.tool_name.clone(),
                    input_summary: request.input_summary.clone(),
                    risk_level: request.risk_level.clone(),
                },
            )
            .await?;
            context
                .session_store
                .update_status(context.session_id.clone(), SessionStatus::WaitingApproval)
                .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::WaitingApproval,
                summary: summary.clone(),
                detail: Some("Press y to allow once, a to always allow, n to deny".to_string()),
            }));
            let _ = runtime_tx.send(RuntimeEvent::ApprovalRequested(ApprovalPrompt {
                request,
                pattern,
                parameters: approval_fields_for_call(&context.config, call),
                file_diffs: approval_diffs_for_call(&context.config, call).await?,
            }));

            if wait_for_signal_approval(
                context,
                session,
                call,
                tool_id,
                summary,
                event_tx,
                runtime_tx,
                signal_rx,
                turn_requested,
                queued_messages,
                soft_cancel_requested,
            )
            .await?
            {
                Ok(ToolCallOutcome::Executed)
            } else if *soft_cancel_requested {
                Ok(ToolCallOutcome::Cancelled)
            } else {
                Ok(ToolCallOutcome::Skipped)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_signal_approval(
    context: &SessionTaskContext,
    session: &SessionMeta,
    call: &ToolInvocation,
    tool_id: Uuid,
    summary: String,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<bool> {
    loop {
        match signal_rx.recv().await {
            Some(SessionSignal::ApprovalDecided {
                request_id,
                decision,
            }) if request_id == tool_id => {
                append_event(
                    &context.session_store,
                    event_tx,
                    context.session_id.clone(),
                    Event::ApprovalDecided {
                        request_id,
                        decision: decision.clone(),
                        decided_by: session.user_id.to_string(),
                        decided_at: Utc::now(),
                    },
                )
                .await?;
                context
                    .session_store
                    .update_status(context.session_id.clone(), SessionStatus::Running)
                    .await?;

                return match decision {
                    ApprovalDecision::AllowOnce => {
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: None,
                        }));
                        execute_tool(context, session, call, tool_id, false, event_tx, runtime_tx)
                            .await
                    }
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        context
                            .tool_router
                            .store_approval_rule(
                                session,
                                &call.name,
                                &pattern,
                                PolicyAction::Allow,
                                session.user_id.clone(),
                            )
                            .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: Some(format!("Always allow rule stored: {pattern}")),
                        }));
                        execute_tool(context, session, call, tool_id, false, event_tx, runtime_tx)
                            .await
                    }
                    ApprovalDecision::Deny { reason } => {
                        append_event(
                            &context.session_store,
                            event_tx,
                            context.session_id.clone(),
                            Event::ToolError {
                                tool_id,
                                error: reason
                                    .clone()
                                    .unwrap_or_else(|| "tool execution denied by user".to_string()),
                                retryable: false,
                            },
                        )
                        .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Failed,
                            summary,
                            detail: Some(
                                reason.unwrap_or_else(|| "Denied by the user".to_string()),
                            ),
                        }));
                        Ok(false)
                    }
                };
            }
            Some(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after the approval decision.".to_string(),
                ));
            }
            Some(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Stop requested. MOA will stop after the current step.".to_string(),
                ));
                return Ok(false);
            }
            Some(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
                return Ok(false);
            }
            Some(SessionSignal::ApprovalDecided { .. }) => {}
            None => {
                return Err(MoaError::ProviderError(
                    "approval channel closed".to_string(),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_resolved_approval(
    context: &SessionTaskContext,
    session: &SessionMeta,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    events: &[EventRecord],
    soft_cancel_requested: &mut bool,
) -> Result<bool> {
    let Some(pending) = find_resolved_pending_tool(events) else {
        return Ok(false);
    };

    match pending.decision.clone() {
        StoredApprovalDecision::AllowOnce => {
            let invocation = ToolInvocation {
                id: Some(pending.tool_id.to_string()),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id,
                tool_name: pending.tool_name.clone(),
                status: ToolCardStatus::Running,
                summary: summarize_call_for_runtime(&pending.tool_name, &pending.input),
                detail: None,
            }));
            execute_tool(
                context,
                session,
                &invocation,
                pending.tool_id,
                false,
                event_tx,
                runtime_tx,
            )
            .await?;
        }
        StoredApprovalDecision::AlwaysAllow {
            pattern,
            decided_by,
        } => {
            context
                .tool_router
                .store_approval_rule(
                    session,
                    &pending.tool_name,
                    &pattern,
                    PolicyAction::Allow,
                    UserId::new(decided_by),
                )
                .await?;
            let invocation = ToolInvocation {
                id: Some(pending.tool_id.to_string()),
                name: pending.tool_name.clone(),
                input: pending.input.clone(),
            };
            execute_tool(
                context,
                session,
                &invocation,
                pending.tool_id,
                false,
                event_tx,
                runtime_tx,
            )
            .await?;
        }
        StoredApprovalDecision::Deny { reason } => {
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolError {
                    tool_id: pending.tool_id,
                    error: reason
                        .clone()
                        .unwrap_or_else(|| "tool execution denied by user".to_string()),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id: pending.tool_id,
                tool_name: pending.tool_name,
                status: ToolCardStatus::Failed,
                summary: "tool denied".to_string(),
                detail: reason,
            }));
        }
    }

    drain_signal_queue(
        context,
        event_tx,
        runtime_tx,
        signal_rx,
        turn_requested,
        queued_messages,
        soft_cancel_requested,
    )
    .await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_approval(
    context: &SessionTaskContext,
    session: &SessionMeta,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    pending: PendingToolApproval,
    soft_cancel_requested: &mut bool,
) -> Result<bool> {
    let invocation = ToolInvocation {
        id: Some(pending.tool_id.to_string()),
        name: pending.tool_name.clone(),
        input: pending.input.clone(),
    };
    let prompt = ApprovalPrompt {
        request: ApprovalRequest {
            request_id: pending.tool_id,
            tool_name: pending.tool_name.clone(),
            input_summary: summarize_call_for_runtime(&pending.tool_name, &pending.input),
            risk_level: risk_level_for_tool(&pending.tool_name),
        },
        pattern: always_allow_pattern(
            &pending.tool_name,
            &normalized_input_for_runtime(&pending.tool_name, &pending.input),
        ),
        parameters: approval_fields_for_call(&context.config, &invocation),
        file_diffs: approval_diffs_for_call(&context.config, &invocation).await?,
    };
    let _ = runtime_tx.send(RuntimeEvent::ApprovalRequested(prompt));
    let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
        tool_id: pending.tool_id,
        tool_name: pending.tool_name.clone(),
        status: ToolCardStatus::WaitingApproval,
        summary: summarize_call_for_runtime(&pending.tool_name, &pending.input),
        detail: Some("Press y to allow once, a to always allow, n to deny".to_string()),
    }));

    loop {
        match signal_rx.recv().await {
            Some(SessionSignal::ApprovalDecided {
                request_id,
                decision,
            }) if request_id == pending.tool_id => {
                append_event(
                    &context.session_store,
                    event_tx,
                    context.session_id.clone(),
                    Event::ApprovalDecided {
                        request_id,
                        decision: decision.clone(),
                        decided_by: session.user_id.to_string(),
                        decided_at: Utc::now(),
                    },
                )
                .await?;
                return match decision {
                    ApprovalDecision::AllowOnce => {
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id: pending.tool_id,
                            tool_name: pending.tool_name.clone(),
                            status: ToolCardStatus::Running,
                            summary: summarize_call_for_runtime(&pending.tool_name, &pending.input),
                            detail: None,
                        }));
                        execute_tool(
                            context,
                            session,
                            &invocation,
                            pending.tool_id,
                            false,
                            event_tx,
                            runtime_tx,
                        )
                        .await
                    }
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        context
                            .tool_router
                            .store_approval_rule(
                                session,
                                &pending.tool_name,
                                &pattern,
                                PolicyAction::Allow,
                                session.user_id.clone(),
                            )
                            .await?;
                        execute_tool(
                            context,
                            session,
                            &invocation,
                            pending.tool_id,
                            false,
                            event_tx,
                            runtime_tx,
                        )
                        .await
                    }
                    ApprovalDecision::Deny { reason } => {
                        append_event(
                            &context.session_store,
                            event_tx,
                            context.session_id.clone(),
                            Event::ToolError {
                                tool_id: pending.tool_id,
                                error: reason
                                    .clone()
                                    .unwrap_or_else(|| "tool execution denied by user".to_string()),
                                retryable: false,
                            },
                        )
                        .await?;
                        let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id: pending.tool_id,
                            tool_name: pending.tool_name,
                            status: ToolCardStatus::Failed,
                            summary: "tool denied".to_string(),
                            detail: reason,
                        }));
                        Ok(true)
                    }
                };
            }
            Some(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
            }
            Some(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
                return Ok(true);
            }
            Some(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
                return Ok(true);
            }
            Some(SessionSignal::ApprovalDecided { .. }) => {}
            None => {
                return Err(MoaError::ProviderError(
                    "approval channel closed".to_string(),
                ));
            }
        }
    }
}

async fn drain_signal_queue(
    _context: &SessionTaskContext,
    _event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    signal_rx: &mut mpsc::Receiver<SessionSignal>,
    turn_requested: &mut bool,
    queued_messages: &mut Vec<UserMessage>,
    soft_cancel_requested: &mut bool,
) -> Result<()> {
    loop {
        match signal_rx.try_recv() {
            Ok(SessionSignal::QueueMessage(message)) => {
                buffer_queued_message(queued_messages, message);
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after current turn.".to_string(),
                ));
            }
            Ok(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::ApprovalDecided { .. }) => {}
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(MoaError::ProviderError(
                    "session signal channel closed".to_string(),
                ));
            }
        }
    }
}

async fn execute_tool(
    context: &SessionTaskContext,
    session: &SessionMeta,
    call: &ToolInvocation,
    tool_id: Uuid,
    emit_call_event: bool,
    event_tx: &broadcast::Sender<EventRecord>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
) -> Result<bool> {
    match context.tool_router.execute_authorized(session, call).await {
        Ok((hand_id, output)) => {
            if emit_call_event {
                append_event(
                    &context.session_store,
                    event_tx,
                    context.session_id.clone(),
                    Event::ToolCall {
                        tool_id,
                        tool_name: call.name.clone(),
                        input: call.input.clone(),
                        hand_id,
                    },
                )
                .await?;
            }
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolResult {
                    tool_id,
                    output: format_tool_output(&output),
                    success: output.exit_code == 0,
                    duration_ms: output.duration.as_millis() as u64,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: if output.exit_code == 0 {
                    ToolCardStatus::Succeeded
                } else {
                    ToolCardStatus::Failed
                },
                summary: summarize_tool_completion(call, &output),
                detail: Some(format_tool_output(&output)),
            }));
            Ok(true)
        }
        Err(error) => {
            append_event(
                &context.session_store,
                event_tx,
                context.session_id.clone(),
                Event::ToolError {
                    tool_id,
                    error: error.to_string(),
                    retryable: false,
                },
            )
            .await?;
            let _ = runtime_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                tool_id,
                tool_name: call.name.clone(),
                status: ToolCardStatus::Failed,
                summary: format!("{} failed", call.name),
                detail: Some(error.to_string()),
            }));
            Ok(false)
        }
    }
}

async fn accept_user_message(
    session_store: &Arc<TursoSessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    message: UserMessage,
    queued: bool,
) -> Result<()> {
    let event = if queued {
        Event::QueuedMessage {
            text: message.text,
            queued_at: Utc::now(),
        }
    } else {
        Event::UserMessage {
            text: message.text,
            attachments: message.attachments,
        }
    };
    append_event(session_store, event_tx, session_id.clone(), event).await?;
    Ok(())
}

fn buffer_queued_message(queued_messages: &mut Vec<UserMessage>, message: UserMessage) {
    queued_messages.push(message);
}

async fn flush_queued_messages(
    session_store: &Arc<TursoSessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    queued_messages: &mut Vec<UserMessage>,
) -> Result<()> {
    for message in queued_messages.drain(..) {
        accept_user_message(session_store, event_tx, session_id, message, true).await?;
    }

    Ok(())
}

async fn flush_next_queued_message(
    session_store: &Arc<TursoSessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    queued_messages: &mut Vec<UserMessage>,
) -> Result<bool> {
    if queued_messages.is_empty() {
        return Ok(false);
    }

    let message = queued_messages.remove(0);
    accept_user_message(session_store, event_tx, session_id, message, true).await?;
    Ok(true)
}

async fn update_status(
    session_store: &Arc<TursoSessionStore>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    next_status: SessionStatus,
) -> Result<()> {
    session_store
        .update_status(session_id, next_status.clone())
        .await?;
    *status.write().await = next_status;
    Ok(())
}

async fn append_event(
    session_store: &Arc<TursoSessionStore>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: SessionId,
    event: Event,
) -> Result<EventRecord> {
    let sequence_num = session_store.emit_event(session_id.clone(), event).await?;
    let mut records = session_store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(sequence_num),
                to_seq: Some(sequence_num),
                event_types: None,
                limit: Some(1),
            },
        )
        .await?;
    let record = records
        .pop()
        .ok_or_else(|| MoaError::StorageError("failed to reload appended event".to_string()))?;
    let _ = event_tx.send(record.clone());
    Ok(record)
}

fn session_requires_processing(session: &SessionMeta, events: &[EventRecord]) -> bool {
    if matches!(session.status, SessionStatus::Cancelled) {
        return false;
    }

    if find_pending_tool_approval(events).is_some() || find_resolved_pending_tool(events).is_some()
    {
        return true;
    }

    events.last().is_some_and(|record| {
        matches!(
            record.event,
            Event::UserMessage { .. }
                | Event::QueuedMessage { .. }
                | Event::ToolResult { .. }
                | Event::ToolError { .. }
                | Event::ApprovalDecided { .. }
                | Event::ToolCall { .. }
        )
    })
}

fn find_pending_tool_approval(events: &[EventRecord]) -> Option<PendingToolApproval> {
    let mut tool_calls = HashMap::new();
    let mut decisions = HashSet::new();
    let mut completed = HashSet::new();
    let mut requested = HashSet::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        decision: StoredApprovalDecision::AllowOnce,
                        sequence_num: record.sequence_num,
                    },
                );
            }
            Event::ApprovalRequested { request_id, .. } => {
                requested.insert(*request_id);
            }
            Event::ApprovalDecided { request_id, .. } => {
                decisions.insert(*request_id);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter(|pending| {
            requested.contains(&pending.tool_id)
                && !decisions.contains(&pending.tool_id)
                && !completed.contains(&pending.tool_id)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);
    pending.into_iter().next()
}

fn find_resolved_pending_tool(events: &[EventRecord]) -> Option<PendingToolApproval> {
    let mut tool_calls = HashMap::new();
    let mut decisions = HashMap::new();
    let mut completed = HashSet::new();
    let mut requested = HashSet::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        decision: StoredApprovalDecision::AllowOnce,
                        sequence_num: record.sequence_num,
                    },
                );
            }
            Event::ApprovalRequested { request_id, .. } => {
                requested.insert(*request_id);
            }
            Event::ApprovalDecided {
                request_id,
                decision,
                decided_by,
                ..
            } => {
                let stored = match decision {
                    ApprovalDecision::AllowOnce => StoredApprovalDecision::AllowOnce,
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        StoredApprovalDecision::AlwaysAllow {
                            pattern: pattern.clone(),
                            decided_by: decided_by.clone(),
                        }
                    }
                    ApprovalDecision::Deny { reason } => StoredApprovalDecision::Deny {
                        reason: reason.clone(),
                    },
                };
                decisions.insert(*request_id, stored);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter_map(|mut pending| {
            if completed.contains(&pending.tool_id) || !requested.contains(&pending.tool_id) {
                return None;
            }
            let decision = decisions.get(&pending.tool_id)?.clone();
            pending.decision = decision;
            Some(pending)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);
    pending.into_iter().next()
}

fn summarize_tool_completion(call: &ToolInvocation, output: &moa_core::ToolOutput) -> String {
    if output.exit_code == 0 {
        format!(
            "{} completed in {} ms",
            call.name,
            output.duration.as_millis()
        )
    } else {
        format!("{} exited with code {}", call.name, output.exit_code)
    }
}

fn format_tool_output(output: &moa_core::ToolOutput) -> String {
    let mut sections = Vec::new();
    if !output.stdout.trim().is_empty() {
        sections.push(output.stdout.trim_end().to_string());
    }
    if !output.stderr.trim().is_empty() {
        sections.push(format!("stderr:\n{}", output.stderr.trim_end()));
    }
    if sections.is_empty() {
        format!("exit_code: {}", output.exit_code)
    } else {
        sections.join("\n\n")
    }
}

fn parse_tool_id(call: &ToolInvocation) -> Uuid {
    call.id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4)
}

fn always_allow_pattern(tool_name: &str, normalized_input: &str) -> String {
    if tool_name == "bash" {
        let tokens = shell_words::split(normalized_input).unwrap_or_default();
        if let Some(command) = tokens.first() {
            return if tokens.len() == 1 {
                command.clone()
            } else {
                format!("{command} *")
            };
        }
    }

    normalized_input.to_string()
}

fn approval_fields_for_call(config: &MoaConfig, call: &ToolInvocation) -> Vec<ApprovalField> {
    match call.name.as_str() {
        "bash" => {
            let command = call
                .input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let working_dir = expand_local_path(&config.local.sandbox_dir)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| config.local.sandbox_dir.clone());
            vec![
                ApprovalField {
                    label: "Command".to_string(),
                    value: command,
                },
                ApprovalField {
                    label: "Working dir".to_string(),
                    value: working_dir,
                },
            ]
        }
        "file_write" => {
            let path = call
                .input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let content_len = call
                .input
                .get("content")
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or_default();
            vec![
                ApprovalField {
                    label: "Path".to_string(),
                    value: path,
                },
                ApprovalField {
                    label: "Content".to_string(),
                    value: format!("{content_len} chars"),
                },
            ]
        }
        "file_read" => single_approval_field("Path", &call.input, "path"),
        "file_search" => single_approval_field("Pattern", &call.input, "pattern"),
        "memory_search" | "web_search" => single_approval_field("Query", &call.input, "query"),
        "memory_write" => single_approval_field("Path", &call.input, "path"),
        "web_fetch" => single_approval_field("URL", &call.input, "url"),
        _ => serde_json::to_string_pretty(&call.input)
            .map(|value| {
                vec![ApprovalField {
                    label: "Input".to_string(),
                    value,
                }]
            })
            .unwrap_or_default(),
    }
}

fn single_approval_field(label: &str, input: &Value, field: &str) -> Vec<ApprovalField> {
    let value = input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    vec![ApprovalField {
        label: label.to_string(),
        value,
    }]
}

async fn approval_diffs_for_call(
    config: &MoaConfig,
    call: &ToolInvocation,
) -> Result<Vec<ApprovalFileDiff>> {
    if call.name != "file_write" {
        return Ok(Vec::new());
    }

    let Some(path) = call.input.get("path").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    let Some(content) = call.input.get("content").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
    let file_path = resolve_sandbox_path(&sandbox_root, path)?;
    let before = read_existing_text_file(&file_path).await?;

    Ok(vec![ApprovalFileDiff {
        path: path.to_string(),
        before,
        after: content.to_string(),
        language_hint: language_hint_for_path(path),
    }])
}

fn summarize_call_for_runtime(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "bash" => input
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "file_write" | "file_read" | "memory_write" => input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("Path: {path}"))
            .unwrap_or_else(|| tool_name.to_string()),
        "file_search" => input
            .get("pattern")
            .and_then(Value::as_str)
            .map(|pattern| format!("Pattern: {pattern}"))
            .unwrap_or_else(|| tool_name.to_string()),
        "memory_search" | "web_search" => input
            .get("query")
            .and_then(Value::as_str)
            .map(|query| format!("Query: {query}"))
            .unwrap_or_else(|| tool_name.to_string()),
        _ => tool_name.to_string(),
    }
}

fn normalized_input_for_runtime(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "bash" => input
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        "file_write" | "file_read" | "memory_write" => input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        "file_search" => input
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        "memory_search" | "web_search" => input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        "web_fetch" => input
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => serde_json::to_string(input).unwrap_or_default(),
    }
}

fn risk_level_for_tool(tool_name: &str) -> moa_core::RiskLevel {
    match tool_name {
        "file_read" | "file_search" | "memory_search" => moa_core::RiskLevel::Low,
        "file_write" | "memory_write" => moa_core::RiskLevel::Medium,
        _ => moa_core::RiskLevel::High,
    }
}

fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

async fn read_existing_text_file(path: &Path) -> Result<String> {
    match fs::read(path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn language_hint_for_path(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(ToOwned::to_owned)
}
