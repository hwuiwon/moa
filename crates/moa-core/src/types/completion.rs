//! Provider completion request, response, and streaming types.

use std::collections::HashMap;
use std::fmt::{self, Formatter};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{MoaError, Result};

use super::{ContextMessage, ModelId};

/// Single tool invocation emitted by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// Provider-specific tool call identifier.
    pub id: Option<String>,
    /// Tool name.
    pub name: String,
    /// JSON input payload.
    pub input: Value,
}

/// Provider-specific metadata attached to one emitted tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ProviderToolCallMetadata {
    /// Gemini thought signature that must be replayed with the original model turn.
    Gemini {
        /// Opaque provider-issued thought signature.
        thought_signature: String,
    },
}

impl ProviderToolCallMetadata {
    /// Returns the thought signature when this metadata carries one.
    pub fn thought_signature(&self) -> Option<&str> {
        match self {
            Self::Gemini { thought_signature } => Some(thought_signature.as_str()),
        }
    }
}

/// One structured tool call emitted in streamed or buffered provider output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContent {
    /// Canonical tool invocation details.
    pub invocation: ToolInvocation,
    /// Optional provider-specific replay metadata for this tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderToolCallMetadata>,
}

/// Logical content blocks in a completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionContent {
    /// Text content.
    Text(String),
    /// Tool call content.
    ToolCall(ToolCallContent),
    /// Informational output from a provider-native tool.
    ProviderToolResult {
        /// Provider-native tool name.
        tool_name: String,
        /// Concise summary suitable for UI status output.
        summary: String,
    },
}

/// Completion stop reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model completed the turn normally.
    EndTurn,
    /// Output stopped because it hit a token limit.
    MaxTokens,
    /// Output stopped to request tool execution.
    ToolUse,
    /// Output stopped because the request was cancelled.
    Cancelled,
    /// Provider-specific or unknown reason.
    Other(String),
}

/// Provider-native JSON response-format request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonResponseFormat {
    /// Stable schema name accepted by providers.
    pub name: String,
    /// Optional model-facing schema description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema object that the provider should enforce.
    pub schema: Value,
    /// Whether providers that support strict schema mode should enable it.
    pub strict: bool,
}

impl JsonResponseFormat {
    /// Creates a strict JSON-schema response format.
    #[must_use]
    pub fn strict_json_schema(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            schema,
            strict: true,
        }
    }
}

/// Provider completion request payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Optional model override.
    pub model: Option<ModelId>,
    /// Context messages.
    pub messages: Vec<ContextMessage>,
    /// Tool schemas available to the provider.
    pub tools: Vec<Value>,
    /// Maximum output token count.
    pub max_output_tokens: Option<usize>,
    /// Optional temperature override.
    pub temperature: Option<f32>,
    /// Optional provider-native JSON response-format request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<JsonResponseFormat>,
    /// Message-boundary cache breakpoints used by providers that support explicit prompt caching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_breakpoints: Vec<usize>,
    /// Detailed explicit cache-control markers for providers that support TTL-aware prompt caching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_controls: Vec<CacheBreakpoint>,
    /// Request-scoped metadata.
    pub metadata: HashMap<String, Value>,
}

impl CompletionRequest {
    /// Creates a minimal request with a single user message.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            model: None,
            messages: vec![ContextMessage::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Creates a minimal request alias for simple prompt-only completions.
    pub fn simple(prompt: impl Into<String>) -> Self {
        Self::new(prompt)
    }
}

/// Supported explicit prompt-cache TTL classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// Anthropic's default short-lived prompt cache.
    FiveMinutes,
    /// Anthropic's long-lived prompt cache for stable prefixes.
    OneHour,
}

impl CacheTtl {
    /// Returns the provider wire-format TTL string for Anthropic-compatible requests.
    pub fn as_anthropic_ttl(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// Explicit cache-control target for one prompt prefix boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CacheBreakpointTarget {
    /// Cache everything through the tool definitions block.
    ToolDefinitions,
    /// Cache everything through the indexed message boundary.
    MessageBoundary {
        /// Count of request messages included in the cached prefix.
        index: usize,
    },
}

/// Explicit prompt-cache marker with a target and TTL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakpoint {
    /// Target boundary for the cached prompt prefix.
    pub target: CacheBreakpointTarget,
    /// Requested cache TTL for providers that support it.
    pub ttl: CacheTtl,
}

impl CacheBreakpoint {
    /// Creates a cache breakpoint on a message boundary.
    pub fn message(index: usize, ttl: CacheTtl) -> Self {
        Self {
            target: CacheBreakpointTarget::MessageBoundary { index },
            ttl,
        }
    }

    /// Creates a cache breakpoint on the tool definitions block.
    pub fn tools(ttl: CacheTtl) -> Self {
        Self {
            target: CacheBreakpointTarget::ToolDefinitions,
            ttl,
        }
    }

    /// Returns the message-boundary index when this breakpoint targets messages.
    pub fn message_index(&self) -> Option<usize> {
        match self.target {
            CacheBreakpointTarget::MessageBoundary { index } => Some(index),
            CacheBreakpointTarget::ToolDefinitions => None,
        }
    }
}

/// Normalized provider token-usage counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens billed at the provider's standard uncached rate.
    pub input_tokens_uncached: usize,
    /// Input tokens billed to create or refresh a cache entry.
    pub input_tokens_cache_write: usize,
    /// Input tokens served from an existing cache entry.
    pub input_tokens_cache_read: usize,
    /// Output tokens emitted by the provider.
    pub output_tokens: usize,
}

impl TokenUsage {
    /// Returns the total number of input tokens across uncached, cache-write, and cache-read usage.
    pub fn total_input_tokens(&self) -> usize {
        self.input_tokens_uncached + self.input_tokens_cache_write + self.input_tokens_cache_read
    }

    /// Returns the fraction of input tokens that were served from cache.
    pub fn cache_hit_rate(&self) -> f64 {
        let denom = self.total_input_tokens();
        if denom == 0 {
            return 0.0;
        }

        self.input_tokens_cache_read as f64 / denom as f64
    }
}

/// Provider completion response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// Aggregated text response.
    pub text: String,
    /// Structured response blocks.
    pub content: Vec<CompletionContent>,
    /// Provider stop reason.
    pub stop_reason: StopReason,
    /// Model identifier used.
    pub model: ModelId,
    /// Normalized provider token-usage counters.
    pub usage: TokenUsage,
    /// Total request duration in milliseconds.
    pub duration_ms: u64,
    /// Provider-specific thought signature that should be replayed on the next turn when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl CompletionResponse {
    /// Returns normalized token usage for the response.
    pub fn token_usage(&self) -> TokenUsage {
        self.usage
    }
}

/// Streaming provider response wrapper.
///
/// NOTE: This type wraps async runtime primitives (`tokio::sync::mpsc`,
/// `tokio::task::JoinHandle`, and `CancellationToken`) and would ideally live
/// alongside provider implementations. It remains in `moa-core` because the
/// `LLMProvider` trait is also defined in `moa-core` and returns this type
/// directly, so moving it out would either create a crate cycle or force a
/// broader trait redesign.
pub struct CompletionStream {
    receiver: mpsc::Receiver<Result<CompletionContent>>,
    completion: JoinHandle<Result<CompletionResponse>>,
    cancel_token: Option<CancellationToken>,
}

impl CompletionStream {
    /// Creates a new completion stream from a content receiver and completion task.
    pub fn new(
        receiver: mpsc::Receiver<Result<CompletionContent>>,
        completion: JoinHandle<Result<CompletionResponse>>,
    ) -> Self {
        Self {
            receiver,
            completion,
            cancel_token: None,
        }
    }

    /// Attaches a cooperative cancellation token to the stream.
    #[must_use = "dropping the returned stream discards cancellation support"]
    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    /// Creates a replayable stream from a fully buffered response.
    pub fn from_response(response: CompletionResponse) -> Self {
        let buffered_blocks = response.content.clone();
        let capacity = buffered_blocks.len().max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let completion = tokio::spawn(async move {
            for block in buffered_blocks {
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }

            Ok(response)
        });

        Self::new(rx, completion)
    }

    /// Receives the next streamed content block, if one is available.
    pub async fn next(&mut self) -> Option<Result<CompletionContent>> {
        self.receiver.recv().await
    }

    /// Drains the remaining stream and returns the final aggregated response.
    pub async fn collect(mut self) -> Result<CompletionResponse> {
        while let Some(block) = self.receiver.recv().await {
            block?;
        }

        self.await_completion().await
    }

    /// Waits for the provider task to finish and returns the final aggregated response.
    pub async fn into_response(self) -> Result<CompletionResponse> {
        self.await_completion().await
    }

    /// Aborts the underlying provider task and signals cooperative cancellation.
    pub fn abort(&self) {
        if let Some(cancel_token) = &self.cancel_token {
            cancel_token.cancel();
        }
        self.completion.abort();
    }

    async fn await_completion(self) -> Result<CompletionResponse> {
        self.completion.await.map_err(|error| {
            MoaError::ProviderError(format!("completion task failed to join: {error}"))
        })?
    }
}

impl fmt::Debug for CompletionStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionStream").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tokio::time::{Duration as TokioDuration, sleep};

    use super::{CompletionContent, CompletionResponse, CompletionStream, StopReason, TokenUsage};
    use crate::ModelId;
    use crate::error::MoaError;

    #[test]
    fn token_usage_cache_hit_rate_handles_zero_and_mixed_usage() {
        assert!(TokenUsage::default().cache_hit_rate().abs() < f64::EPSILON);

        let usage = TokenUsage {
            input_tokens_uncached: 40,
            input_tokens_cache_write: 10,
            input_tokens_cache_read: 50,
            output_tokens: 8,
        };

        assert_eq!(usage.total_input_tokens(), 100);
        assert!((usage.cache_hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn completion_stream_abort_stops_completion_task() {
        let (_tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            sleep(TokioDuration::from_secs(30)).await;
            Ok(CompletionResponse {
                text: "late".to_string(),
                content: vec![CompletionContent::Text("late".to_string())],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("test"),
                usage: TokenUsage::default(),
                duration_ms: 30_000,
                thought_signature: None,
            })
        });
        let stream = CompletionStream::new(rx, completion);
        stream.abort();

        let error = stream
            .into_response()
            .await
            .expect_err("aborted completion task should not resolve successfully");
        assert!(matches!(error, MoaError::ProviderError(message) if message.contains("join")));
    }
}
