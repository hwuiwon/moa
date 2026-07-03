//! Test-only scripted provider utilities for deterministic integration coverage.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    MessageRole, MoaError, ModelCapabilities, Result, StopReason, TokenUsage, ToolCallContent,
    ToolInvocation,
};
use serde_json::Value;
const DEFAULT_INPUT_TOKENS: usize = 64;
const DEFAULT_DURATION_MS: u64 = 1;

/// Wall-clock pacing simulated by a scripted response.
///
/// `ttft` delays the first streamed block; the remaining budget up to `total`
/// elapses before the stream completes. This makes `llm_ttft` and `llm_call`
/// turn-step metrics meaningful under scripted load instead of ~0ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptedTiming {
    /// Delay before the first content block is emitted.
    pub ttft: Duration,
    /// Total simulated call duration (must be >= `ttft`).
    pub total: Duration,
}

/// Deterministic fault plan attached to one scripted response.
///
/// The first `fail_first_n` requests that resolve to this response fail with
/// a provider error modeled on `status`; subsequent requests succeed. Keyed
/// entries share one attempt counter across clones, so "fail twice then
/// recover" behaves the same no matter how many concurrent callers match.
#[derive(Debug, Clone)]
pub struct ScriptedFault {
    /// Number of leading requests to fail.
    pub fail_first_n: u32,
    /// Modeled provider status: 429 maps to a rate-limit error, everything
    /// else to a generic provider error.
    pub status: u16,
    /// Optional retry-after hint carried in the error message.
    pub retry_after: Option<Duration>,
    /// Emit the first block, then abort the stream with an error instead of
    /// completing. Applies on every matching request (not counted).
    pub abort_mid_stream: bool,
    attempts: Arc<AtomicU32>,
}

impl ScriptedFault {
    /// Creates a fault plan that fails the first `fail_first_n` requests.
    pub fn fail_first(fail_first_n: u32, status: u16, retry_after: Option<Duration>) -> Self {
        Self {
            fail_first_n,
            status,
            retry_after,
            abort_mid_stream: false,
            attempts: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Creates a fault plan that aborts every stream after the first block.
    pub fn abort_mid_stream() -> Self {
        Self {
            fail_first_n: 0,
            status: 500,
            retry_after: None,
            abort_mid_stream: true,
            attempts: Arc::new(AtomicU32::new(0)),
        }
    }

    fn take_failure(&self) -> Option<MoaError> {
        if self.fail_first_n == 0 {
            return None;
        }
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        if attempt >= self.fail_first_n {
            return None;
        }
        Some(self.to_error())
    }

    fn to_error(&self) -> MoaError {
        let retry_hint = self
            .retry_after
            .map(|delay| format!("; retry after {}ms", delay.as_millis()))
            .unwrap_or_default();
        if self.status == 429 {
            MoaError::RateLimited {
                retries: 0,
                message: format!("scripted rate limit (429){retry_hint}"),
            }
        } else {
            MoaError::ProviderError(format!(
                "scripted provider fault (status {}){retry_hint}",
                self.status
            ))
        }
    }
}

/// One scripted response block emitted by [`ScriptedProvider`].
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptedBlock {
    /// Plain assistant text.
    Text(String),
    /// Structured tool call block.
    ToolCall {
        /// Tool name.
        name: String,
        /// JSON input payload.
        input: Value,
        /// Provider-visible tool-use identifier.
        id: String,
    },
    /// Provider-native tool summary block.
    ProviderToolResult {
        /// Provider tool name.
        tool_name: String,
        /// Human-readable summary.
        summary: String,
    },
}

impl ScriptedBlock {
    /// Creates a scripted text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Creates a scripted tool-call block.
    pub fn tool_call(name: impl Into<String>, input: Value, id: impl Into<String>) -> Self {
        Self::ToolCall {
            name: name.into(),
            input,
            id: id.into(),
        }
    }

    /// Creates a provider-native tool-result block.
    pub fn provider_tool_result(tool_name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::ProviderToolResult {
            tool_name: tool_name.into(),
            summary: summary.into(),
        }
    }

    fn into_completion_content(self) -> CompletionContent {
        match self {
            Self::Text(text) => CompletionContent::Text(text),
            Self::ToolCall { name, input, id } => CompletionContent::ToolCall(ToolCallContent {
                invocation: ToolInvocation {
                    id: Some(id),
                    name,
                    input,
                },
                provider_metadata: None,
            }),
            Self::ProviderToolResult { tool_name, summary } => {
                CompletionContent::ProviderToolResult { tool_name, summary }
            }
        }
    }
}

/// One buffered scripted response returned by [`ScriptedProvider`].
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    /// Response content blocks.
    pub content: Vec<CompletionContent>,
    /// Provider stop reason.
    pub stop_reason: StopReason,
    /// Synthetic input token usage.
    pub input_tokens: usize,
    /// Synthetic cached input token usage.
    pub cached_input_tokens: usize,
    /// Synthetic cache-write token usage.
    pub cache_write_input_tokens: usize,
    /// Synthetic duration reported in the response.
    pub duration_ms: u64,
    /// Optional simulated wall-clock pacing. `None` keeps the historical
    /// return-immediately behavior for existing tests.
    pub timing: Option<ScriptedTiming>,
    /// Optional deterministic fault plan.
    pub fault: Option<ScriptedFault>,
}

impl ScriptedResponse {
    /// Creates a response from scripted blocks.
    pub fn from_blocks(blocks: Vec<ScriptedBlock>) -> Self {
        let has_tool_call = blocks
            .iter()
            .any(|block| matches!(block, ScriptedBlock::ToolCall { .. }));
        Self {
            content: blocks
                .into_iter()
                .map(ScriptedBlock::into_completion_content)
                .collect(),
            stop_reason: if has_tool_call {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            input_tokens: DEFAULT_INPUT_TOKENS,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            duration_ms: DEFAULT_DURATION_MS,
            timing: None,
            fault: None,
        }
    }

    /// Creates an end-turn response with one text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::from_blocks(vec![ScriptedBlock::text(text)])
    }

    /// Creates a tool-use response with one tool-call block.
    pub fn tool_call(name: impl Into<String>, input: Value, id: impl Into<String>) -> Self {
        Self::from_blocks(vec![ScriptedBlock::tool_call(name, input, id)])
    }

    /// Overrides synthetic token usage for the scripted response.
    pub fn with_usage(mut self, usage: TokenUsage) -> Self {
        self.input_tokens = usage.total_input_tokens();
        self.cached_input_tokens = usage.input_tokens_cache_read;
        self.cache_write_input_tokens = usage.input_tokens_cache_write;
        self
    }

    /// Attaches simulated wall-clock pacing and reports it as the duration.
    pub fn with_timing(mut self, ttft: Duration, total: Duration) -> Self {
        let total = total.max(ttft);
        self.timing = Some(ScriptedTiming { ttft, total });
        self.duration_ms = total.as_millis() as u64;
        self
    }

    /// Attaches a deterministic fault plan.
    pub fn with_fault(mut self, fault: ScriptedFault) -> Self {
        self.fault = Some(fault);
        self
    }
}

/// Deterministic provider that replays one scripted response per request and records requests.
///
/// Selection order per [`complete`](ScriptedProvider::complete) call is: keyed request matching
/// first, then the FIFO response queue, then the fallback response. Keyed entries are reusable and
/// resolved without mutation, so concurrent callers that share a match all receive the same
/// scripted completion; the FIFO queue keeps its consume-once semantics.
#[derive(Clone)]
pub struct ScriptedProvider {
    capabilities: ModelCapabilities,
    keyed: Arc<Vec<(String, ScriptedResponse)>>,
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    fallback_response: Arc<Mutex<Option<ScriptedResponse>>>,
    recorded_requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedProvider {
    /// Creates an empty scripted provider with fixed model capabilities.
    pub fn new(capabilities: ModelCapabilities) -> Self {
        Self {
            capabilities,
            keyed: Arc::new(Vec::new()),
            responses: Arc::new(Mutex::new(VecDeque::new())),
            fallback_response: Arc::new(Mutex::new(None)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Appends one prebuilt scripted response.
    pub fn push_response(self, response: ScriptedResponse) -> Self {
        if let Ok(mut responses) = self.responses.lock() {
            responses.push_back(response);
        }
        self
    }

    /// Configures a response used after the explicit response queue is exhausted.
    pub fn with_fallback_response(self, response: ScriptedResponse) -> Self {
        if let Ok(mut fallback) = self.fallback_response.lock() {
            *fallback = Some(response);
        }
        self
    }

    /// Registers a reusable response returned whenever `match_substring` appears in a request's
    /// system, user, or tool-result message text.
    ///
    /// Keyed entries are checked in registration order and the first match wins, so callers should
    /// register specific substrings before general ones. Unlike the FIFO queue, keyed entries are
    /// never consumed, which lets multiple concurrent callers that share a match resolve the same
    /// completion deterministically. Tool-result text participates so scripts can key an agent
    /// loop's follow-up iteration on the output of the tool it just ran — the only content that
    /// distinguishes one iteration from the next.
    pub fn push_keyed(
        mut self,
        match_substring: impl Into<String>,
        response: ScriptedResponse,
    ) -> Self {
        Arc::make_mut(&mut self.keyed).push((match_substring.into(), response));
        self
    }

    /// Returns the first keyed response whose match substring is contained in the request's
    /// system, user, or tool-result message text, resolving without mutating the keyed table.
    fn match_keyed(&self, request: &CompletionRequest) -> Option<ScriptedResponse> {
        if self.keyed.is_empty() {
            return None;
        }
        let haystack = request
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message.role,
                    MessageRole::System | MessageRole::User | MessageRole::Tool
                )
            })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.keyed
            .iter()
            .find(|(needle, _)| haystack.contains(needle.as_str()))
            .map(|(_, response)| response.clone())
    }

    /// Appends one end-turn text response.
    pub fn push_text(self, text: impl Into<String>) -> Self {
        self.push_response(ScriptedResponse::text(text))
    }

    /// Appends one tool-call response.
    pub fn push_tool_call(
        self,
        name: impl Into<String>,
        input: Value,
        id: impl Into<String>,
    ) -> Self {
        self.push_response(ScriptedResponse::tool_call(name, input, id))
    }

    /// Appends one response composed from multiple scripted blocks.
    pub fn push_multi_block(self, blocks: Vec<ScriptedBlock>) -> Self {
        self.push_response(ScriptedResponse::from_blocks(blocks))
    }

    /// Appends one final end-turn response.
    pub fn push_end_turn(self, text: impl Into<String>) -> Self {
        self.push_text(text)
    }

    /// Returns all completion requests recorded so far.
    pub fn recorded_requests(&self) -> Vec<CompletionRequest> {
        self.recorded_requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl LLMProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities.clone()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        // Resolve the keyed match before recording moves the request; keyed lookup is pure so it
        // stays correct under concurrent callers.
        let keyed_response = self.match_keyed(&request);
        self.recorded_requests
            .lock()
            .map_err(|error| {
                moa_core::MoaError::ProviderError(format!(
                    "scripted provider request log poisoned: {error}"
                ))
            })?
            .push(request);
        let response = match keyed_response {
            Some(response) => response,
            None => {
                let queued = self
                    .responses
                    .lock()
                    .map_err(|error| {
                        moa_core::MoaError::ProviderError(format!(
                            "scripted provider response queue poisoned: {error}"
                        ))
                    })?
                    .pop_front();
                match queued {
                    Some(response) => response,
                    None => self
                        .fallback_response
                        .lock()
                        .map_err(|error| {
                            moa_core::MoaError::ProviderError(format!(
                                "scripted provider fallback response poisoned: {error}"
                            ))
                        })?
                        .clone()
                        .ok_or_else(|| {
                            moa_core::MoaError::ProviderError(
                                "scripted provider ran out of queued responses".to_string(),
                            )
                        })?,
                }
            }
        };
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                CompletionContent::Text(text) => Some(text.as_str()),
                CompletionContent::ToolCall(_) | CompletionContent::ProviderToolResult { .. } => {
                    None
                }
            })
            .collect::<String>();
        let output_tokens = response
            .content
            .iter()
            .map(|block| match block {
                CompletionContent::Text(text) => text.chars().count().div_ceil(4),
                CompletionContent::ToolCall(call) => {
                    8 + call
                        .invocation
                        .input
                        .to_string()
                        .chars()
                        .count()
                        .div_ceil(4)
                }
                CompletionContent::ProviderToolResult { summary, .. } => {
                    summary.chars().count().div_ceil(4)
                }
            })
            .sum();

        if let Some(fault) = &response.fault
            && let Some(error) = fault.take_failure()
        {
            return Err(error);
        }
        let abort_mid_stream = response
            .fault
            .as_ref()
            .is_some_and(|fault| fault.abort_mid_stream);
        let timing = response.timing;

        let completion_response = CompletionResponse {
            text,
            content: response.content,
            stop_reason: response.stop_reason,
            model: self.capabilities.model_id.clone(),
            usage: TokenUsage {
                input_tokens_uncached: response
                    .input_tokens
                    .saturating_sub(response.cached_input_tokens)
                    .saturating_sub(response.cache_write_input_tokens),
                input_tokens_cache_write: response.cache_write_input_tokens,
                input_tokens_cache_read: response.cached_input_tokens,
                output_tokens,
            },
            duration_ms: response.duration_ms,
            thought_signature: None,
        };

        if timing.is_none() && !abort_mid_stream {
            // Historical fast path: fully buffered, returns immediately.
            return Ok(CompletionStream::from_response(completion_response));
        }

        // Simulated streaming: honor TTFT before the first block, pace the
        // remaining blocks across the rest of the budget, and optionally
        // abort after the first block instead of completing.
        let (tx, rx) = tokio::sync::mpsc::channel(completion_response.content.len().max(1));
        let completion = tokio::spawn(async move {
            let timing = timing.unwrap_or(ScriptedTiming {
                ttft: Duration::ZERO,
                total: Duration::ZERO,
            });
            tokio::time::sleep(timing.ttft).await;
            let mut blocks = completion_response.content.clone().into_iter();
            if let Some(first) = blocks.next() {
                let _ = tx.send(Ok(first)).await;
            }
            if abort_mid_stream {
                let make_error = || {
                    MoaError::ProviderError(
                        "scripted provider aborted the stream mid-response".to_string(),
                    )
                };
                let _ = tx.send(Err(make_error())).await;
                return Err(make_error());
            }
            let rest: Vec<_> = blocks.collect();
            let remaining = timing.total.saturating_sub(timing.ttft);
            if rest.is_empty() {
                tokio::time::sleep(remaining).await;
            } else {
                let per_block = remaining / rest.len() as u32;
                for block in rest {
                    tokio::time::sleep(per_block).await;
                    let _ = tx.send(Ok(block)).await;
                }
            }
            Ok(completion_response)
        });
        Ok(CompletionStream::new(rx, completion))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::ContextMessage;

    /// Builds a request whose concatenated system + user text contains `text`.
    fn user_request(text: &str) -> CompletionRequest {
        CompletionRequest::new(text)
    }

    /// Drains a completion stream and returns its aggregated assistant text.
    async fn complete_text(provider: &ScriptedProvider, request: CompletionRequest) -> String {
        provider
            .complete(request)
            .await
            .expect("scripted completion")
            .collect()
            .await
            .expect("aggregated response")
            .text
    }

    #[tokio::test]
    async fn keyed_match_returns_completion_and_is_reusable() {
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_keyed("worker-alpha", ScriptedResponse::text("alpha-done"));

        // The same keyed entry resolves across repeated concurrent-style calls without being
        // consumed, so every worker sharing the match sees the same completion.
        for _ in 0..3 {
            let text = complete_text(&provider, user_request("please dispatch worker-alpha")).await;
            assert_eq!(text, "alpha-done");
        }
    }

    #[tokio::test]
    async fn no_keyed_match_falls_back_to_fifo_then_default() {
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_keyed("worker-alpha", ScriptedResponse::text("alpha-done"))
            .push_text("queued-1")
            .with_fallback_response(ScriptedResponse::text("fallback"));

        // No keyed substring present: the FIFO queue is consumed first, then the fallback.
        let first = complete_text(&provider, user_request("unrelated instruction")).await;
        assert_eq!(first, "queued-1");
        let second = complete_text(&provider, user_request("still unrelated")).await;
        assert_eq!(second, "fallback");
    }

    #[tokio::test]
    async fn first_registered_keyed_match_wins() {
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_keyed("worker-alpha", ScriptedResponse::text("specific"))
            .push_keyed("worker", ScriptedResponse::text("general"));

        // Both substrings match; registration order decides the winner.
        let text = complete_text(&provider, user_request("run worker-alpha")).await;
        assert_eq!(text, "specific");

        // A request matching only the general entry still resolves it.
        let text = complete_text(&provider, user_request("run worker-beta")).await;
        assert_eq!(text, "general");
    }

    #[tokio::test]
    async fn keyed_match_reads_system_and_user_messages() {
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_keyed("role=reviewer", ScriptedResponse::text("reviewing"));

        let request = CompletionRequest {
            messages: vec![
                ContextMessage::system("You are role=reviewer for this session."),
                ContextMessage::user("Look at the diff."),
            ],
            ..CompletionRequest::new("ignored")
        };
        assert_eq!(complete_text(&provider, request).await, "reviewing");
    }

    #[tokio::test]
    async fn keyed_match_reads_tool_result_messages() {
        // Pins: tool-result text participates in keyed matching, so a script can
        // key an agent loop's next iteration on the output of the tool it just ran.
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_keyed("probe-output-ok", ScriptedResponse::text("observed"))
            .push_keyed("run the probe", ScriptedResponse::text("still-running"));

        let request = CompletionRequest {
            messages: vec![
                ContextMessage::user("run the probe"),
                ContextMessage::tool_result("tool-1", "probe-output-ok", None),
            ],
            ..CompletionRequest::new("ignored")
        };
        assert_eq!(complete_text(&provider, request).await, "observed");
    }

    #[tokio::test]
    async fn fault_fails_first_n_requests_then_recovers() {
        // Pins: a fail_first_n fault plan errors deterministically (429 maps to
        // RateLimited) and then recovers, sharing one counter across keyed reuse.
        let provider = ScriptedProvider::new(ModelCapabilities::default()).push_keyed(
            "flaky",
            ScriptedResponse::text("recovered").with_fault(ScriptedFault::fail_first(
                2,
                429,
                Some(Duration::from_millis(250)),
            )),
        );

        for attempt in 0..2 {
            let error = provider
                .complete(user_request("flaky prompt"))
                .await
                .expect_err("first two attempts should fail");
            assert!(
                matches!(error, MoaError::RateLimited { .. }),
                "attempt {attempt} should be rate limited, got {error:?}"
            );
        }
        let text = complete_text(&provider, user_request("flaky prompt")).await;
        assert_eq!(text, "recovered");
    }

    #[tokio::test]
    async fn mid_stream_abort_emits_first_block_then_errors() {
        // Pins: abort_mid_stream yields partial content and then a stream
        // error, never a completed response.
        let provider = ScriptedProvider::new(ModelCapabilities::default()).push_keyed(
            "abort",
            ScriptedResponse::text("partial").with_fault(ScriptedFault::abort_mid_stream()),
        );

        let mut stream = provider
            .complete(user_request("abort now"))
            .await
            .expect("stream opens before the abort");
        let first = stream.next().await.expect("first block present");
        assert!(first.is_ok(), "first block should be content: {first:?}");
        let second = stream.next().await.expect("second frame present");
        assert!(second.is_err(), "stream should abort after first block");
    }

    #[tokio::test(start_paused = true)]
    async fn timing_delays_first_block_by_ttft_and_completion_by_total() {
        // Pins: latency simulation sleeps ttft before the first block and the
        // rest of the budget before completion, so llm_ttft/llm_call step
        // metrics are meaningful under scripted load.
        let provider = ScriptedProvider::new(ModelCapabilities::default()).push_keyed(
            "timed",
            ScriptedResponse::text("slow")
                .with_timing(Duration::from_millis(200), Duration::from_millis(800)),
        );

        let started = tokio::time::Instant::now();
        let mut stream = provider
            .complete(user_request("timed prompt"))
            .await
            .expect("stream opens");
        let first = stream.next().await.expect("first block");
        assert!(first.is_ok());
        let ttft_elapsed = started.elapsed();
        assert!(
            ttft_elapsed >= Duration::from_millis(200),
            "first block arrived before ttft: {ttft_elapsed:?}"
        );
        let response = stream.collect().await.expect("completion");
        let total_elapsed = started.elapsed();
        assert!(
            total_elapsed >= Duration::from_millis(800),
            "completion arrived before total budget: {total_elapsed:?}"
        );
        assert_eq!(response.duration_ms, 800);
    }
}
