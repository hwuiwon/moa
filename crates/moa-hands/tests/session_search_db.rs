include!("local_tools_support/common.rs");
include!("local_tools_support/session_search.rs");

use tokio::sync::Mutex;

static SESSION_SEARCH_DB_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn session_search_finds_prior_events() {
    let _guard = SESSION_SEARCH_DB_LOCK.lock().await;
    let dir = tempdir().unwrap();
    let session_store = test_session_store().await;
    let router = ToolRouter::new_local(dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "deploy failed on port binding".to_string(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "I found the deploy failure".to_string(),
                thought_signature: None,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 10,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
                llm_ttft_ms: None,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &identity(),
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "port binding", "last_n": 3 }),
            },
        )
        .await
        .unwrap();

    assert!(output.to_text().contains("deploy failed on port binding"));
    assert!(
        output
            .structured
            .as_ref()
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty())
    );
}

#[tokio::test]
async fn session_search_filters_error_events() {
    let _guard = SESSION_SEARCH_DB_LOCK.lock().await;
    let dir = tempdir().unwrap();
    let session_store = test_session_store().await;
    let router = ToolRouter::new_local(dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id,
            Event::Error {
                message: "deploy error".to_string(),
                recoverable: true,
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "deploy completed successfully".to_string(),
                thought_signature: None,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 10,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
                llm_ttft_ms: None,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &identity(),
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "deploy", "event_type": "error" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("deploy error"));
    assert!(!rendered.contains("deploy completed successfully"));
}
