// Live counterpart: see query_rewrite_offline.rs for the wiremock version that runs in PR CI.

//! Ignored live smoke test for query rewrite gate behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use moa_brain::pipeline::query_rewrite::QueryRewriter;
use moa_core::{
    Channel, CompletionRequest, CompletionResponse, CompletionStream, ContextMessage,
    ContextProcessor, LLMProvider, MoaConfig, ModelCapabilities, QueryRewriteResult, RewriteReason,
    RewriteSource, SessionMeta, UserId, WorkspaceId,
};
use moa_providers::resolve_rewriter_provider;
use tokio::sync::Mutex;

struct CapturingProvider {
    inner: Arc<dyn LLMProvider>,
    calls: AtomicUsize,
    responses: Mutex<Vec<String>>,
    errors: Mutex<Vec<String>>,
}

impl CapturingProvider {
    fn new(inner: Arc<dyn LLMProvider>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            responses: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
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
                self.errors.lock().await.push(error.to_string());
                return Err(error);
            }
        };
        let response: CompletionResponse = match stream.collect().await {
            Ok(response) => response,
            Err(error) => {
                self.errors.lock().await.push(error.to_string());
                return Err(error);
            }
        };
        self.responses.lock().await.push(response.text.clone());
        Ok(CompletionStream::from_response(response))
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1, provider API key env, and performs live query rewrite calls"]
async fn live_query_rewrite_gate_matrix() -> moa_core::Result<()> {
    if std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
        return Ok(());
    }

    let mut config = MoaConfig::default();
    config.models.auxiliary = Some("gpt-5.4-mini".to_string());
    config.query_rewrite.model = Some("gpt-5.4-mini".to_string());
    config.query_rewrite.timeout_ms = 5_000;

    let provider = Arc::new(CapturingProvider::new(resolve_rewriter_provider(&config)?));

    let explicit_query = "Which runbook is required for deploy?";
    let started = Instant::now();
    let explicit = process_case(
        &config,
        provider.clone(),
        vec![ContextMessage::user(explicit_query)],
        true,
        true,
    )
    .await?;
    assert_eq!(explicit.source, RewriteSource::Original);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    println!(
        "LIVE_REWRITE_CASE explicit decision=skip calls=0 latency_ms={} original={:?} retrieval={:?}",
        started.elapsed().as_millis(),
        explicit_query,
        explicit.retrieval_query
    );

    let exact_query = "Find docs/runbook.md";
    let started = Instant::now();
    let exact = process_case(
        &config,
        provider.clone(),
        vec![ContextMessage::user(exact_query)],
        true,
        true,
    )
    .await?;
    assert_eq!(exact.source, RewriteSource::Original);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    println!(
        "LIVE_REWRITE_CASE exact_identifier decision=skip calls=0 latency_ms={} original={:?} retrieval={:?}",
        started.elapsed().as_millis(),
        exact_query,
        exact.retrieval_query
    );

    let coreference_query = "fix that and add tests";
    let started = Instant::now();
    let coreference = process_case(
        &config,
        provider.clone(),
        vec![
            ContextMessage::user(
                "We found an OAuth refresh token race condition in auth/refresh.rs.",
            ),
            ContextMessage::assistant(
                "I can patch the auth/refresh.rs race and add regression coverage.",
            ),
            ContextMessage::user(coreference_query),
        ],
        true,
        true,
    )
    .await?;
    assert_eq!(
        coreference.source,
        RewriteSource::Rewritten,
        "coreference rewrite should use history; result={coreference:?} calls={} responses={:?} errors={:?}",
        provider.calls.load(Ordering::SeqCst),
        provider.responses.lock().await,
        provider.errors.lock().await
    );
    assert_eq!(
        coreference.reason,
        Some(RewriteReason::CoreferenceWithHistory)
    );
    assert!(coreference.retrieval_query.contains("auth/refresh.rs"));
    assert!(coreference.retrieval_query.to_lowercase().contains("oauth"));
    assert!(
        !coreference
            .retrieval_query
            .to_lowercase()
            .contains("kubernetes")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    println!(
        "LIVE_REWRITE_CASE coreference decision=rewrite calls=1 latency_ms={} original={:?} retrieval={:?}",
        started.elapsed().as_millis(),
        coreference_query,
        coreference.retrieval_query
    );

    let vector_query = "How often do deploy incidents look similar to policy releases?";
    let started = Instant::now();
    let vector_without_embedder = process_case(
        &config,
        provider.clone(),
        vec![ContextMessage::user(vector_query)],
        true,
        false,
    )
    .await?;
    assert_eq!(vector_without_embedder.source, RewriteSource::Original);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    println!(
        "LIVE_REWRITE_CASE vector_first_no_embedder decision=skip calls=1 latency_ms={} original={:?} retrieval={:?}",
        started.elapsed().as_millis(),
        vector_query,
        vector_without_embedder.retrieval_query
    );

    let started = Instant::now();
    let vector_with_embedder = process_case(
        &config,
        provider.clone(),
        vec![ContextMessage::user(vector_query)],
        true,
        true,
    )
    .await?;
    assert_eq!(
        vector_with_embedder.source,
        RewriteSource::Rewritten,
        "vector-first rewrite should use semantic query; result={vector_with_embedder:?} calls={} responses={:?} errors={:?}",
        provider.calls.load(Ordering::SeqCst),
        provider.responses.lock().await,
        provider.errors.lock().await
    );
    assert_eq!(
        vector_with_embedder.reason,
        Some(RewriteReason::VectorFirstSemantic)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    println!(
        "LIVE_REWRITE_CASE vector_first_with_embedder decision=rewrite calls=2 latency_ms={} original={:?} retrieval={:?}",
        started.elapsed().as_millis(),
        vector_query,
        vector_with_embedder.retrieval_query
    );

    Ok(())
}

async fn process_case(
    config: &MoaConfig,
    provider: Arc<CapturingProvider>,
    messages: Vec<ContextMessage>,
    memory_retrieval_available: bool,
    vector_retrieval_available: bool,
) -> moa_core::Result<QueryRewriteResult> {
    let mut ctx = moa_core::WorkingContext::new(
        &SessionMeta {
            workspace_id: WorkspaceId::new("live-query-rewrite"),
            user_id: UserId::new("live-query-rewrite-user"),
            channel: Channel::Chat,
            model: provider.capabilities().model_id.clone(),
            ..SessionMeta::default()
        },
        provider.capabilities(),
    );
    ctx.set_tools(vec![serde_json::json!({
        "name": "file_read",
        "description": "Read a file",
        "input_schema": {"type": "object"}
    })]);
    for message in messages {
        ctx.append_message(message);
    }

    QueryRewriter::new(config.query_rewrite.clone(), provider.clone())
        .with_retrieval_availability(memory_retrieval_available, vector_retrieval_available)
        .process(&mut ctx)
        .await?;

    serde_json::from_value(
        ctx.metadata()
            .get("query_rewrite")
            .expect("query rewrite metadata should be present")
            .clone(),
    )
    .map_err(Into::into)
}
