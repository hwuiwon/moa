//! Ignored live smoke test for query rewrite quality.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_core::{
    CompletionRequest, CompletionResponse, CompletionStream, ContextMessage, ContextProcessor,
    LLMProvider, MoaConfig, ModelCapabilities, Platform, QueryRewriteResult, RewriteSource,
    SessionMeta, UserId, WorkspaceId,
};
use moa_providers::resolve_rewriter_provider;
use tokio::sync::Mutex;

struct CapturingProvider {
    inner: Arc<dyn LLMProvider>,
    calls: AtomicUsize,
    responses: Mutex<Vec<String>>,
}

impl CapturingProvider {
    fn new(inner: Arc<dyn LLMProvider>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            responses: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LLMProvider for CapturingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> moa_core::Result<CompletionStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = match self.inner.complete(request).await {
            Ok(stream) => stream,
            Err(error) => {
                println!("RAW_REWRITE_PROVIDER_ERROR={error}");
                return Err(error);
            }
        };
        let response: CompletionResponse = match stream.collect().await {
            Ok(response) => response,
            Err(error) => {
                println!("RAW_REWRITE_COLLECT_ERROR={error}");
                return Err(error);
            }
        };
        println!("RAW_REWRITE_RESPONSE={}", response.text);
        self.responses.lock().await.push(response.text.clone());
        Ok(CompletionStream::from_response(response))
    }
}

#[tokio::test]
#[ignore = "requires provider API key env and performs one live query rewrite call"]
async fn live_query_rewriter_resolves_coreference_without_new_entities() -> moa_core::Result<()> {
    let mut config = MoaConfig::default();
    config.models.auxiliary = Some("gpt-5.4-mini".to_string());
    config.query_rewrite.model = Some("gpt-5.4-mini".to_string());
    config.query_rewrite.timeout_ms = 5_000;

    let provider = Arc::new(CapturingProvider::new(resolve_rewriter_provider(&config)?));
    let mut ctx = moa_core::WorkingContext::new(
        &SessionMeta {
            workspace_id: WorkspaceId::new("live-query-rewrite"),
            user_id: UserId::new("live-query-rewrite-user"),
            platform: Platform::Cli,
            model: provider.capabilities().model_id.clone(),
            ..SessionMeta::default()
        },
        provider.capabilities(),
    );
    ctx.set_tools(vec![
        serde_json::json!({
            "name": "file_read",
            "description": "Read a file",
            "input_schema": {"type": "object"}
        }),
        serde_json::json!({
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
    println!("REWRITER_CALLS={}", provider.calls.load(Ordering::SeqCst));

    let result: QueryRewriteResult = serde_json::from_value(
        ctx.metadata()
            .get("query_rewrite")
            .expect("query rewrite metadata should be present")
            .clone(),
    )?;
    println!("{}", serde_json::to_string_pretty(&result)?);

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

    Ok(())
}
