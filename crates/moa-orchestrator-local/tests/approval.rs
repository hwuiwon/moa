//! Local orchestrator approval-flow integration tests.

mod support;

use support::local_orchestrator::*;

#[tokio::test]
async fn approval_requested_event_persists_full_prompt_details() -> Result<()> {
    let _cwd_guard = cwd_lock().lock().await;
    let workspace = tempfile::tempdir()?;
    let _dir_guard = CurrentDirGuard::set(workspace.path())?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(FileWriteApprovalProvider { model, requests });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let session = start_session(&orchestrator).await?;

    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "write approval test".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;

    let event = wait_for_approval_event(&orchestrator, session.session_id).await?;
    match event {
        Event::ApprovalRequested {
            tool_name,
            input_summary,
            risk_level,
            prompt,
            ..
        } => {
            assert_eq!(tool_name, "file_write");
            assert!(input_summary.contains("docs/approval-check.md"));
            assert_eq!(risk_level, moa_core::RiskLevel::Medium);
            assert_eq!(prompt.request.tool_name, "file_write");
            assert_eq!(prompt.parameters.len(), 2);
            assert_eq!(prompt.file_diffs.len(), 1);
            assert_eq!(prompt.file_diffs[0].path, "docs/approval-check.md");
            assert!(
                prompt.file_diffs[0].before.is_empty()
                    || prompt.file_diffs[0].before == "approved via orchestrator\n"
            );
            assert_eq!(
                prompt.file_diffs[0].after,
                "approved via orchestrator\n".to_string()
            );
            assert!(prompt.pattern.contains("docs/approval-check.md"));
        }
        other => panic!("expected approval requested event, got {other:?}"),
    }

    Ok(())
}
#[tokio::test]
async fn queued_message_waiting_for_approval_runs_after_allowed_turn() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "python3 -c 'print(\"tool-complete\")'".to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, None);
    assert_queued_message_waiting_for_approval_runs_after_allowed_turn(&harness, "first", "queued")
        .await
}
#[tokio::test]
async fn soft_cancel_waiting_for_approval_cancels_cleanly() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model: MoaConfig::default().models.main,
        first_tool_cmd: "printf 'awaiting approval'".to_string(),
        requests,
    });
    let (_dir, orchestrator) = test_orchestrator_with_provider(provider).await?;
    let harness = LocalContractHarness::new(&orchestrator, None);
    assert_soft_cancel_waiting_for_approval_cancels_cleanly(&harness, "first").await
}
#[tokio::test]
async fn denied_tool_preserves_queued_follow_up() -> Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = MoaConfig::default().models.main;
    let provider: Arc<dyn LLMProvider> = Arc::new(ToolThenEchoProvider {
        model,
        first_tool_cmd: "python3 -c 'print(\"tool-complete\")'".to_string(),
        requests,
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

    let request_id = wait_for_approval_request(&orchestrator, session.session_id).await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: "after-deny".to_string(),
                attachments: Vec::new(),
            }),
        )
        .await?;
    orchestrator
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::Deny { reason: None },
            },
        )
        .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
    let events = orchestrator
        .session_store()
        .get_events(session.session_id, EventRange::all())
        .await?;

    assert!(
        events
            .iter()
            .any(|record| matches!(record.event, Event::ToolError { .. }))
    );
    assert_eq!(
        brain_response_texts(&events),
        vec!["assistant:first", "assistant:after-deny"]
    );

    Ok(())
}
