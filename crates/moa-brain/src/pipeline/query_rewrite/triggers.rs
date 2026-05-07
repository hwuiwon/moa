//! Skip heuristics for deciding whether query rewriting should run.

use super::QueryRewriter;
use super::input::RewriteInput;
use super::terms::normalize_entity_token;

impl QueryRewriter {
    pub(super) fn should_skip(&self, input: &RewriteInput) -> bool {
        if !self.config.enabled || self.circuit_breaker.is_open() {
            return true;
        }

        if input.query.trim().is_empty() {
            return true;
        }

        if starts_with_tool_like_verb(&input.query) {
            return true;
        }

        self.config.skip_single_turn
            && input.user_message_count <= 1
            && approximate_query_tokens(&input.query) < self.config.min_query_tokens
    }
}

fn starts_with_tool_like_verb(query: &str) -> bool {
    let Some(first_token) = query
        .split_whitespace()
        .next()
        .map(normalize_entity_token)
        .filter(|token| !token.is_empty())
    else {
        return false;
    };

    matches!(
        first_token.as_str(),
        "read" | "write" | "search" | "run" | "deploy"
    )
}

fn approximate_query_tokens(query: &str) -> usize {
    query.split_whitespace().count()
}
