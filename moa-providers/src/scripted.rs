//! Test-only scripted provider utilities for deterministic integration coverage.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    ModelCapabilities, Result, StopReason, TokenUsage, ToolCallContent, ToolInvocation,
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
#[derive(Clone)]
pub struct ScriptedProvider {
    capabilities: ModelCapabilities,
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    recorded_requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedProvider {
    /// Creates an empty scripted provider with fixed model capabilities.
    pub fn new(capabilities: ModelCapabilities) -> Self {
        Self {
            capabilities,
            responses: Arc::new(Mutex::new(VecDeque::new())),
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
    pub async fn recorded_requests(&self) -> Vec<CompletionRequest> {
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
        self.recorded_requests
            .lock()
            .map_err(|error| {
                moa_core::MoaError::ProviderError(format!(
                    "scripted provider request log poisoned: {error}"
                ))
            })?
            .push(request);
        let response = self
            .responses
            .lock()
            .map_err(|error| {
                moa_core::MoaError::ProviderError(format!(
                    "scripted provider response queue poisoned: {error}"
                ))
            })?
            .pop_front()
            .ok_or_else(|| {
                moa_core::MoaError::ProviderError(
                    "scripted provider ran out of queued responses".to_string(),
                )
            })?;
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
            input_tokens: response.input_tokens,
            output_tokens,
            cached_input_tokens: response.cached_input_tokens,
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
