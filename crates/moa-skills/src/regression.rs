//! Skill regression suite source generation and comparison helpers.

use std::path::PathBuf;

use moa_core::{
    error::MoaError, error::Result, events::Event, types::events_stream::EventRecord,
    types::identifiers::TenantId,
};
use moa_eval_core::{ExpectedOutput, SuiteOracle, TestCase, TestSuite};

use crate::format::{SkillDocument, slugify_skill_name};

const DEFAULT_SUITE_TIMEOUT_SECONDS: u64 = 120;

/// Maximum number of grounded facts (or fallback keywords) asserted per case.
const MAX_ORACLE_FACTS: usize = 5;

/// Facts longer than this many bytes are dropped as truncated tool-output noise.
const MAX_FACT_LEN: usize = 80;

/// Package-relative path where a proposal's generated suite rides its draft
/// revision. Each promoted revision therefore carries the suite derived from
/// its own source session, and the review gate pools previous revisions'
/// suites as held-out material for the next candidate.
pub const REGRESSION_SUITE_PACKAGE_PATH: &str = "tests/regression-suite.toml";

/// Generated regression suite source for a skill draft proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSkillSuite {
    /// Path relative to the configured memory root where the suite would be stored.
    pub relative_path: String,
    /// Pretty TOML source for the generated suite.
    pub source_toml: String,
}

/// Aggregate regression scoring summary for one skill version.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionSummary {
    /// Average normalized score across all evaluated results.
    pub average_score: f64,
    /// Number of results that ended failed, errored, or timed out.
    pub failed_runs: usize,
    /// Number of results evaluated.
    pub total_runs: usize,
    /// Total dollar cost across the suite.
    pub total_cost_dollars: f64,
}

/// Generates regression suite TOML for a newly proposed skill without writing files.
pub fn generate_skill_test_suite_source(
    tenant_id: TenantId,
    skill: &SkillDocument,
    events: &[EventRecord],
) -> Result<GeneratedSkillSuite> {
    generate_skill_test_suite_source_for_name(tenant_id, &skill.frontmatter.name, events)
}

/// Generates regression suite TOML for a skill known only by name.
///
/// Sibling-suite accumulation uses this when a recurring task dedupes onto an
/// open proposal: the new session's events become held-out material for the
/// open candidate without regenerating (or even parsing) its skill document.
pub fn generate_skill_test_suite_source_for_name(
    tenant_id: TenantId,
    skill_name: &str,
    events: &[EventRecord],
) -> Result<GeneratedSkillSuite> {
    let suite = build_generated_suite(skill_name, events);
    let source_toml = toml::to_string_pretty(&suite)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    Ok(GeneratedSkillSuite {
        relative_path: skill_suite_relative_path(tenant_id, skill_name),
        source_toml,
    })
}

/// Compares baseline and candidate summaries and returns whether the candidate is acceptable.
#[must_use]
pub fn compare_scores(
    previous: &SkillRegressionSummary,
    candidate: &SkillRegressionSummary,
) -> bool {
    if candidate.failed_runs != previous.failed_runs {
        return candidate.failed_runs < previous.failed_runs;
    }

    candidate.average_score + f64::EPSILON >= previous.average_score
}

fn build_generated_suite(skill_name: &str, events: &[EventRecord]) -> TestSuite {
    let case_name = slugify_case_name(&extract_task_input(events));
    let (contains, oracle) = extract_expected_output(events);
    TestSuite {
        name: format!("{skill_name}-regression"),
        description: Some(format!("Auto-generated regression suite for {skill_name}")),
        cases: vec![TestCase {
            name: if case_name.is_empty() {
                "smoke".to_string()
            } else {
                case_name
            },
            input: extract_task_input(events),
            expected_output: Some(ExpectedOutput {
                contains,
                ..ExpectedOutput::default()
            }),
            expected_trajectory: Some(extract_tool_trajectory(events)),
            oracle: Some(oracle),
            timeout_seconds: Some(DEFAULT_SUITE_TIMEOUT_SECONDS),
            tags: vec!["skill".to_string(), "auto-generated".to_string()],
            metadata: std::collections::HashMap::new(),
            ..TestCase::default()
        }],
        default_timeout_seconds: DEFAULT_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), skill_name.to_string()],
    }
}

/// Derives the case's `contains` expectations and records which oracle produced
/// them.
///
/// Grounded facts — verifiable tokens (numbers, identifiers, quoted strings,
/// file paths, URLs) that appear in BOTH a tool result and the final response —
/// are preferred, because a candidate skill that reproduces them has actually
/// carried a real result forward rather than merely echoing the response's
/// longest words. When a segment yields no grounded facts (no tool results, or
/// none of the response's facts trace back to one), the extractor falls back to
/// the response-keyword heuristic so every case still has an oracle.
fn extract_expected_output(events: &[EventRecord]) -> (Vec<String>, SuiteOracle) {
    let response = final_response_text(events);
    let tool_results = tool_result_texts(events);
    let facts = grounded_facts(&response, &tool_results);
    if facts.is_empty() {
        (extract_response_keywords(events), SuiteOracle::Keywords)
    } else {
        (facts, SuiteOracle::GroundedFacts)
    }
}

fn skill_suite_relative_path(tenant_id: TenantId, skill_name: &str) -> String {
    PathBuf::from("tenants")
        .join(tenant_id.to_string())
        .join("skills")
        .join(slugify_skill_name(skill_name))
        .join("tests")
        .join("suite.toml")
        .to_string_lossy()
        .into_owned()
}

fn extract_task_input(events: &[EventRecord]) -> String {
    events
        .iter()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(text.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| "Run the learned workflow".to_string())
}

fn extract_response_keywords(events: &[EventRecord]) -> Vec<String> {
    let mut keywords = events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(keywords_from_text(text)),
            _ => None,
        })
        .unwrap_or_default();
    if keywords.is_empty() {
        keywords.push("completed".to_string());
    }
    keywords
}

fn extract_tool_trajectory(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

fn keywords_from_text(text: &str) -> Vec<String> {
    let mut keywords = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 5)
        .map(str::to_ascii_lowercase)
        .take(5)
        .collect::<Vec<_>>();
    keywords.sort();
    keywords.dedup();
    keywords
}

/// Returns the last brain response text in the segment, or empty when none.
fn final_response_text(events: &[EventRecord]) -> String {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Concatenates the rendered text of every successful tool result in the
/// segment. This is the grounding corpus: a response token counts as a fact
/// only if it also appears here.
///
/// A result is included only when the event's `success` flag is set AND its
/// output is not an error envelope (`output.is_error`). Denied/disallowed calls
/// carry `success: false`, and a process tool that ran but exited nonzero
/// carries `is_error: true`; both render error messages, exit codes, or failure
/// identifiers through [`ToolOutput::to_text`], which must never become required
/// regression expectations. `Event::ToolError` is a distinct variant and is
/// never a `ToolResult`, so it is excluded here as well.
fn tool_result_texts(events: &[EventRecord]) -> String {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolResult {
                output, success, ..
            } if *success && !output.is_error => Some(output.to_text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Selects the response's fact candidates that are also grounded in a tool
/// result, in first-appearance order, deduplicated and capped.
///
/// Grounding is a case-insensitive, word-boundary-aware match against the
/// concatenated tool output (see [`grounded_in`]): the fact must occur bounded
/// by non-word characters so that `"42"` does not ground against `"1420"`.
/// Facts longer than [`MAX_FACT_LEN`] are dropped as truncated noise.
fn grounded_facts(response: &str, tool_results: &str) -> Vec<String> {
    let haystack = tool_results.to_ascii_lowercase();
    let mut selected: Vec<String> = Vec::new();
    for fact in fact_candidates_in_order(response) {
        if selected.len() >= MAX_ORACLE_FACTS {
            break;
        }
        if fact.len() > MAX_FACT_LEN {
            continue;
        }
        let lowered = fact.to_ascii_lowercase();
        if !grounded_in(&lowered, &haystack) {
            continue;
        }
        if selected.iter().any(|kept| kept.eq_ignore_ascii_case(&fact)) {
            continue;
        }
        selected.push(fact);
    }
    selected
}

/// True when `needle` occurs in `haystack` bounded by non-word characters on
/// both sides (start/end of string count as boundaries). Both inputs are
/// assumed already lowercased.
///
/// A word character is an ASCII alphanumeric or `_`. Requiring a boundary on
/// each side prevents substring false positives: `"42"` does not ground against
/// `"1420"`, and `"45ms"` does not ground against `"845ms"`. Facts that begin or
/// end with punctuation (`"#482"`, `"$1,200"`, URLs, paths) still match because
/// only the characters immediately outside the occurrence are inspected.
fn grounded_in(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = haystack.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = haystack[search_from..].find(needle) {
        let start = search_from + relative;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        search_from = start + 1;
    }
    false
}

/// Extracts every fact candidate from `text` ordered by first byte appearance.
///
/// Candidate kinds: quoted or backticked spans, URLs, file paths, numbers (with
/// adjacent currency/unit), and identifiers (UUIDs, `#`/`ABC-123` refs, hex
/// SHAs, and `snake_case`/`camelCase` code identifiers). Words inside a quoted
/// span are not scanned twice. Returned candidates may repeat; the caller
/// deduplicates.
fn fact_candidates_in_order(text: &str) -> Vec<String> {
    let quoted = quoted_spans(text);
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for (start, _end, inner) in &quoted {
        let trimmed = inner.trim();
        if is_acceptable_quoted(trimmed) {
            candidates.push((*start, trimmed.to_string()));
        }
    }
    for (offset, word) in words_with_offsets(text) {
        if in_any_span(offset, &quoted) {
            continue;
        }
        if let Some(fact) = classify_word(word) {
            candidates.push((offset, fact));
        }
    }
    candidates.sort_by_key(|(offset, _)| *offset);
    candidates.into_iter().map(|(_, fact)| fact).collect()
}

/// Locates `"..."` and `` `...` `` spans as `(open_offset, past_close_offset,
/// inner)`. Unterminated openers are ignored. Both delimiters are ASCII, so the
/// recorded byte offsets always fall on char boundaries.
fn quoted_spans(text: &str) -> Vec<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let delimiter = bytes[index];
        if delimiter != b'"' && delimiter != b'`' {
            index += 1;
            continue;
        }
        match bytes[index + 1..]
            .iter()
            .position(|&byte| byte == delimiter)
        {
            Some(offset) => {
                let close = index + 1 + offset;
                spans.push((index, close + 1, text[index + 1..close].to_string()));
                index = close + 1;
            }
            None => index += 1,
        }
    }
    spans
}

/// Returns true when `offset` falls within any recorded quoted span.
fn in_any_span(offset: usize, spans: &[(usize, usize, String)]) -> bool {
    spans
        .iter()
        .any(|(start, end, _)| offset >= *start && offset < *end)
}

/// Splits `text` into whitespace-delimited words paired with their byte offset.
fn words_with_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                words.push((begin, &text[begin..index]));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        words.push((begin, &text[begin..]));
    }
    words
}

/// Classifies a single word as a fact after trimming surrounding punctuation,
/// returning the fact string when it matches a recognized kind.
fn classify_word(word: &str) -> Option<String> {
    let trimmed = trim_word(word);
    if trimmed.is_empty() {
        return None;
    }
    if is_url(trimmed) || is_file_path(trimmed) || is_number_fact(trimmed) || is_identifier(trimmed)
    {
        return Some(trimmed.to_string());
    }
    None
}

/// Strips surrounding brackets, quotes, and sentence punctuation while keeping
/// leading `#`/`$` (ref and currency markers) and trailing `%` (percent unit).
fn trim_word(word: &str) -> &str {
    const LEADING: &[char] = &[
        '(', '[', '{', '<', '"', '\'', '`', '.', ',', ';', ':', '!', '?',
    ];
    const TRAILING: &[char] = &[
        ')', ']', '}', '>', '"', '\'', '`', '.', ',', ';', ':', '!', '?',
    ];
    word.trim_start_matches(LEADING).trim_end_matches(TRAILING)
}

/// True for `http`/`https` URLs.
fn is_url(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// True for path-like tokens: any leading-slash path, any token with a file
/// extension, or a multi-segment (`>= 2` slashes) path. Single-slash tokens
/// without an extension (for example `and/or`) are deliberately excluded.
fn is_file_path(word: &str) -> bool {
    if word.contains('/') {
        return word.starts_with('/') || word.matches('/').count() >= 2 || has_file_extension(word);
    }
    has_file_extension(word)
}

/// True when the final path segment ends in `name.ext` with a `>= 2`-char name
/// and a 2..=8 alphanumeric extension (excludes noise like `e.g`).
fn has_file_extension(word: &str) -> bool {
    let segment = word.rsplit('/').next().unwrap_or(word);
    let Some(dot) = segment.rfind('.') else {
        return false;
    };
    let name = &segment[..dot];
    let ext = &segment[dot + 1..];
    let name_ok = name.len() >= 2 && name.chars().any(|c| c.is_ascii_alphabetic());
    let ext_ok = (2..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric());
    name_ok && ext_ok
}

/// True for a numeric fact: a run of digits with optional grouping/decimal
/// separators, an optional leading currency sign, and an optional short unit
/// suffix (`%` or up to four letters). Bare integers 0-2 and bare four-digit
/// years (1900-2099) are excluded as trivially common.
fn is_number_fact(word: &str) -> bool {
    let core = word.strip_prefix(['$', '€', '£', '¥']).unwrap_or(word);
    let had_currency = core.len() != word.len();
    let numeric_end = core
        .find(|c: char| !(c.is_ascii_digit() || c == ',' || c == '.' || c == '_'))
        .unwrap_or(core.len());
    let numeric = &core[..numeric_end];
    let suffix = &core[numeric_end..];
    if !numeric.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let suffix_ok = suffix.is_empty()
        || suffix == "%"
        || (suffix.len() <= 4 && suffix.chars().all(|c| c.is_ascii_alphabetic()));
    if !suffix_ok {
        return false;
    }
    let has_unit = had_currency || !suffix.is_empty();
    let has_grouping_or_decimal = numeric.contains([',', '.', '_']);
    let is_trivial_bare = !has_unit
        && !has_grouping_or_decimal
        && numeric
            .parse::<i64>()
            .is_ok_and(|value| (0..=2).contains(&value) || (1900..=2099).contains(&value));
    !is_trivial_bare
}

/// True for any recognized identifier kind.
fn is_identifier(word: &str) -> bool {
    is_uuid(word) || is_ref_token(word) || is_hex_sha(word) || is_code_identifier(word)
}

/// True for an 8-4-4-4-12 hexadecimal UUID.
fn is_uuid(word: &str) -> bool {
    let parts: Vec<&str> = word.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    [8usize, 4, 4, 4, 12]
        .iter()
        .zip(&parts)
        .all(|(len, part)| part.len() == *len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// True for `#123` issue refs and `ABC-123` ticket/PR refs.
fn is_ref_token(word: &str) -> bool {
    if let Some(number) = word.strip_prefix('#') {
        return !number.is_empty() && number.chars().all(|c| c.is_ascii_digit());
    }
    if let Some((prefix, number)) = word.split_once('-') {
        let prefix_ok = prefix.len() >= 2
            && prefix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            && prefix
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        let number_ok = !number.is_empty() && number.chars().all(|c| c.is_ascii_digit());
        return prefix_ok && number_ok;
    }
    false
}

/// True for a 7..=40 char hex string with at least one letter (a git SHA);
/// all-digit strings are left to the numeric rule.
fn is_hex_sha(word: &str) -> bool {
    (7..=40).contains(&word.len())
        && word.chars().all(|c| c.is_ascii_hexdigit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// True for a `snake_case` or `camelCase`/`PascalCase` code identifier.
fn is_code_identifier(word: &str) -> bool {
    if !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    if !word.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let snake = word.contains('_') && !word.starts_with('_') && !word.ends_with('_');
    let camel = word
        .chars()
        .zip(word.chars().skip(1))
        .any(|(first, second)| first.is_ascii_lowercase() && second.is_ascii_uppercase());
    snake || camel
}

/// True when a quoted span's trimmed content is a usable fact (3..=80 bytes).
fn is_acceptable_quoted(content: &str) -> bool {
    (3..=MAX_FACT_LEN).contains(&content.len())
}

fn slugify_case_name(input: &str) -> String {
    let slug = input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(str::to_ascii_lowercase)
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    if slug.len() > 64 {
        slug.chars().take(64).collect()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use moa_core::events::Event;
    use moa_core::types::channel::Attachment;
    use moa_core::types::events_stream::EventRecord;
    use moa_core::types::identifiers::{ModelId, SessionId, ToolCallId};
    use moa_core::types::provider::ModelTier;
    use moa_core::types::tools::ToolOutput;
    use uuid::Uuid;

    use super::*;

    fn record(session: SessionId, sequence: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: session,
            sequence_num: sequence,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    /// Builds a one-segment event log: a user prompt, one tool call/result per
    /// `tool_outputs`, and a final brain response.
    fn segment(prompt: &str, tool_outputs: &[&str], response: &str) -> Vec<EventRecord> {
        let session = SessionId(Uuid::now_v7());
        let mut events = Vec::new();
        let mut sequence = 1u64;
        events.push(record(
            session,
            sequence,
            Event::UserMessage {
                text: prompt.to_string(),
                attachments: Vec::<Attachment>::new(),
            },
        ));
        for output in tool_outputs {
            let tool_id = ToolCallId::new();
            sequence += 1;
            events.push(record(
                session,
                sequence,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "run" }),
                    hand_id: None,
                },
            ));
            sequence += 1;
            events.push(record(
                session,
                sequence,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text((*output).to_string(), Duration::from_millis(1)),
                    original_output_tokens: None,
                    success: true,
                    duration_ms: 1,
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
                },
            ));
        }
        sequence += 1;
        events.push(record(
            session,
            sequence,
            Event::BrainResponse {
                text: response.to_string(),
                thought_signature: None,
                model: ModelId::new("scripted-model"),
                model_tier: ModelTier::Auxiliary,
                input_tokens_uncached: 8,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 8,
                cost_cents: 0,
                duration_ms: 1,
                llm_ttft_ms: None,
            },
        ));
        events
    }

    #[test]
    fn grounded_number_and_identifier_become_expectations() {
        // Pins: a number and a code identifier present in BOTH a tool result and
        // the final response are lifted into the case `contains` gate, and the
        // case is marked as fact-grounded.
        let events = segment(
            "fix the refresh regression",
            &[
                "refresh_token found in auth handler",
                "verification ran in 45ms",
                "closed ticket #482",
            ],
            "Patched refresh_token; the run took 45ms and closed #482.",
        );

        let (contains, oracle) = extract_expected_output(&events);

        assert_eq!(oracle, SuiteOracle::GroundedFacts);
        assert!(
            contains.contains(&"refresh_token".to_string()),
            "{contains:?}"
        );
        assert!(contains.contains(&"45ms".to_string()), "{contains:?}");
        assert!(contains.contains(&"#482".to_string()), "{contains:?}");
    }

    #[test]
    fn response_only_keyword_is_not_selected_when_grounded_facts_exist() {
        // Pins: a long response word that never appears in a tool result is not
        // treated as a fact; only the grounded token is asserted.
        let events = segment(
            "measure latency",
            &["measured 45ms latency"],
            "The regression analysis measured 45ms of latency.",
        );

        let (contains, oracle) = extract_expected_output(&events);

        assert_eq!(oracle, SuiteOracle::GroundedFacts);
        assert_eq!(contains, vec!["45ms".to_string()]);
        assert!(
            !contains.iter().any(|fact| fact.contains("regression")),
            "ungrounded response keyword leaked into the oracle: {contains:?}"
        );
    }

    #[test]
    fn keyword_fallback_when_no_tool_results() {
        // Pins: a segment with no tool results has no grounding corpus, so the
        // oracle falls back to response keywords and records the fallback mode.
        let events = segment(
            "summarize",
            &[],
            "Completed the lengthy analysis workflow successfully.",
        );

        let (contains, oracle) = extract_expected_output(&events);

        assert_eq!(oracle, SuiteOracle::Keywords);
        assert!(!contains.is_empty());
        assert!(
            contains
                .iter()
                .all(|keyword| keyword.chars().all(|c| c.is_ascii_lowercase())),
            "keyword fallback lowercases every token: {contains:?}"
        );
    }

    #[test]
    fn grounded_facts_dedup_cap_and_order_are_deterministic() {
        // Pins: facts are emitted in first-appearance order, duplicates collapse,
        // and the set is capped at MAX_ORACLE_FACTS; repeated runs are identical.
        let response = "10ms 20ms 10ms 30ms 40ms 50ms 60ms";
        let tool_results = "10ms 20ms 30ms 40ms 50ms 60ms";

        let facts = grounded_facts(response, tool_results);

        assert_eq!(
            facts,
            vec!["10ms", "20ms", "30ms", "40ms", "50ms"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(facts.len(), MAX_ORACLE_FACTS);
        assert_eq!(facts, grounded_facts(response, tool_results));
    }

    #[test]
    fn trivial_numbers_and_ungrounded_facts_are_excluded() {
        // Pins: bare 0/1/2 and bare years are dropped; a fact absent from the
        // tool corpus is dropped even when it is a valid candidate.
        assert!(!is_number_fact("1"));
        assert!(!is_number_fact("2024"));
        assert!(is_number_fact("42"));
        assert!(is_number_fact("45ms"));
        assert!(is_number_fact("$1,200"));

        // "88ms" is a valid number candidate but is not in the tool corpus.
        let facts = grounded_facts("saw 45ms and 88ms", "only 45ms was measured");
        assert_eq!(facts, vec!["45ms".to_string()]);
    }

    /// Builds a one-segment log whose single tool result is marked failed and
    /// carries an error output, then a final response echoing the failure token.
    fn failed_tool_segment(prompt: &str, error_output: &str, response: &str) -> Vec<EventRecord> {
        let session = SessionId(Uuid::now_v7());
        let tool_id = ToolCallId::new();
        vec![
            record(
                session,
                1,
                Event::UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::<Attachment>::new(),
                },
            ),
            record(
                session,
                2,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "run" }),
                    hand_id: None,
                },
            ),
            record(
                session,
                3,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::error(error_output.to_string(), Duration::from_millis(1)),
                    original_output_tokens: None,
                    success: false,
                    duration_ms: 1,
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
                },
            ),
            record(
                session,
                4,
                Event::BrainResponse {
                    text: response.to_string(),
                    thought_signature: None,
                    model: ModelId::new("scripted-model"),
                    model_tier: ModelTier::Auxiliary,
                    input_tokens_uncached: 8,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 8,
                    cost_cents: 0,
                    duration_ms: 1,
                    llm_ttft_ms: None,
                },
            ),
        ]
    }

    #[test]
    fn failed_tool_result_identifier_is_not_grounded() {
        // Pins: an identifier that appears only in a FAILED tool result (success
        // false, error output) is not lifted as a grounded fact; with no other
        // grounding the oracle falls back to response keywords.
        let events = failed_tool_segment(
            "deploy the service",
            "error E1234: connection refused to node_7",
            "Deployment failed with error E1234 on node_7.",
        );

        let (contains, oracle) = extract_expected_output(&events);

        assert_eq!(
            oracle,
            SuiteOracle::Keywords,
            "failed-result tokens must not ground: {contains:?}"
        );
        assert!(
            !contains
                .iter()
                .any(|fact| fact == "node_7" || fact == "E1234"),
            "failure identifier leaked into expectations: {contains:?}"
        );
    }

    #[test]
    fn number_does_not_ground_against_longer_number_substring() {
        // Pins: word-boundary grounding — bare "42" in the response is not
        // grounded by "1420" in the tool corpus (raw substring would match).
        assert!(!grounded_in("42", "exit code 1420 returned"));
        assert!(grounded_in("42", "the answer is 42."));
        assert!(!grounded_in("45ms", "took 845ms overall"));
        assert!(grounded_in("45ms", "measured 45ms latency"));

        let facts = grounded_facts("saw 42 items", "processed 1420 rows");
        assert!(
            facts.is_empty(),
            "42 must not ground against 1420: {facts:?}"
        );
    }

    #[test]
    fn generated_toml_round_trips_with_grounded_oracle() {
        // Pins: the generated suite TOML parses back through the suite type with
        // the grounded facts and oracle marker preserved.
        let events = segment(
            "close the ticket",
            &["closed ticket #77 in 12ms"],
            "Done: closed #77 after a 12ms verification.",
        );

        let generated =
            generate_skill_test_suite_source_for_name(TenantId::new(), "round-trip-skill", &events)
                .expect("generate suite source");

        let suite: TestSuite =
            toml::from_str(&generated.source_toml).expect("generated suite is valid TOML");
        let case = suite.cases.first().expect("one generated case");

        assert_eq!(case.oracle, Some(SuiteOracle::GroundedFacts));
        let contains = &case
            .expected_output
            .as_ref()
            .expect("case has expected output")
            .contains;
        assert!(contains.contains(&"#77".to_string()), "{contains:?}");
        assert!(contains.contains(&"12ms".to_string()), "{contains:?}");
        assert!(case.expected_trajectory.is_some());
    }
}
