//! Tokio-task local orchestrator for multi-session MOA execution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::{
    StreamedTurnResult, build_default_pipeline_with_runtime, find_pending_tool_approval,
    find_resolved_pending_tool_approval, run_streamed_turn_with_signals,
    update_workspace_tool_stats,
};
use moa_core::{
    BrainOrchestrator, BranchManager, BufferedUserMessage, CronHandle, CronSpec, Event, EventRange,
    EventRecord, EventStream, LLMProvider, MoaConfig, MoaError, ObserveLevel, PendingSignal,
    Result, RuntimeEvent, SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal,
    SessionStatus, SessionStore, SessionSummary, StartSessionRequest, TraceContext, UserMessage,
};
use moa_hands::ToolRouter;
use moa_memory::{ConsolidationReport, FileMemoryStore};
use moa_providers::{build_provider_from_config, resolve_provider_selection};
use moa_session::{NeonBranchManager, SessionDatabase, create_session_store};
use moa_skills::maybe_distill_skill;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Local orchestrator backed by Tokio tasks and broadcast channels.
#[derive(Clone)]
pub struct LocalOrchestrator {
    config: MoaConfig,
    session_store: Arc<SessionDatabase>,
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
    scheduler: Arc<JobScheduler>,
    branch_manager: Option<Arc<NeonBranchManager>>,
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
    session_store: Arc<SessionDatabase>,
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
    session_id: SessionId,
}

impl LocalOrchestrator {
    /// Creates a local orchestrator from explicit component instances.
    pub async fn new(
        config: MoaConfig,
        session_store: Arc<SessionDatabase>,
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

        let branch_manager = NeonBranchManager::maybe_from_config(&config)?.map(Arc::new);
        let orchestrator = Self {
            config,
            session_store,
            memory_store,
            llm_provider,
            tool_router,
            scheduler: Arc::new(scheduler),
            branch_manager,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        orchestrator.register_memory_maintenance_job().await?;
        orchestrator.register_neon_checkpoint_cleanup_job().await?;
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

        let session_store = create_session_store(&config).await?;
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
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
    pub fn session_store(&self) -> Arc<SessionDatabase> {
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
        initial_queued_messages: Vec<BufferedUserMessage>,
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
        let task_event_tx = event_tx.clone();
        let task_runtime_tx = runtime_tx.clone();
        let task_cancel_token = cancel_token.clone();
        let task_hard_cancel_token = hard_cancel_token.clone();
        let task = tokio::spawn(async move {
            run_session_task(
                context,
                signal_rx,
                task_event_tx,
                task_runtime_tx,
                task_status,
                initial_turn_requested,
                initial_queued_messages,
                task_cancel_token,
                task_hard_cancel_token,
            )
            .await
        });
        let supervisor_session_store = self.session_store.clone();
        let supervisor_tool_router = self.tool_router.clone();
        let supervisor_status = status.clone();
        let supervisor_finished = finished.clone();
        let supervisor_event_tx = event_tx.clone();
        let supervisor_session_id = session_id.clone();
        tokio::spawn(async move {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if let Err(report_error) = report_session_task_failure(
                        &supervisor_session_store,
                        &supervisor_event_tx,
                        &supervisor_status,
                        supervisor_session_id.clone(),
                        format!("session task exited with error: {error}"),
                    )
                    .await
                    {
                        tracing::warn!(
                            session_id = %supervisor_session_id,
                            error = %report_error,
                            "failed to persist session task error"
                        );
                    }
                }
                Err(join_error) => {
                    if let Err(report_error) = report_session_task_failure(
                        &supervisor_session_store,
                        &supervisor_event_tx,
                        &supervisor_status,
                        supervisor_session_id.clone(),
                        format!("session task panicked: {join_error}"),
                    )
                    .await
                    {
                        tracing::warn!(
                            session_id = %supervisor_session_id,
                            error = %report_error,
                            "failed to persist session task panic"
                        );
                    }
                }
            }

            supervisor_tool_router
                .destroy_session_hands(&supervisor_session_id)
                .await;
            supervisor_finished.store(true, Ordering::SeqCst);
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

    async fn register_neon_checkpoint_cleanup_job(&self) -> Result<()> {
        let Some(branch_manager) = self.branch_manager.clone() else {
            return Ok(());
        };

        let job = Job::new_async("0 0 */6 * * *", move |_id, _lock| {
            let branch_manager = branch_manager.clone();
            Box::pin(async move {
                match branch_manager.cleanup_expired().await {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "cleaned up expired Neon checkpoint branches")
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        error = %error,
                        "Neon checkpoint cleanup job failed"
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
        self.spawn_session(
            session_id.clone(),
            req.initial_message.is_some(),
            Vec::new(),
        )
        .await?;
        Ok(SessionHandle { session_id })
    }

    /// Resumes an existing persisted session by spawning a new background task if needed.
    async fn resume_session(&self, session_id: SessionId) -> Result<SessionHandle> {
        if self.handle_is_active(&session_id).await {
            return Ok(SessionHandle { session_id });
        }

        let wake = self.session_store.wake(session_id.clone()).await?;
        let initial_queued_messages = wake
            .pending_signals
            .into_iter()
            .map(BufferedUserMessage::from_pending_signal)
            .collect::<Result<Vec<_>>>()?;
        let initial_turn_requested =
            session_requires_processing(&wake.session, &wake.recent_events)
                || !initial_queued_messages.is_empty();
        self.spawn_session(
            session_id.clone(),
            initial_turn_requested,
            initial_queued_messages,
        )
        .await?;
        Ok(SessionHandle { session_id })
    }

    /// Sends a signal to a running local session.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.ensure_session_running(session_id.clone()).await?;
        if let SessionSignal::QueueMessage(message) = &signal {
            let session = self.session_store.get_session(session_id.clone()).await?;
            if matches!(
                session.status,
                SessionStatus::Running | SessionStatus::WaitingApproval
            ) {
                let pending = PendingSignal::queue_message(session_id.clone(), message.clone())?;
                self.session_store
                    .store_pending_signal(session_id.clone(), pending)
                    .await?;
            }
        }
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
    mut queued_messages: Vec<BufferedUserMessage>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
) -> Result<()> {
    let pipeline = build_default_pipeline_with_runtime(
        &context.config,
        context.session_store.clone(),
        context.memory_store.clone(),
        Some(context.llm_provider.clone()),
        context.tool_router.tool_schemas(),
    );
    loop {
        if !turn_requested {
            match signal_rx.recv().await {
                Some(SessionSignal::QueueMessage(message)) => {
                    accept_user_message(
                        &context.session_store,
                        &event_tx,
                        &context.session_id,
                        message.clone(),
                        false,
                    )
                    .await?;
                    if let Some(signal_id) = resolve_matching_pending_signal(
                        &context.session_store,
                        &context.session_id,
                        &message,
                    )
                    .await?
                    {
                        best_effort_resolve_pending_signal(
                            &context.session_store,
                            &context.session_id,
                            signal_id,
                        )
                        .await?;
                    } else {
                        tracing::warn!(
                            session_id = %context.session_id,
                            text = %message.text,
                            "live queue message did not have a matching durable pending signal"
                        );
                    }
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

        let session = context
            .session_store
            .get_session(context.session_id.clone())
            .await?;
        let events = context
            .session_store
            .get_events(context.session_id.clone(), EventRange::all())
            .await?;
        if !session_requires_processing(&session, &events)
            && !queued_messages.is_empty()
            && flush_next_queued_message(
                &context.session_store,
                &event_tx,
                &context.session_id,
                &mut queued_messages,
            )
            .await?
        {
            turn_requested = true;
            continue;
        }

        turn_requested = false;
        let mut soft_cancel_requested = false;
        let turn_number = turn_number_for_events(&events);
        let trace_context =
            TraceContext::from_session_meta(&session, last_user_message_text(&events))
                .with_environment(context.config.observability.environment.clone());
        let trace_name = trace_context
            .trace_name
            .clone()
            .unwrap_or_else(|| format!("MOA turn {turn_number}"));
        let turn_root_span = tracing::info_span!(
            "session_turn",
            otel.name = %trace_name,
            moa.turn.number = turn_number,
            langfuse.trace.metadata.turn_number = turn_number,
        );
        trace_context.apply_to_span(&turn_root_span);

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
        .instrument(turn_root_span)
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
                    &context.config,
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
                refresh_workspace_tool_stats(
                    &context.session_store,
                    &context.memory_store,
                    &context.session_id,
                )
                .await;
                update_status(
                    &context.session_store,
                    &event_tx,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Completed,
                )
                .await?;
                context
                    .tool_router
                    .destroy_session_hands(&context.session_id)
                    .await;
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
                refresh_workspace_tool_stats(
                    &context.session_store,
                    &context.memory_store,
                    &context.session_id,
                )
                .await;
                update_status(
                    &context.session_store,
                    &event_tx,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Cancelled,
                )
                .await?;
                context
                    .tool_router
                    .destroy_session_hands(&context.session_id)
                    .await;
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
                return Ok(());
            }
            Err(error) => {
                let budget_exhausted = matches!(error, MoaError::BudgetExhausted(_));
                if !budget_exhausted {
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
                }
                flush_queued_messages(
                    &context.session_store,
                    &event_tx,
                    &context.session_id,
                    &mut queued_messages,
                )
                .await?;
                refresh_workspace_tool_stats(
                    &context.session_store,
                    &context.memory_store,
                    &context.session_id,
                )
                .await;
                update_status(
                    &context.session_store,
                    &event_tx,
                    &status,
                    context.session_id.clone(),
                    SessionStatus::Failed,
                )
                .await?;
                context
                    .tool_router
                    .destroy_session_hands(&context.session_id)
                    .await;
                if !budget_exhausted {
                    let _ = runtime_tx.send(RuntimeEvent::Error(error.to_string()));
                }
                let _ = runtime_tx.send(RuntimeEvent::TurnCompleted);
                return Err(error);
            }
        }
    }
}

async fn accept_user_message(
    session_store: &Arc<SessionDatabase>,
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
    session_store: &Arc<SessionDatabase>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
) -> Result<()> {
    for message in queued_messages.drain(..) {
        flush_pending_signal(session_store, event_tx, session_id, message).await?;
    }

    Ok(())
}

async fn flush_next_queued_message(
    session_store: &Arc<SessionDatabase>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    queued_messages: &mut Vec<BufferedUserMessage>,
) -> Result<bool> {
    if queued_messages.is_empty() {
        return Ok(false);
    }

    let message = queued_messages.remove(0);
    flush_pending_signal(session_store, event_tx, session_id, message).await?;
    Ok(true)
}

async fn flush_pending_signal(
    session_store: &Arc<SessionDatabase>,
    event_tx: &broadcast::Sender<EventRecord>,
    session_id: &SessionId,
    buffered: BufferedUserMessage,
) -> Result<()> {
    accept_user_message(
        session_store,
        event_tx,
        session_id,
        buffered.message.clone(),
        true,
    )
    .await?;

    if let Some(signal_id) = buffered.pending_signal_id {
        best_effort_resolve_pending_signal(session_store, session_id, signal_id).await?;
        return Ok(());
    }

    if let Some(signal_id) =
        resolve_matching_pending_signal(session_store, session_id, &buffered.message).await?
    {
        best_effort_resolve_pending_signal(session_store, session_id, signal_id).await?;
    } else {
        tracing::warn!(
            session_id = %session_id,
            text = %buffered.message.text,
            "queued message did not have a matching durable pending signal"
        );
    }
    Ok(())
}

async fn best_effort_resolve_pending_signal(
    session_store: &Arc<SessionDatabase>,
    session_id: &SessionId,
    signal_id: moa_core::PendingSignalId,
) -> Result<()> {
    match session_store
        .resolve_pending_signal(signal_id.clone())
        .await
    {
        Ok(()) => Ok(()),
        Err(MoaError::StorageError(message)) => {
            tracing::warn!(
                session_id = %session_id,
                signal_id = %signal_id,
                error = %message,
                "pending signal was already resolved before flush completed"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn resolve_matching_pending_signal(
    session_store: &Arc<SessionDatabase>,
    session_id: &SessionId,
    message: &UserMessage,
) -> Result<Option<moa_core::PendingSignalId>> {
    let pending = session_store
        .get_pending_signals(session_id.clone())
        .await?;
    for signal in pending {
        if signal.user_message()? == *message {
            return Ok(Some(signal.id));
        }
    }
    Ok(None)
}

async fn update_status(
    session_store: &Arc<SessionDatabase>,
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

async fn refresh_workspace_tool_stats(
    session_store: &Arc<SessionDatabase>,
    memory_store: &Arc<FileMemoryStore>,
    session_id: &SessionId,
) {
    if let Err(error) =
        update_workspace_tool_stats(session_store.as_ref(), memory_store.as_ref(), session_id).await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "failed to refresh workspace tool stats"
        );
    }
}

async fn append_event(
    session_store: &Arc<SessionDatabase>,
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

    events
        .iter()
        .rev()
        .find_map(|record| match record.event {
            Event::SessionStatusChanged { .. }
            | Event::Warning { .. }
            | Event::MemoryWrite { .. }
            | Event::HandDestroyed { .. }
            | Event::HandError { .. }
            | Event::Checkpoint { .. } => None,
            Event::UserMessage { .. }
            | Event::QueuedMessage { .. }
            | Event::ToolResult { .. }
            | Event::ToolError { .. }
            | Event::ApprovalDecided { .. }
            | Event::ToolCall { .. } => Some(true),
            _ => Some(false),
        })
        .unwrap_or(false)
}

async fn report_session_task_failure(
    session_store: &Arc<SessionDatabase>,
    event_tx: &broadcast::Sender<EventRecord>,
    status: &Arc<RwLock<SessionStatus>>,
    session_id: SessionId,
    message: String,
) -> Result<()> {
    let current = session_store.get_session(session_id.clone()).await?;
    if matches!(current.status, SessionStatus::Failed) {
        return Ok(());
    }

    append_event(
        session_store,
        event_tx,
        session_id.clone(),
        Event::Error {
            message,
            recoverable: false,
        },
    )
    .await?;
    update_status(
        session_store,
        event_tx,
        status,
        session_id,
        SessionStatus::Failed,
    )
    .await
}

fn turn_number_for_events(events: &[EventRecord]) -> i64 {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .count() as i64
        + 1
}

fn last_user_message_text(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}
