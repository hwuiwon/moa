//! Local orchestrator observation integration tests.

mod support;

use support::local_orchestrator::*;

#[tokio::test]
async fn observe_runtime_streams_assistant_text_and_turn_completion() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(40)).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "stream this".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let runtime_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;

    let delta_text = runtime_events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantDelta(ch) => Some(*ch),
            _ => None,
        })
        .collect::<String>();
    let finished_text = runtime_events.iter().find_map(|event| match event {
        RuntimeEvent::AssistantFinished { text, .. } => Some(text.clone()),
        _ => None,
    });

    assert!(
        runtime_events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::AssistantStarted))
    );
    assert_eq!(delta_text, "assistant:stream this");
    assert_eq!(finished_text, Some("assistant:stream this".to_string()));
    assert!(matches!(
        runtime_events.last(),
        Some(RuntimeEvent::TurnCompleted)
    ));
    Ok(())
}
#[tokio::test]
async fn observe_runtime_reports_tool_updates_and_approval_flow() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(FileWriteApprovalProvider { model, requests });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "write approval test".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let pre_approval_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::ApprovalRequested(_))
    })
    .await?;
    let approval_prompt = pre_approval_events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ApprovalRequested(prompt) => Some(prompt.clone()),
            _ => None,
        })
        .expect("approval prompt missing from runtime stream");

    assert!(pre_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolUpdate(update)
            if update.tool_name == "file_write"
                && matches!(update.status, moa_core::ToolCardStatus::WaitingApproval)
    )));

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id: approval_prompt.request.request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    let post_approval_events = collect_runtime_events_until(&mut runtime_rx, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await?;

    assert!(post_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolUpdate(update)
            if update.tool_name == "file_write"
                && matches!(update.status, moa_core::ToolCardStatus::Succeeded)
    )));
    assert!(post_approval_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantFinished { text, .. } if text == "done"
    )));
    assert!(matches!(
        post_approval_events.last(),
        Some(RuntimeEvent::TurnCompleted)
    ));
    Ok(())
}
#[tokio::test]
async fn observe_stream_receives_events_in_order() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;
    let mut stream = orchestrator
        .observe(session.session_id, moa_core::ObserveLevel::Normal)
        .await?;

    orchestrator
        .signal(
            session.session_id,
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
    let fourth = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing fourth observed event".to_string())
    })?;
    let fifth = stream.next().await.transpose()?.ok_or_else(|| {
        moa_core::MoaError::ProviderError("missing fifth observed event".to_string())
    })?;

    let LiveEvent::Event(first) = first else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for first observed event".to_string(),
        ));
    };
    let LiveEvent::Event(second) = second else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for second observed event".to_string(),
        ));
    };
    let LiveEvent::Event(third) = third else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for third observed event".to_string(),
        ));
    };
    let LiveEvent::Event(fourth) = fourth else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for fourth observed event".to_string(),
        ));
    };
    let LiveEvent::Event(fifth) = fifth else {
        return Err(moa_core::MoaError::ProviderError(
            "unexpected gap marker for fifth observed event".to_string(),
        ));
    };

    assert_eq!(first.sequence_num, 0);
    assert_eq!(first.event_type, EventType::SessionCreated);
    assert_eq!(second.sequence_num, 1);
    assert_eq!(second.event_type, EventType::UserMessage);
    assert_eq!(third.sequence_num, 2);
    assert_eq!(third.event_type, EventType::SessionStatusChanged);
    assert_eq!(fourth.sequence_num, 3);
    assert_eq!(fourth.event_type, EventType::CacheReport);
    assert_eq!(fifth.sequence_num, 4);
    assert_eq!(fifth.event_type, EventType::BrainResponse);
    Ok(())
}
#[tokio::test]
async fn observe_uses_postgres_listener_for_remote_active_sessions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

    let provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: config.models.main.clone(),
        first_turn_delay: Duration::from_millis(20),
    });
    let session_store = create_test_store().await?;
    let observer = test_orchestrator_with_config_provider_and_store(
        config.clone(),
        provider,
        session_store.clone(),
    )
    .await?;

    let now = Utc::now();
    let session_id = SessionId::new();
    session_store
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            status: SessionStatus::Running,
            platform: Platform::Cli,
            model: moa_core::ModelId::new(observer.model()),
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;

    let mut stream = observer
        .observe(session_id, moa_core::ObserveLevel::Normal)
        .await?;

    session_store
        .emit_event(
            session_id,
            Event::Warning {
                message: "remote-listener".to_string(),
            },
        )
        .await?;

    let observed = tokio::time::timeout(ASYNC_TEST_DEADLINE, stream.next())
        .await
        .map_err(|_| {
            MoaError::ProviderError(
                "timed out waiting for Postgres LISTEN-backed observation".to_string(),
            )
        })?
        .transpose()?
        .ok_or_else(|| {
            MoaError::ProviderError("missing observed event from Postgres listener".to_string())
        })?;

    let LiveEvent::Event(record) = observed else {
        return Err(MoaError::ProviderError(
            "expected concrete event from Postgres listener path".to_string(),
        ));
    };
    assert!(matches!(
        record.event,
        Event::Warning { ref message } if message == "remote-listener"
    ));

    Ok(())
}
