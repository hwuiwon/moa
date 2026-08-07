//! Test-only scripted provider utilities for deterministic integration coverage.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    error::MoaError, error::Result, traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionRequestView,
    types::completion::CompletionResponse, types::completion::CompletionStream,
    types::completion::SharedCompletionRequest, types::completion::StopReason,
    types::completion::TokenUsage, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::context::MessageRole,
    types::model::ModelCapabilities,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;

const DEFAULT_INPUT_TOKENS: usize = 64;
const DEFAULT_DURATION_MS: u64 = 1;

/// Append-only request journal shared by every clone of one scripted provider.
///
/// One async lock owns both lazy file initialization and writes so concurrent
/// completions cannot interleave JSONL records. The file is opened in append
/// mode to retain records when a test restarts the orchestrator process.
struct ScriptedRequestJournal {
    path: PathBuf,
    file: AsyncMutex<Option<tokio::fs::File>>,
}

impl ScriptedRequestJournal {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: AsyncMutex::new(None),
        }
    }

    async fn append(&self, request: &CompletionRequest) -> Result<()> {
        let mut line = request_journal_json_bytes(request).map_err(|error| {
            MoaError::ProviderError(format!(
                "failed to serialize scripted provider request journal record: {error}"
            ))
        })?;
        line.push(b'\n');

        let mut file = self.file.lock().await;
        if file.is_none() {
            let opened = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
                .map_err(|error| request_journal_io_error("open", &self.path, error))?;
            *file = Some(opened);
        }
        let Some(file) = file.as_mut() else {
            return Err(MoaError::ProviderError(
                "scripted provider request journal was not initialized".to_string(),
            ));
        };
        file.write_all(&line)
            .await
            .map_err(|error| request_journal_io_error("write", &self.path, error))?;
        file.flush()
            .await
            .map_err(|error| request_journal_io_error("flush", &self.path, error))?;
        file.sync_data()
            .await
            .map_err(|error| request_journal_io_error("sync", &self.path, error))?;
        Ok(())
    }
}

fn request_journal_json_bytes(request: &CompletionRequest) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&sort_json_objects(serde_json::to_value(request)?))
}

fn sort_json_objects(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_json_objects(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json_objects).collect()),
        scalar => scalar,
    }
}

fn request_journal_io_error(operation: &str, path: &Path, error: std::io::Error) -> MoaError {
    MoaError::ProviderError(format!(
        "failed to {operation} scripted provider request journal {}: {error}",
        path.display()
    ))
}

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
    /// completing. Applies to the first `fail_first_n` matching requests, or
    /// to every request when `fail_first_n` is zero.
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
        if self.abort_mid_stream || self.fail_first_n == 0 {
            return None;
        }
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        if attempt >= self.fail_first_n {
            return None;
        }
        Some(self.to_error())
    }

    /// Whether this request's stream should abort mid-response.
    fn take_abort(&self) -> bool {
        if !self.abort_mid_stream {
            return false;
        }
        if self.fail_first_n == 0 {
            return true;
        }
        self.attempts.fetch_add(1, Ordering::Relaxed) < self.fail_first_n
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
    request_journal: Option<Arc<ScriptedRequestJournal>>,
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
            request_journal: None,
        }
    }

    /// Enables append-only JSONL request journaling at the configured test-owned path.
    pub(crate) fn with_request_journal(mut self, path: PathBuf) -> Self {
        self.request_journal = Some(Arc::new(ScriptedRequestJournal::new(path)));
        self
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

    /// Returns all completion requests recorded so far.
    pub fn recorded_requests(&self) -> Vec<CompletionRequest> {
        self.recorded_requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

impl ScriptedProvider {
    async fn complete_request<R: CompletionRequestView + ?Sized>(
        &self,
        request: &R,
        effective_model: Option<&moa_core::types::identifiers::ModelId>,
    ) -> Result<CompletionStream> {
        let mut recorded_request = CompletionRequest {
            model: request.model().cloned(),
            messages: request.messages().to_vec(),
            tools: request.tools().to_vec(),
            max_output_tokens: request.max_output_tokens(),
            temperature: request.temperature(),
            response_format: request.response_format().cloned(),
            native_web_search: request.native_web_search(),
            metadata: request.metadata().clone(),
        };
        if let Some(model) = effective_model {
            recorded_request.model = Some(model.clone());
        }
        // Resolve the keyed match before recording moves the request; keyed lookup is pure so it
        // stays correct under concurrent callers.
        let keyed_response = self.match_keyed(&recorded_request);
        if let Some(journal) = &self.request_journal {
            journal.append(&recorded_request).await?;
        }
        self.recorded_requests
            .lock()
            .map_err(|error| {
                moa_core::error::MoaError::ProviderError(format!(
                    "scripted provider request log poisoned: {error}"
                ))
            })?
            .push(recorded_request);
        let response = match keyed_response {
            Some(response) => response,
            None => {
                let queued = self
                    .responses
                    .lock()
                    .map_err(|error| {
                        moa_core::error::MoaError::ProviderError(format!(
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
                            moa_core::error::MoaError::ProviderError(format!(
                                "scripted provider fallback response poisoned: {error}"
                            ))
                        })?
                        .clone()
                        .ok_or_else(|| {
                            moa_core::error::MoaError::ProviderError(
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
            .is_some_and(|fault| fault.take_abort());
        let timing = response.timing;

        let completion_response = CompletionResponse {
            text,
            content: response.content,
            stop_reason: response.stop_reason,
            model: effective_model
                .cloned()
                .unwrap_or_else(|| self.capabilities.model_id.clone()),
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

#[async_trait]
impl LLMProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities.clone()
    }

    async fn complete(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        self.complete_request(&request, request.model()).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use moa_core::types::context::ContextMessage;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(test_name: &str) -> Self {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "moa-scripted-provider-{test_name}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create isolated request journal test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Builds a request whose concatenated system + user text contains `text`.
    fn user_request(text: &str) -> CompletionRequest {
        CompletionRequest::new(text)
    }

    fn ordered_object<const N: usize>(entries: [(&str, Value); N]) -> Value {
        let mut object = serde_json::Map::new();
        for (key, value) in entries {
            object.insert(key.to_string(), value);
        }
        Value::Object(object)
    }

    fn nested_value(reverse: bool) -> Value {
        let leaf = if reverse {
            ordered_object([
                ("zeta", serde_json::json!(2)),
                ("alpha", serde_json::json!(1)),
            ])
        } else {
            ordered_object([
                ("alpha", serde_json::json!(1)),
                ("zeta", serde_json::json!(2)),
            ])
        };
        if reverse {
            ordered_object([("second", serde_json::json!(true)), ("first", leaf)])
        } else {
            ordered_object([("first", leaf), ("second", serde_json::json!(true))])
        }
    }

    fn request_with_nested_object_order(reverse: bool) -> CompletionRequest {
        let mut message = ContextMessage::user("canonical request");
        message.tools = Some(nested_value(reverse));
        message.tool_invocation = Some(ToolInvocation {
            id: Some("tool-use-1".to_string()),
            name: "fixture_tool".to_string(),
            input: nested_value(reverse),
        });

        let mut request = CompletionRequest::new("ignored");
        request.messages = vec![message];
        request.tools = vec![nested_value(reverse)];
        request.response_format = Some(moa_core::types::completion::JsonResponseFormat {
            name: "fixture_output".to_string(),
            description: Some("nested canonical schema".to_string()),
            schema: nested_value(reverse),
            strict: true,
        });
        request
            .metadata
            .insert("nested".to_string(), nested_value(reverse));
        request
    }

    /// Drains a completion stream and returns its aggregated assistant text.
    async fn complete_text(provider: &ScriptedProvider, request: CompletionRequest) -> String {
        provider
            .complete(request.into_shared())
            .await
            .expect("scripted completion")
            .collect()
            .await
            .expect("aggregated response")
            .text
    }

    #[tokio::test]
    async fn request_journal_disabled_preserves_existing_in_memory_behavior() {
        // Pins: a provider without an explicitly configured journal performs no
        // filesystem I/O and still records the exact request in memory.
        let request = user_request("journal disabled");
        let provider = ScriptedProvider::new(ModelCapabilities::default()).push_text("done");

        assert_eq!(complete_text(&provider, request.clone()).await, "done");
        assert_eq!(provider.recorded_requests(), vec![request]);
    }

    #[tokio::test]
    async fn request_journal_appends_canonical_jsonl_across_provider_restarts() {
        // Pins: a fixture-owned journal survives provider reconstruction, keeps
        // request order, and stores the actual CompletionRequest DTO per line.
        let temp = TempDirectory::new("append");
        let journal_path = temp.join("requests.jsonl");
        let mut first_request = user_request("first request");
        first_request.metadata = HashMap::from([
            ("zeta".to_string(), serde_json::json!(2)),
            ("alpha".to_string(), serde_json::json!(1)),
        ]);
        let second_request = user_request("second request");

        let first_provider = ScriptedProvider::new(ModelCapabilities::default())
            .with_request_journal(journal_path.clone())
            .push_text("first response");
        assert_eq!(
            complete_text(&first_provider, first_request.clone()).await,
            "first response"
        );
        drop(first_provider);

        let restarted_provider = ScriptedProvider::new(ModelCapabilities::default())
            .with_request_journal(journal_path.clone())
            .push_text("second response");
        assert_eq!(
            complete_text(&restarted_provider, second_request.clone()).await,
            "second response"
        );

        let journal = fs::read_to_string(&journal_path).expect("read request journal");
        let lines = journal.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "one JSON value must be written per request");
        assert!(
            lines[0].contains("\"metadata\":{\"alpha\":1,\"zeta\":2}"),
            "metadata keys must be serialized deterministically: {}",
            lines[0]
        );
        let persisted = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<CompletionRequest>(line)
                    .expect("journal line should be a CompletionRequest")
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted, vec![first_request, second_request]);
        assert!(
            journal.ends_with('\n'),
            "JSONL journal must end each record"
        );
    }

    #[tokio::test]
    async fn request_journal_accepts_float_bearing_completion_requests() {
        // Pins: request journaling accepts valid completion DTO floats instead of applying the
        // stricter execution-plan canonicalization contract that forbids floating-point values.
        let temp = TempDirectory::new("float-request");
        let journal_path = temp.join("requests.jsonl");
        let mut request = user_request("float-bearing request");
        request.temperature = Some(0.25);
        request
            .metadata
            .insert("ranking_score".to_string(), serde_json::json!(0.875));
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .with_request_journal(journal_path.clone())
            .push_text("done");

        assert_eq!(complete_text(&provider, request.clone()).await, "done");

        let persisted = fs::read_to_string(&journal_path)
            .expect("read float-bearing request journal")
            .lines()
            .map(|line| {
                serde_json::from_str::<CompletionRequest>(line)
                    .expect("float-bearing journal line should round-trip")
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted, vec![request]);
    }

    #[tokio::test]
    async fn request_journal_canonicalizes_every_nested_request_object() {
        // Pins: object insertion order in any Value-bearing CompletionRequest
        // field cannot alter the durable JSONL bytes.
        let temp = TempDirectory::new("nested-canonical");
        let journal_path = temp.join("requests.jsonl");
        let forward = request_with_nested_object_order(false);
        let reverse = request_with_nested_object_order(true);
        assert_eq!(forward, reverse, "requests must be semantically equivalent");
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .with_request_journal(journal_path.clone())
            .with_fallback_response(ScriptedResponse::text("done"));

        assert_eq!(complete_text(&provider, forward.clone()).await, "done");
        assert_eq!(complete_text(&provider, reverse.clone()).await, "done");

        let journal = fs::read_to_string(&journal_path).expect("read canonical request journal");
        let lines = journal.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].as_bytes(),
            lines[1].as_bytes(),
            "semantically equal nested request objects must have identical canonical bytes"
        );
        for line in lines {
            let persisted = serde_json::from_str::<CompletionRequest>(line)
                .expect("canonical journal line should round-trip as CompletionRequest");
            assert_eq!(persisted, forward);
        }
    }

    #[tokio::test]
    async fn request_journal_concurrent_completions_write_exact_parseable_records() {
        // Pins: concurrent completions share one append lock, yielding one
        // complete parseable line per request without relying on task order.
        const REQUEST_COUNT: usize = 32;

        let temp = TempDirectory::new("concurrent");
        let journal_path = temp.join("requests.jsonl");
        let provider = Arc::new(
            ScriptedProvider::new(ModelCapabilities::default())
                .with_request_journal(journal_path.clone())
                .with_fallback_response(ScriptedResponse::text("done")),
        );
        let mut completions = Vec::with_capacity(REQUEST_COUNT);
        for index in 0..REQUEST_COUNT {
            let provider = provider.clone();
            completions.push(tokio::spawn(async move {
                let request = user_request(&format!("concurrent-request-{index:02}"));
                provider
                    .complete(request.into_shared())
                    .await
                    .expect("open concurrent scripted completion")
                    .collect()
                    .await
                    .expect("collect concurrent scripted completion")
            }));
        }
        for completion in completions {
            let response = completion.await.expect("join concurrent completion task");
            assert_eq!(response.text, "done");
        }

        let journal = fs::read(&journal_path).expect("read concurrent request journal");
        assert_eq!(
            journal.iter().filter(|byte| **byte == b'\n').count(),
            REQUEST_COUNT,
            "every request must end in exactly one JSONL record"
        );
        let journal = std::str::from_utf8(&journal).expect("journal must be UTF-8 JSONL");
        let lines = journal.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), REQUEST_COUNT);
        let observed = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<CompletionRequest>(line)
                    .expect("concurrent journal record must be complete JSON")
                    .messages
                    .into_iter()
                    .next()
                    .expect("concurrent request must retain its message")
                    .content
            })
            .collect::<BTreeSet<_>>();
        let expected = (0..REQUEST_COUNT)
            .map(|index| format!("concurrent-request-{index:02}"))
            .collect::<BTreeSet<_>>();
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn request_journal_open_failure_is_a_typed_provider_error() {
        // Pins: an invalid fixture journal path fails the provider request
        // without panicking or consuming the scripted response.
        let temp = TempDirectory::new("open-failure");
        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .with_request_journal(temp.0.clone())
            .push_text("must remain queued");

        let error = provider
            .complete(user_request("cannot journal").into_shared())
            .await
            .expect_err("opening a directory as the journal must fail");
        assert!(
            matches!(&error, MoaError::ProviderError(message) if message.contains("failed to open scripted provider request journal")),
            "journal open failure must stay in the provider error boundary: {error:?}"
        );
        assert!(provider.recorded_requests().is_empty());
        assert_eq!(
            provider
                .responses
                .lock()
                .expect("scripted response queue lock")
                .len(),
            1,
            "journal failure must not consume the scripted completion"
        );
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
                .complete(user_request("flaky prompt").into_shared())
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
            .complete(user_request("abort now").into_shared())
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
            .complete(user_request("timed prompt").into_shared())
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
