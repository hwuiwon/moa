//! Stage 5: rewrites the current user query before memory retrieval.

mod circuit_breaker;
mod input;
mod llm_call;
mod postprocess;
mod prompt;
mod terms;
mod triggers;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ContextProcessor, LLMProvider, ProcessorOutput, QueryRewriteConfig, QueryRewriteResult, Result,
    SessionStore, WorkingContext,
};
use serde_json::json;

pub use self::circuit_breaker::CircuitBreaker;
use self::input::RewriteInput;
use self::postprocess::store_rewrite_result;

const METADATA_KEY: &str = "query_rewrite";

/// Query-rewriting context processor.
pub struct QueryRewriter {
    config: QueryRewriteConfig,
    llm: Arc<dyn LLMProvider>,
    session_store: Option<Arc<dyn SessionStore>>,
    circuit_breaker: CircuitBreaker,
}

impl QueryRewriter {
    /// Creates a query rewriter backed by the provided LLM.
    pub fn new(config: QueryRewriteConfig, llm: Arc<dyn LLMProvider>) -> Self {
        let circuit_breaker = CircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_window_secs,
            config.circuit_breaker_cooldown_secs,
        );
        Self {
            config,
            llm,
            session_store: None,
            circuit_breaker,
        }
    }

    /// Configures the rewriter to load recent user history directly from the session log.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }
}

#[async_trait]
impl ContextProcessor for QueryRewriter {
    fn name(&self) -> &str {
        "query_rewrite"
    }

    fn stage(&self) -> u8 {
        5
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let input = match self.load_input(ctx).await {
            Ok(Some(input)) => input,
            Ok(None) => RewriteInput::empty(),
            Err(error) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed to load history, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(""))?;
                return Ok(ProcessorOutput::default());
            }
        };

        if self.should_skip(&input) {
            store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
            return Ok(ProcessorOutput::default());
        }

        let timeout = Duration::from_millis(self.config.timeout_ms);
        match tokio::time::timeout(timeout, self.rewrite(&input, ctx)).await {
            Ok(Ok(result)) => {
                self.circuit_breaker.record_success();
                let metadata = HashMap::from([
                    ("rewrite_source".to_string(), json!("rewritten")),
                    ("task_kind".to_string(), json!(result.task_kind.clone())),
                ]);
                store_rewrite_result(ctx, result)?;
                Ok(ProcessorOutput {
                    metadata,
                    ..ProcessorOutput::default()
                })
            }
            Ok(Err(error)) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
                Ok(ProcessorOutput::default())
            }
            Err(_) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    timeout_ms = self.config.timeout_ms,
                    "query rewriter timed out, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
                Ok(ProcessorOutput::default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, ContextMessage, ContextProcessor,
        LLMProvider, ModelCapabilities, ModelId, Platform, QueryRewriteConfig, QueryRewriteResult,
        Result, RewriteSource, SessionId, SessionMeta, StopReason, TokenPricing, TokenUsage,
        ToolCallFormat, UserId, WorkingContext, WorkspaceId,
    };
    use serde_json::json;

    use super::{CircuitBreaker, METADATA_KEY, QueryRewriter};

    #[derive(Clone)]
    struct MockProvider {
        response: Arc<std::sync::Mutex<String>>,
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ModelCapabilities {
            capabilities()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let text = self
                .response
                .lock()
                .expect("mock response lock should not be poisoned")
                .clone();
            Ok(CompletionStream::from_response(CompletionResponse {
                text: text.clone(),
                content: vec![moa_core::CompletionContent::Text(text)],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("mock"),
                usage: TokenUsage::default(),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("mock"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    fn context_with_messages(messages: Vec<ContextMessage>) -> WorkingContext {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        for message in messages {
            ctx.append_message(message);
        }
        ctx
    }

    fn response_json(rewritten_query: &str, sub_queries: Vec<&str>) -> String {
        json!({
            "rewritten_query": rewritten_query,
            "task_kind": "coding",
            "sub_queries": sub_queries,
            "suggested_tools": [],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string()
    }

    fn rewriter_with_response(response: String) -> (QueryRewriter, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response)),
            delay: Duration::ZERO,
            calls: calls.clone(),
        };
        (
            QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider)),
            calls,
        )
    }

    fn metadata_result(ctx: &WorkingContext) -> QueryRewriteResult {
        serde_json::from_value(
            ctx.metadata()
                .get(METADATA_KEY)
                .expect("rewrite metadata should exist")
                .clone(),
        )
        .expect("rewrite metadata should deserialize")
    }

    #[tokio::test]
    async fn skips_single_turn_short_query() {
        let (rewriter, calls) = rewriter_with_response(response_json("hello there", Vec::new()));
        let mut ctx = context_with_messages(vec![ContextMessage::user("hello")]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Passthrough);
        assert_eq!(result.rewritten_query, "hello");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rewrites_multiturn_coreference() {
        let (rewriter, calls) = rewriter_with_response(response_json(
            "Fix the OAuth refresh token race condition in auth/refresh.rs",
            Vec::new(),
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert_eq!(
            result.rewritten_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_timeout_allows_segment_transition_rewrite_latency() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(
                json!({
                    "rewritten_query": "Write a five-word project status headline about database migrations.",
                    "task_kind": "creative",
                    "sub_queries": [],
                    "suggested_tools": [],
                    "needs_clarification": false,
                    "clarification_question": null,
                    "is_new_task": true,
                    "task_summary": "Write a short project status headline about database migrations.",
                })
                .to_string(),
            )),
            delay: Duration::from_millis(600),
            calls: calls.clone(),
        };
        let rewriter = QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("What is 2 + 2? Answer with only the number."),
            ContextMessage::assistant("4"),
            ContextMessage::user(
                "Now switch tasks: write a five-word project status headline about database migrations.",
            ),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("default timeout should allow live-like rewrite latency");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert!(result.is_new_task);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn preserves_compound_sub_queries() {
        let (rewriter, _) = rewriter_with_response(response_json(
            "Review auth/refresh.rs and add tests",
            vec!["Review auth/refresh.rs", "add tests"],
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("auth/refresh.rs handles OAuth refresh tokens"),
            ContextMessage::assistant("Noted."),
            ContextMessage::user("review that and add tests"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(
            result.sub_queries,
            vec![
                "Review auth/refresh.rs".to_string(),
                "add tests".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn timeout_falls_back_to_passthrough() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response_json("unused", Vec::new()))),
            delay: Duration::from_millis(50),
            calls: calls.clone(),
        };
        let config = QueryRewriteConfig {
            timeout_ms: 1,
            ..QueryRewriteConfig::default()
        };
        let rewriter = QueryRewriter::new(config, Arc::new(provider));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("OAuth refresh token race condition"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("timeout should fail open");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Passthrough);
        assert_eq!(result.rewritten_query, "fix that");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_failures() {
        let (rewriter, calls) = rewriter_with_response("not json".to_string());
        let mut first = context_with_messages(vec![
            ContextMessage::user("OAuth refresh token race condition"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);
        let mut second = first.clone();

        rewriter
            .process(&mut first)
            .await
            .expect("first failure should fail open");
        rewriter
            .process(&mut second)
            .await
            .expect("open circuit should skip");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(metadata_result(&second).source, RewriteSource::Passthrough);
    }

    #[tokio::test]
    async fn invalid_task_kind_falls_open_to_unknown() {
        let invalid_response = json!({
            "rewritten_query": "Fix the OAuth refresh token race condition in auth/refresh.rs",
            "task_kind": "software engineering task",
            "sub_queries": [],
            "suggested_tools": [],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string();
        let (rewriter, calls) = rewriter_with_response(invalid_response);
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("invalid task_kind should fail open");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert_eq!(
            result.rewritten_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
        assert_eq!(result.task_kind, moa_core::TaskKind::Unknown);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn circuit_breaker_resets_after_cooldown() {
        let breaker = CircuitBreaker::new(0.05, 60, 1);
        breaker.record_failure();
        assert!(breaker.is_open());

        std::thread::sleep(Duration::from_millis(1_100));

        assert!(!breaker.is_open());
    }

    #[tokio::test]
    async fn validation_strips_entities_not_present_in_history() {
        let (rewriter, _) = rewriter_with_response(response_json(
            "Fix the OAuth refresh token race condition in auth/refresh.rs and Kubernetes deployment",
            Vec::new(),
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert!(!result.rewritten_query.contains("Kubernetes"));
        assert!(!result.rewritten_query.contains("deployment"));
        assert_eq!(
            result.rewritten_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
    }
}
