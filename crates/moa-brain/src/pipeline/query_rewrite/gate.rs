//! Deterministic gate for deciding whether query rewriting should call an LLM.

use moa_config::QueryRewriteConfig;
use moa_core::types::context::ContextMessage;

use crate::query_rewrite::RewriteReason;
use moa_retrieval::planning::{Strategy, classify_strategy};

use super::terms::normalize_entity_token;

/// Decision returned by the query-rewrite gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteDecision {
    /// Run the LLM rewriter for this retrieval query.
    Rewrite(RewriteReason),
    /// Preserve the original query and skip the LLM call.
    Skip(SkipReason),
}

/// Reason the query-rewrite gate skipped the LLM call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SkipReason {
    /// Query rewriting is disabled by config.
    Disabled,
    /// The query rewrite circuit breaker is open.
    CircuitOpen,
    /// Graph memory retrieval is not present in the pipeline.
    NoMemoryRetrieval,
    /// Vector retrieval is unavailable, so the rewrite cannot help semantic search.
    NoVectorRetrieval,
    /// There is no user query to rewrite.
    EmptyQuery,
    /// The query starts with a direct tool-like command verb.
    ToolLikeVerb,
    /// The query contains exact anchors and no history-dependent reference pressure.
    ExactAnchors,
    /// A first-turn query is explicit and below the configured rewrite threshold.
    FirstTurnExplicit,
    /// The query has no cheap signal that an LLM rewrite would improve retrieval.
    NoRewriteSignal,
}

impl SkipReason {
    pub(super) fn as_str(self) -> &'static str {
        self.into()
    }
}

pub(super) struct RewriteGateInput<'a> {
    pub(super) query: &'a str,
    pub(super) history: &'a [ContextMessage],
    pub(super) user_message_count: usize,
    pub(super) config: &'a QueryRewriteConfig,
    pub(super) memory_retrieval_available: bool,
    pub(super) vector_retrieval_available: bool,
    pub(super) circuit_open: bool,
}

pub(super) fn decide(input: RewriteGateInput<'_>) -> RewriteDecision {
    let query = input.query.trim();
    if !input.config.enabled {
        return RewriteDecision::Skip(SkipReason::Disabled);
    }
    if input.circuit_open {
        return RewriteDecision::Skip(SkipReason::CircuitOpen);
    }
    if !input.memory_retrieval_available {
        return RewriteDecision::Skip(SkipReason::NoMemoryRetrieval);
    }
    if !input.vector_retrieval_available {
        return RewriteDecision::Skip(SkipReason::NoVectorRetrieval);
    }
    if query.is_empty() {
        return RewriteDecision::Skip(SkipReason::EmptyQuery);
    }
    if starts_with_tool_like_verb(query) {
        return RewriteDecision::Skip(SkipReason::ToolLikeVerb);
    }

    let has_history = !input.history.is_empty();
    let has_coreference = contains_coreference(query);
    if has_history && has_coreference {
        return RewriteDecision::Rewrite(RewriteReason::CoreferenceWithHistory);
    }

    if has_exact_anchor(query) {
        return RewriteDecision::Skip(SkipReason::ExactAnchors);
    }

    if classify_strategy(query) == Strategy::VectorFirst && is_semantic_memory_query(query) {
        return RewriteDecision::Rewrite(RewriteReason::VectorFirstSemantic);
    }

    if input.config.skip_single_turn
        && input.user_message_count <= 1
        && approximate_query_tokens(query) < input.config.min_query_tokens
        && has_standalone_anchor(query)
    {
        return RewriteDecision::Skip(SkipReason::FirstTurnExplicit);
    }

    if has_history && is_short_followup_without_anchor(query) {
        return RewriteDecision::Rewrite(RewriteReason::VagueFollowup);
    }

    if has_multihop_relation(query) && !has_standalone_anchor(query) {
        return RewriteDecision::Rewrite(RewriteReason::MultiHopWithoutSeeds);
    }

    RewriteDecision::Skip(SkipReason::NoRewriteSignal)
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

fn contains_coreference(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any_word(
        &lower,
        &[
            "that", "it", "this", "those", "these", "previous", "above", "earlier", "same",
        ],
    )
}

fn has_exact_anchor(query: &str) -> bool {
    query.contains("://")
        || query.contains('/')
        || query.contains('\\')
        || query.contains('"')
        || query.contains('\'')
        || query.split_whitespace().any(|token| {
            let token = token.trim_matches(|ch: char| ch.is_ascii_punctuation());
            looks_like_uuid(token) || looks_like_issue_id(token) || looks_like_path_token(token)
        })
}

fn has_standalone_anchor(query: &str) -> bool {
    has_exact_anchor(query)
        || query.split_whitespace().any(|token| {
            let normalized = normalize_entity_token(token);
            normalized.len() >= 3
                && (token.chars().any(char::is_uppercase)
                    || normalized.contains('_')
                    || normalized.contains('-')
                    || normalized.chars().any(|ch| ch.is_ascii_digit()))
        })
}

fn looks_like_uuid(token: &str) -> bool {
    let parts = token.split('-').collect::<Vec<_>>();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .into_iter()
            .zip(parts)
            .all(|(len, part)| part.len() == len && part.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn looks_like_issue_id(token: &str) -> bool {
    token
        .strip_prefix('#')
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
        || token.contains('-') && token.chars().any(|ch| ch.is_ascii_digit())
}

fn looks_like_path_token(token: &str) -> bool {
    token.contains('.')
        && token
            .rsplit('.')
            .next()
            .is_some_and(|ext| matches!(ext, "rs" | "py" | "ts" | "tsx" | "go" | "md" | "toml"))
}

fn is_short_followup_without_anchor(query: &str) -> bool {
    approximate_query_tokens(query) <= 7 && !has_standalone_anchor(query)
}

fn is_semantic_memory_query(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "history of",
            "similar to",
            "usually",
            "preference",
            "prefer",
            "how often",
            "has anything been done",
        ],
    )
}

fn has_multihop_relation(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "depends on",
            "owned by",
            "owner of",
            "related to",
            "connects to",
            "upstream",
            "downstream",
            "because of",
        ],
    )
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn contains_any_word(text: &str, needles: &[&str]) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|word| needles.contains(&word))
}

fn approximate_query_tokens(query: &str) -> usize {
    query.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use moa_config::QueryRewriteConfig;
    use moa_core::types::context::ContextMessage;

    use crate::query_rewrite::RewriteReason;

    use super::{RewriteDecision, RewriteGateInput, SkipReason, decide};

    #[test]
    fn skip_reason_labels_are_pinned() {
        // Pins: these strings are emitted as metric labels; keep them byte-identical
        // so dashboards keyed on the snake_case reason continue to aggregate.
        let labels = [
            (SkipReason::Disabled, "disabled"),
            (SkipReason::CircuitOpen, "circuit_open"),
            (SkipReason::NoMemoryRetrieval, "no_memory_retrieval"),
            (SkipReason::NoVectorRetrieval, "no_vector_retrieval"),
            (SkipReason::EmptyQuery, "empty_query"),
            (SkipReason::ToolLikeVerb, "tool_like_verb"),
            (SkipReason::ExactAnchors, "exact_anchors"),
            (SkipReason::FirstTurnExplicit, "first_turn_explicit"),
            (SkipReason::NoRewriteSignal, "no_rewrite_signal"),
        ];
        for (reason, label) in labels {
            assert_eq!(reason.as_str(), label);
        }
    }

    fn gate(query: &str, history: Vec<ContextMessage>) -> RewriteDecision {
        decide(RewriteGateInput {
            query,
            history: &history,
            user_message_count: history
                .iter()
                .filter(|message| message.role == moa_core::types::context::MessageRole::User)
                .count()
                + 1,
            config: &QueryRewriteConfig::default(),
            memory_retrieval_available: true,
            vector_retrieval_available: true,
            circuit_open: false,
        })
    }

    #[test]
    fn skips_identifier_heavy_queries() {
        // Pins: exact anchors are preserved by skipping the LLM rewrite path.
        assert_eq!(
            gate("Find #1234 in crates/moa-brain/src/pipeline.rs", Vec::new()),
            RewriteDecision::Skip(SkipReason::ExactAnchors)
        );
    }

    #[test]
    fn rewrites_coreference_with_history() {
        // Pins: history-resolvable coreference is the primary LLM rewrite case.
        assert_eq!(
            gate(
                "fix that and add tests",
                vec![ContextMessage::user(
                    "The OAuth refresh token race is in auth/refresh.rs"
                )],
            ),
            RewriteDecision::Rewrite(RewriteReason::CoreferenceWithHistory)
        );
    }

    #[test]
    fn skips_without_vector_retrieval() {
        // Pins: no vector leg means no rewrite LLM tax.
        assert_eq!(
            decide(RewriteGateInput {
                query: "fix that",
                history: &[ContextMessage::user("The bug is in auth/refresh.rs")],
                user_message_count: 2,
                config: &QueryRewriteConfig::default(),
                memory_retrieval_available: true,
                vector_retrieval_available: false,
                circuit_open: false,
            }),
            RewriteDecision::Skip(SkipReason::NoVectorRetrieval)
        );
    }

    #[test]
    fn rewrites_first_turn_vector_semantic_queries_before_single_turn_skip() {
        // Pins: semantic memory questions are worth rewriting even when short and first-turn.
        assert_eq!(
            gate(
                "How often do deploy incidents look similar to policy releases?",
                Vec::new(),
            ),
            RewriteDecision::Rewrite(RewriteReason::VectorFirstSemantic)
        );
    }

    /// One row of the gate decision matrix: input shape plus the expected arm.
    struct GateCase {
        name: &'static str,
        query: &'static str,
        history: Vec<ContextMessage>,
        config: QueryRewriteConfig,
        user_message_count: usize,
        memory_retrieval_available: bool,
        vector_retrieval_available: bool,
        circuit_open: bool,
        expected: RewriteDecision,
    }

    #[test]
    fn gate_decision_matrix_covers_each_remaining_arm() {
        // Pins: every short-circuit arm of `decide` is reachable with the exact
        // input shape it guards, in priority order. These reasons are emitted as
        // metric labels and drive whether the billed LLM rewrite runs.
        let disabled_config = QueryRewriteConfig {
            enabled: false,
            ..QueryRewriteConfig::default()
        };
        let cases = vec![
            GateCase {
                name: "disabled config skips before any other signal",
                query: "fix that and add tests",
                history: vec![ContextMessage::user("The bug is in auth/refresh.rs")],
                config: disabled_config,
                user_message_count: 2,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Skip(SkipReason::Disabled),
            },
            GateCase {
                name: "open circuit skips even with rewritable coreference",
                query: "fix that and add tests",
                history: vec![ContextMessage::user("The bug is in auth/refresh.rs")],
                config: QueryRewriteConfig::default(),
                user_message_count: 2,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: true,
                expected: RewriteDecision::Skip(SkipReason::CircuitOpen),
            },
            GateCase {
                name: "no graph-memory retrieval skips the rewrite",
                query: "fix that and add tests",
                history: vec![ContextMessage::user("The bug is in auth/refresh.rs")],
                config: QueryRewriteConfig::default(),
                user_message_count: 2,
                memory_retrieval_available: false,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Skip(SkipReason::NoMemoryRetrieval),
            },
            GateCase {
                name: "tool-like command verb preserves the literal query",
                query: "read the auth config file",
                history: Vec::new(),
                config: QueryRewriteConfig::default(),
                user_message_count: 1,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Skip(SkipReason::ToolLikeVerb),
            },
            GateCase {
                name: "explicit short first-turn query with an anchor is left alone",
                query: "Explain the AuthService startup",
                history: Vec::new(),
                config: QueryRewriteConfig::default(),
                user_message_count: 1,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Skip(SkipReason::FirstTurnExplicit),
            },
            GateCase {
                name: "short anchorless follow-up with history is rewritten",
                query: "what about the deadlines",
                history: vec![ContextMessage::user("We discussed the project timeline")],
                config: QueryRewriteConfig::default(),
                user_message_count: 2,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Rewrite(RewriteReason::VagueFollowup),
            },
            GateCase {
                name: "multi-hop relation without seeds is rewritten",
                query: "what services are downstream of the billing pipeline",
                history: Vec::new(),
                config: QueryRewriteConfig::default(),
                user_message_count: 1,
                memory_retrieval_available: true,
                vector_retrieval_available: true,
                circuit_open: false,
                expected: RewriteDecision::Rewrite(RewriteReason::MultiHopWithoutSeeds),
            },
        ];

        for case in cases {
            let decision = decide(RewriteGateInput {
                query: case.query,
                history: &case.history,
                user_message_count: case.user_message_count,
                config: &case.config,
                memory_retrieval_available: case.memory_retrieval_available,
                vector_retrieval_available: case.vector_retrieval_available,
                circuit_open: case.circuit_open,
            });
            assert_eq!(decision, case.expected, "case: {}", case.name);
        }
    }
}
