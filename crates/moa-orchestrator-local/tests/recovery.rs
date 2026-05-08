//! Local orchestrator recovery integration tests.

mod support;

use support::local_orchestrator::*;

#[tokio::test]
async fn resume_session_recovers_unresolved_pending_prompt() -> Result<()> {
    let (dir, orchestrator) = test_orchestrator().await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "initial".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let pending = moa_core::PendingSignal::queue_message(
        session.session_id,
        UserMessage {
            text: "recovered follow-up".to_string(),
            attachments: Vec::new(),
        },
    )?;
    orchestrator
        .session_store()
        .store_pending_signal(session.session_id, pending)
        .await?;

    let reopened_store = orchestrator.session_store();
    drop(orchestrator);

    let mut reopened_config = MoaConfig::default();
    disable_query_rewrite(&mut reopened_config);
    reopened_config.local.memory_dir = dir.path().join("memory").display().to_string();
    reopened_config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    let reopened_provider: Arc<dyn LLMProvider> = Arc::new(MockProvider {
        model: reopened_config.models.main.clone(),
        first_turn_delay: Duration::from_millis(5),
    });
    let reopened_router = Arc::new(
        ToolRouter::from_config(&reopened_config)
            .await?
            .with_rule_store(reopened_store.clone())
            .with_session_store(reopened_store.clone()),
    );
    let reopened = LocalOrchestrator::new(
        reopened_config,
        reopened_store,
        Arc::new(ModelRouter::new(reopened_provider, None)),
        reopened_router,
    )
    .await?;

    reopened.resume_session(session.session_id).await?;
    wait_for_status(&reopened, session.session_id, SessionStatus::Completed).await?;
    wait_for_pending_signal_count(&reopened, session.session_id, 0).await?;

    let events = reopened
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::QueuedMessage { ref text, .. } if text == "recovered follow-up"
    )));
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text.contains("recovered follow-up")
    )));
    Ok(())
}
#[tokio::test]
async fn resume_session_processes_user_message_before_trailing_status_event() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(10)).await?;
    let session_id = SessionId::new();
    let now = chrono::Utc::now();
    orchestrator
        .session_store()
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: moa_core::ModelId::new(orchestrator.model()),
            status: SessionStatus::Running,
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionCreated {
                workspace_id: moa_core::WorkspaceId::new("workspace"),
                user_id: moa_core::UserId::new("user"),
                model: moa_core::ModelId::new(orchestrator.model()),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "recover trailing status".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionStatusChanged {
                from: SessionStatus::Created,
                to: SessionStatus::Running,
            },
        )
        .await?;

    orchestrator.resume_session(session_id).await?;
    wait_for_status(&orchestrator, session_id, SessionStatus::Completed).await?;

    let events = orchestrator
        .session_store()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::BrainResponse { ref text, .. } if text == "assistant:recover trailing status"
    )));
    Ok(())
}
#[tokio::test]
async fn resumed_session_observe_runtime_streams_from_persisted_events() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(150)).await?;
    let session_id = SessionId::new();
    let now = chrono::Utc::now();
    orchestrator
        .session_store()
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: moa_core::ModelId::new(orchestrator.model()),
            status: SessionStatus::Created,
            created_at: now,
            updated_at: now,
            ..SessionMeta::default()
        })
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::SessionCreated {
                workspace_id: moa_core::WorkspaceId::new("workspace"),
                user_id: moa_core::UserId::new("user"),
                model: moa_core::ModelId::new(orchestrator.model()),
            },
        )
        .await?;
    orchestrator
        .session_store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "resume me".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    orchestrator.resume_session(session_id).await?;
    let mut runtime_rx = orchestrator
        .observe_runtime(session_id)
        .await?
        .expect("local orchestrator should support runtime observation");

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
    assert_eq!(delta_text, "assistant:resume me");
    assert!(runtime_events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantFinished { text, .. } if text == "assistant:resume me"
    )));
    Ok(())
}
#[tokio::test]
async fn resume_cancelled_session_waits_for_new_input() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "sleep 0.35 && printf 'tool-finished\\n'".to_string(),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "cancel during tool".to_string(),
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
    wait_for_tool_call_count(&orchestrator, session.session_id, 1).await?;
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;

    orchestrator.resume_session(session.session_id).await?;
    sleep(Duration::from_millis(450)).await;

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(brain_response_texts(&events).is_empty());
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 1);

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "after resume".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
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
async fn panicking_provider_marks_session_failed() -> Result<()> {
    let provider: Arc<dyn LLMProvider> = Arc::new(PanicProvider {
        model: MoaConfig::default().models.main,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "panic please".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Failed).await?;

    Ok(())
}
