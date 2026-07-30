//! Skill regression suite source generation and comparison helpers.

use std::{collections::HashSet, path::PathBuf};

use moa_core::{error::MoaError, error::Result, types::identifiers::TenantId};
use moa_eval_core::assertion::{AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect};
use moa_eval_core::evaluators::{
    ORDERED_ACTIONS_EVALUATOR_ID, REQUIRED_ACTIONS_EVALUATOR_ID, TEXT_MATCH_EVALUATOR_ID,
};
use moa_eval_core::{ExpectedOutput, SuiteOracle, TestCase, TestSuite};

use crate::evidence::{EvidenceSource, SanitizedLearningEvidence};
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
///
/// The generated case's input, expectations, and trajectory are all lifted from
/// the transcript, so a suite built from raw events would persist unredacted
/// caller content into a durable draft artifact. Only sanitized evidence is
/// accepted.
pub fn generate_skill_test_suite_source(
    tenant_id: TenantId,
    skill: &SkillDocument,
    evidence: &SanitizedLearningEvidence,
) -> Result<GeneratedSkillSuite> {
    generate_skill_test_suite_source_for_name(tenant_id, &skill.frontmatter.name, evidence)
}

/// Generates regression suite TOML for a skill known only by name.
///
/// Sibling-suite accumulation uses this when a recurring task dedupes onto an
/// open proposal: the new session's sanitized evidence becomes held-out material
/// for the open candidate without regenerating (or even parsing) its skill
/// document.
pub fn generate_skill_test_suite_source_for_name(
    tenant_id: TenantId,
    skill_name: &str,
    evidence: &SanitizedLearningEvidence,
) -> Result<GeneratedSkillSuite> {
    let suite = build_generated_suite(skill_name, evidence);
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

fn build_generated_suite(skill_name: &str, evidence: &SanitizedLearningEvidence) -> TestSuite {
    let case_name = slugify_case_name(&extract_task_input(evidence));
    let (contains, oracle) = extract_expected_output(evidence);
    let trajectory = successful_tool_trajectory(evidence);
    TestSuite {
        name: format!("{skill_name}-regression"),
        description: Some(format!("Auto-generated regression suite for {skill_name}")),
        cases: vec![TestCase {
            name: if case_name.is_empty() {
                "smoke".to_string()
            } else {
                case_name
            },
            input: extract_task_input(evidence),
            assertions: generated_assertions(contains, &trajectory),
            oracle: Some(oracle),
            timeout_seconds: Some(DEFAULT_SUITE_TIMEOUT_SECONDS),
            tags: vec!["skill".to_string(), "auto-generated".to_string()],
            metadata: std::collections::HashMap::new(),
            ..TestCase::default()
        }],
        default_timeout_seconds: DEFAULT_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), skill_name.to_string()],
        ..TestSuite::default()
    }
}

fn successful_tool_trajectory(evidence: &SanitizedLearningEvidence) -> Vec<String> {
    let successful_tool_ids = evidence
        .entries_from(EvidenceSource::ToolResult)
        .filter(|entry| entry.success() == Some(true) && !entry.is_error())
        .filter_map(|entry| entry.tool_id())
        .collect::<HashSet<_>>();

    evidence
        .entries_from(EvidenceSource::ToolInput)
        .filter(|entry| {
            entry
                .tool_id()
                .is_some_and(|tool_id| successful_tool_ids.contains(&tool_id))
        })
        .filter_map(|entry| entry.tool_name().map(str::to_string))
        .collect()
}

/// Builds the typed assertions for one generated regression case.
///
/// Two things changed when the generic LCS trajectory gate was deleted, and
/// both are deliberate:
///
/// - the recorded tool set becomes a **blocking** `required_actions` assertion
///   over the *distinct* tools the source session actually used successfully.
///   That is what "the skill still does the work" means, and it does not
///   punish a candidate for reaching the same result by a different route;
/// - the recorded *order* becomes a **diagnostic** `ordered_actions` assertion.
///   The source session's exact ordering is one observation of one run, not a
///   requirement, so it is reported for drift triage and never gates.
fn generated_assertions(contains: Vec<String>, trajectory: &[String]) -> Vec<AssertionSpec> {
    let mut assertions = Vec::new();
    if !contains.is_empty() {
        assertions.push(AssertionSpec {
            id: "response-carries-source-facts".to_string(),
            category: AssertionCategory::Communication,
            gate_effect: GateEffect::Blocking,
            evaluator: EvaluatorRef::deterministic(TEXT_MATCH_EVALUATOR_ID, 1),
            config: serde_json::to_value(ExpectedOutput {
                contains,
                ..ExpectedOutput::default()
            })
            .unwrap_or_default(),
        });
    }

    let mut distinct = trajectory.to_vec();
    distinct.sort();
    distinct.dedup();
    if !distinct.is_empty() {
        assertions.push(AssertionSpec {
            id: "recorded-tools-were-used".to_string(),
            category: AssertionCategory::Action,
            gate_effect: GateEffect::Blocking,
            evaluator: EvaluatorRef::deterministic(REQUIRED_ACTIONS_EVALUATOR_ID, 1),
            config: serde_json::json!({
                "actions": distinct
                    .iter()
                    .map(|name| serde_json::json!({ "name": name }))
                    .collect::<Vec<_>>(),
            }),
        });
    }

    if trajectory.len() >= 2 {
        assertions.push(AssertionSpec {
            id: "recorded-tool-order".to_string(),
            category: AssertionCategory::Action,
            gate_effect: GateEffect::Diagnostic,
            evaluator: EvaluatorRef::deterministic(ORDERED_ACTIONS_EVALUATOR_ID, 1),
            config: serde_json::json!({ "sequence": trajectory }),
        });
    }

    assertions
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
fn extract_expected_output(evidence: &SanitizedLearningEvidence) -> (Vec<String>, SuiteOracle) {
    let response = final_response_text(evidence);
    let tool_results = tool_result_texts(evidence);
    let facts = grounded_facts(&response, &tool_results);
    if facts.is_empty() {
        (extract_response_keywords(evidence), SuiteOracle::Keywords)
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

fn extract_task_input(evidence: &SanitizedLearningEvidence) -> String {
    evidence
        .entries()
        .iter()
        .find(|entry| {
            matches!(
                entry.source(),
                EvidenceSource::UserMessage | EvidenceSource::QueuedMessage
            )
        })
        .map(|entry| entry.text().to_string())
        .unwrap_or_else(|| "Run the learned workflow".to_string())
}

fn extract_response_keywords(evidence: &SanitizedLearningEvidence) -> Vec<String> {
    let mut keywords = evidence
        .entries_from(EvidenceSource::AssistantMessage)
        .next_back()
        .map(|entry| keywords_from_text(entry.text()))
        .unwrap_or_default();
    if keywords.is_empty() {
        keywords.push("completed".to_string());
    }
    keywords
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

/// Returns the last assistant response text in the segment, or empty when none.
fn final_response_text(evidence: &SanitizedLearningEvidence) -> String {
    evidence
        .entries_from(EvidenceSource::AssistantMessage)
        .next_back()
        .map(|entry| entry.text().to_string())
        .unwrap_or_default()
}

/// Concatenates the rendered text of every successful tool result in the
/// segment. This is the grounding corpus: a response token counts as a fact
/// only if it also appears here.
///
/// A result is included only when the source event's `success` flag was set AND
/// its output was not an error envelope. Denied/disallowed calls carry
/// `success: false`, and a process tool that ran but exited nonzero carries
/// `is_error: true`; both render error messages, exit codes, or failure
/// identifiers, which must never become required regression expectations. Tool
/// errors arrive under a distinct carrier and are excluded here as well.
fn tool_result_texts(evidence: &SanitizedLearningEvidence) -> String {
    evidence
        .entries_from(EvidenceSource::ToolResult)
        .filter(|entry| entry.success() == Some(true) && !entry.is_error())
        .map(|entry| entry.text().to_string())
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
    use moa_eval_core::types::TEST_CASE_SCHEMA_VERSION;

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

    /// Builds a one-segment event log, then sanitizes it into learning evidence.
    ///
    /// Suite generation only ever sees sanitized evidence, so the fixtures go
    /// through the real gate rather than around it.
    async fn segment(
        prompt: &str,
        tool_outputs: &[&str],
        response: &str,
    ) -> crate::evidence::SanitizedLearningEvidence {
        crate::evidence::sanitize_for_tests(&segment_events(prompt, tool_outputs, response)).await
    }

    fn segment_events(prompt: &str, tool_outputs: &[&str], response: &str) -> Vec<EventRecord> {
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

    #[tokio::test]
    async fn grounded_number_and_identifier_become_expectations() {
        // Pins: a number and a code identifier present in BOTH a tool result and
        // the final response are lifted into the case `contains` gate, and the
        // case is marked as fact-grounded.
        let evidence = segment(
            "fix the refresh regression",
            &[
                "refresh_token found in auth handler",
                "verification ran in 45ms",
                "closed ticket #482",
            ],
            "Patched refresh_token; the run took 45ms and closed #482.",
        )
        .await;

        let (contains, oracle) = extract_expected_output(&evidence);

        assert_eq!(oracle, SuiteOracle::GroundedFacts);
        assert!(
            contains.contains(&"refresh_token".to_string()),
            "{contains:?}"
        );
        assert!(contains.contains(&"45ms".to_string()), "{contains:?}");
        assert!(contains.contains(&"#482".to_string()), "{contains:?}");
    }

    #[tokio::test]
    async fn response_only_keyword_is_not_selected_when_grounded_facts_exist() {
        // Pins: a long response word that never appears in a tool result is not
        // treated as a fact; only the grounded token is asserted.
        let evidence = segment(
            "measure latency",
            &["measured 45ms latency"],
            "The regression analysis measured 45ms of latency.",
        )
        .await;

        let (contains, oracle) = extract_expected_output(&evidence);

        assert_eq!(oracle, SuiteOracle::GroundedFacts);
        assert_eq!(contains, vec!["45ms".to_string()]);
        assert!(
            !contains.iter().any(|fact| fact.contains("regression")),
            "ungrounded response keyword leaked into the oracle: {contains:?}"
        );
    }

    #[tokio::test]
    async fn keyword_fallback_when_no_tool_results() {
        // Pins: a segment with no tool results has no grounding corpus, so the
        // oracle falls back to response keywords and records the fallback mode.
        let evidence = segment(
            "summarize",
            &[],
            "Completed the lengthy analysis workflow successfully.",
        )
        .await;

        let (contains, oracle) = extract_expected_output(&evidence);

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
    async fn failed_tool_segment(
        prompt: &str,
        error_output: &str,
        response: &str,
    ) -> crate::evidence::SanitizedLearningEvidence {
        crate::evidence::sanitize_for_tests(&failed_tool_segment_events(
            prompt,
            error_output,
            response,
        ))
        .await
    }

    fn failed_tool_segment_events(
        prompt: &str,
        error_output: &str,
        response: &str,
    ) -> Vec<EventRecord> {
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

    #[tokio::test]
    async fn failed_tool_result_identifier_is_not_grounded() {
        // Pins: an identifier that appears only in a FAILED tool result (success
        // false, error output) is not lifted as a grounded fact; with no other
        // grounding the oracle falls back to response keywords.
        let evidence = failed_tool_segment(
            "deploy the service",
            "error E1234: connection refused to node_7",
            "Deployment failed with error E1234 on node_7.",
        )
        .await;

        let (contains, oracle) = extract_expected_output(&evidence);

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

    #[tokio::test]
    async fn generated_actions_include_only_successful_tool_calls() {
        // Pins: generated action requirements join calls to terminal results by
        // tool ID. A failed result and a durable ToolError are evidence of work
        // attempted, not work the candidate must reproduce successfully.
        let session = SessionId(Uuid::now_v7());
        let bash_id = ToolCallId::new();
        let failed_result_id = ToolCallId::new();
        let tool_error_id = ToolCallId::new();
        let file_read_id = ToolCallId::new();
        let tool_call = |sequence, tool_id, tool_name: &str| {
            record(
                session,
                sequence,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: tool_name.to_string(),
                    input: serde_json::json!({ "input": tool_name }),
                    hand_id: None,
                },
            )
        };
        let tool_result = |sequence, tool_id, tool_name: &str, success| {
            record(
                session,
                sequence,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text(
                        format!("{tool_name} output"),
                        Duration::from_millis(1),
                    ),
                    original_output_tokens: None,
                    success,
                    duration_ms: 1,
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin(tool_name),
                },
            )
        };
        let events = vec![
            record(
                session,
                1,
                Event::UserMessage {
                    text: "inspect and deploy".to_string(),
                    attachments: Vec::<Attachment>::new(),
                },
            ),
            tool_call(2, bash_id, "bash"),
            tool_result(3, bash_id, "bash", true),
            tool_call(4, failed_result_id, "http"),
            tool_result(5, failed_result_id, "http", false),
            tool_call(6, tool_error_id, "deploy"),
            record(
                session,
                7,
                Event::ToolError {
                    tool_id: tool_error_id,
                    provider_tool_use_id: None,
                    tool_name: "deploy".to_string(),
                    error: "deployment rejected".to_string(),
                    retryable: false,
                },
            ),
            tool_call(8, file_read_id, "file_read"),
            tool_result(9, file_read_id, "file_read", true),
            record(
                session,
                10,
                Event::BrainResponse {
                    text: "Inspection completed.".to_string(),
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
        ];
        let evidence = crate::evidence::sanitize_for_tests(&events).await;

        let suite = build_generated_suite("successful-tools", &evidence);
        let assertions = &suite.cases[0].assertions;
        let required = assertions
            .iter()
            .find(|spec| spec.evaluator.id == REQUIRED_ACTIONS_EVALUATOR_ID)
            .expect("successful calls produce a required-actions assertion");
        let required_names = required.config["actions"]
            .as_array()
            .expect("required actions array")
            .iter()
            .map(|action| {
                action["name"]
                    .as_str()
                    .expect("required action name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        let ordered = assertions
            .iter()
            .find(|spec| spec.evaluator.id == ORDERED_ACTIONS_EVALUATOR_ID)
            .expect("two successful calls produce an ordered-actions diagnostic");

        assert_eq!(
            required_names,
            vec!["bash".to_string(), "file_read".to_string()]
        );
        assert_eq!(
            ordered.config["sequence"],
            serde_json::json!(["bash", "file_read"])
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

    #[tokio::test]
    async fn generated_toml_round_trips_with_typed_assertions() {
        // Pins: the generated suite TOML parses back through the suite type with
        // its declared schema version, the grounded facts as a blocking text
        // assertion, and the recorded order as a non-blocking diagnostic.
        let evidence = segment(
            "close the ticket",
            &["closed ticket #77 in 12ms"],
            "Done: closed #77 after a 12ms verification.",
        )
        .await;

        let generated = generate_skill_test_suite_source_for_name(
            TenantId::new(),
            "round-trip-skill",
            &evidence,
        )
        .expect("generate suite source");

        let suite: TestSuite =
            toml::from_str(&generated.source_toml).expect("generated suite is valid TOML");
        suite
            .validate()
            .expect("a generated suite must satisfy the assertion registry");
        assert_eq!(suite.schema_version, TEST_CASE_SCHEMA_VERSION);
        let case = suite.cases.first().expect("one generated case");

        assert_eq!(case.oracle, Some(SuiteOracle::GroundedFacts));
        let text = case
            .assertions
            .iter()
            .find(|spec| spec.category == AssertionCategory::Communication)
            .expect("case carries a text assertion");
        assert_eq!(text.gate_effect, GateEffect::Blocking);
        let expected: ExpectedOutput =
            serde_json::from_value(text.config.clone()).expect("text config is ExpectedOutput");
        assert!(
            expected.contains.contains(&"#77".to_string()),
            "{:?}",
            expected.contains
        );
        assert!(
            expected.contains.contains(&"12ms".to_string()),
            "{:?}",
            expected.contains
        );

        let required = case
            .assertions
            .iter()
            .find(|spec| spec.evaluator.id == REQUIRED_ACTIONS_EVALUATOR_ID)
            .expect("case carries a required-action assertion");
        assert_eq!(required.gate_effect, GateEffect::Blocking);
        let ordered = case
            .assertions
            .iter()
            .find(|spec| spec.evaluator.id == ORDERED_ACTIONS_EVALUATOR_ID);
        assert!(
            ordered.is_none_or(|spec| spec.gate_effect == GateEffect::Diagnostic),
            "the recorded order is one observation, never a gate"
        );
    }

    #[test]
    fn a_single_tool_session_emits_no_ordering_assertion() {
        // Pins: an ordering claim needs at least two actions to constrain, so a
        // one-tool session does not manufacture a vacuous one.
        let assertions = generated_assertions(vec!["fact".to_string()], &["bash".to_string()]);

        assert!(
            !assertions
                .iter()
                .any(|spec| spec.evaluator.id == ORDERED_ACTIONS_EVALUATOR_ID)
        );
        assert!(
            assertions
                .iter()
                .any(|spec| spec.evaluator.id == REQUIRED_ACTIONS_EVALUATOR_ID)
        );
    }

    #[test]
    fn repeated_tools_collapse_into_one_required_action() {
        // Pins: the old LCS gate punished a candidate for calling bash twice
        // instead of three times. The required-action assertion asks whether the
        // tool was used at all, not how many times the source session used it.
        let assertions = generated_assertions(
            Vec::new(),
            &["bash".to_string(), "bash".to_string(), "bash".to_string()],
        );

        let required = assertions
            .iter()
            .find(|spec| spec.evaluator.id == REQUIRED_ACTIONS_EVALUATOR_ID)
            .expect("required-action assertion");
        let actions = required.config["actions"]
            .as_array()
            .expect("actions array");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["name"], serde_json::json!("bash"));
    }
}
