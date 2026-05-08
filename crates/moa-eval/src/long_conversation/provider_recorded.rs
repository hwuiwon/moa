//! Recorded scripted LLM provider for deterministic transcript replay.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    MessageRole, MoaError, ModelCapabilities, ModelId, Result as MoaResult, StopReason,
    TokenPricing, TokenUsage, ToolCallFormat,
};
use moa_test_support::transcript::{ProviderEvent, Transcript};

/// Errors returned by [`RecordedScriptedProvider`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordedProviderError {
    /// Replay was asked for more provider turns than the transcript contains.
    #[error(
        "recorded transcript exhausted at turn {turn_index}; transcript contains {total_turns} turns"
    )]
    TranscriptExhausted {
        /// Zero-based requested turn index.
        turn_index: usize,
        /// Total recorded turns.
        total_turns: usize,
    },
    /// The compiled request diverged from the scripted user turn.
    #[error(
        "recorded transcript mismatch at turn {turn_index}: expected user message {expected:?}, got {actual:?}"
    )]
    TranscriptMismatch {
        /// Expected user text from the transcript.
        expected: String,
        /// Actual latest user text in the provider request.
        actual: String,
        /// Zero-based turn index.
        turn_index: usize,
    },
    /// Internal replay state could not be accessed.
    #[error("recorded provider state lock poisoned: {0}")]
    StateLockPoisoned(String),
}

/// LLM provider that replays a JSONL transcript in order.
#[derive(Debug, Clone)]
pub struct RecordedScriptedProvider {
    transcript: Arc<Transcript>,
    cursor: Arc<Mutex<usize>>,
    strict_matching: bool,
}

impl RecordedScriptedProvider {
    /// Creates a strict provider that checks the latest user message for every turn.
    #[must_use]
    pub fn new(transcript: Transcript) -> Self {
        Self::with_strict_matching(transcript)
    }

    /// Creates a provider that requires request user text to match the transcript.
    #[must_use]
    pub fn with_strict_matching(transcript: Transcript) -> Self {
        Self {
            transcript: Arc::new(transcript),
            cursor: Arc::new(Mutex::new(0)),
            strict_matching: true,
        }
    }

    /// Creates a provider that only checks transcript turn count.
    #[must_use]
    pub fn with_loose_matching(transcript: Transcript) -> Self {
        Self {
            transcript: Arc::new(transcript),
            cursor: Arc::new(Mutex::new(0)),
            strict_matching: false,
        }
    }

    /// Returns whether request/user strict matching is enabled.
    #[must_use]
    pub const fn strict_matching(&self) -> bool {
        self.strict_matching
    }

    /// Returns the current zero-based replay cursor.
    pub fn cursor(&self) -> Result<usize, RecordedProviderError> {
        self.cursor
            .lock()
            .map(|cursor| *cursor)
            .map_err(|error| RecordedProviderError::StateLockPoisoned(error.to_string()))
    }

    /// Advances replay and returns the next recorded provider events exactly as stored.
    pub fn complete_events(
        &self,
        request: &CompletionRequest,
    ) -> Result<Vec<ProviderEvent>, RecordedProviderError> {
        let mut cursor = self
            .cursor
            .lock()
            .map_err(|error| RecordedProviderError::StateLockPoisoned(error.to_string()))?;
        let turn_index = *cursor;
        let turn = self.transcript.turns.get(turn_index).ok_or(
            RecordedProviderError::TranscriptExhausted {
                turn_index,
                total_turns: self.transcript.turns.len(),
            },
        )?;

        if self.strict_matching {
            let actual = latest_user_message(request).unwrap_or_default();
            if actual != turn.user.text {
                return Err(RecordedProviderError::TranscriptMismatch {
                    expected: turn.user.text.clone(),
                    actual,
                    turn_index,
                });
            }
        }

        *cursor += 1;
        Ok(turn.expected.clone())
    }

    /// Advances replay and returns a buffered MOA completion stream.
    pub fn complete_recorded(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionStream, RecordedProviderError> {
        let events = self.complete_events(request)?;
        Ok(CompletionStream::from_response(response_from_events(
            events,
            self.capabilities().model_id,
        )))
    }
}

impl Default for RecordedScriptedProvider {
    fn default() -> Self {
        Self::with_strict_matching(Transcript {
            version: 1,
            scenario: "empty".to_string(),
            turns: Vec::new(),
        })
    }
}

#[async_trait]
impl LLMProvider for RecordedScriptedProvider {
    fn name(&self) -> &str {
        "recorded"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("recorded-scripted"),
            context_window: 128_000,
            max_output: 16_384,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: Some(0.0),
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: CompletionRequest) -> MoaResult<CompletionStream> {
        self.complete_recorded(&request)
            .map_err(|error| MoaError::ProviderError(error.to_string()))
    }
}

fn latest_user_message(request: &CompletionRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.clone())
}

fn response_from_events(events: Vec<ProviderEvent>, model: ModelId) -> CompletionResponse {
    let mut text = String::new();
    let mut content = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason = StopReason::EndTurn;

    for event in events {
        match event {
            ProviderEvent::TextDelta { text: delta } => {
                text.push_str(&delta);
                content.push(CompletionContent::Text(delta));
            }
            ProviderEvent::ToolCall { call } => {
                content.push(CompletionContent::ToolCall(call));
            }
            ProviderEvent::Usage { usage: event_usage } => {
                usage = event_usage;
            }
            ProviderEvent::Terminal {
                stop_reason: event_stop_reason,
            } => {
                stop_reason = event_stop_reason;
            }
        }
    }

    CompletionResponse {
        text,
        content,
        stop_reason,
        model,
        usage,
        duration_ms: 0,
        thought_signature: None,
    }
}
