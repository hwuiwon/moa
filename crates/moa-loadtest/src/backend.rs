//! Local and daemon load-test backend adapters.

use crate::*;

#[async_trait]
pub(crate) trait SessionTarget: Send + Sync {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId>;
    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation>;
    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta>;
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
    async fn cleanup(&self) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct LocalTarget {
    orchestrator: Arc<LocalOrchestrator>,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
    mock_provider: Option<Arc<PerSessionScriptedProvider>>,
    database_url: String,
    schema_name: String,
    _scratch_dir: Arc<TempDir>,
}

#[async_trait]
impl SessionTarget for LocalTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        let session_id = self
            .orchestrator
            .start_session(StartSessionRequest {
                workspace_id: self.workspace_id.clone(),
                user_id: self.user_id.clone(),
                platform: Platform::Cli,
                model: self.model.clone(),
                initial_message: None,
                title: Some(plan.title.clone()),
                parent_session_id: None,
            })
            .await?
            .session_id;
        if let Some(mock_provider) = &self.mock_provider {
            mock_provider.register_session(&session_id, plan)?;
        }
        Ok(session_id)
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let runtime_rx = match self.orchestrator.observe_runtime(session_id).await? {
            Some(runtime_rx) => runtime_rx,
            None => {
                self.orchestrator.resume_session(session_id).await?;
                self.orchestrator
                    .observe_runtime(session_id)
                    .await?
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "runtime observation unavailable for session {session_id}"
                        ))
                    })?
            }
        };
        let runtime_rx = Arc::new(Mutex::new(runtime_rx));
        self.orchestrator
            .signal(
                session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
        let orchestrator = self.orchestrator.clone();
        wait_for_turn_completion(
            timeout,
            move || {
                let runtime_rx = runtime_rx.clone();
                async move {
                    let mut runtime_rx = runtime_rx.lock().await;
                    runtime_rx.recv().await.map_err(map_broadcast_error)
                }
            },
            move |request_id| {
                let orchestrator = orchestrator.clone();
                async move {
                    orchestrator
                        .signal(
                            session_id,
                            SessionSignal::ApprovalDecided {
                                request_id,
                                decision: ApprovalDecision::Deny {
                                    reason: Some("auto-denied by moa-loadtest".to_string()),
                                },
                            },
                        )
                        .await
                }
            },
        )
        .await
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.orchestrator.get_session(session_id).await
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.orchestrator
            .session_store()
            .get_events(session_id, moa_core::EventRange::all())
            .await
    }

    async fn cleanup(&self) -> Result<()> {
        self.orchestrator.session_store().pool().close().await;
        cleanup_test_schema(&self.database_url, &self.schema_name).await
    }
}

#[derive(Clone)]
pub(crate) struct DaemonTarget {
    socket_path: PathBuf,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for DaemonTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::CreateSession {
                request: StartSessionRequest {
                    workspace_id: self.workspace_id.clone(),
                    user_id: self.user_id.clone(),
                    platform: Platform::Cli,
                    model: self.model.clone(),
                    initial_message: None,
                    title: Some(plan.title.clone()),
                    parent_session_id: None,
                },
            },
        )
        .await?
        {
            DaemonReply::SessionId(session_id) => Ok(session_id),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_id", &other)),
        }
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let reader = Arc::new(Mutex::new(
            daemon_open_stream(
                &self.socket_path,
                &DaemonCommand::ObserveSession { session_id },
            )
            .await?,
        ));
        daemon_expect_ack(
            &self.socket_path,
            &DaemonCommand::QueueMessage {
                session_id,
                prompt: prompt.to_string(),
            },
        )
        .await?;
        let socket_path = self.socket_path.clone();
        wait_for_turn_completion(
            timeout,
            move || {
                let reader = reader.clone();
                async move {
                    let mut reader = reader.lock().await;
                    daemon_recv_runtime_event(&mut reader).await
                }
            },
            move |request_id| {
                let socket_path = socket_path.clone();
                async move {
                    daemon_expect_ack(
                        &socket_path,
                        &DaemonCommand::RespondToApproval {
                            session_id,
                            request_id,
                            decision: ApprovalDecision::Deny {
                                reason: Some("auto-denied by moa-loadtest".to_string()),
                            },
                        },
                    )
                    .await
                }
            },
        )
        .await
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        match daemon_request(&self.socket_path, &DaemonCommand::GetSession { session_id }).await? {
            DaemonReply::Session(session) => Ok(session),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session", &other)),
        }
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        match daemon_request(
            &self.socket_path,
            &DaemonCommand::GetSessionEvents { session_id },
        )
        .await?
        {
            DaemonReply::SessionEvents(events) => Ok(events),
            DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
            other => Err(unexpected_daemon_reply("session_events", &other)),
        }
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
}

pub(crate) async fn build_backend(
    options: &LoadTestOptions,
    config: &mut MoaConfig,
    workspace_root: Option<PathBuf>,
) -> Result<Arc<dyn SessionTarget>> {
    match options.target {
        LoadTarget::Local => build_local_target(options, config, workspace_root).await,
        LoadTarget::Daemon => build_daemon_target(options, config).await,
    }
}

pub(crate) async fn build_local_target(
    options: &LoadTestOptions,
    config: &mut MoaConfig,
    workspace_root: Option<PathBuf>,
) -> Result<Arc<dyn SessionTarget>> {
    let workspace_root = workspace_root.ok_or_else(|| {
        MoaError::ValidationError("local target requires a workspace root".to_string())
    })?;
    // The in-process loadtest target always uses an isolated Postgres schema so it
    // exercises the real session-store path without polluting a user's configured DB.
    config.database.url = test_database_url();
    let scratch_dir = tempfile::tempdir()
        .map_err(|error| MoaError::ProviderError(format!("failed to create tempdir: {error}")))?;
    config.local.memory_dir = scratch_dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = scratch_dir.path().join("sandbox").display().to_string();
    config.local.docker_enabled = false;
    let schema_name = format!("moa_loadtest_{}", Uuid::now_v7().simple());
    let session_store = Arc::new(
        PostgresSessionStore::new_in_schema(config.database.runtime_url(), &schema_name).await?,
    );
    let tool_router = Arc::new(
        ToolRouter::from_config(config)
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );

    let mock_provider = match options.mode {
        LoadMode::Mock => Some(Arc::new(PerSessionScriptedProvider::new(
            scripted_capabilities(),
        ))),
        LoadMode::Live => None,
    };
    let model_router = match options.mode {
        LoadMode::Mock => Arc::new(ModelRouter::new(
            mock_provider.clone().ok_or_else(|| {
                MoaError::ProviderError(
                    "mock mode requires a per-session scripted provider".to_string(),
                )
            })?,
            None,
        )),
        LoadMode::Live => {
            if let Some(model) = options.model.as_deref() {
                let selection = resolve_provider_selection(config, Some(model))?;
                config.set_main_model(selection.provider_name, selection.model_id);
            }
            Arc::new(ModelRouter::from_config(config)?)
        }
    };
    let model = model_router
        .provider_for(ModelTask::MainLoop)
        .capabilities()
        .model_id;
    let workspace_id = workspace_id_for_root(&workspace_root, "local");
    let orchestrator = Arc::new(
        LocalOrchestrator::new(config.clone(), session_store, model_router, tool_router).await?,
    );
    orchestrator
        .remember_workspace_root(workspace_id.clone(), workspace_root)
        .await;

    Ok(Arc::new(LocalTarget {
        orchestrator,
        workspace_id,
        user_id: UserId::new("loadtest"),
        model,
        mock_provider,
        database_url: config.database.runtime_url().to_string(),
        schema_name,
        _scratch_dir: Arc::new(scratch_dir),
    }))
}

pub(crate) async fn build_daemon_target(
    options: &LoadTestOptions,
    config: &MoaConfig,
) -> Result<Arc<dyn SessionTarget>> {
    #[cfg(not(unix))]
    {
        let _ = options;
        let _ = config;
        return Err(MoaError::Unsupported(
            "daemon load testing requires unix-domain sockets".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let socket_path = options
            .daemon_socket
            .clone()
            .unwrap_or_else(|| expand_local_path(&config.daemon.socket_path));
        let workspace_root = resolve_workspace_root(options.workspace_root.as_deref())?;
        Ok(Arc::new(DaemonTarget {
            socket_path,
            workspace_id: workspace_id_for_root(&workspace_root, "daemon"),
            user_id: UserId::new("loadtest"),
            model: ModelId::new(config.model_for_task(ModelTask::MainLoop)),
        }))
    }
}
