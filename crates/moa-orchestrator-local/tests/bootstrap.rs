//! Local orchestrator bootstrap and configuration integration tests.

mod support;

use support::local_orchestrator::*;

#[tokio::test]
async fn compaction_uses_auxiliary_model_router_tier() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.models.main = "claude-sonnet-4-6".to_string();
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
async fn workspace_graph_memory_bootstrap_ingests_agents_file_without_provider_call() -> Result<()>
{
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
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
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let workspace_id = WorkspaceId::new("workspace-bootstrap-ingest");

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;

    let count = graph_node_count(orchestrator.session_store().as_ref(), &workspace_id).await?;
    assert!(count > 0, "expected graph bootstrap to write nodes");
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);
    Ok(())
}
#[tokio::test]
async fn workspace_memory_bootstrap_informs_first_turn_from_instruction_file() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
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
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let workspace_id = WorkspaceId::new("workspace-bootstrap-prompt");

    let session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id,
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
async fn workspace_graph_memory_bootstrap_skips_when_nodes_already_exist() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Project Agent Instructions\n\nFact: version-one is the canonical bootstrap marker.\n",
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
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let workspace_id = WorkspaceId::new("workspace-bootstrap-skip");

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;
    let first_count =
        graph_node_count(orchestrator.session_store().as_ref(), &workspace_id).await?;
    assert!(first_count > 0);

    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Project Agent Instructions\n\nFact: version-two is the canonical bootstrap marker.\n",
    )
    .await?;
    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;
    let second_count =
        graph_node_count(orchestrator.session_store().as_ref(), &workspace_id).await?;
    assert_eq!(second_count, first_count);
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 0);

    Ok(())
}
#[tokio::test]
async fn workspace_memory_bootstrap_can_be_disabled() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(
        workspace.path().join("AGENTS.md"),
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
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
        requests: requests.clone(),
    });
    let orchestrator = test_orchestrator_with_config_and_provider(config, provider).await?;
    let workspace_id = WorkspaceId::new("workspace-bootstrap-disabled");

    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await?;

    let count = graph_node_count(orchestrator.session_store().as_ref(), &workspace_id).await?;
    assert_eq!(count, 0);
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
        model: config.models.main.clone(),
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
        model: config.models.main.clone(),
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
        model: MoaConfig::default().models.main,
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
        model: MoaConfig::default().models.main,
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
        model: config.models.main.clone(),
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
        model: config.models.main.clone(),
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
