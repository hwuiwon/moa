//! Tokio-task local orchestrator for multi-session MOA execution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::{
    StreamedTurnResult, build_default_pipeline_with_tools, find_pending_tool_approval,
    find_resolved_pending_tool_approval, run_streamed_turn_with_signals,
};
use moa_core::{
    BrainOrchestrator, CronHandle, CronSpec, Event, EventRange, EventRecord, EventStream,
    LLMProvider, MoaConfig, MoaError, ObserveLevel, Result, RuntimeEvent, SessionFilter,
    SessionHandle, SessionId, SessionMeta, SessionSignal, SessionStatus, SessionStore,
    SessionSummary, StartSessionRequest, UserMessage,
};
use moa_hands::ToolRouter;
use moa_memory::{ConsolidationReport, FileMemoryStore};
use moa_providers::{build_provider_from_config, resolve_provider_selection};
use moa_session::TursoSessionStore;
use moa_skills::maybe_distill_skill;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::sync::CancellationToken;

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
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
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

        let orchestrator = Self {
            config,
            session_store,
            memory_store,
            llm_provider,
            tool_router,
            scheduler: Arc::new(scheduler),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        orchestrator.register_memory_maintenance_job().await?;
        Ok(orchestrator)
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
        let selection = resolve_provider_selection(&config, model_override.as_deref())?;
        config.general.default_provider = selection.provider_name;
        config.general.default_model = selection.model_id;

        let session_store = Arc::new(TursoSessionStore::from_config(&config).await?);
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone()),
        );
        let llm_provider = build_provider_from_config(&config)?;
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

    /// Returns the underlying file-backed memory store.
    pub fn memory_store(&self) -> Arc<FileMemoryStore> {
        self.memory_store.clone()
    }

    /// Returns the registered tool names exposed through the active router.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router.tool_names()
    }

    /// Returns the configured default model identifier.
    pub fn model(&self) -> &str {
        &self.config.general.default_model
    }

    /// Runs the memory consolidation maintenance check immediately.
    pub async fn run_memory_maintenance_once(&self) -> Result<Vec<ConsolidationReport>> {
        let reports = self
            .memory_store
            .run_due_consolidations(self.session_store.as_ref())
            .await?;

        for report in &reports {
            tracing::info!(
                scope = ?report.scope,
                pages_updated = report.pages_updated,
                pages_deleted = report.pages_deleted,
                contradictions_resolved = report.contradictions_resolved,
                "completed scheduled memory consolidation"
            );
        }

        Ok(reports)
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
        let cancel_token = CancellationToken::new();
        let hard_cancel_token = CancellationToken::new();
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
        let task_cancel_token = cancel_token.clone();
        let task_hard_cancel_token = hard_cancel_token.clone();
        let _task = tokio::spawn(async move {
            let result = run_session_task(
                context,
                signal_rx,
                task_event_tx,
                task_runtime_tx,
                task_status,
                initial_turn_requested,
                task_cancel_token,
                task_hard_cancel_token,
            )
            .await;
            task_finished.store(true, Ordering::SeqCst);
            result
        });

        let handle = LocalBrainHandle {
            signal_tx,
            event_tx,
            runtime_tx,
            cancel_token,
            hard_cancel_token,
            finished,
        };
        self.sessions.write().await.insert(session_id, handle);
        Ok(())
    }

    async fn register_memory_maintenance_job(&self) -> Result<()> {
        let memory_store = self.memory_store.clone();
        let session_store = self.session_store.clone();
        let job = Job::new_async("0 0 * * * *", move |_id, _lock| {
            let memory_store = memory_store.clone();
            let session_store = session_store.clone();
            Box::pin(async move {
                match memory_store
                    .run_due_consolidations(session_store.as_ref())
                    .await
                {
                    Ok(reports) => tracing::info!(
                        count = reports.len(),
                        "completed hourly memory maintenance check"
                    ),
                    Err(error) => tracing::error!(
                        error = %error,
                        "hourly memory maintenance check failed"
                    ),
                }
            })
        })
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        self.scheduler
            .add(job)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
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

        if matches!(
            signal,
            SessionSignal::SoftCancel | SessionSignal::HardCancel
        ) {
            handle.cancel_token.cancel();
        }
        if matches!(signal, SessionSignal::HardCancel) {
            handle.hard_cancel_token.cancel();
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

    /// Subscribes to live runtime events for a running local session.
    async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> Result<Option<broadcast::Receiver<RuntimeEvent>>> {
        self.ensure_session_running(session_id.clone()).await?;
        let sessions = self.sessions.read().await;
        let handle = sessions
            .get(&session_id)
            .ok_or_else(|| MoaError::SessionNotFound(session_id.clone()))?;
        Ok(Some(handle.runtime_tx.subscribe()))
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

#[allow(clippy::too_many_arguments)]
async fn run_session_task(
    context: SessionTaskContext,
    mut signal_rx: mpsc::Receiver<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    status: Arc<RwLock<SessionStatus>>,
    mut turn_requested: bool,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
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
                        &event_tx,
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
                        &event_tx,
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
        let mut soft_cancel_requested = false;
        let turn_result = run_streamed_turn_with_signals(
            context.session_id.clone(),
            context.session_store.clone(),
            context.llm_provider.clone(),
            &pipeline,
            Some(context.tool_router.clone()),
            &runtime_tx,
            Some(&event_tx),
            &mut signal_rx,
            &mut turn_requested,
            &mut queued_messages,
            &mut soft_cancel_requested,
            Some(&cancel_token),
            Some(&hard_cancel_token),
        )
        .await;

        match turn_result {
            Ok(StreamedTurnResult::Complete) => {
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
                let session = context
                    .session_store
                    .get_session(context.session_id.clone())
                    .await?;
                let events = context
                    .session_store
                    .get_events(context.session_id.clone(), EventRange::all())
                    .await?;
                if let Some(skill) = maybe_distill_skill(
                    &session,
                    &events,
                    context.memory_store.clone(),
                    context.llm_provider.clone(),
                )
                .await?
                {
                    append_event(
                        &context.session_store,
                        &event_tx,
                        context.session_id.clone(),
                        Event::MemoryWrite {
                            path: skill.path.to_string(),
                            scope: session.workspace_id.to_string(),
                            summary: format!("Distilled skill {}", skill.name),
                        },
                    )
                    .await?;
                    let _ = runtime_tx.send(RuntimeEvent::Notice(format!(
                        "Distilled skill: {}",
                        skill.name
                    )));
                }
                update_status(
                    &context.session_store,
                    &event_tx,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Completed,
                )
                .await?;
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
            }
            Ok(StreamedTurnResult::Continue) => {
                turn_requested = true;
                continue;
            }
            Ok(StreamedTurnResult::NeedsApproval(_)) => {
                continue;
            }
            Ok(StreamedTurnResult::Cancelled) => {
                flush_queued_messages(
                    &context.session_store,
                    &event_tx,
                    &context.session_id,
                    &mut queued_messages,
                )
                .await?;
                update_status(
                    &context.session_store,
                    &event_tx,
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
                    &event_tx,
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
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    next_status: SessionStatus,
) -> Result<()> {
    let previous_status = status.read().await.clone();
    if previous_status == next_status {
        return Ok(());
    }
    session_store
        .update_status(session_id.clone(), next_status.clone())
        .await?;
    append_event(
        session_store,
        event_tx,
        session_id,
        Event::SessionStatusChanged {
            from: previous_status,
            to: next_status.clone(),
        },
    )
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

    if find_pending_tool_approval(events).is_some()
        || find_resolved_pending_tool_approval(events).is_some()
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
