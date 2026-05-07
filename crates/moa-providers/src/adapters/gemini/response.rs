//! Gemini response wire types and normalization helpers.

use super::*;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiGenerateContentResponse {
    #[serde(default)]
    pub(super) candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    pub(super) usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default, rename = "modelVersion")]
    pub(super) model_version: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiCandidate {
    #[serde(default)]
    pub(super) content: Option<GeminiContent>,
    #[serde(default, rename = "finishReason")]
    pub(super) finish_reason: Option<String>,
    #[serde(default, rename = "groundingMetadata")]
    pub(super) grounding_metadata: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiContent {
    #[serde(default)]
    pub(super) parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiPart {
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default, rename = "functionCall")]
    pub(super) function_call: Option<GeminiFunctionCall>,
    #[serde(default, rename = "thoughtSignature")]
    pub(super) thought_signature: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiFunctionCall {
    pub(super) name: String,
    #[serde(default)]
    pub(super) args: Value,
    #[serde(default)]
    pub(super) id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    pub(super) prompt_token_count: Option<usize>,
    #[serde(default, rename = "candidatesTokenCount")]
    pub(super) candidates_token_count: Option<usize>,
    #[serde(default, rename = "cachedContentTokenCount")]
    pub(super) cached_content_token_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GeminiCachedContent {
    pub(super) name: String,
}

pub(super) fn token_usage_from_gemini_usage(metadata: &GeminiUsageMetadata) -> TokenUsage {
    let input_tokens = metadata.prompt_token_count.unwrap_or_default();
    let cached_input_tokens = metadata.cached_content_token_count.unwrap_or_default();
    let output_tokens = metadata.candidates_token_count.unwrap_or_default();

    TokenUsage {
        input_tokens_uncached: input_tokens.saturating_sub(cached_input_tokens),
        input_tokens_cache_write: 0,
        input_tokens_cache_read: cached_input_tokens,
        output_tokens,
    }
}

pub(super) fn finish_reason_to_stop_reason(finish_reason: &str) -> StopReason {
    match finish_reason {
        "MAX_TOKENS" => StopReason::MaxTokens,
        "CANCELLED" => StopReason::Cancelled,
        "STOP" => StopReason::EndTurn,
        other => StopReason::Other(other.to_string()),
    }
}
