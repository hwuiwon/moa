//! Provider completion request, response, and streaming types.

use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Formatter};
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{MoaError, Result};

use super::{context::ContextMessage, identifiers::ModelId};

/// Request-scoped policy for provider-native web search tools.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NativeWebSearchPolicy {
    /// Preserve the provider adapter's configured native-web-search behavior.
    #[default]
    ProviderDefault,
    /// Suppress provider-native web search for this request.
    Disabled,
}

/// Request metadata key that asks `LLMGateway` to return without appending a `BrainResponse`.
pub const DEFER_BRAIN_RESPONSE_METADATA_KEY: &str = "_moa.defer_brain_response";

/// Request metadata key carrying the index one past the last frozen-history
/// message in `CompletionRequest.messages`.
///
/// Messages before this index replay byte-identically on later turns, so
/// provider adapters may mark a cache breakpoint there; messages at or after
/// it (per-turn reminders, the active user turn, live tool exchanges) change
/// every turn.
pub const STABLE_HISTORY_END_METADATA_KEY: &str = "_moa.cache.stable_history_end";

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
    /// Request-scoped provider-native web-search policy.
    #[serde(default)]
    pub native_web_search: NativeWebSearchPolicy,
    /// Request-scoped metadata.
    #[serde(serialize_with = "serialize_metadata_deterministically")]
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
            native_web_search: NativeWebSearchPolicy::ProviderDefault,
            metadata: HashMap::new(),
        }
    }

    /// Creates a minimal request alias for simple prompt-only completions.
    pub fn simple(prompt: impl Into<String>) -> Self {
        Self::new(prompt)
    }

    /// Moves this durable/inspectable DTO into canonical provider request storage.
    #[must_use]
    pub fn into_shared(self) -> SharedCompletionRequest {
        SharedCompletionRequest::new(self)
    }

    /// Materializes an owned request from a read-only provider view.
    ///
    /// This is reserved for an ownership or serialization boundary that
    /// requires the concrete request DTO, such as a durable service call.
    /// Providers that can consume a [`CompletionRequestView`] directly should
    /// keep using the view so shared failover storage remains single-copy.
    #[must_use]
    pub fn from_view(request: &impl CompletionRequestView) -> Self {
        Self {
            model: request.model().cloned(),
            messages: request.messages().to_vec(),
            tools: request.tools().to_vec(),
            max_output_tokens: request.max_output_tokens(),
            temperature: request.temperature(),
            response_format: request.response_format().cloned(),
            native_web_search: request.native_web_search(),
            metadata: request.metadata().clone(),
        }
    }
}

/// Read-only view of a completion request at a provider boundary.
///
/// Implementations use this view for both ordinary owned requests and shared
/// failover requests. The shared implementation can overlay only fields that
/// a decorator semantically transforms while borrowing every untouched field
/// from the original request allocation.
pub trait CompletionRequestView {
    /// Returns the effective model requested by the caller or a decorator.
    fn model(&self) -> Option<&ModelId>;

    /// Returns the messages visible to the provider.
    fn messages(&self) -> &[ContextMessage];

    /// Returns the tool schemas visible to the provider.
    fn tools(&self) -> &[Value];

    /// Returns the maximum output token budget.
    fn max_output_tokens(&self) -> Option<usize>;

    /// Returns the optional temperature override.
    fn temperature(&self) -> Option<f32>;

    /// Returns the provider-native response format.
    fn response_format(&self) -> Option<&JsonResponseFormat>;

    /// Returns the request-scoped native web-search policy.
    fn native_web_search(&self) -> NativeWebSearchPolicy;

    /// Returns request metadata without materializing another map.
    fn metadata(&self) -> &HashMap<String, Value>;
}

impl CompletionRequestView for CompletionRequest {
    fn model(&self) -> Option<&ModelId> {
        self.model.as_ref()
    }

    fn messages(&self) -> &[ContextMessage] {
        &self.messages
    }

    fn tools(&self) -> &[Value] {
        &self.tools
    }

    fn max_output_tokens(&self) -> Option<usize> {
        self.max_output_tokens
    }

    fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    fn response_format(&self) -> Option<&JsonResponseFormat> {
        self.response_format.as_ref()
    }

    fn native_web_search(&self) -> NativeWebSearchPolicy {
        self.native_web_search
    }

    fn metadata(&self) -> &HashMap<String, Value> {
        &self.metadata
    }
}

/// Canonical immutable, cheaply clonable ownership of one provider request.
///
/// Provider failover may replay the same logical request against several
/// candidates. The ordinary [`CompletionRequest`] remains the owned serde/API
/// DTO at durable serialization and explicit inspection boundaries. Provider
/// dispatch uses this handle so the payload stays behind one reference-counted
/// allocation. A candidate-specific model override is kept in the handle and
/// does not copy the request's messages, tools, or metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedCompletionRequest {
    request: Arc<CompletionRequest>,
    model_override: Option<ModelId>,
    transformed_messages: Option<Arc<[ContextMessage]>>,
    transformed_tools: Option<Arc<[Value]>>,
    transformed_response_format: Option<Option<Arc<JsonResponseFormat>>>,
}

impl SharedCompletionRequest {
    /// Moves an owned request into shared immutable storage.
    #[must_use]
    pub fn new(request: CompletionRequest) -> Self {
        Self {
            request: Arc::new(request),
            model_override: None,
            transformed_messages: None,
            transformed_tools: None,
            transformed_response_format: None,
        }
    }

    /// Returns a handle sharing this request with a candidate-specific model.
    #[must_use]
    pub fn with_model_override(&self, model: Option<&ModelId>) -> Self {
        Self {
            request: Arc::clone(&self.request),
            model_override: model.cloned(),
            transformed_messages: self.transformed_messages.clone(),
            transformed_tools: self.transformed_tools.clone(),
            transformed_response_format: self.transformed_response_format.clone(),
        }
    }

    /// Returns a request view with explicitly transformed provider fields.
    ///
    /// This is for semantic decorators such as egress DLP. Only the fields
    /// passed here are materialized; model, metadata, token limits, temperature,
    /// and native-search policy continue borrowing from the original request.
    /// The transformed fields are a new provider payload, not a second failover
    /// copy of the untouched request.
    #[must_use]
    pub fn with_transformed_fields(
        &self,
        messages: Vec<ContextMessage>,
        tools: Vec<Value>,
        response_format: Option<JsonResponseFormat>,
    ) -> Self {
        Self {
            request: Arc::clone(&self.request),
            model_override: self.model_override.clone(),
            transformed_messages: Some(Arc::from(messages)),
            transformed_tools: Some(Arc::from(tools)),
            transformed_response_format: Some(response_format.map(Arc::new)),
        }
    }

    /// Returns the immutable request payload shared by all candidates.
    #[must_use]
    pub fn request(&self) -> &CompletionRequest {
        self.request.as_ref()
    }

    /// Returns the effective model, including a candidate override when set.
    #[must_use]
    pub fn model(&self) -> Option<&ModelId> {
        self.model_override.as_ref().or(self.request.model.as_ref())
    }
}

impl CompletionRequestView for SharedCompletionRequest {
    fn model(&self) -> Option<&ModelId> {
        self.model()
    }

    fn messages(&self) -> &[ContextMessage] {
        self.transformed_messages
            .as_deref()
            .unwrap_or(self.request.messages.as_slice())
    }

    fn tools(&self) -> &[Value] {
        self.transformed_tools
            .as_deref()
            .unwrap_or(self.request.tools.as_slice())
    }

    fn max_output_tokens(&self) -> Option<usize> {
        self.request.max_output_tokens
    }

    fn temperature(&self) -> Option<f32> {
        self.request.temperature
    }

    fn response_format(&self) -> Option<&JsonResponseFormat> {
        match &self.transformed_response_format {
            None => self.request.response_format.as_ref(),
            Some(None) => None,
            Some(Some(format)) => Some(format.as_ref()),
        }
    }

    fn native_web_search(&self) -> NativeWebSearchPolicy {
        self.request.native_web_search
    }

    fn metadata(&self) -> &HashMap<String, Value> {
        &self.request.metadata
    }
}

fn serialize_metadata_deterministically<S>(
    metadata: &HashMap<String, Value>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    metadata
        .iter()
        .collect::<BTreeMap<_, _>>()
        .serialize(serializer)
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
/// NOTE: This type wraps async runtime primitives (`tokio::sync::mpsc` and
/// `tokio::task::JoinHandle`) and would ideally live
/// alongside provider implementations. It remains in `moa-core` because the
/// `LLMProvider` trait is also defined in `moa-core` and returns this type
/// directly, so moving it out would either create a crate cycle or force a
/// broader trait redesign.
pub struct CompletionStream {
    receiver: mpsc::Receiver<Result<CompletionContent>>,
    completion: Option<JoinHandle<Result<CompletionResponse>>>,
}

impl CompletionStream {
    /// Creates a new completion stream from a content receiver and completion task.
    pub fn new(
        receiver: mpsc::Receiver<Result<CompletionContent>>,
        completion: JoinHandle<Result<CompletionResponse>>,
    ) -> Self {
        Self {
            receiver,
            completion: Some(completion),
        }
    }

    /// Wraps this stream in a cancellation-safe asynchronous transform.
    ///
    /// Dropping the returned stream aborts the transform task, which drops this
    /// input stream and aborts its provider task in turn. Decorators therefore do
    /// not need to duplicate forwarding-task ownership or cancellation logic.
    pub fn transform<F, Fut>(self, capacity: usize, transform: F) -> Self
    where
        F: FnOnce(Self, mpsc::Sender<Result<CompletionContent>>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<CompletionResponse>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let completion = tokio::spawn(transform(self, tx));
        Self::new(rx, completion)
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
    pub async fn collect(self) -> Result<CompletionResponse> {
        self.into_response().await
    }

    /// Drains pending blocks before waiting for the final aggregated response.
    ///
    /// Draining is required even when the caller only wants the response because
    /// the producer may be blocked on a bounded stream channel.
    pub async fn into_response(mut self) -> Result<CompletionResponse> {
        while let Some(block) = self.receiver.recv().await {
            block?;
        }
        self.await_completion().await
    }

    /// Aborts the underlying provider task.
    pub fn abort(&self) {
        if let Some(completion) = &self.completion {
            completion.abort();
        }
    }

    async fn await_completion(mut self) -> Result<CompletionResponse> {
        let completion = self.completion.take().ok_or_else(|| {
            MoaError::ProviderError("completion task was already consumed".to_string())
        })?;
        completion.await.map_err(|error| {
            MoaError::ProviderError(format!("completion task failed to join: {error}"))
        })?
    }
}

impl Drop for CompletionStream {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.abort();
        }
    }
}

impl fmt::Debug for CompletionStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionStream").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio::time::{Duration as TokioDuration, sleep};

    use super::{
        CompletionContent, CompletionRequest, CompletionRequestView, CompletionResponse,
        CompletionStream, SharedCompletionRequest, StopReason, TokenUsage,
    };
    use crate::error::MoaError;
    use crate::types::identifiers::ModelId;

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

    #[test]
    fn completion_request_serializes_metadata_deterministically() {
        let mut first = CompletionRequest::new("hello");
        first.metadata = HashMap::from([
            ("zeta".to_string(), json!(3)),
            ("alpha".to_string(), json!(1)),
            ("middle".to_string(), json!(2)),
        ]);

        let mut second = CompletionRequest::new("hello");
        second.metadata = HashMap::from([
            ("middle".to_string(), json!(2)),
            ("zeta".to_string(), json!(3)),
            ("alpha".to_string(), json!(1)),
        ]);

        let first_json =
            serde_json::to_string(&first).expect("completion request should serialize");
        let second_json =
            serde_json::to_string(&second).expect("completion request should serialize");

        assert_eq!(first_json, second_json);
        assert!(
            first_json.contains(r#""metadata":{"alpha":1,"middle":2,"zeta":3}"#),
            "metadata should be serialized in stable key order: {first_json}"
        );
    }

    #[test]
    fn shared_completion_request_reuses_payload_for_model_overrides() {
        // Pins: failover candidates share the request allocation and only own
        // the model override needed by their provider boundary.
        let mut request = CompletionRequest::new("hello");
        request.tools.push(json!({"name": "search"}));
        request.metadata.insert("tenant".to_string(), json!("acme"));
        let shared = SharedCompletionRequest::new(request);
        let fallback_model = ModelId::new("fallback");
        let fallback = shared.with_model_override(Some(&fallback_model));

        assert!(std::ptr::eq(shared.request(), fallback.request()));
        assert_eq!(shared.model(), None);
        assert_eq!(fallback.model(), Some(&fallback_model));

        let transformed = fallback.with_transformed_fields(
            shared.request().messages.clone(),
            shared.request().tools.clone(),
            None,
        );
        assert!(std::ptr::eq(
            transformed.metadata(),
            shared.request().metadata()
        ));
        assert_eq!(transformed.model(), Some(&fallback_model));
        assert_eq!(transformed.tools(), shared.request().tools);
    }

    #[tokio::test]
    async fn completion_stream_abort_stops_completion_task() {
        let (tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            // The producer owns the sender, so aborting the producer closes the
            // channel before `into_response` drains it.
            let sender = tx;
            sleep(TokioDuration::from_secs(30)).await;
            drop(sender);
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

    #[tokio::test]
    async fn into_response_drains_capacity_limited_stream() {
        // Pins: waiting only for the final response cannot deadlock a producer on channel capacity.
        let (tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            for index in 0..8 {
                tx.send(Ok(CompletionContent::Text(index.to_string())))
                    .await
                    .expect("stream receiver remains alive");
            }
            Ok(CompletionResponse {
                text: "done".to_string(),
                content: vec![CompletionContent::Text("done".to_string())],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("test"),
                usage: TokenUsage::default(),
                duration_ms: 1,
                thought_signature: None,
            })
        });

        let response = CompletionStream::new(rx, completion)
            .into_response()
            .await
            .expect("drain then join");
        assert_eq!(response.text, "done");
    }

    #[tokio::test]
    async fn dropping_a_transform_aborts_the_source_task() {
        // Pins: cancellation propagates through decorator layers without pod-local correctness state.
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&dropped);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (_tx, rx) = mpsc::channel(1);
        let completion = tokio::spawn(async move {
            struct MarkDrop(Arc<AtomicBool>);
            impl Drop for MarkDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _mark = MarkDrop(marker);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
            unreachable!("pending task only exits through cancellation")
        });
        let transformed =
            CompletionStream::new(rx, completion).transform(1, |mut inner, tx| async move {
                while let Some(item) = inner.next().await {
                    if tx.send(item).await.is_err() {
                        break;
                    }
                }
                inner.into_response().await
            });
        started_rx.await.expect("source task started");
        drop(transformed);
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::SeqCst));
    }
}
