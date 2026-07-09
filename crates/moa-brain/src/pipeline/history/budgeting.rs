//! Token-budget allocation for history replay.

use moa_core::{ContextMessage, MessageRole, estimate_text_tokens};

pub(super) fn keep_budgeted_older_messages(
    stable_prefix_tokens: usize,
    older_messages: &[ContextMessage],
    recent_messages: &[ContextMessage],
    recent_tokens: usize,
    remaining_budget: usize,
) -> (Vec<ContextMessage>, usize) {
    let mut tokens_used = stable_prefix_tokens + recent_tokens;
    let mut kept_older_reversed = Vec::new();

    for message in older_messages.iter().rev() {
        let message_tokens = estimate_text_tokens(&message.content);
        if tokens_used + message_tokens > remaining_budget {
            break;
        }

        tokens_used += message_tokens;
        kept_older_reversed.push(message.clone());
    }

    kept_older_reversed.reverse();
    let mut kept_older = kept_older_reversed;

    // Quantize the kept region to a user-turn boundary whenever the budget
    // dropped anything. A boundary that lands mid-turn would cut a tool
    // call/result exchange apart and would oscillate with per-turn variation
    // in the dynamic sections, churning bytes the provider prompt cache could
    // otherwise reuse.
    if kept_older.len() < older_messages.len() {
        let extra_drop = kept_older
            .iter()
            .position(|message| message.role == MessageRole::User)
            .unwrap_or(kept_older.len());
        for message in kept_older.drain(..extra_drop) {
            tokens_used = tokens_used.saturating_sub(estimate_text_tokens(&message.content));
        }
    }

    let tokens_used = if older_messages.is_empty() && recent_messages.is_empty() {
        stable_prefix_tokens
    } else {
        tokens_used
    };

    (kept_older, tokens_used)
}

#[cfg(test)]
mod tests {
    use moa_core::ContextMessage;

    use super::keep_budgeted_older_messages;

    #[test]
    fn budget_boundary_quantizes_to_a_user_turn_start() {
        // Pins: when the budget drops older messages, the kept region starts
        // at a user message so replay never opens mid-exchange and the
        // boundary stays stable across small budget fluctuations.
        let older = vec![
            ContextMessage::user("turn one"),
            ContextMessage::assistant("answer one, deliberately the longest message in history"),
            ContextMessage::user("turn two"),
            ContextMessage::assistant("answer two"),
        ];
        let recent = vec![ContextMessage::user("current turn")];

        // Budget fits everything except the first user message; the mid-turn
        // assistant message must be dropped too so the window opens at "turn two".
        let budget = [
            "answer one, deliberately the longest message in history",
            "turn two",
            "answer two",
            "current turn",
        ]
        .iter()
        .map(|text| moa_core::estimate_text_tokens(text))
        .sum::<usize>();

        let (kept, _) = keep_budgeted_older_messages(0, &older, &recent, 3, budget);

        assert_eq!(
            kept.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["turn two", "answer two"],
        );
    }

    #[test]
    fn budget_keeps_all_older_messages_when_they_fit() {
        let older = vec![
            ContextMessage::assistant("continuation without a user turn"),
            ContextMessage::user("turn"),
        ];
        let recent = vec![ContextMessage::user("current")];

        let (kept, _) = keep_budgeted_older_messages(0, &older, &recent, 2, 100_000);

        assert_eq!(kept.len(), 2, "nothing dropped means nothing quantized");
    }
}
