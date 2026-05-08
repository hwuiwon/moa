//! Local orchestrator session lifecycle integration tests.

mod support;

use support::local_orchestrator::*;

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
        model: MoaConfig::default().models.main,
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
async fn list_sessions_includes_active_session() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;
    let meta = orchestrator.get_session(session.session_id).await?;

    let sessions = orchestrator.list_sessions(SessionFilter::default()).await?;
    assert_eq!(sessions.len(), 1);
    let summary = &sessions[0];
    assert_eq!(summary.session_id, session.session_id);
    assert_eq!(summary.status, SessionStatus::Created);
    assert_eq!(summary.workspace_id, meta.workspace_id);
    assert_eq!(summary.user_id, meta.user_id);
    assert_eq!(summary.updated_at, meta.updated_at);
    assert!(meta.created_at <= summary.updated_at);
    let refetched = orchestrator.get_session(session.session_id).await?;
    assert_eq!(refetched.created_at, meta.created_at);
    Ok(())
}
#[tokio::test]
async fn graph_memory_maintenance_remains_noop_across_repeated_checks() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;

    let first = orchestrator.run_memory_maintenance_once().await?;
    let second = orchestrator.run_memory_maintenance_once().await?;

    assert!(first.is_empty());
    assert!(second.is_empty());
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
        ToolRouter::new(registry, providers)
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let llm_provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: config.models.main.clone(),
        first_tool_cmd: "echo tracked".to_string(),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store,
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
