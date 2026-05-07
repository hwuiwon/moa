//! Token-budget allocation for history replay.

use crate::pipeline::estimate_tokens;

use super::conversion::CompiledRecordMessage;

pub(super) fn keep_budgeted_older_messages(
    stable_prefix_tokens: usize,
    older_messages: &[CompiledRecordMessage],
    recent_messages: &[CompiledRecordMessage],
    recent_tokens: usize,
    remaining_budget: usize,
) -> (Vec<CompiledRecordMessage>, usize) {
    let mut tokens_used = stable_prefix_tokens + recent_tokens;
    let mut kept_older_reversed = Vec::new();

    for compiled in older_messages.iter().rev() {
        let message_tokens = estimate_tokens(&compiled.message.content);
        if tokens_used + message_tokens > remaining_budget {
            break;
        }

        tokens_used += message_tokens;
        kept_older_reversed.push(compiled.clone());
    }

    kept_older_reversed.reverse();

    let tokens_used = if older_messages.is_empty() && recent_messages.is_empty() {
        stable_prefix_tokens
    } else {
        tokens_used
    };

    (kept_older_reversed, tokens_used)
}
