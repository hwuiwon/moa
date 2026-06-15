//! Stage 4: rewrites the current user query before memory retrieval.

mod circuit_breaker;
mod gate;
mod input;
mod llm_call;
mod postprocess;
mod prompt;
mod terms;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ContextProcessor, LLMProvider, ProcessorOutput, QueryRewriteConfig, QueryRewriteResult, Result,
    SessionStore, WorkingContext,
};
use serde_json::json;

pub use self::circuit_breaker::CircuitBreaker;
use self::gate::{RewriteDecision, RewriteGateInput};
use self::input::RewriteInput;
use self::postprocess::store_rewrite_result;

const METADATA_KEY: &str = "query_rewrite";

/// Query-rewriting context processor.
pub struct QueryRewriter {
    config: QueryRewriteConfig,
    llm: Arc<dyn LLMProvider>,
    session_store: Option<Arc<dyn SessionStore>>,
    circuit_breaker: Arc<CircuitBreaker>,
    memory_retrieval_available: bool,
    vector_retrieval_available: bool,
}

impl QueryRewriter {
    /// Creates a query rewriter backed by the provided LLM.
    pub fn new(config: QueryRewriteConfig, llm: Arc<dyn LLMProvider>) -> Self {
        let circuit_breaker = Arc::new(CircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_window_secs,
            config.circuit_breaker_cooldown_secs,
        ));
        Self::with_circuit_breaker(config, llm, circuit_breaker)
    }

    /// Creates a query rewriter that shares circuit-breaker state across fresh instances.
    pub fn new_with_shared_circuit(
        config: QueryRewriteConfig,
        llm: Arc<dyn LLMProvider>,
        namespace: impl AsRef<str>,
    ) -> Self {
        let circuit_breaker = shared_circuit_breaker(namespace.as_ref(), &config);
        Self::with_circuit_breaker(config, llm, circuit_breaker)
    }

    fn with_circuit_breaker(
        config: QueryRewriteConfig,
        llm: Arc<dyn LLMProvider>,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        Self {
            config,
            llm,
            session_store: None,
            circuit_breaker,
            memory_retrieval_available: false,
            vector_retrieval_available: false,
        }
    }

    /// Configures the rewriter to load recent user history directly from the session log.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }

    /// Configures whether downstream graph-memory and vector retrieval are available.
    #[must_use]
    pub fn with_retrieval_availability(
        mut self,
        memory_retrieval_available: bool,
        vector_retrieval_available: bool,
    ) -> Self {
        self.memory_retrieval_available = memory_retrieval_available;
        self.vector_retrieval_available = vector_retrieval_available;
        self
    }
}

static SHARED_CIRCUIT_BREAKERS: OnceLock<Mutex<HashMap<String, Arc<CircuitBreaker>>>> =
    OnceLock::new();

fn shared_circuit_breaker(namespace: &str, config: &QueryRewriteConfig) -> Arc<CircuitBreaker> {
    let key = format!(
        "{}:{:.6}:{}:{}",
        namespace,
        config.circuit_breaker_threshold,
        config.circuit_breaker_window_secs,
        config.circuit_breaker_cooldown_secs
    );
    let registry = SHARED_CIRCUIT_BREAKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = match registry.lock() {
        Ok(registry) => registry,
        Err(poisoned) => poisoned.into_inner(),
    };
    registry
        .entry(key)
        .or_insert_with(|| {
            Arc::new(CircuitBreaker::new(
                config.circuit_breaker_threshold,
                config.circuit_breaker_window_secs,
                config.circuit_breaker_cooldown_secs,
            ))
        })
        .clone()
}

#[async_trait]
impl ContextProcessor for QueryRewriter {
    fn name(&self) -> &str {
        "query_rewrite"
    }

    fn stage(&self) -> u8 {
        4
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if cached_rewrite_result(ctx).is_some() {
            let metadata = HashMap::from([
                ("moa.query_rewrite.decision".to_string(), json!("cached")),
                ("moa.query_rewrite.llm_called".to_string(), json!(false)),
                ("rewrite_source".to_string(), json!("cached")),
            ]);
            return Ok(ProcessorOutput {
                metadata,
                ..ProcessorOutput::default()
            });
        }

        let input = match self.load_input(ctx).await {
            Ok(Some(input)) => input,
            Ok(None) => RewriteInput::empty(),
            Err(error) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed to load history, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::original(""))?;
                return Ok(ProcessorOutput::default());
            }
        };

        let decision = gate::decide(RewriteGateInput {
            query: &input.query,
            history: &input.history,
            user_message_count: input.user_message_count,
            config: &self.config,
            memory_retrieval_available: self.memory_retrieval_available,
            vector_retrieval_available: self.vector_retrieval_available,
            circuit_open: self.circuit_breaker.is_open(),
        });
        if let RewriteDecision::Skip(reason) = decision {
            tracing::debug!(
                decision = "skip",
                reason = reason.as_str(),
                llm_called = false,
                "query rewrite gate skipped LLM call"
            );
            store_rewrite_result(ctx, QueryRewriteResult::original(input.query))?;
            return Ok(ProcessorOutput {
                metadata: decision_metadata("skip", reason.as_str(), false),
                ..ProcessorOutput::default()
            });
        }
        let RewriteDecision::Rewrite(reason) = decision else {
            unreachable!("skip handled above");
        };
        tracing::debug!(
            decision = "rewrite",
            reason = ?reason,
            llm_called = true,
            "query rewrite gate allowed LLM call"
        );

        let timeout = Duration::from_millis(self.config.timeout_ms);
        match tokio::time::timeout(timeout, self.rewrite(&input, ctx, reason)).await {
            Ok(Ok(result)) => {
                self.circuit_breaker.record_success();
                store_rewrite_result(ctx, result)?;
                Ok(ProcessorOutput {
                    metadata: decision_metadata("rewrite", &format!("{reason:?}"), true),
                    ..ProcessorOutput::default()
                })
            }
            Ok(Err(error)) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::original(input.query))?;
                Ok(ProcessorOutput {
                    metadata: decision_metadata("fallback", "llm_error", true),
                    ..ProcessorOutput::default()
                })
            }
            Err(_) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    timeout_ms = self.config.timeout_ms,
                    "query rewriter timed out, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::original(input.query))?;
                Ok(ProcessorOutput {
                    metadata: decision_metadata("fallback", "timeout", true),
                    ..ProcessorOutput::default()
                })
            }
        }
    }
}

fn cached_rewrite_result(ctx: &WorkingContext) -> Option<QueryRewriteResult> {
    ctx.metadata()
        .get(METADATA_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn decision_metadata(
    decision: &str,
    reason: &str,
    llm_called: bool,
) -> HashMap<String, serde_json::Value> {
    HashMap::from([
        ("moa.query_rewrite.decision".to_string(), json!(decision)),
        ("moa.query_rewrite.reason".to_string(), json!(reason)),
        (
            "moa.query_rewrite.llm_called".to_string(),
            json!(llm_called),
        ),
        ("rewrite_source".to_string(), json!(decision)),
    ])
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
        Result, RewriteReason, RewriteSource, SessionId, SessionMeta, StopReason, TokenPricing,
        TokenUsage, ToolCallFormat, UserId, WorkingContext, WorkspaceId,
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
            platform: Platform::Api,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        for message in messages {
            ctx.append_message(message);
        }
        ctx
    }

    fn response_json(retrieval_query: &str) -> String {
        json!({
            "retrieval_query": retrieval_query,
            "is_new_task": false,
            "task_summary": null,
            "task_facets": null,
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
            QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider))
                .with_retrieval_availability(true, true),
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
        // Pins: skipped first-turn queries store the original text and do not call the LLM.
        let (rewriter, calls) = rewriter_with_response(response_json("hello there"));
        let mut ctx = context_with_messages(vec![ContextMessage::user("hello")]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Original);
        assert_eq!(result.retrieval_query, "hello");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rewrites_multiturn_coreference() {
        // Pins: history-resolvable coreference calls the rewriter and records the rewrite reason.
        let (rewriter, calls) = rewriter_with_response(response_json(
            "Fix the OAuth refresh token race condition in auth/refresh.rs",
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
            result.retrieval_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
        assert_eq!(result.reason, Some(RewriteReason::CoreferenceWithHistory));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_rewrite_metadata_skips_provider_call() {
        // Pins: repeated compile steps reuse the turn's rewrite metadata instead of calling the rewrite LLM again.
        let (rewriter, calls) = rewriter_with_response(response_json("provider response"));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);
        let cached = QueryRewriteResult::rewritten(
            "Fix the OAuth refresh token race condition in auth/refresh.rs",
            RewriteReason::CoreferenceWithHistory,
        );
        ctx.insert_metadata(
            METADATA_KEY,
            serde_json::to_value(cached.clone()).expect("cached rewrite should serialize"),
        );

        let output = rewriter
            .process(&mut ctx)
            .await
            .expect("cached query rewrite should process");

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            output.metadata.get("rewrite_source"),
            Some(&json!("cached"))
        );
        assert_eq!(metadata_result(&ctx), cached);
    }

    #[tokio::test]
    async fn default_timeout_allows_segment_transition_rewrite_latency() {
        // Pins: the default timeout allows a live-like rewriter call that reports a segment transition.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(
                json!({
                    "retrieval_query": "Write a five-word project status headline about database migrations.",
                    "is_new_task": true,
                    "task_summary": "Write a short project status headline about database migrations.",
                })
                .to_string(),
            )),
            delay: Duration::from_millis(600),
            calls: calls.clone(),
        };
        let rewriter = QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider))
            .with_retrieval_availability(true, true);
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("We track database migration status in memory."),
            ContextMessage::assistant("Noted."),
            ContextMessage::user("What is the history of database migration status headlines?"),
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
    async fn skips_without_vector_retrieval() {
        // Pins: no embedder means no rewrite LLM call; original query is stored for retrieval.
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response_json("unused"))),
            delay: Duration::ZERO,
            calls: calls.clone(),
        };
        let rewriter = QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider))
            .with_retrieval_availability(true, false);
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("auth/refresh.rs handles OAuth refresh tokens"),
            ContextMessage::assistant("Noted."),
            ContextMessage::user("fix that and add tests"),
        ]);

        let output = rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(result.source, RewriteSource::Original);
        assert_eq!(result.retrieval_query, "fix that and add tests");
        assert_eq!(
            output.metadata.get("moa.query_rewrite.reason"),
            Some(&json!("no_vector_retrieval"))
        );
    }

    #[tokio::test]
    async fn timeout_falls_back_to_original_query() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response_json("unused"))),
            delay: Duration::from_millis(50),
            calls: calls.clone(),
        };
        let config = QueryRewriteConfig {
            timeout_ms: 1,
            ..QueryRewriteConfig::default()
        };
        let rewriter =
            QueryRewriter::new(config, Arc::new(provider)).with_retrieval_availability(true, true);
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
        assert_eq!(result.source, RewriteSource::Original);
        assert_eq!(result.retrieval_query, "fix that");
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
        assert_eq!(metadata_result(&second).source, RewriteSource::Original);
    }

    #[tokio::test]
    async fn shared_circuit_breaker_trips_across_rewriter_instances() {
        // Pins: production pipeline rebuilds reuse breaker state after rewrite failures.
        let namespace = format!("test-{}", uuid::Uuid::now_v7());
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let first_provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new("not json".to_string())),
            delay: Duration::ZERO,
            calls: first_calls.clone(),
        };
        let second_provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response_json("unused"))),
            delay: Duration::ZERO,
            calls: second_calls.clone(),
        };
        let first_rewriter = QueryRewriter::new_with_shared_circuit(
            QueryRewriteConfig::default(),
            Arc::new(first_provider),
            &namespace,
        )
        .with_retrieval_availability(true, true);
        let second_rewriter = QueryRewriter::new_with_shared_circuit(
            QueryRewriteConfig::default(),
            Arc::new(second_provider),
            &namespace,
        )
        .with_retrieval_availability(true, true);
        let mut first = context_with_messages(vec![
            ContextMessage::user("OAuth refresh token race condition"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);
        let mut second = first.clone();

        first_rewriter
            .process(&mut first)
            .await
            .expect("first failure should fail open");
        second_rewriter
            .process(&mut second)
            .await
            .expect("second instance should fail open through shared breaker");

        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
        assert_eq!(metadata_result(&second).source, RewriteSource::Original);
    }

    #[tokio::test]
    async fn parses_only_retrieval_and_segment_fields() {
        // Pins: advisory fields are gone from the response contract.
        let response = json!({
            "retrieval_query": "Fix the OAuth refresh token race condition in auth/refresh.rs",
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string();
        let (rewriter, calls) = rewriter_with_response(response);
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("slim response should parse");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert_eq!(
            result.retrieval_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
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
        assert!(!result.retrieval_query.contains("Kubernetes"));
        assert!(!result.retrieval_query.contains("deployment"));
        assert_eq!(
            result.retrieval_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
    }
}
