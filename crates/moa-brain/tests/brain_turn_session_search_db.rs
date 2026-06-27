#[cfg(feature = "eval-harness")]
include!("brain_turn_support/common.rs");
#[cfg(feature = "eval-harness")]
include!("brain_turn_support/db.rs");
#[cfg(feature = "eval-harness")]
include!("brain_turn_support/session_search.rs");

static BRAIN_TURN_SESSION_SEARCH_DB_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn brain_turn_session_search_finds_user_message_db() {
    // Pins: the brain turn session-search DB lane uses the real event FTS path.
    let _guard = BRAIN_TURN_SESSION_SEARCH_DB_LOCK.lock().await;

    use moa_core::{
        Event, EventFilter, EventType, ModelId, SessionActorRef, SessionMeta, SessionStore as _,
        TenantId,
    };

    let (store, database_url, schema_name) = moa_session::testing::create_isolated_test_store()
        .await
        .expect("create isolated brain session-search DB store");
    let session = SessionMeta {
        tenant_id: TenantId::new(),
        model: ModelId::new("claude-sonnet-4-6"),
        created_by: Some(SessionActorRef::Identity {
            id: uuid::Uuid::now_v7(),
        }),
        ..SessionMeta::default()
    };
    let session_id = store
        .create_session(session.clone())
        .await
        .expect("create session for search");
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "release rollback failed on the payment worker".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit searchable user message");

    let results = store
        .search_events(
            "payment worker",
            EventFilter {
                session_id: Some(session_id),
                tenant_id: Some(session.tenant_id),
                event_types: Some(vec![EventType::UserMessage]),
                limit: Some(5),
                ..EventFilter::default()
            },
        )
        .await
        .expect("search session events");

    assert_eq!(results.len(), 1);
    assert!(matches!(
        &results[0].event,
        Event::UserMessage { text, .. } if text == "release rollback failed on the payment worker"
    ));

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated brain session-search DB schema");
}

#[cfg(feature = "eval-harness")]
#[tokio::test]
async fn run_brain_turn_recovers_old_artifact_via_session_search() {
    let _guard = BRAIN_TURN_SESSION_SEARCH_DB_LOCK.lock().await;

    let store = test_session_store().await;
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id: test_tenant_id(),
        contact: Some(test_contact_ref()),
        created_by: Some(SessionActorRef::Contact {
            id: test_contact_id(),
        }),
        model: moa_core::ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    };
    store.create_session(session.clone()).await.unwrap();

    let old_tool_id = ToolCallId::new();
    let old_output_text = (1..=260)
        .map(|index| format!("bash-line-{index}-{}", "x".repeat(120)))
        .collect::<Vec<_>>()
        .join("\n");
    let combined = store
        .store_text_artifact(session.id, &old_output_text)
        .await
        .unwrap();
    let stdout = store
        .store_text_artifact(session.id, &old_output_text)
        .await
        .unwrap();

    store
        .emit_event(
            session.id,
            Event::UserMessage {
                text: "Run a noisy bash command".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
    store
        .emit_event(
            session.id,
            Event::ToolCall {
                tool_id: old_tool_id,
                provider_tool_use_id: Some("toolu_old_bash".to_string()),
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: json!({
                    "cmd": "python3 -c \"for i in range(1, 261): print(f'bash-line-{i}-' + ('x' * 120))\""
                }),
                hand_id: None,
            },
        )
        .await
        .unwrap();
    store
        .emit_event(
            session.id,
            Event::ToolResult {
                tool_id: old_tool_id,
                provider_tool_use_id: Some("toolu_old_bash".to_string()),
                output: ToolOutput::text(
                    "available_streams: combined, stdout\nrecovery_hint: use the tool_result id from this message; call tool_result_search for exact patterns, then tool_result_read for a narrow range or a specific stream\n[full output stored separately: ~8000 tokens, 260 lines, 32000 bytes; use tool_result_search first to locate exact matches, then tool_result_read to inspect a narrow span or stream]",
                    std::time::Duration::from_millis(7),
                )
                .with_truncated(true)
                .with_original_output_tokens(Some(8_000))
                .with_artifact(Some(moa_core::ToolOutputArtifact {
                    combined,
                    estimated_tokens: 8_000,
                    line_count: count_lines(&old_output_text),
                    stdout: Some(stdout),
                    stderr: None,
                })),
                original_output_tokens: Some(8_000),
                success: true,
                duration_ms: 7,
            },
        )
        .await
        .unwrap();
    store
        .emit_event(
            session.id,
            Event::BrainResponse {
                text: "The noisy command ran successfully.".to_string(),
                thought_signature: None,
                model: moa_core::ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::ModelTier::Main,
                input_tokens_uncached: 20,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 8,
                cost_cents: 1,
                duration_ms: 10,
            },
        )
        .await
        .unwrap();

    for index in 0..8 {
        store
            .emit_event(
                session.id,
                Event::UserMessage {
                    text: filler_text(&format!("Follow-up user turn {index}"), 1_200),
                    attachments: Vec::new(),
                },
            )
            .await
            .unwrap();
        store
            .emit_event(
                session.id,
                Event::BrainResponse {
                    text: filler_text(&format!("Follow-up assistant turn {index}"), 1_200),
                    thought_signature: None,
                    model: moa_core::ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: 24,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 10,
                    cost_cents: 1,
                    duration_ms: 10,
                },
            )
            .await
            .unwrap();
    }

    store
        .emit_event(
            session.id,
            Event::UserMessage {
                text: "Find bash-line-140 from that old noisy bash run".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let sandbox_dir = tempdir().unwrap();
    let tool_router = Arc::new(
        ToolRouter::new_local(sandbox_dir.path())
            .await
            .unwrap()
            .with_session_store(store.clone()),
    );
    let pipeline = build_default_pipeline_with_tools(
        &MoaConfig::default(),
        store.clone(),
        tool_router.tool_schemas(),
    );
    let llm = Arc::new(SessionSearchArtifactLlmProvider::new(old_tool_id));

    let result = run_brain_turn(
        session.id,
        store.clone(),
        llm.clone(),
        &pipeline,
        Some(tool_router),
    )
    .await
    .unwrap();

    assert_eq!(result, TurnResult::Complete);

    let events = store
        .get_events(session.id, EventRange::all())
        .await
        .unwrap();
    let session_search_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output,
                success,
                provider_tool_use_id: Some(provider_tool_use_id),
                ..
            } if *success && provider_tool_use_id == "77777777-7777-7777-7777-777777777777" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("expected session_search output");
    assert!(
        session_search_result
            .to_text()
            .contains(&old_tool_id.to_string())
    );
    let artifact_search_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output,
                success,
                provider_tool_use_id: Some(provider_tool_use_id),
                ..
            } if *success && provider_tool_use_id == "88888888-8888-8888-8888-888888888888" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("expected tool_result_search output");
    assert!(artifact_search_result.to_text().contains("bash-line-140-"));
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "Recovered old artifact via session_search and tool_result_search"
    )));
}

#[cfg(feature = "eval-harness")]
#[tokio::test]
async fn auto_mode_repeated_tool_runs_without_persisted_action_policy_rules() {
    let _guard = BRAIN_TURN_SESSION_SEARCH_DB_LOCK.lock().await;

    let dir = tempdir().unwrap();
    let store = test_session_store().await;
    let tool_router = Arc::new(
        ToolRouter::new_local(dir.path())
            .await
            .unwrap()
            .with_rule_store(store.clone())
            .with_session_store(store.clone()),
    );
    let session_id = store
        .create_session(SessionMeta {
            tenant_id: test_tenant_id(),
            contact: Some(test_contact_ref()),
            created_by: Some(SessionActorRef::Contact {
                id: test_contact_id(),
            }),
            model: moa_core::ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        })
        .await
        .unwrap();
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Use a tool".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let pipeline = build_default_pipeline_with_tools(
        &MoaConfig::default(),
        store.clone(),
        tool_router.tool_schemas(),
    );
    let llm = Arc::new(RepeatingToolLlmProvider::default());

    let first = run_brain_turn(
        session_id,
        store.clone(),
        llm.clone(),
        &pipeline,
        Some(tool_router.clone()),
    )
    .await
    .unwrap();
    assert_eq!(first, TurnResult::Complete);
    assert_eq!(
        store
            .list_action_policy_rules_for_tool(&test_tenant_id(), &UserId::new("user"), "bash",)
            .await
            .unwrap()
            .len(),
        0
    );

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Use the same tool again".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let final_result = run_brain_turn(
        session_id,
        store.clone(),
        llm.clone(),
        &pipeline,
        Some(tool_router),
    )
    .await
    .unwrap();

    assert_eq!(final_result, TurnResult::Complete);
    assert_eq!(llm.requests.lock().await.len(), 4);
}
