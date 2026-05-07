//! Wiremock offline counterpart for query rewrite live coverage.

mod support;

use std::sync::Arc;

use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_core::{
    ContextMessage, ContextProcessor, LLMProvider, MoaConfig, Platform, QueryRewriteResult,
    RewriteSource, SessionMeta, UserId, WorkspaceId,
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
            "intent": "coding",
            "sub_queries": ["patch auth/refresh.rs", "add regression tests"],
            "suggested_tools": ["file_read", "file_write"],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null
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
            platform: Platform::Cli,
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

    let bodies = captured_json_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["text"]["format"]["name"], "query_rewrite_result");

    Ok(())
}
