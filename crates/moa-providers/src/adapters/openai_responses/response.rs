//! OpenAI Responses API response normalization state.

use super::*;

pub(super) fn token_usage_from_openai_usage(usage: &ResponseUsage) -> TokenUsage {
    let cached_input_tokens = usage.input_tokens_details.cached_tokens as usize;
    let input_tokens = usage.input_tokens as usize;
    let output_tokens = usage.output_tokens as usize;

    TokenUsage {
        input_tokens_uncached: input_tokens.saturating_sub(cached_input_tokens),
        input_tokens_cache_write: 0,
        input_tokens_cache_read: cached_input_tokens,
        output_tokens,
    }
}

pub(super) struct ResponsesStreamError {
    pub(super) error: MoaError,
    pub(super) retryable: bool,
    pub(super) emitted_content: bool,
}
