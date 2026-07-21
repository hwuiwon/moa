//! Wiremock offline counterpart for query rewrite live coverage.

#[path = "../support/openai_wiremock.rs"]
mod openai_wiremock;

use std::sync::Arc;

use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_brain::query_rewrite::{QueryRewriteResult, RewriteSource};
use moa_config::MoaConfig;
use moa_core::{
    traits::ContextProcessor, traits::LLMProvider, types::channel::Channel,
    types::context::ContextMessage, types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_providers::OpenAIProvider;
use serde_json::json;
use wiremock::MockServer;

use openai_wiremock::{captured_json_bodies, mount_openai_json_text};

#[tokio::test]
async fn query_rewrite_offline_resolves_coreference_without_new_entities()
-> moa_core::error::Result<()> {
    let server = MockServer::start().await;
    mount_openai_json_text(
        &server,
        json!({
            "retrieval_query": "Fix the OAuth refresh token race condition in auth/refresh.rs and add regression tests.",
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string(),
        0,
    )
    .await;

    let mut config = MoaConfig::default();
    config.query_rewrite.model = Some("gpt-5.4-nano".to_string());
    let provider = Arc::new(
        OpenAIProvider::new("test-key", "gpt-5.4-nano")?
            .with_api_base(format!("{}/v1", server.uri()))?,
    );
    let mut ctx = moa_core::types::context::WorkingContext::new(
        &SessionMeta {
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
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
        .with_retrieval_availability(true, true)
        .process(&mut ctx)
        .await?;

    let result: QueryRewriteResult = serde_json::from_value(
        ctx.metadata()
            .get("query_rewrite")
            .expect("query rewrite metadata should be present")
            .clone(),
    )?;
    assert_eq!(result.source, RewriteSource::Rewritten);
    assert!(result.retrieval_query.contains("auth/refresh.rs"));
    assert!(result.retrieval_query.to_lowercase().contains("oauth"));
    assert!(
        result
            .retrieval_query
            .to_lowercase()
            .contains("refresh token")
    );
    assert!(!result.retrieval_query.to_lowercase().contains("kubernetes"));

    let bodies = captured_json_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "gpt-5.4-nano");
    assert_eq!(bodies[0]["max_output_tokens"], 384);
    assert_eq!(bodies[0]["reasoning"]["effort"], "none");
    assert_eq!(bodies[0]["text"]["format"]["name"], "query_rewrite_result");
    assert!(
        bodies[0]
            .get("tools")
            .is_none_or(serde_json::Value::is_null),
        "query rewrite should not send tools: {:?}",
        bodies[0].get("tools")
    );
    assert_eq!(bodies[0]["tool_choice"], "none");
    assert!(
        !bodies[0].to_string().contains("web_search"),
        "query rewrite should not trigger provider-native web search"
    );
    let instructions = bodies[0]["instructions"]
        .as_str()
        .expect("query rewrite request should include static instructions");
    assert!(
        instructions.contains("Produce retrieval and segment-boundary metadata only"),
        "query rewrite prompt should stay retrieval-scoped"
    );
    assert!(
        instructions.contains("Do not classify intent, choose tools"),
        "query rewrite prompt should not act as an intent router"
    );
    let schema = &bodies[0]["text"]["format"]["schema"];
    assert!(
        schema["required"]
            .as_array()
            .expect("query rewrite schema required list should be an array")
            .iter()
            .any(|field| field == "retrieval_query"),
        "query rewrite schema should require retrieval_query"
    );

    Ok(())
}
