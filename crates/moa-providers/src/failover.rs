//! Rate-limit-aware LLM failover across a configured model chain.
//!
//! [`FailoverLLMProvider`] wraps a primary [`LLMProvider`] plus an ordered chain
//! of fallback providers/models. When the primary is blocked (a paused provider,
//! an exhausted retry budget, a saturated concurrency gate, or a 429/overload
//! response) *before any tokens are streamed*, the wrapper transparently retries
//! the request on the next chain entry. Once a candidate emits its first content
//! block the stream is committed to that candidate; a later failure surfaces to
//! the caller exactly as it would without failover — there is no mid-stream
//! provider switch.
//!
//! The streamed response and its final [`CompletionResponse`] carry the model of
//! the candidate that actually served the request, so token usage and pricing are
//! attributed to the real model rather than the primary.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionStream, LLMProvider, MoaError,
    ModelCapabilities, ModelId, Result,
};
use tokio::sync::mpsc;

/// Buffer size for the failover forwarding stream.
const FORWARD_STREAM_BUFFER: usize = 64;

/// One entry in a failover chain: a provider and the model to request from it.
struct FailoverCandidate {
    provider: Arc<dyn LLMProvider>,
    /// Model requested from this candidate. The primary carries `None` so it
    /// honors the caller's requested model; each fallback pins its own model.
    model: Option<ModelId>,
}

impl FailoverCandidate {
    /// Returns a stable model label for tracing/metrics attribution.
    fn model_label(&self) -> String {
        self.model
            .as_ref()
            .map(|model| model.as_str().to_string())
            .unwrap_or_else(|| self.provider.capabilities().model_id.as_str().to_string())
    }
}

/// LLM provider that fails over across a chain of providers on rate-limit-class
/// blocks encountered before streaming begins.
pub struct FailoverLLMProvider {
    chain: Vec<FailoverCandidate>,
}

impl FailoverLLMProvider {
    /// Wraps a primary provider with an ordered failover chain of fallbacks.
    ///
    /// Each fallback is a `(provider, model)` pair; the primary honors the
    /// caller's requested model. Returns the bare primary when `fallbacks` is
    /// empty so no wrapper overhead is added unless failover is configured.
    #[must_use]
    pub fn wrap(
        primary: Arc<dyn LLMProvider>,
        fallbacks: Vec<(Arc<dyn LLMProvider>, ModelId)>,
    ) -> Arc<dyn LLMProvider> {
        if fallbacks.is_empty() {
            return primary;
        }
        let mut chain = Vec::with_capacity(fallbacks.len() + 1);
        chain.push(FailoverCandidate {
            provider: primary,
            model: None,
        });
        for (provider, model) in fallbacks {
            chain.push(FailoverCandidate {
                provider,
                model: Some(model),
            });
        }
        Arc::new(Self { chain })
    }

    async fn try_candidate(
        &self,
        candidate: &FailoverCandidate,
        request: CompletionRequest,
    ) -> CandidateOutcome {
        let mut stream = match candidate.provider.complete(request).await {
            Ok(stream) => stream,
            // A rate-class failure before the stream was even created is the
            // cleanest failover signal (paused provider / exhausted budget /
            // saturated gate all surface here as `complete()` errors).
            Err(error) => return CandidateOutcome::from_pre_token_error(error),
        };

        match stream.next().await {
            // First content block: this candidate owns the stream from here on.
            Some(Ok(block)) => CandidateOutcome::Streaming(prepend_and_forward(block, stream)),
            // First item is an error, before any tokens reached the caller.
            Some(Err(error)) => {
                stream.abort();
                CandidateOutcome::from_pre_token_error(error)
            }
            // No streamed blocks; the terminal result decides success vs failover.
            None => match stream.into_response().await {
                Ok(response) => {
                    CandidateOutcome::Streaming(CompletionStream::from_response(response))
                }
                Err(error) => CandidateOutcome::from_pre_token_error(error),
            },
        }
    }
}

#[async_trait]
impl LLMProvider for FailoverLLMProvider {
    fn name(&self) -> &str {
        self.chain[0].provider.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.chain[0].provider.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let last_index = self.chain.len() - 1;
        let mut last_error: Option<MoaError> = None;

        for index in 0..self.chain.len() {
            let candidate = &self.chain[index];
            let mut candidate_request = request.clone();
            if let Some(model) = &candidate.model {
                candidate_request.model = Some(model.clone());
            }

            match self.try_candidate(candidate, candidate_request).await {
                CandidateOutcome::Streaming(stream) => return Ok(stream),
                CandidateOutcome::Blocked(error) if index < last_index => {
                    let reason = failover_reason(&error).unwrap_or("rate_limited");
                    record_failover(
                        &candidate.model_label(),
                        &self.chain[index + 1].model_label(),
                        reason,
                    );
                    last_error = Some(error);
                }
                // Last candidate blocked, or any non-rate error before tokens:
                // surface it. There is nothing left to fail over to.
                CandidateOutcome::Blocked(error) | CandidateOutcome::Surface(error) => {
                    return Err(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            MoaError::ConfigError("failover chain produced no result".to_string())
        }))
    }
}

/// Outcome of trying one failover candidate.
enum CandidateOutcome {
    /// The candidate produced (or is producing) a response; forward it.
    Streaming(CompletionStream),
    /// A rate-limit-class block before any tokens; eligible for failover.
    Blocked(MoaError),
    /// A non-rate error before any tokens; surface without failover.
    Surface(MoaError),
}

impl CandidateOutcome {
    fn from_pre_token_error(error: MoaError) -> Self {
        if failover_reason(&error).is_some() {
            Self::Blocked(error)
        } else {
            Self::Surface(error)
        }
    }
}

/// Returns the failover reason label for a rate-limit-class error, or `None`.
fn failover_reason(error: &MoaError) -> Option<&'static str> {
    match error {
        MoaError::RateLimited { .. } => Some("rate_limited"),
        MoaError::HttpStatus { status: 429, .. } => Some("rate_limited"),
        MoaError::HttpStatus {
            status: 502..=504, ..
        } => Some("overloaded"),
        _ => None,
    }
}

/// Builds a stream that yields `first` and then forwards the rest of `inner`,
/// preserving `inner`'s final response (and thus its model attribution).
fn prepend_and_forward(first: CompletionContent, mut inner: CompletionStream) -> CompletionStream {
    let (tx, rx) = mpsc::channel(FORWARD_STREAM_BUFFER);
    let completion = tokio::spawn(async move {
        if tx.send(Ok(first)).await.is_err() {
            inner.abort();
            return Err(MoaError::ProviderError(
                "completion stream receiver closed before forwarding started".to_string(),
            ));
        }
        while let Some(item) = inner.next().await {
            if tx.send(item).await.is_err() {
                inner.abort();
                return Err(MoaError::ProviderError(
                    "completion stream receiver closed while forwarding".to_string(),
                ));
            }
        }
        inner.into_response().await
    });
    CompletionStream::new(rx, completion)
}

/// Emits a structured tracing event and a counter for one failover hop.
fn record_failover(from_model: &str, to_model: &str, reason: &str) {
    tracing::warn!(
        moa.llm.failover = true,
        from_model,
        to_model,
        reason,
        "LLM request failed over to a fallback model after a rate-limit-class block"
    );
    metrics::counter!(
        "moa_llm_failover_total",
        "from_model" => from_model.to_string(),
        "to_model" => to_model.to_string(),
        "reason" => reason.to_string(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
        MoaError, ModelCapabilities, ModelId, Result, StopReason, TokenPricing, TokenUsage,
        ToolCallFormat,
    };
    use tokio::sync::mpsc;
    use tokio::time::sleep;

    use super::{FailoverLLMProvider, prepend_and_forward};

    /// A fake provider that either serves a fixed model or fails with a preset error.
    struct FakeProvider {
        name: &'static str,
        model: &'static str,
        behavior: Behavior,
        calls: Arc<AtomicUsize>,
    }

    enum Behavior {
        /// Succeeds, streaming one text block attributed to `model`.
        Serve,
        /// Fails `complete()` with the given error.
        Fail(fn() -> MoaError),
    }

    impl FakeProvider {
        fn new(name: &'static str, model: &'static str, behavior: Behavior) -> Self {
            Self {
                name,
                model,
                behavior,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LLMProvider for FakeProvider {
        fn name(&self) -> &str {
            self.name
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new(self.model),
                context_window: 32_000,
                max_output: 1_024,
                supports_tools: false,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::OpenAiCompatible,
                pricing: TokenPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                Behavior::Fail(make_error) => Err(make_error()),
                Behavior::Serve => {
                    // Attribute the response to the model the request selected, so
                    // tests can assert the real serving model.
                    let served_model = request
                        .model
                        .clone()
                        .unwrap_or_else(|| ModelId::new(self.model));
                    Ok(CompletionStream::from_response(CompletionResponse {
                        text: "served".to_string(),
                        content: vec![CompletionContent::Text("served".to_string())],
                        stop_reason: StopReason::EndTurn,
                        model: served_model,
                        usage: TokenUsage::default(),
                        duration_ms: 1,
                        thought_signature: None,
                    }))
                }
            }
        }
    }

    fn rate_limited() -> MoaError {
        MoaError::RateLimited {
            retries: 0,
            message: "paused".to_string(),
        }
    }

    fn fatal() -> MoaError {
        MoaError::ProviderError("boom".to_string())
    }

    struct AbortMarker(Arc<AtomicBool>);

    impl Drop for AbortMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn forwarded_stream_aborts_inner_when_receiver_is_dropped() {
        // Pins: dropping the caller-facing failover stream must abort the inner
        // provider stream instead of waiting forever on its completion task.
        let aborted = Arc::new(AtomicBool::new(false));
        let aborted_task = Arc::clone(&aborted);
        let (_tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            let _marker = AbortMarker(aborted_task);
            std::future::pending::<Result<CompletionResponse>>().await
        });
        let inner = CompletionStream::new(rx, completion);

        let forwarded = prepend_and_forward(CompletionContent::Text("first".to_string()), inner);
        drop(forwarded);

        for _ in 0..20 {
            if aborted.load(Ordering::SeqCst) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }

        panic!("dropping the forwarded receiver should abort the inner stream");
    }

    #[tokio::test]
    async fn failover_on_rate_limited_primary_serves_fallback_with_its_model() {
        // Pins: a rate-limited primary fails over to the fallback, and the response
        // is attributed to the fallback's model, not the primary's.
        let primary = Arc::new(FakeProvider::new(
            "anthropic",
            "claude-sonnet-4-6",
            Behavior::Fail(rate_limited),
        ));
        let fallback = Arc::new(FakeProvider::new("openai", "gpt-5.4", Behavior::Serve));

        let failover = FailoverLLMProvider::wrap(
            primary.clone(),
            vec![(fallback.clone(), ModelId::new("gpt-5.4"))],
        );

        let response = failover
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("failover should produce the fallback stream")
            .into_response()
            .await
            .expect("fallback response should resolve");

        assert_eq!(response.model, ModelId::new("gpt-5.4"));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
    }

    #[tokio::test]
    async fn primary_success_does_not_touch_fallback() {
        // Pins: when the primary serves, the fallback is never called and the
        // primary's model is attributed.
        let primary = Arc::new(FakeProvider::new(
            "anthropic",
            "claude-sonnet-4-6",
            Behavior::Serve,
        ));
        let fallback = Arc::new(FakeProvider::new("openai", "gpt-5.4", Behavior::Serve));

        let failover = FailoverLLMProvider::wrap(
            primary.clone(),
            vec![(fallback.clone(), ModelId::new("gpt-5.4"))],
        );

        let response = failover
            .complete(CompletionRequest::new("hi"))
            .await
            .expect("primary should serve")
            .into_response()
            .await
            .expect("primary response should resolve");

        assert_eq!(response.model, ModelId::new("claude-sonnet-4-6"));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 0);
    }

    #[tokio::test]
    async fn non_rate_error_surfaces_without_failover() {
        // Pins: a non-rate error is surfaced immediately; the fallback is not tried.
        let primary = Arc::new(FakeProvider::new(
            "anthropic",
            "claude-sonnet-4-6",
            Behavior::Fail(fatal),
        ));
        let fallback = Arc::new(FakeProvider::new("openai", "gpt-5.4", Behavior::Serve));

        let failover = FailoverLLMProvider::wrap(
            primary.clone(),
            vec![(fallback.clone(), ModelId::new("gpt-5.4"))],
        );

        let error = failover
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("a non-rate primary error must not fail over");

        assert!(matches!(error, MoaError::ProviderError(_)));
        assert_eq!(fallback.calls(), 0);
    }

    #[tokio::test]
    async fn exhausted_chain_returns_the_last_rate_error() {
        // Pins: when every candidate is rate limited, the terminal error is surfaced.
        let primary = Arc::new(FakeProvider::new(
            "anthropic",
            "claude-sonnet-4-6",
            Behavior::Fail(rate_limited),
        ));
        let fallback = Arc::new(FakeProvider::new(
            "openai",
            "gpt-5.4",
            Behavior::Fail(rate_limited),
        ));

        let failover = FailoverLLMProvider::wrap(
            primary.clone(),
            vec![(fallback.clone(), ModelId::new("gpt-5.4"))],
        );

        let error = failover
            .complete(CompletionRequest::new("hi"))
            .await
            .expect_err("an all-rate-limited chain surfaces the terminal error");

        assert!(matches!(error, MoaError::RateLimited { .. }));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
    }

    #[tokio::test]
    async fn empty_fallbacks_returns_bare_primary() {
        // Pins: with no fallbacks configured, no wrapper is added.
        let primary = Arc::new(FakeProvider::new(
            "anthropic",
            "claude-sonnet-4-6",
            Behavior::Serve,
        ));
        let provider = FailoverLLMProvider::wrap(primary.clone(), Vec::new());
        assert!(Arc::ptr_eq(&(primary as Arc<dyn LLMProvider>), &provider));
    }
}
