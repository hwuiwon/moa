//! Temporal-backed cloud orchestrator for durable MOA session execution.

use std::io;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::{
    TurnResult, build_default_pipeline_with_runtime, find_pending_tool_approval,
    find_resolved_pending_tool_approval, run_brain_turn_with_tools_stepwise,
    update_workspace_tool_stats,
};
use moa_core::{
    BrainOrchestrator, CronHandle, CronSpec, Event, EventRange, EventRecord, EventStream,
    LLMProvider, MoaConfig, MoaError, ObserveLevel, Result as MoaResult, RuntimeEvent,
    SessionFilter, SessionHandle, SessionId, SessionMeta, SessionSignal, SessionStatus,
    SessionStore, SessionSummary, StartSessionRequest, ToolCardStatus, ToolUpdate, UserMessage,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_providers::build_provider_from_config;
use moa_session::{SessionDatabase, create_session_store};
use moa_skills::maybe_distill_skill;
use serde::{Deserialize, Serialize};
use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions, UntypedSignal, UntypedWorkflow,
    UntypedWorkflowHandle, WorkflowDescribeOptions, WorkflowSignalOptions, WorkflowStartOptions,
    WorkflowTerminateOptions,
};
use temporalio_common::data_converters::{PayloadConverter, RawValue};
use temporalio_common::protos::temporal::api::common::v1::RetryPolicy;
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::activities::{ActivityContext, ActivityError};
use temporalio_sdk::{
    ActivityOptions, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext,
    WorkflowContextView, WorkflowResult, WorkflowTermination,
};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions};
use tokio::sync::broadcast;
use tokio_cron_scheduler::{Job, JobScheduler};
use url::Url;
use uuid::Uuid;

const DEFAULT_WORKFLOW_EXECUTION_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 24);
const DEFAULT_ACTIVITY_START_TO_CLOSE_TIMEOUT: Duration = Duration::from_secs(60 * 5);
const DEFAULT_BRAIN_TURN_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_ACTIVITY_RETRY_INITIAL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_ACTIVITY_RETRY_MAX_ATTEMPTS: i32 = 3;
const DEFAULT_ACTIVITY_RETRY_BACKOFF_COEFFICIENT: f64 = 2.0;
const DEFAULT_OBSERVE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Durable Temporal orchestrator for cloud session execution.
#[derive(Clone)]
pub struct TemporalOrchestrator {
    session_store: Arc<SessionDatabase>,
    scheduler: Arc<JobScheduler>,
    runtime: Arc<TemporalRuntime>,
}

struct TemporalRuntime {
    client: Client,
    task_queue: String,
    _worker_thread: std::thread::JoinHandle<()>,
}

#[derive(Clone)]
struct TemporalActivities {
    config: MoaConfig,
    session_store: Arc<SessionDatabase>,
    memory_store: Arc<FileMemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionWorkflowInput {
    session_id: SessionId,
    turn_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ApprovalSignalInput {
    request_id: Uuid,
    decision: moa_core::ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum CancelMode {
    Soft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum TemporalTurnResult {
    Complete,
    Continue,
    NeedsApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct QueueMessageActivityInput {
    session_id: SessionId,
    message: UserMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FlushQueuedMessagesActivityInput {
    session_id: SessionId,
    messages: Vec<UserMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ApprovalDecisionActivityInput {
    session_id: SessionId,
    request_id: Uuid,
    decision: moa_core::ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionStatusActivityInput {
    session_id: SessionId,
    status: SessionStatus,
}

#[workflow]
struct SessionWorkflow {
    session_id: SessionId,
    turn_requested: bool,
    waiting_for_approval: bool,
    queued_messages: Vec<UserMessage>,
    approval_decisions: Vec<ApprovalSignalInput>,
    cancel_requested: Option<CancelMode>,
}

#[workflow_methods]
impl SessionWorkflow {
    #[init]
    fn new(_ctx: &WorkflowContextView, input: SessionWorkflowInput) -> Self {
        Self {
            session_id: input.session_id,
            turn_requested: input.turn_requested,
            waiting_for_approval: false,
            queued_messages: Vec::new(),
            approval_decisions: Vec::new(),
            cancel_requested: None,
        }
    }

    #[run]
    async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<()> {
        loop {
            ctx.wait_condition(|state| {
                if state.waiting_for_approval {
                    !state.approval_decisions.is_empty() || state.cancel_requested.is_some()
                } else {
                    state.turn_requested
                        || !state.queued_messages.is_empty()
                        || !state.approval_decisions.is_empty()
                        || state.cancel_requested.is_some()
                }
            })
            .await;

            if ctx.state(|state| state.waiting_for_approval) {
                let (decision, cancel_requested, session_id) = ctx.state_mut(|state| {
                    (
                        state.approval_decisions.first().cloned(),
                        state.cancel_requested.take(),
                        state.session_id.clone(),
                    )
                });

                if let Some(cancel_mode) = cancel_requested {
                    flush_all_queued_messages(ctx, session_id.clone()).await?;
                    mark_cancelled(ctx, session_id, cancel_mode).await?;
                    return Ok(());
                }

                if let Some(decision) = decision {
                    ctx.state_mut(|state| {
                        let _ = state.approval_decisions.remove(0);
                        state.waiting_for_approval = false;
                        state.turn_requested = true;
                    });
                    apply_approval_decision(ctx, decision).await?;
                }

                continue;
            }

            if !ctx.state(|state| state.turn_requested)
                && !ctx.state(|state| state.queued_messages.is_empty())
            {
                let activity_input = ctx.state_mut(|state| QueueMessageActivityInput {
                    session_id: state.session_id.clone(),
                    message: state.queued_messages.remove(0),
                });
                ctx.start_activity(
                    TemporalActivities::append_queued_message,
                    activity_input,
                    activity_options(),
                )
                .await
                .map_err(workflow_activity_error)?;
                ctx.state_mut(|state| state.turn_requested = true);
                continue;
            }

            if !ctx.state(|state| state.turn_requested) {
                let (cancel_requested, session_id) = ctx
                    .state_mut(|state| (state.cancel_requested.take(), state.session_id.clone()));
                if let Some(cancel_mode) = cancel_requested {
                    flush_all_queued_messages(ctx, session_id.clone()).await?;
                    mark_cancelled(ctx, session_id, cancel_mode).await?;
                    return Ok(());
                }
                continue;
            }

            let session_id = ctx.state(|state| state.session_id.clone());
            let outcome = ctx
                .start_activity(
                    TemporalActivities::brain_turn,
                    session_id.clone(),
                    brain_turn_activity_options(),
                )
                .await
                .map_err(workflow_activity_error)?;
            ctx.state_mut(|state| state.turn_requested = false);

            match outcome {
                TemporalTurnResult::Continue => {
                    if ctx.state(|state| state.cancel_requested.is_some()) {
                        flush_all_queued_messages(ctx, session_id.clone()).await?;
                        mark_cancelled(ctx, session_id, CancelMode::Soft).await?;
                        return Ok(());
                    }
                    ctx.state_mut(|state| state.turn_requested = true);
                }
                TemporalTurnResult::NeedsApproval => {
                    ctx.state_mut(|state| state.waiting_for_approval = true);
                }
                TemporalTurnResult::Complete => {
                    if ctx.state(|state| state.cancel_requested.is_some()) {
                        flush_all_queued_messages(ctx, session_id.clone()).await?;
                        mark_cancelled(ctx, session_id, CancelMode::Soft).await?;
                        return Ok(());
                    }

                    if ctx.state(|state| state.queued_messages.is_empty()) {
                        ctx.start_activity(
                            TemporalActivities::finalize_completed_session,
                            session_id,
                            activity_options(),
                        )
                        .await
                        .map_err(workflow_activity_error)?;
                        return Ok(());
                    }
                }
            }
        }
    }

    #[signal]
    fn queue_message(&mut self, _ctx: &mut SyncWorkflowContext<Self>, message: UserMessage) {
        self.queued_messages.push(message);
    }

    #[signal]
    fn approval_decided(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        decision: ApprovalSignalInput,
    ) {
        self.approval_decisions.push(decision);
    }

    #[signal]
    fn request_soft_cancel(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _input: ()) {
        self.cancel_requested = Some(CancelMode::Soft);
    }
}

#[activities]
impl TemporalActivities {
    #[activity]
    async fn brain_turn(
        self: Arc<Self>,
        ctx: ActivityContext,
        session_id: SessionId,
    ) -> std::result::Result<TemporalTurnResult, ActivityError> {
        self.session_store
            .update_status(session_id.clone(), SessionStatus::Running)
            .await
            .map_err(non_retryable_activity_error)?;
        let heartbeat_ctx = ctx.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let heartbeat = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = ticker.tick() => heartbeat_ctx.record_heartbeat(Vec::new()),
                    changed = stop_rx.changed() => {
                        if changed.is_err() || *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = heartbeat_ctx.cancelled() => break,
                }
            }
        });
        let pipeline = build_default_pipeline_with_runtime(
            &self.config,
            self.session_store.clone(),
            self.memory_store.clone(),
            Some(self.llm_provider.clone()),
            self.tool_router.tool_schemas(),
        );
        let turn_result = run_brain_turn_with_tools_stepwise(
            session_id,
            self.session_store.clone(),
            self.llm_provider.clone(),
            &pipeline,
            Some(self.tool_router.clone()),
        )
        .await;
        let _ = stop_tx.send(true);
        let _ = heartbeat.await;
        let turn_result = turn_result.map_err(non_retryable_activity_error)?;

        Ok(match turn_result {
            TurnResult::Complete => TemporalTurnResult::Complete,
            TurnResult::Continue => TemporalTurnResult::Continue,
            TurnResult::NeedsApproval(_) => TemporalTurnResult::NeedsApproval,
        })
    }

    #[activity]
    async fn append_queued_message(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: QueueMessageActivityInput,
    ) -> std::result::Result<(), ActivityError> {
        self.session_store
            .emit_event(
                input.session_id,
                Event::QueuedMessage {
                    text: input.message.text,
                    queued_at: Utc::now(),
                },
            )
            .await
            .map_err(non_retryable_activity_error)?;
        Ok(())
    }

    #[activity]
    async fn append_queued_messages(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: FlushQueuedMessagesActivityInput,
    ) -> std::result::Result<(), ActivityError> {
        for message in input.messages {
            self.session_store
                .emit_event(
                    input.session_id.clone(),
                    Event::QueuedMessage {
                        text: message.text,
                        queued_at: Utc::now(),
                    },
                )
                .await
                .map_err(non_retryable_activity_error)?;
        }
        Ok(())
    }

    #[activity]
    async fn append_approval_decision(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: ApprovalDecisionActivityInput,
    ) -> std::result::Result<(), ActivityError> {
        let session = self
            .session_store
            .get_session(input.session_id.clone())
            .await
            .map_err(non_retryable_activity_error)?;
        self.session_store
            .emit_event(
                input.session_id.clone(),
                Event::ApprovalDecided {
                    request_id: input.request_id,
                    decision: input.decision,
                    decided_by: session.user_id.to_string(),
                    decided_at: Utc::now(),
                },
            )
            .await
            .map_err(non_retryable_activity_error)?;
        self.session_store
            .update_status(input.session_id, SessionStatus::Running)
            .await
            .map_err(non_retryable_activity_error)?;
        Ok(())
    }

    #[activity]
    async fn update_status(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: SessionStatusActivityInput,
    ) -> std::result::Result<(), ActivityError> {
        self.session_store
            .update_status(input.session_id, input.status)
            .await
            .map_err(non_retryable_activity_error)?;
        Ok(())
    }

    #[activity]
    async fn finalize_completed_session(
        self: Arc<Self>,
        _ctx: ActivityContext,
        session_id: SessionId,
    ) -> std::result::Result<(), ActivityError> {
        let session = self
            .session_store
            .get_session(session_id.clone())
            .await
            .map_err(non_retryable_activity_error)?;
        let events = self
            .session_store
            .get_events(session_id.clone(), EventRange::all())
            .await
            .map_err(non_retryable_activity_error)?;

        if let Some(skill) = maybe_distill_skill(
            &session,
            &events,
            self.memory_store.clone(),
            self.llm_provider.clone(),
        )
        .await
        .map_err(non_retryable_activity_error)?
        {
            self.session_store
                .emit_event(
                    session_id.clone(),
                    Event::MemoryWrite {
                        path: skill.path.to_string(),
                        scope: session.workspace_id.to_string(),
                        summary: format!("Distilled skill {}", skill.name),
                    },
                )
                .await
                .map_err(non_retryable_activity_error)?;
        }

        update_workspace_tool_stats(
            self.session_store.as_ref(),
            self.memory_store.as_ref(),
            &session_id,
        )
        .await
        .map_err(non_retryable_activity_error)?;

        self.session_store
            .update_status(session_id, SessionStatus::Completed)
            .await
            .map_err(non_retryable_activity_error)?;
        Ok(())
    }
}

impl TemporalOrchestrator {
    /// Creates a Temporal orchestrator from explicit local dependencies and cloud config.
    pub async fn new(
        config: MoaConfig,
        session_store: Arc<SessionDatabase>,
        memory_store: Arc<FileMemoryStore>,
        llm_provider: Arc<dyn LLMProvider>,
        tool_router: Arc<ToolRouter>,
    ) -> MoaResult<Self> {
        let scheduler = JobScheduler::new()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        scheduler
            .start()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;

        let runtime = Arc::new(
            TemporalRuntime::connect(
                &config,
                session_store.clone(),
                memory_store.clone(),
                llm_provider.clone(),
                tool_router.clone(),
            )
            .await?,
        );

        let orchestrator = Self {
            session_store,
            scheduler: Arc::new(scheduler),
            runtime,
        };
        Ok(orchestrator)
    }

    /// Creates a Temporal orchestrator from config using the configured LLM provider.
    pub async fn from_config(config: MoaConfig) -> MoaResult<Self> {
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

    /// Returns the connected Temporal task queue name.
    pub fn task_queue(&self) -> &str {
        &self.runtime.task_queue
    }

    /// Starts a child-like sub-session workflow linked to an existing parent session.
    pub async fn spawn_child_workflow(&self, req: StartSessionRequest) -> MoaResult<SessionHandle> {
        self.start_session(req).await
    }

    async fn ensure_workflow_started(&self, session_id: SessionId) -> MoaResult<()> {
        let handle = self.runtime.workflow_handle(session_id.clone());
        if handle
            .describe(WorkflowDescribeOptions::default())
            .await
            .is_ok()
        {
            return Ok(());
        }

        let wake = self.session_store.wake(session_id.clone()).await?;
        self.runtime
            .start_session_workflow(
                session_id,
                session_requires_processing(&wake.session, &wake.recent_events),
            )
            .await
    }

    async fn observe_live_tail(
        &self,
        session_id: SessionId,
        history: &[EventRecord],
    ) -> broadcast::Receiver<EventRecord> {
        let (tx, rx) = broadcast::channel(256);
        let session_store = self.session_store.clone();
        let mut next_seq = history
            .last()
            .map(|record| record.sequence_num.saturating_add(1))
            .unwrap_or(0);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(DEFAULT_OBSERVE_POLL_INTERVAL);
            loop {
                ticker.tick().await;
                if tx.receiver_count() == 0 {
                    break;
                }

                match session_store
                    .get_events(
                        session_id.clone(),
                        EventRange {
                            from_seq: Some(next_seq),
                            to_seq: None,
                            event_types: None,
                            limit: None,
                        },
                    )
                    .await
                {
                    Ok(events) => {
                        for record in events {
                            next_seq = record.sequence_num.saturating_add(1);
                            let _ = tx.send(record);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "temporal observe tail polling failed");
                        break;
                    }
                }
            }
        });

        rx
    }
}

impl TemporalRuntime {
    /// Connects a Temporal client and starts an in-process worker for the configured task queue.
    async fn connect(
        config: &MoaConfig,
        session_store: Arc<SessionDatabase>,
        memory_store: Arc<FileMemoryStore>,
        llm_provider: Arc<dyn LLMProvider>,
        tool_router: Arc<ToolRouter>,
    ) -> MoaResult<Self> {
        let temporal = config.cloud.temporal.as_ref().ok_or_else(|| {
            MoaError::ConfigError("cloud.temporal configuration is missing".to_string())
        })?;
        let address = temporal
            .address
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                MoaError::ConfigError("cloud.temporal.address is required".to_string())
            })?;
        let namespace = temporal
            .namespace
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                MoaError::ConfigError("cloud.temporal.namespace is required".to_string())
            })?;
        let task_queue = temporal.task_queue.clone();
        let api_key = temporal
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok());

        let client = connect_temporal_client(
            address,
            namespace,
            api_key.clone(),
            "moa-temporal-orchestrator",
        )
        .await?;
        let config_for_worker = config.clone();
        let session_store_for_worker = session_store.clone();
        let memory_store_for_worker = memory_store.clone();
        let llm_provider_for_worker = llm_provider.clone();
        let tool_router_for_worker = tool_router.clone();
        let address_for_worker = address.to_string();
        let namespace_for_worker = namespace.to_string();
        let task_queue_for_worker = task_queue.clone();
        let worker_thread = std::thread::Builder::new()
            .name(format!("moa-temporal-worker-{task_queue}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(error = %error, "failed to build Temporal worker runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let core_runtime = match RuntimeOptions::builder().build().map_err(|error| {
                        MoaError::ProviderError(format!(
                            "failed to build Temporal runtime options: {error}"
                        ))
                    }).and_then(|options| {
                        CoreRuntime::new_assume_tokio(options).map_err(|error| {
                            MoaError::ProviderError(format!(
                                "failed to create Temporal runtime: {error}"
                            ))
                        })
                    }) {
                        Ok(core_runtime) => core_runtime,
                        Err(error) => {
                            tracing::error!(error = %error, "failed to create Temporal worker runtime");
                            return;
                        }
                    };
                    let worker_client = match connect_temporal_client(
                        &address_for_worker,
                        &namespace_for_worker,
                        api_key,
                        "moa-temporal-worker",
                    )
                    .await
                    {
                        Ok(client) => client,
                        Err(error) => {
                            tracing::error!(error = %error, "failed to connect Temporal worker client");
                            return;
                        }
                    };
                    let worker_activities = TemporalActivities {
                        config: config_for_worker,
                        session_store: session_store_for_worker,
                        memory_store: memory_store_for_worker,
                        llm_provider: llm_provider_for_worker,
                        tool_router: tool_router_for_worker,
                    };
                    let worker_options = WorkerOptions::new(task_queue_for_worker.clone())
                        .register_activities(worker_activities)
                        .register_workflow::<SessionWorkflow>()
                        .build();
                    let mut worker = match Worker::new(&core_runtime, worker_client, worker_options)
                    {
                        Ok(worker) => worker,
                        Err(error) => {
                            tracing::error!(error = %error, "failed to create Temporal worker");
                            return;
                        }
                    };
                    if let Err(error) = worker.run().await {
                        tracing::error!(error = %error, "temporal worker exited with an error");
                    }
                });
            })
            .map_err(|error| MoaError::ProviderError(format!("failed to spawn Temporal worker thread: {error}")))?;

        Ok(Self {
            client,
            task_queue,
            _worker_thread: worker_thread,
        })
    }

    async fn start_session_workflow(
        &self,
        session_id: SessionId,
        turn_requested: bool,
    ) -> MoaResult<()> {
        let workflow_id = workflow_id_for_session(&session_id);
        self.client
            .start_workflow(
                SessionWorkflow::run,
                SessionWorkflowInput {
                    session_id,
                    turn_requested,
                },
                WorkflowStartOptions::new(self.task_queue.clone(), workflow_id)
                    .execution_timeout(DEFAULT_WORKFLOW_EXECUTION_TIMEOUT)
                    .build(),
            )
            .await
            .map(|_| ())
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to start Temporal workflow: {error}"))
            })
    }

    fn workflow_handle(&self, session_id: SessionId) -> UntypedWorkflowHandle<Client> {
        self.client
            .get_workflow_handle::<UntypedWorkflow>(workflow_id_for_session(&session_id))
    }
}

#[async_trait]
impl BrainOrchestrator for TemporalOrchestrator {
    /// Starts a new durable Temporal workflow for the session.
    async fn start_session(&self, req: StartSessionRequest) -> MoaResult<SessionHandle> {
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
        self.session_store
            .emit_event(
                session_id.clone(),
                Event::SessionCreated {
                    workspace_id: req.workspace_id.to_string(),
                    user_id: req.user_id.to_string(),
                    model: req.model,
                },
            )
            .await?;
        let has_initial_message = req.initial_message.is_some();
        if let Some(message) = req.initial_message {
            self.session_store
                .emit_event(
                    session_id.clone(),
                    Event::UserMessage {
                        text: message.text,
                        attachments: message.attachments,
                    },
                )
                .await?;
        }
        self.runtime
            .start_session_workflow(session_id.clone(), has_initial_message)
            .await?;
        Ok(SessionHandle { session_id })
    }

    /// Resumes an existing Temporal workflow or restarts it from the persisted session log.
    async fn resume_session(&self, session_id: SessionId) -> MoaResult<SessionHandle> {
        self.ensure_workflow_started(session_id.clone()).await?;
        Ok(SessionHandle { session_id })
    }

    /// Delivers a queue, approval, or cancel signal to a Temporal workflow.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> MoaResult<()> {
        self.ensure_workflow_started(session_id.clone()).await?;
        let handle = self.runtime.workflow_handle(session_id.clone());

        match signal {
            SessionSignal::QueueMessage(message) => handle
                .signal(
                    UntypedSignal::new("queue_message"),
                    raw_temporal_value(&message),
                    WorkflowSignalOptions::default(),
                )
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to signal Temporal workflow: {error}"))
                }),
            SessionSignal::ApprovalDecided {
                request_id,
                decision,
            } => handle
                .signal(
                    UntypedSignal::new("approval_decided"),
                    raw_temporal_value(&ApprovalSignalInput {
                        request_id,
                        decision,
                    }),
                    WorkflowSignalOptions::default(),
                )
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to signal Temporal workflow: {error}"))
                }),
            SessionSignal::SoftCancel => handle
                .signal(
                    UntypedSignal::new("request_soft_cancel"),
                    raw_temporal_value(&()),
                    WorkflowSignalOptions::default(),
                )
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to signal Temporal workflow: {error}"))
                }),
            SessionSignal::HardCancel => {
                self.session_store
                    .update_status(session_id.clone(), SessionStatus::Cancelled)
                    .await?;
                handle
                    .terminate(
                        WorkflowTerminateOptions::builder()
                            .reason("hard cancel requested")
                            .build(),
                    )
                    .await
                    .map_err(|error| {
                        MoaError::ProviderError(format!(
                            "failed to terminate Temporal workflow: {error}"
                        ))
                    })
            }
        }
    }

    /// Lists sessions from the durable session store.
    async fn list_sessions(&self, filter: SessionFilter) -> MoaResult<Vec<SessionSummary>> {
        self.session_store.list_sessions(filter).await
    }

    /// Returns buffered history plus a polling live tail backed by the session store.
    async fn observe(&self, session_id: SessionId, _level: ObserveLevel) -> MoaResult<EventStream> {
        let history = self
            .session_store
            .get_events(session_id.clone(), EventRange::all())
            .await?;
        let receiver = self.observe_live_tail(session_id, &history).await;
        Ok(EventStream::from_history_and_broadcast(history, receiver))
    }

    /// Returns a polling runtime stream synthesized from persisted session events.
    async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> MoaResult<Option<broadcast::Receiver<RuntimeEvent>>> {
        self.session_store.get_session(session_id.clone()).await?;
        let (tx, rx) = broadcast::channel(256);
        let session_store = self.session_store.clone();
        tokio::spawn(async move {
            let mut last_seq = 0_u64;
            loop {
                tokio::time::sleep(DEFAULT_OBSERVE_POLL_INTERVAL).await;
                let events = session_store
                    .get_events(
                        session_id.clone(),
                        EventRange {
                            from_seq: Some(last_seq + 1),
                            ..EventRange::all()
                        },
                    )
                    .await;
                let new_events = match events {
                    Ok(events) => events,
                    Err(error) => {
                        tracing::warn!(error = %error, "Temporal runtime polling failed");
                        return;
                    }
                };

                for record in &new_events {
                    last_seq = record.sequence_num;
                    if let Some(event) = event_to_runtime_event(record)
                        && tx.send(event).is_err()
                    {
                        return;
                    }
                }

                match session_store.get_session(session_id.clone()).await {
                    Ok(session) if session_is_terminal(&session.status) => {
                        let _ = tx.send(RuntimeEvent::TurnCompleted);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "Temporal runtime session lookup failed");
                        return;
                    }
                }
            }
        });
        Ok(Some(rx))
    }

    /// Registers a Temporal-cloud cron hook using the existing local scheduler wrapper.
    async fn schedule_cron(&self, spec: CronSpec) -> MoaResult<CronHandle> {
        let job_name = spec.name.clone();
        let task_name = spec.task.clone();
        let job = Job::new_async(spec.schedule.as_str(), move |_id, _lock| {
            let job_name = job_name.clone();
            let task_name = task_name.clone();
            Box::pin(async move {
                tracing::info!(job = %job_name, task = %task_name, "running scheduled Temporal job");
            })
        })
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let job_id = job.guid().to_string();
        self.scheduler
            .add(job)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(CronHandle::Temporal { id: job_id })
    }
}

async fn flush_all_queued_messages(
    ctx: &WorkflowContext<SessionWorkflow>,
    session_id: SessionId,
) -> WorkflowResult<()> {
    let messages = ctx.state_mut(|state| std::mem::take(&mut state.queued_messages));
    if messages.is_empty() {
        return Ok(());
    }
    ctx.start_activity(
        TemporalActivities::append_queued_messages,
        FlushQueuedMessagesActivityInput {
            session_id,
            messages,
        },
        activity_options(),
    )
    .await
    .map_err(workflow_activity_error)?;
    Ok(())
}

async fn apply_approval_decision(
    ctx: &WorkflowContext<SessionWorkflow>,
    decision: ApprovalSignalInput,
) -> WorkflowResult<()> {
    let session_id = ctx.state(|state| state.session_id.clone());
    ctx.start_activity(
        TemporalActivities::append_approval_decision,
        ApprovalDecisionActivityInput {
            session_id,
            request_id: decision.request_id,
            decision: decision.decision,
        },
        activity_options(),
    )
    .await
    .map_err(workflow_activity_error)?;
    Ok(())
}

async fn mark_cancelled(
    ctx: &WorkflowContext<SessionWorkflow>,
    session_id: SessionId,
    _mode: CancelMode,
) -> WorkflowResult<()> {
    ctx.start_activity(
        TemporalActivities::update_status,
        SessionStatusActivityInput {
            session_id,
            status: SessionStatus::Cancelled,
        },
        activity_options(),
    )
    .await
    .map_err(workflow_activity_error)?;
    Ok(())
}

fn activity_options() -> ActivityOptions {
    ActivityOptions {
        start_to_close_timeout: Some(DEFAULT_ACTIVITY_START_TO_CLOSE_TIMEOUT),
        retry_policy: Some(RetryPolicy {
            initial_interval: Some(
                DEFAULT_ACTIVITY_RETRY_INITIAL_INTERVAL
                    .try_into()
                    .expect("default activity retry interval must convert"),
            ),
            backoff_coefficient: DEFAULT_ACTIVITY_RETRY_BACKOFF_COEFFICIENT,
            maximum_attempts: DEFAULT_ACTIVITY_RETRY_MAX_ATTEMPTS,
            ..RetryPolicy::default()
        }),
        ..ActivityOptions::default()
    }
}

fn event_to_runtime_event(record: &EventRecord) -> Option<RuntimeEvent> {
    match &record.event {
        Event::BrainResponse { text, .. } => {
            Some(RuntimeEvent::AssistantFinished { text: text.clone() })
        }
        Event::ApprovalRequested { prompt, .. } => {
            Some(RuntimeEvent::ApprovalRequested(prompt.clone()))
        }
        Event::ToolCall {
            tool_id, tool_name, ..
        } => Some(RuntimeEvent::ToolUpdate(ToolUpdate {
            tool_id: *tool_id,
            tool_name: tool_name.clone(),
            status: ToolCardStatus::Pending,
            summary: format!("Queued {}", tool_name),
            detail: None,
        })),
        Event::ToolResult {
            tool_id,
            success,
            output,
            ..
        } => Some(RuntimeEvent::ToolUpdate(ToolUpdate {
            tool_id: *tool_id,
            tool_name: "tool".to_string(),
            status: if *success {
                ToolCardStatus::Succeeded
            } else {
                ToolCardStatus::Failed
            },
            summary: output
                .to_text()
                .lines()
                .next()
                .unwrap_or("tool completed")
                .to_string(),
            detail: None,
        })),
        Event::ToolError { tool_id, error, .. } => Some(RuntimeEvent::ToolUpdate(ToolUpdate {
            tool_id: *tool_id,
            tool_name: "tool".to_string(),
            status: ToolCardStatus::Failed,
            summary: "Tool failed".to_string(),
            detail: Some(error.clone()),
        })),
        Event::Warning { message } => Some(RuntimeEvent::Notice(message.clone())),
        Event::Error { message, .. } => Some(RuntimeEvent::Error(message.clone())),
        _ => None,
    }
}

fn session_is_terminal(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    )
}

fn brain_turn_activity_options() -> ActivityOptions {
    ActivityOptions {
        heartbeat_timeout: Some(DEFAULT_BRAIN_TURN_HEARTBEAT_TIMEOUT),
        ..activity_options()
    }
}

fn workflow_activity_error(error: temporalio_sdk::ActivityExecutionError) -> WorkflowTermination {
    WorkflowTermination::failed(io::Error::other(error.to_string()))
}

fn non_retryable_activity_error(error: MoaError) -> ActivityError {
    ActivityError::NonRetryable(Box::new(io::Error::other(error.to_string())))
}

async fn connect_temporal_client(
    address: &str,
    namespace: &str,
    api_key: Option<String>,
    identity: &str,
) -> MoaResult<Client> {
    let mut connection_options = ConnectionOptions::new(normalize_temporal_address(address)?)
        .identity(identity)
        .build();
    connection_options.api_key = api_key;

    let connection = Connection::connect(connection_options)
        .await
        .map_err(|error| {
            MoaError::ProviderError(format!("failed to connect to Temporal: {error}"))
        })?;
    Client::new(connection, ClientOptions::new(namespace).build()).map_err(|error| {
        MoaError::ProviderError(format!("failed to build Temporal client: {error}"))
    })
}

fn normalize_temporal_address(address: &str) -> MoaResult<Url> {
    let trimmed = address.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.starts_with("localhost")
        || trimmed.starts_with("127.0.0.1")
        || trimmed.starts_with("[::1]")
    {
        format!("http://{trimmed}")
    } else {
        format!("https://{trimmed}")
    };
    Url::from_str(&with_scheme).map_err(|error| {
        MoaError::ConfigError(format!("invalid Temporal address {trimmed}: {error}"))
    })
}

fn workflow_id_for_session(session_id: &SessionId) -> String {
    format!("moa-session-{session_id}")
}

fn raw_temporal_value<T>(value: &T) -> RawValue
where
    T: Serialize + 'static,
{
    RawValue::from_value(value, &PayloadConverter::serde_json())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_cloud_addresses_with_https_by_default() {
        let url = normalize_temporal_address("example.tmprl.cloud:7233").expect("url");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("example.tmprl.cloud"));
    }

    #[test]
    fn normalizes_local_addresses_with_http() {
        let url = normalize_temporal_address("127.0.0.1:7233").expect("url");
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
    }

    #[test]
    fn workflow_ids_are_stable_and_prefixed() {
        let session_id =
            SessionId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("uuid"));
        assert_eq!(
            workflow_id_for_session(&session_id),
            "moa-session-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        );
    }
}
