//! Local orchestrator signal-routing integration tests.

mod support;

use support::local_orchestrator::*;

#[tokio::test]
async fn soft_cancel_marks_session_cancelled() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(250)).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    let events =
        wait_for_status_event(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::SessionStatusChanged {
            to: SessionStatus::Cancelled,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::Error { .. }))
    );
    Ok(())
}
#[tokio::test]
async fn hard_cancel_aborts_stream_and_emits_cancelled_status() -> Result<()> {
    let provider: Arc<dyn LLMProvider> = Arc::new(SlowStreamingProvider {
        model: MoaConfig::default().models.main,
        text: "streaming response that should be interrupted".to_string(),
        delay: Duration::from_millis(40),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;
    let mut runtime = orchestrator
        .observe_runtime(session.session_id)
        .await?
        .expect("local runtime stream should exist");

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "interrupt me".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let mut delta_text = String::new();
    let cancel_deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    while delta_text.len() < 3 && Instant::now() < cancel_deadline {
        if let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(250), runtime.recv()).await
            && let RuntimeEvent::AssistantDelta(ch) = event
        {
            delta_text.push(ch);
        }
    }
    assert!(
        delta_text.len() >= 3,
        "expected to receive streamed deltas before cancelling"
    );

    orchestrator
        .signal(session.session_id, SessionSignal::HardCancel)
        .await?;

    let finish_deadline = Instant::now() + ASYNC_TEST_DEADLINE;
    let mut saw_turn_completed = false;
    while Instant::now() < finish_deadline {
        match tokio::time::timeout(Duration::from_millis(250), runtime.recv()).await {
            Ok(Ok(RuntimeEvent::AssistantDelta(ch))) => delta_text.push(ch),
            Ok(Ok(RuntimeEvent::TurnCompleted)) => {
                saw_turn_completed = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    assert!(saw_turn_completed);
    assert!(delta_text.len() < "streaming response that should be interrupted".len());

    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::SessionStatusChanged {
            from: SessionStatus::Running,
            to: SessionStatus::Cancelled,
        }
    )));
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::Error { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
    );
    Ok(())
}
#[tokio::test]
async fn soft_cancel_stops_after_current_tool_call() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolCancelProvider {
        model,
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
    orchestrator
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Cancelled).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;
    assert!(
        events.iter().any(|record| matches!(
            record.event,
            Event::ToolResult { .. } | Event::ToolError { .. }
        )),
        "expected ToolResult or ToolError in events: {:?}",
        event_labels(&events)
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. }))
    );
    assert_eq!(requests.lock().expect("request log lock poisoned").len(), 1);
    Ok(())
}
#[tokio::test]
async fn queued_message_is_processed_after_current_turn() -> Result<()> {
    let (_dir, orchestrator) = test_orchestrator_with_delay(Duration::from_millis(200)).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_pending_signal_count(&orchestrator, session.session_id, 1).await?;
    let pending = orchestrator
        .session_store()
        .get_pending_signals(session.session_id)
        .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].user_message()?.text, "second");

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    wait_for_pending_signal_count(&orchestrator, session.session_id, 0).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;

    assert!(events.iter().any(|record| matches!(
        record.event,
        Event::QueuedMessage { ref text, .. } if text == "second"
    )));
    let responses = events
        .iter()
        .filter(|record| record.event_type == EventType::BrainResponse)
        .count();
    assert!(responses >= 2);
    Ok(())
}
#[tokio::test]
async fn queued_follow_up_request_ends_with_user_message() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(200),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(30)).await;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "second".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;

    let logged = requests.lock().expect("request log lock poisoned").clone();
    assert!(logged.len() >= 2);
    assert_eq!(
        logged[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role != MessageRole::System)
            .expect("second request should contain a non-system message")
            .role,
        MessageRole::User
    );
    assert_eq!(last_user_message(&logged[1].messages), Some("second"));

    Ok(())
}
#[tokio::test]
async fn multiple_queued_messages_are_processed_fifo_one_turn_at_a_time() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(200),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, Some(requests));
    assert_processes_multiple_queued_messages_fifo(&harness, "first", &["second", "third"]).await
}
#[tokio::test]
async fn burst_of_queued_messages_preserves_fifo_under_hot_session_pressure() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(RequestGuardProvider {
        model,
        first_turn_delay: Duration::from_millis(150),
        requests: requests.clone(),
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let queued = (0..10)
        .map(|index| format!("burst-{index:02}"))
        .collect::<Vec<_>>();
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "first".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    sleep(Duration::from_millis(40)).await;
    for message in &queued {
        orchestrator
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: message.clone(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
    }

    wait_for_status_with_timeout(
        &orchestrator,
        session.session_id,
        SessionStatus::Completed,
        Duration::from_secs(60),
    )
    .await?;
    let events = wait_for_brain_response_count_with_timeout(
        &orchestrator,
        session.session_id,
        queued.len() + 1,
        Duration::from_secs(60),
    )
    .await?;
    let expected = std::iter::once("first".to_string())
        .chain(queued.iter().cloned())
        .map(|prompt| format!("assistant:{prompt}"))
        .collect::<Vec<_>>();
    assert_eq!(brain_response_texts(&events), expected);

    let requests = requests
        .lock()
        .expect("request log mutex should not be poisoned")
        .clone();
    let ordered_prompts = requests
        .iter()
        .filter_map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == moa_core::MessageRole::User)
                .map(|message| message.content.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_prompts,
        expected
            .iter()
            .map(|text| text.trim_start_matches("assistant:").to_string())
            .collect::<Vec<_>>()
    );

    Ok(())
}
