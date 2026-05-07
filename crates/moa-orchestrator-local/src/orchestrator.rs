//! Inherent local orchestrator construction and lifecycle helpers.

use crate::*;

impl LocalOrchestrator {
    /// Creates a local orchestrator from explicit component instances.
    pub async fn new(
        config: MoaConfig,
        session_store: Arc<PostgresSessionStore>,
        model_router: Arc<ModelRouter>,
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
        let instrumented_session_store: Arc<dyn SessionStore> =
            Arc::new(CountedSessionStore::new(session_store.clone()));
        let graph_pool = session_store.pool().clone();
        let _ = memory_ingest::install_runtime_with_pool(graph_pool.clone());
        let (lineage, lineage_writer) = build_lineage_sink(&config, graph_pool.clone()).await?;
        let session_task_monitor = SessionTaskMonitor::shared();
        let orchestrator = Self {
            config: Arc::new(config),
            session_store,
            instrumented_session_store,
            graph_pool,
            model_router,
            tool_router,
            lineage,
            lineage_writer,
            scheduler: Arc::new(scheduler),
            branch_manager,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            discovered_workspace_instructions: Arc::new(RwLock::new(HashMap::new())),
            session_task_monitor,
        };
        orchestrator
            .session_task_monitor
            .spawn_publisher(orchestrator.config.metrics.enabled);
        orchestrator.register_memory_maintenance_job().await?;
        orchestrator.register_neon_checkpoint_cleanup_job().await?;
        // Non-fatal: if the sweep errors out, the app still boots and the
        // empty sessions just linger until the next startup.
        if let Err(err) = orchestrator.prune_empty_sessions().await {
            tracing::warn!(%err, "prune_empty_sessions skipped on startup");
        }
        Ok(orchestrator)
    }

    /// Returns lineage writer statistics when durable lineage capture is enabled.
    pub fn lineage_writer_stats(&self) -> Option<moa_lineage_sink::WriterStats> {
        self.lineage_writer.as_ref().map(|writer| writer.stats())
    }

    /// Gracefully drains and shuts down the lineage writer when capture is enabled.
    pub async fn shutdown_lineage_writer(&self) -> Result<Option<moa_lineage_sink::WriterStats>> {
        let Some(writer) = &self.lineage_writer else {
            return Ok(None);
        };
        let stats = writer.shutdown().await.map_err(|error| {
            MoaError::StorageError(format!("lineage writer shutdown failed: {error}"))
        })?;
        Ok(Some(stats))
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
        config.set_main_model(selection.provider_name, selection.model_id);

        let session_store = create_session_store(&config).await?;
        let tool_router = Arc::new(
            ToolRouter::from_config(&config)
                .await?
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
        );
        let model_router = Arc::new(ModelRouter::from_config(&config)?);
        Self::new(config, session_store, model_router, tool_router).await
    }

    /// Returns the underlying local session store.
    pub fn session_store(&self) -> Arc<PostgresSessionStore> {
        self.session_store.clone()
    }

    /// Returns the registered tool names exposed through the active router.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router.tool_names()
    }

    /// Returns the configured default model identifier.
    pub fn model(&self) -> &str {
        self.config.model_for_task(ModelTask::MainLoop)
    }

    /// Deletes sessions whose event log contains no user-authored events.
    ///
    /// A fresh session is created eagerly when the UI clicks `+ New Session`
    /// — which writes a `sessions` row plus a `SessionCreated` event. If
    /// the user never submits a prompt the session just clutters the
    /// sidebar, so this sweep drops any session whose only events are
    /// bookkeeping (`SessionCreated`, `SessionStatusChanged`, notices).
    /// Invoked at orchestrator startup. Returns the number pruned.
    pub async fn prune_empty_sessions(&self) -> Result<u32> {
        let sessions = self
            .session_store
            .list_sessions(SessionFilter::default())
            .await?;

        let mut pruned: u32 = 0;
        for summary in sessions {
            // Skip sessions that are actively running — a brain task might
            // be in the middle of persisting its first user message.
            if matches!(summary.status, SessionStatus::Running) {
                continue;
            }
            // The first user-authored event lands within the first few
            // records of any session, so a small `limit` keeps the
            // startup probe O(1)-ish per session instead of loading
            // entire histories just to confirm activity.
            let events = self
                .session_store
                .get_events(summary.session_id, EventRange::recent(16))
                .await?;
            let has_user_input = events.iter().any(|rec| {
                matches!(
                    rec.event,
                    Event::UserMessage { .. } | Event::QueuedMessage { .. }
                )
            });
            if !has_user_input {
                if let Err(err) = self.session_store.delete_session(summary.session_id).await {
                    tracing::warn!(
                        %err,
                        session_id = %summary.session_id,
                        "prune_empty_sessions: delete failed",
                    );
                    continue;
                }
                pruned = pruned.saturating_add(1);
            }
        }
        if pruned > 0 {
            tracing::info!(pruned, "prune_empty_sessions removed empty sessions");
        }
        Ok(pruned)
    }

    /// Runs the graph-memory maintenance check immediately.
    pub async fn run_memory_maintenance_once(&self) -> Result<Vec<GraphMemoryMaintenanceReport>> {
        tracing::debug!("graph memory maintenance has no scheduled local work");
        Ok(Vec::new())
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

    pub(super) async fn handle_is_active(&self, session_id: &SessionId) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|handle| !handle.finished.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    pub(super) async fn wait_for_handle_shutdown(&self, session_id: &SessionId) {
        for _ in 0..300 {
            if !self.handle_is_active(session_id).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(super) async fn spawn_session(
        &self,
        session_id: SessionId,
        initial_turn_requested: bool,
        initial_queued_messages: Vec<BufferedUserMessage>,
    ) -> Result<()> {
        let (signal_tx, signal_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(64);
        let (runtime_tx, _) = broadcast::channel(128);
        let cancel_token = CancellationToken::new();
        let hard_cancel_token = CancellationToken::new();
        let session = self.session_store.get_session(session_id).await?;
        let status = Arc::new(RwLock::new(session.status.clone()));
        if initial_turn_requested
            && !matches!(
                session.status,
                SessionStatus::Running | SessionStatus::WaitingApproval
            )
        {
            update_status(
                &self.instrumented_session_store,
                &event_tx,
                &status,
                session_id,
                SessionStatus::Running,
            )
            .await?;
        }
        let finished = Arc::new(AtomicBool::new(false));
        let context = SessionTaskContext {
            config: Arc::clone(&self.config),
            session_store: self.instrumented_session_store.clone(),
            graph_pool: self.graph_pool.clone(),
            model_router: self.model_router.clone(),
            tool_router: self.tool_router.clone(),
            lineage: self.lineage.clone(),
            session_id,
            discovered_workspace_instructions: self
                .discovered_workspace_instructions
                .read()
                .await
                .get(&session.workspace_id)
                .cloned(),
        };
        let task_status = status.clone();
        let task_event_tx = event_tx.clone();
        let task_runtime_tx = runtime_tx.clone();
        let task_cancel_token = cancel_token.clone();
        let task_hard_cancel_token = hard_cancel_token.clone();
        let task = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(run_session_task(
                context,
                signal_rx,
                task_event_tx,
                task_runtime_tx,
                task_status,
                initial_turn_requested,
                initial_queued_messages,
                task_cancel_token,
                task_hard_cancel_token,
            ))
        });
        let supervisor_session_store = self.instrumented_session_store.clone();
        let supervisor_tool_router = self.tool_router.clone();
        let supervisor_status = status.clone();
        let supervisor_finished = finished.clone();
        let supervisor_event_tx = event_tx.clone();
        let supervisor_session_id = session_id;
        tokio::spawn(async move {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    // Recoverable errors (provider quirks, single bad
                    // payloads, transient stream problems) should not
                    // mark the session Failed — they pause it so the
                    // user can resume or retry. Fatal errors (auth,
                    // config, storage) keep the old Failed path.
                    let is_fatal = error.is_fatal();
                    let message = format!("session task exited with error: {error}");
                    let report_result = if is_fatal {
                        report_session_task_failure(
                            &supervisor_session_store,
                            &supervisor_event_tx,
                            &supervisor_status,
                            supervisor_session_id,
                            message,
                        )
                        .await
                    } else {
                        report_session_task_paused(
                            &supervisor_session_store,
                            &supervisor_event_tx,
                            &supervisor_status,
                            supervisor_session_id,
                            message,
                        )
                        .await
                    };
                    if let Err(report_error) = report_result {
                        tracing::warn!(
                            session_id = %supervisor_session_id,
                            error = %report_error,
                            fatal = is_fatal,
                            "failed to persist session task outcome"
                        );
                    }
                }
                Err(join_error) => {
                    if let Err(report_error) = report_session_task_failure(
                        &supervisor_session_store,
                        &supervisor_event_tx,
                        &supervisor_status,
                        supervisor_session_id,
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
        let job = Job::new_async("0 0 * * * *", move |_id, _lock| {
            Box::pin(async move {
                tracing::debug!("hourly graph memory maintenance check has no local work");
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
                        tracing::info!(count, "cleaned up expired Neon checkpoint branches");
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

    pub(super) async fn maybe_bootstrap_workspace_memory(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Option<GraphBootstrapReport> {
        if !self.config.memory.auto_bootstrap {
            return None;
        }

        match workspace_has_graph_nodes(&self.graph_pool, workspace_id).await {
            Ok(true) => return None,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = %error,
                    "failed to inspect graph memory bootstrap state"
                );
                return None;
            }
        }

        let instructions = self
            .discovered_workspace_instructions
            .read()
            .await
            .get(workspace_id)
            .cloned()?;

        let workspace_path = match detect_workspace_path(workspace_id).await {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = %error,
                    "failed to resolve workspace path for memory bootstrap"
                );
                return None;
            }
        };
        let source_file = workspace_path.join("AGENTS.md");
        let source_file_display = source_file.display().to_string();

        tracing::info!(
            workspace_id = %workspace_id,
            source_file = %source_file_display,
            "empty graph memory detected; bootstrapping from workspace instruction file"
        );

        let turn = SessionTurn {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("system"),
            session_id: SessionId::new(),
            turn_seq: 0,
            transcript: format!("Workspace instructions from AGENTS.md:\n\n{instructions}"),
            dominant_pii_class: "pii".to_string(),
            finalized_at: Utc::now(),
        };
        match ingest_turn_direct_with_pool(self.graph_pool.clone(), turn).await {
            Ok(report) => {
                tracing::info!(
                    workspace_id = %workspace_id,
                    inserted = report.inserted,
                    superseded = report.superseded,
                    skipped = report.skipped,
                    failed = report.failed,
                    "workspace graph memory bootstrapped from instruction file"
                );
                Some(GraphBootstrapReport {
                    source_file: Some(source_file_display),
                    ingest: report,
                })
            }
            Err(error) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = ?error,
                    "workspace graph memory bootstrap failed"
                );
                None
            }
        }
    }

    /// Registers the filesystem root for a logical workspace with the tool router.
    pub async fn remember_workspace_root(
        &self,
        workspace_id: WorkspaceId,
        workspace_root: PathBuf,
    ) {
        let discovered_instructions =
            moa_core::workspace::discover_workspace_instructions(&workspace_root);
        let mut discovered_workspace_instructions =
            self.discovered_workspace_instructions.write().await;
        if let Some(instructions) = discovered_instructions {
            discovered_workspace_instructions.insert(workspace_id.clone(), instructions);
        } else {
            discovered_workspace_instructions.remove(&workspace_id);
        }
        drop(discovered_workspace_instructions);

        self.tool_router
            .remember_workspace_root(workspace_id.clone(), workspace_root.clone())
            .await;
        tracing::debug!(
            workspace_id = %workspace_id,
            workspace_path = %workspace_root.display(),
            "registered workspace root for local tools"
        );
    }

    pub(super) async fn remember_detected_workspace_root(&self, workspace_id: &WorkspaceId) {
        if let Some(workspace_root) = self.tool_router.workspace_root(workspace_id).await {
            let discovered_instructions =
                moa_core::workspace::discover_workspace_instructions(&workspace_root);
            let mut discovered_workspace_instructions =
                self.discovered_workspace_instructions.write().await;
            if let Some(instructions) = discovered_instructions {
                discovered_workspace_instructions.insert(workspace_id.clone(), instructions);
            } else {
                discovered_workspace_instructions.remove(workspace_id);
            }
            return;
        }

        match detect_workspace_path(workspace_id).await {
            Ok(workspace_path) => {
                self.remember_workspace_root(workspace_id.clone(), workspace_path.clone())
                    .await;
            }
            Err(error) => {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    error = %error,
                    "failed to resolve workspace root for local tools"
                );
            }
        }
    }
}
