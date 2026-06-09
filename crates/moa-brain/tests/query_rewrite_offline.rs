//! Wiremock offline counterpart for query rewrite live coverage.

mod support;

use std::sync::Arc;

use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_core::{
    ContextMessage, ContextProcessor, LLMProvider, MemoryAction, MoaConfig, Platform,
    QueryRewriteResult, RewriteSource, SessionMeta, UserId, WorkspaceId,
};
use moa_providers::OpenAIProvider;
use serde_json::json;
use wiremock::MockServer;

use support::{captured_json_bodies, mount_openai_text};

#[tokio::test]
async fn query_rewrite_offline_resolves_coreference_without_new_entities() -> moa_core::Result<()> {
    let server = MockServer::start().await;
    mount_openai_text(
        &server,
        json!({
            "rewritten_query": "Fix the OAuth refresh token race condition in auth/refresh.rs and add regression tests.",
            "task_kind": "coding",
            "sub_queries": ["patch auth/refresh.rs", "add regression tests"],
            "suggested_tools": ["file_read", "file_write"],
            "freshness_required": false,
            "repo_context_required": true,
            "memory_action": "retrieve",
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null,
            "tool_bias": ["read_before_write", "repo_inspection", "repo_inspection"],
            "suggested_promptlets": ["observe_first", "test_authoring"]
        })
        .to_string(),
        0,
    )
    .await;

    let mut config = MoaConfig::default();
    config.query_rewrite.model = Some("gpt-5.4".to_string());
    let provider = Arc::new(
        OpenAIProvider::new("test-key", "gpt-5.4")?
            .with_api_base(format!("{}/v1", server.uri()))?,
    );
    let mut ctx = moa_core::WorkingContext::new(
        &SessionMeta {
            workspace_id: WorkspaceId::new("offline-query-rewrite"),
            user_id: UserId::new("offline-query-rewrite-user"),
            platform: Platform::Api,
            model: provider.capabilities().model_id.clone(),
            ..SessionMeta::default()
        },
        provider.capabilities(),
    );
    ctx.set_tools(vec![
        json!({
            "name": "file_read",
            "description": "Read a file",
            "input_schema": {"type": "object"}
        }),
        json!({
            "name": "file_write",
            "description": "Write a file",
            "input_schema": {"type": "object"}
        }),
    ]);
    ctx.append_message(ContextMessage::user(
        "We found an OAuth refresh token race condition in auth/refresh.rs.",
    ));
    ctx.append_message(ContextMessage::assistant(
        "I can patch the auth/refresh.rs race and add regression coverage.",
    ));
    ctx.append_message(ContextMessage::user("fix that and add tests"));

    QueryRewriter::new(config.query_rewrite, provider.clone())
        .process(&mut ctx)
        .await?;

    let result: QueryRewriteResult = serde_json::from_value(
        ctx.metadata()
            .get("query_rewrite")
            .expect("query rewrite metadata should be present")
            .clone(),
    )?;
    assert_eq!(result.source, RewriteSource::Rewritten);
    assert!(result.rewritten_query.contains("auth/refresh.rs"));
    assert!(result.rewritten_query.to_lowercase().contains("oauth"));
    assert!(
        result
            .rewritten_query
            .to_lowercase()
            .contains("refresh token")
    );
    assert!(!result.rewritten_query.to_lowercase().contains("kubernetes"));
    assert!(
        result.repo_context_required,
        "query rewrite should mark repo inspection before coding"
    );
    assert!(
        !result.freshness_required,
        "query rewrite should not require current external information"
    );
    assert_eq!(
        result.memory_action,
        MemoryAction::Retrieve,
        "query rewrite should preserve the memory action hint"
    );
    assert_eq!(
        result.tool_bias,
        vec![
            "read_before_write".to_string(),
            "repo_inspection".to_string()
        ],
        "query rewrite should trim and dedupe tool bias hints"
    );
    assert_eq!(
        result.suggested_promptlets,
        vec!["observe_first".to_string(), "test_authoring".to_string()],
        "query rewrite should preserve promptlet hints"
    );

    let bodies = captured_json_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["text"]["format"]["name"], "query_rewrite_result");
    let prompt = bodies[0]["input"][0]["content"]
        .as_str()
        .expect("query rewrite request should include prompt text");
    assert!(
        prompt.contains("Produce retrieval and segment-boundary metadata only"),
        "query rewrite prompt should avoid final action routing"
    );
    assert!(
        prompt.contains("main agent model chooses tools and actions"),
        "query rewrite prompt should leave action choice to the main agent"
    );
    assert!(
        !prompt.contains("mode router"),
        "query rewrite prompt should not describe itself as a mode router"
    );
    assert!(
        prompt.contains("freshness_required"),
        "query rewrite prompt should request freshness metadata"
    );
    let schema = &bodies[0]["text"]["format"]["schema"];
    assert_eq!(
        schema["properties"]["memory_action"]["enum"],
        json!([
            "none",
            "retrieve",
            "remember",
            "forget",
            "supersede",
            "ingest"
        ]),
        "query rewrite schema should constrain memory actions"
    );
    assert!(
        schema["required"]
            .as_array()
            .expect("query rewrite schema required list should be an array")
            .iter()
            .any(|field| field == "suggested_promptlets"),
        "query rewrite schema should require promptlet hints"
    );

    Ok(())
}
