//! Test-only scripted provider utilities for deterministic integration coverage.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    MessageRole, ModelCapabilities, Result, StopReason, TokenUsage, ToolCallContent,
    ToolInvocation,
};
use serde_json::Value;
const DEFAULT_INPUT_TOKENS: usize = 64;
const DEFAULT_DURATION_MS: u64 = 1;

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
#[derive(Debug, Clone, PartialEq)]
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
    /// Synthetic duration.
    pub duration_ms: u64,
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
    /// system or user message text.
    ///
    /// Keyed entries are checked in registration order and the first match wins, so callers should
    /// register specific substrings before general ones. Unlike the FIFO queue, keyed entries are
    /// never consumed, which lets multiple concurrent callers that share a match resolve the same
    /// completion deterministically.
    pub fn push_keyed(
        mut self,
        match_substring: impl Into<String>,
        response: ScriptedResponse,
    ) -> Self {
        Arc::make_mut(&mut self.keyed).push((match_substring.into(), response));
        self
    }

    /// Returns the first keyed response whose match substring is contained in the request's system
    /// or user message text, resolving without mutating the keyed table.
    fn match_keyed(&self, request: &CompletionRequest) -> Option<ScriptedResponse> {
        if self.keyed.is_empty() {
            return None;
        }
        let haystack = request
            .messages
            .iter()
            .filter(|message| matches!(message.role, MessageRole::System | MessageRole::User))
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

        Ok(CompletionStream::from_response(CompletionResponse {
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
        }))
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
}
