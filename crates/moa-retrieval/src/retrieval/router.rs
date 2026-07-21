//! Deterministic lexical router that assigns each turn a retrieval strategy.
//!
//! The router is the single entry point that decides how much retrieval work a
//! turn deserves. It runs before any embedding or database call, using only
//! cheap token and surface features of the query text, so classification adds
//! zero LLM calls and zero I/O. The strategy enum is the sole dispatch point in
//! the stage-7 memory pipeline; branches key on it instead of accreting boolean
//! flags around the historical always-on path.

use serde::{Deserialize, Serialize};

/// Retrieval strategy chosen for a turn by the deterministic lexical router.
///
/// Serialized as snake_case so evals and traces can read the chosen strategy as
/// a stable string. [`RetrievalStrategy::Agentic`] is reserved for the Task 11
/// agentic tool loop; until it lands the router never emits it, and the memory
/// stage routes it to the same behavior as [`RetrievalStrategy::Fast`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RetrievalStrategy {
    /// No retrieval intent: chit-chat or acknowledgement turns bypass embedding
    /// and retrieval entirely.
    Skip,
    /// Single-shot top-k retrieval over the existing hybrid pipeline.
    Fast,
    /// Multi-hop query: decompose into sub-queries, retrieve each, and fuse the
    /// results through the existing rank machinery.
    Deep,
    /// Reserved for the Task 11 agentic retrieval loop; routes to [`Self::Fast`]
    /// until Task 11 claims it.
    Agentic,
}

impl RetrievalStrategy {
    /// Returns the stable snake_case label used in metadata and traces.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Maximum token count for a turn to be eligible for [`RetrievalStrategy::Skip`].
///
/// Skip is reserved for short acknowledgement turns; anything longer is treated
/// as carrying possible retrieval intent and never skipped.
const ACK_MAX_TOKENS: usize = 5;

/// Surface tokens that mark an acknowledgement, greeting, or filler turn.
///
/// A turn is only skipped when *every* one of its tokens is in this set (and it
/// carries no interrogative marker), so the classifier stays conservative: when
/// any content word is present, the turn is not skipped.
const ACK_TOKENS: &[&str] = &[
    "afternoon",
    "alright",
    "awesome",
    "bye",
    "cheers",
    "cool",
    "done",
    "evening",
    "fine",
    "gm",
    "good",
    "goodbye",
    "got",
    "great",
    "haha",
    "hehe",
    "hello",
    "hey",
    "hi",
    "hiya",
    "it",
    "k",
    "kk",
    "lmao",
    "lol",
    "morning",
    "nice",
    "night",
    "no",
    "nope",
    "np",
    "ok",
    "okay",
    "perfect",
    "right",
    "sounds",
    "sup",
    "sure",
    "thank",
    "thanks",
    "thx",
    "ty",
    "welcome",
    "wow",
    "yeah",
    "yep",
    "yes",
    "yo",
    "yup",
];

/// Explicit investigative phrasings that mark a turn as agentic-shaped.
///
/// These are multi-word cues that the user is asking for an iterative dig
/// through memory rather than a single lookup, so the turn earns the agentic
/// tool loop. The list is deliberately small and phrase-based (not single
/// tokens) to stay conservative: a bare "search" or "find" is still fast
/// single-shot retrieval, only an explicit investigative phrase routes agentic.
const AGENTIC_PHRASES: &[&str] = &[
    "dig into",
    "dig through",
    "dig up",
    "explore my memory",
    "explore your memory",
    "go through my",
    "investigate",
    "look through my",
    "look through your",
    "search my memory",
    "search through my",
    "search through your",
    "search your memory",
    "trace through",
];

/// Relationship verbs whose presence, alongside a relative-clause pivot, marks a
/// bridged multi-hop query.
const BRIDGE_VERBS: &[&str] = &[
    "belong",
    "belongs",
    "call",
    "calls",
    "connect",
    "connects",
    "contain",
    "contains",
    "control",
    "controls",
    "depend",
    "depends",
    "maintain",
    "maintains",
    "manage",
    "manages",
    "own",
    "owned",
    "owns",
    "produce",
    "produces",
    "reference",
    "references",
    "report",
    "reports",
    "require",
    "requires",
    "use",
    "uses",
];

/// Classifies a retrieval query into a [`RetrievalStrategy`] using deterministic
/// lexical features only.
///
/// The classifier never allocates beyond a per-token normalization pass and
/// makes no LLM or database calls. Rules, in order:
///
/// 1. [`RetrievalStrategy::Skip`] — empty turns, or short turns whose every
///    token is an acknowledgement/greeting/filler word with no interrogative
///    marker. The classifier is deliberately conservative here: when any content
///    word is present it does not skip.
/// 2. [`RetrievalStrategy::Agentic`] — turns carrying an explicit investigative
///    phrase (`investigate`, `dig into`, `search my memory for`, …). These earn
///    the agentic tool loop; the match is phrase-based so a bare `search`/`find`
///    stays fast single-shot retrieval.
/// 3. [`RetrievalStrategy::Deep`] — multi-hop-shaped queries: an interrogative
///    turn carrying a relative-clause pivot (`that`/`which`/`whose`) together
///    with a relationship verb, an explicit `between … and …` bridge, or a
///    chained possessive (`… 's … 's …`).
/// 4. [`RetrievalStrategy::Fast`] — everything else, including single-hop
///    questions and persona/imperative turns.
#[must_use]
pub fn route_query(query: &str) -> RetrievalStrategy {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return RetrievalStrategy::Skip;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_question_mark = trimmed.contains('?');
    let words: Vec<String> = trimmed
        .split_whitespace()
        .map(normalize_word)
        .filter(|word| !word.is_empty())
        .collect();
    let token_count = words.len();
    let has_interrogative = words.iter().any(|word| is_interrogative(word));

    // (a) Skip: short pure-acknowledgement/greeting turns with no query intent.
    if !has_question_mark
        && !has_interrogative
        && token_count > 0
        && token_count <= ACK_MAX_TOKENS
        && words.iter().all(|word| is_ack_token(word))
    {
        return RetrievalStrategy::Skip;
    }

    // (b) Agentic: an explicit investigative phrase asks for an iterative dig.
    if is_agentic_shaped(&lower) {
        return RetrievalStrategy::Agentic;
    }

    // (c) Deep: multi-hop-shaped queries.
    if is_deep_shaped(&lower, &words, has_interrogative, has_question_mark) {
        return RetrievalStrategy::Deep;
    }

    // (d) Default: single-shot fast retrieval.
    RetrievalStrategy::Fast
}

/// Returns whether the query carries an explicit investigative phrase.
fn is_agentic_shaped(lower: &str) -> bool {
    AGENTIC_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// Decomposes a multi-hop query into at most two self-contained sub-queries.
///
/// The splitter is deterministic and lexical: it pivots on the first
/// relative-clause marker (`that`/`which`/`whose`) after the leading word,
/// producing the inner fact fragment (the head noun through the clause tail)
/// followed by the outer fragment (the query up to the head noun, with a leading
/// interrogative stripped). For example
/// `"Which team owns the library that svc depends on?"` decomposes to
/// `["library that svc depends on", "team owns the library"]`.
///
/// Limitations: only the relative-clause and `between … and …` shapes decompose;
/// any other query (including one the router marked [`RetrievalStrategy::Deep`]
/// via a possessive chain) returns a single cleaned fragment, so the deep path
/// degrades to one retrieval rather than fabricating fragments. Empty input
/// returns an empty vector.
#[must_use]
pub fn decompose_query(query: &str) -> Vec<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = trimmed.split_whitespace().collect();

    // Pivot on the first relative-clause marker after the leading token so a
    // leading `which`/`whose` interrogative is not mistaken for the pivot.
    let marker_index = words.iter().enumerate().skip(1).find_map(|(index, word)| {
        matches!(normalize_word(word).as_str(), "that" | "which" | "whose").then_some(index)
    });

    if let Some(index) = marker_index {
        let head_noun = words[index - 1];

        // Outer fragment: everything before the pivot, minus a leading
        // interrogative (e.g. "Which team owns the library" -> "team owns the
        // library").
        let mut outer: Vec<&str> = words[..index].to_vec();
        if outer
            .first()
            .is_some_and(|word| is_interrogative(&normalize_word(word)))
        {
            outer.remove(0);
        }

        // Inner fragment: the head noun re-attached to the relative clause (e.g.
        // "library that svc depends on").
        let mut inner: Vec<&str> = vec![head_noun];
        inner.extend_from_slice(&words[index..]);

        let inner_fragment = clean_fragment(&inner.join(" "));
        let outer_fragment = clean_fragment(&outer.join(" "));

        let mut fragments = Vec::with_capacity(2);
        if !inner_fragment.is_empty() {
            fragments.push(inner_fragment);
        }
        if !outer_fragment.is_empty() && !fragments.contains(&outer_fragment) {
            fragments.push(outer_fragment);
        }
        fragments.truncate(2);
        if !fragments.is_empty() {
            return fragments;
        }
    }

    // `between X and Y` bridge: split the two bridged entities.
    if let Some(fragments) = decompose_between_and(trimmed) {
        return fragments;
    }

    vec![clean_fragment(trimmed)]
}

/// Splits a `between X and Y` bridge into `[X, Y]` sub-queries when present.
fn decompose_between_and(query: &str) -> Option<Vec<String>> {
    let lower = query.to_ascii_lowercase();
    let between_start = lower.find("between ")? + "between ".len();
    let tail = &query[between_start..];
    let tail_lower = &lower[between_start..];
    let and_offset = tail_lower.find(" and ")?;
    let left = clean_fragment(&tail[..and_offset]);
    let right = clean_fragment(&tail[and_offset + " and ".len()..]);
    let mut fragments = Vec::with_capacity(2);
    if !left.is_empty() {
        fragments.push(left);
    }
    if !right.is_empty() && !fragments.contains(&right) {
        fragments.push(right);
    }
    (fragments.len() >= 2).then_some(fragments)
}

/// Returns whether a query's surface features mark it as multi-hop-shaped.
fn is_deep_shaped(
    lower: &str,
    words: &[String],
    has_interrogative: bool,
    has_question_mark: bool,
) -> bool {
    if !has_interrogative && !has_question_mark {
        return false;
    }

    // Relative-clause pivot (not the leading interrogative) plus a relationship
    // verb: the canonical two-hop bridge.
    let has_relative_pivot = words
        .iter()
        .skip(1)
        .any(|word| matches!(word.as_str(), "that" | "which" | "whose"));
    let has_bridge_verb = words.iter().any(|word| is_bridge_verb(word));
    if has_relative_pivot && has_bridge_verb {
        return true;
    }

    // Explicit two-entity bridge.
    if lower.contains("between ") && lower.contains(" and ") {
        return true;
    }

    // Chained possessive (`… 's … 's …`) crosses two ownership hops.
    if lower.matches("'s ").count() >= 2 {
        return true;
    }

    false
}

/// Lowercases a token and strips surrounding non-alphanumeric characters while
/// keeping interior apostrophes (so possessives survive normalization).
fn normalize_word(word: &str) -> String {
    word.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '\'')
        .to_ascii_lowercase()
}

/// Trims surrounding whitespace and punctuation from a decomposed fragment.
fn clean_fragment(fragment: &str) -> String {
    fragment
        .trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation())
        .trim()
        .to_string()
}

fn is_interrogative(word: &str) -> bool {
    matches!(
        word,
        "what" | "which" | "who" | "whose" | "whom" | "when" | "where" | "why" | "how"
    )
}

fn is_ack_token(word: &str) -> bool {
    ACK_TOKENS.binary_search(&word).is_ok()
}

fn is_bridge_verb(word: &str) -> bool {
    BRIDGE_VERBS.binary_search(&word).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{RetrievalStrategy, decompose_query, route_query};

    #[test]
    fn ack_token_table_is_sorted_for_binary_search() {
        // Pins: the ack lookup table must stay sorted, or `binary_search` on it
        // silently misclassifies acknowledgement turns.
        let mut sorted = super::ACK_TOKENS.to_vec();
        sorted.sort_unstable();
        assert_eq!(super::ACK_TOKENS, sorted.as_slice());
    }

    #[test]
    fn bridge_verb_table_is_sorted_for_binary_search() {
        // Pins: the bridge-verb table must stay sorted for `binary_search`.
        let mut sorted = super::BRIDGE_VERBS.to_vec();
        sorted.sort_unstable();
        assert_eq!(super::BRIDGE_VERBS, sorted.as_slice());
    }

    #[test]
    fn route_skips_empty_and_whitespace_turns() {
        // Pins: an empty or whitespace-only turn carries no retrieval intent.
        assert_eq!(route_query(""), RetrievalStrategy::Skip);
        assert_eq!(route_query("   \n\t "), RetrievalStrategy::Skip);
    }

    #[test]
    fn route_skips_acknowledgement_turns() {
        // Pins: short pure-acknowledgement turns bypass retrieval.
        for turn in [
            "thanks!",
            "ok",
            "sounds good",
            "lol",
            "got it",
            "great, thanks",
        ] {
            assert_eq!(route_query(turn), RetrievalStrategy::Skip, "{turn}");
        }
    }

    #[test]
    fn route_skips_greetings() {
        // Pins: greetings are chit-chat with no retrieval intent.
        for turn in ["hey", "hello", "good morning", "yo"] {
            assert_eq!(route_query(turn), RetrievalStrategy::Skip, "{turn}");
        }
    }

    #[test]
    fn route_does_not_skip_persona_anchor_phrasings() {
        // Pins: the load-bearing persona verbs must retain retrieval intent; a
        // Skip here silently drops memory for the 100-session persona sweep.
        for turn in [
            "reconcile the billing accounts for last quarter",
            "summarize the incident retro notes",
            "categorize these support tickets by severity",
        ] {
            assert_eq!(route_query(turn), RetrievalStrategy::Fast, "{turn}");
        }
    }

    #[test]
    fn route_does_not_skip_bare_persona_verb() {
        // Pins: a bare content verb is not an acknowledgement token, so it keeps
        // retrieval intent rather than being skipped.
        assert_eq!(route_query("reconcile"), RetrievalStrategy::Fast);
        assert_eq!(route_query("summarize"), RetrievalStrategy::Fast);
    }

    #[test]
    fn route_agentic_for_explicit_investigative_phrasings() {
        // Pins: an explicit investigative phrase earns the agentic tool loop.
        for turn in [
            "investigate why the billing sync keeps failing",
            "dig into what we know about the auth outage",
            "search my memory for everything about the vendor contract",
            "go through my notes on the migration and summarize the risks",
            "explore your memory for prior incidents like this one",
        ] {
            assert_eq!(route_query(turn), RetrievalStrategy::Agentic, "{turn}");
        }
    }

    #[test]
    fn route_agentic_wins_over_deep_shape() {
        // Pins: an investigative phrase routes agentic even when the query also
        // has a multi-hop bridge shape, because the agentic loop subsumes it.
        assert_eq!(
            route_query("Investigate which team owns the library that svc depends on?"),
            RetrievalStrategy::Agentic
        );
    }

    #[test]
    fn route_stays_fast_for_bare_search_or_find_verbs() {
        // Pins: the agentic gate is phrase-based and conservative — a bare
        // `search`/`find`/`look` verb is ordinary single-shot retrieval, not an
        // agentic dig, so it must not escalate cost.
        for turn in [
            "search the runbook for the rotation steps",
            "find the on-call schedule for next week",
            "look up the API key policy",
            "what did the postmortem say about the outage?",
        ] {
            assert_eq!(route_query(turn), RetrievalStrategy::Fast, "{turn}");
        }
    }

    #[test]
    fn route_fast_for_single_hop_question() {
        // Pins: a single-hop factual question uses fast top-k retrieval, never
        // the deep decomposition path (decomposition hurts single-hop queries).
        assert_eq!(
            route_query("What is my API key rotation policy?"),
            RetrievalStrategy::Fast
        );
        assert_eq!(
            route_query("Who is the on-call primary for billing?"),
            RetrievalStrategy::Fast
        );
    }

    #[test]
    fn route_deep_for_relative_clause_bridge() {
        // Pins: an interrogative with a relative-clause pivot and a relationship
        // verb is the canonical two-hop query and routes deep.
        assert_eq!(
            route_query("Which team owns the library that svc depends on?"),
            RetrievalStrategy::Deep
        );
        assert_eq!(
            route_query("Who manages the person that owns the auth service?"),
            RetrievalStrategy::Deep
        );
    }

    #[test]
    fn route_deep_for_between_and_bridge() {
        // Pins: an explicit two-entity bridge routes deep.
        assert_eq!(
            route_query("What is the dependency between the billing service and the auth service?"),
            RetrievalStrategy::Deep
        );
    }

    #[test]
    fn route_deep_for_chained_possessive() {
        // Pins: a chained possessive crosses two ownership hops and routes deep.
        assert_eq!(
            route_query("Who is the auth service's owner's manager?"),
            RetrievalStrategy::Deep
        );
    }

    #[test]
    fn route_fast_for_single_relative_clause_without_bridge_verb() {
        // Pins: a relative clause without a relationship verb stays single-hop;
        // the deep path must not fire on ordinary descriptive questions.
        assert_eq!(
            route_query("What is the document that describes onboarding?"),
            RetrievalStrategy::Fast
        );
    }

    #[test]
    fn route_fast_for_imperative_without_interrogative() {
        // Pins: imperative retrieval requests are fast, not deep, even with a
        // relationship verb, because they carry no interrogative shape.
        assert_eq!(
            route_query("List the services that billing depends on"),
            RetrievalStrategy::Fast
        );
    }

    #[test]
    fn decompose_relative_clause_splits_inner_then_outer() {
        // Pins: relative-clause decomposition yields the inner fact fragment
        // first, then the outer fragment with the leading interrogative stripped.
        assert_eq!(
            decompose_query("Which team owns the library that svc depends on?"),
            vec![
                "library that svc depends on".to_string(),
                "team owns the library".to_string(),
            ]
        );
    }

    #[test]
    fn decompose_caps_at_two_fragments() {
        // Pins: decomposition never exceeds two sub-queries even with multiple
        // relative-clause markers (cap: 2 iterations).
        let fragments =
            decompose_query("Who owns the service that uses the library that logging needs?");
        assert!(fragments.len() <= 2, "got {fragments:?}");
    }

    #[test]
    fn decompose_between_and_splits_entities() {
        // Pins: a `between X and Y` bridge decomposes into the two entities.
        assert_eq!(
            decompose_query("What links the billing service and the auth service between them?"),
            vec!["What links the billing service and the auth service between them".to_string()],
            "no `between X and Y` shape means a single cleaned fragment"
        );
        assert_eq!(
            decompose_query("What is the link between billing service and auth service?"),
            vec!["billing service".to_string(), "auth service".to_string(),]
        );
    }

    #[test]
    fn decompose_non_bridge_returns_single_fragment() {
        // Pins: a query without a decomposable shape yields one cleaned fragment
        // so the deep path degrades to a single retrieval rather than fabricating.
        assert_eq!(
            decompose_query("What is my API key rotation policy?"),
            vec!["What is my API key rotation policy".to_string()]
        );
    }

    #[test]
    fn decompose_empty_query_yields_no_fragments() {
        // Pins: empty input decomposes to nothing, letting the caller fall back.
        assert!(decompose_query("   ").is_empty());
    }

    #[test]
    fn strategy_labels_are_snake_case() {
        // Pins: metadata/trace labels stay stable snake_case across deploys.
        assert_eq!(RetrievalStrategy::Skip.as_str(), "skip");
        assert_eq!(RetrievalStrategy::Fast.as_str(), "fast");
        assert_eq!(RetrievalStrategy::Deep.as_str(), "deep");
        assert_eq!(RetrievalStrategy::Agentic.as_str(), "agentic");
    }
}
