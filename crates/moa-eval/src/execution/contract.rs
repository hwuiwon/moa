//! Deterministic one-to-one scoring for generated execution goal contracts.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{CompletionCheckKind, GeneratedExecutionCandidate};
use moa_eval_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Expected normalized text for one requirement or constraint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextExpectation {
    /// Stable expectation identifier.
    pub expectation_id: String,
    /// Normalized terms that must all be present.
    pub all_terms: Vec<String>,
    /// Optional alternatives, at least one of which must be present.
    pub any_terms: Vec<String>,
    /// Terms whose presence invalidates a match.
    pub forbidden_terms: Vec<String>,
}

/// Expected structured deliverable semantics.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverableExpectation {
    /// Stable expectation identifier.
    pub expectation_id: String,
    /// Exact terminal-output JSON pointer.
    pub output_pointer: String,
    /// Exact declared JSON Schema.
    pub schema: Value,
}

/// Expected independent map-coverage semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageExpectation {
    /// Stable expectation identifier.
    pub expectation_id: String,
    /// Stable map node identifier.
    pub map_node_id: String,
    /// Independently supplied expected item keys.
    pub expected_keys: Vec<String>,
    /// Whether every expected item is required.
    pub require_all: bool,
}

/// Closed completion-check category used for structural matching.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionCheckKindExpectation {
    /// Validate terminal deliverable schemas.
    OutputSchema,
    /// Require declared nodes to complete.
    RequiredNodes,
    /// Require one map universe to complete.
    MapCoverage,
    /// Require per-task citations.
    Citations,
    /// Run a bounded semantic verifier.
    AgentVerifier,
}

/// Expected completion-check kind and linked gold semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheckExpectation {
    /// Stable expectation identifier.
    pub expectation_id: String,
    /// Expected closed completion-check category.
    pub kind: CompletionCheckKindExpectation,
    /// Gold requirement expectation IDs linked by the check.
    pub requirement_expectation_ids: Vec<String>,
    /// Gold constraint expectation IDs linked by the check.
    pub constraint_expectation_ids: Vec<String>,
}

/// Exact expected value at one run-input JSON pointer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunInputExpectation {
    /// Stable expectation identifier.
    pub expectation_id: String,
    /// RFC 6901 pointer into `GeneratedExecutionCandidate.run_input`.
    pub pointer: String,
    /// Exact expected JSON value.
    pub value: Value,
}

/// Human-authored expected contract entries grouped by production goal category.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContractExpectations {
    /// Expected user requirements.
    pub requirements: Vec<TextExpectation>,
    /// Expected immutable constraints.
    pub constraints: Vec<TextExpectation>,
    /// Expected structured deliverables.
    pub deliverables: Vec<DeliverableExpectation>,
    /// Expected independent universes.
    pub coverage: Vec<CoverageExpectation>,
    /// Expected completion gates.
    pub completion_checks: Vec<CompletionCheckExpectation>,
    /// Expected structured run inputs.
    pub run_input: Vec<RunInputExpectation>,
}

/// One strict recorded planner candidate and its gold contract.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContractCase {
    /// Case schema version, fixed at `1`.
    pub schema_version: u8,
    /// Stable unique case identifier.
    pub case_id: String,
    /// Strict production planner response envelope.
    pub candidate: GeneratedExecutionCandidate,
    /// Human-authored deterministic expectations.
    pub expected: ExecutionContractExpectations,
    /// Stable corpus grouping labels.
    pub tags: Vec<String>,
}

/// Precision, recall, and F1 with exact category counts.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCategoryMetrics {
    /// Number of gold entries.
    pub expected_count: u64,
    /// Number of generated entries.
    pub actual_count: u64,
    /// Maximum one-to-one matches.
    pub matched_count: u64,
    /// Matched divided by generated entries.
    pub precision: f64,
    /// Matched divided by gold entries.
    pub recall: f64,
    /// Harmonic mean of precision and recall.
    pub f1: f64,
}

/// One category's metrics and stable matching diagnostics.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContractCategoryScore {
    /// Exact category metrics.
    pub metrics: ContractCategoryMetrics,
    /// Gold expectation ID to generated entry ID.
    pub matches: BTreeMap<String, String>,
    /// Gold expectation IDs that were not matched.
    pub missing_expectation_ids: Vec<String>,
    /// Generated entry IDs that matched no gold expectation.
    pub unsupported_actual_ids: Vec<String>,
}

/// Complete deterministic contract-fidelity score for one case.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContractScore {
    /// Stable corpus case identifier.
    pub case_id: String,
    /// Requirement matching score.
    pub requirements: ExecutionContractCategoryScore,
    /// Constraint matching score.
    pub constraints: ExecutionContractCategoryScore,
    /// Deliverable matching score.
    pub deliverables: ExecutionContractCategoryScore,
    /// Coverage matching score.
    pub coverage: ExecutionContractCategoryScore,
    /// Completion-check matching score.
    pub completion_checks: ExecutionContractCategoryScore,
    /// Run-input predicate score.
    pub run_input: ExecutionContractCategoryScore,
    /// Mean F1 across categories carrying gold or generated entries.
    pub macro_f1: f64,
    /// Whether at least one required gold entry was omitted.
    pub contract_omission: bool,
}

/// Scores one recorded candidate with maximum one-to-one matching per category.
pub fn score_contract_case(case: &ExecutionContractCase) -> Result<ExecutionContractScore> {
    validate_contract_case(case)?;
    let goal = &case.candidate.goal;
    let requirements = score_text(
        &case.expected.requirements,
        &goal
            .requirements
            .iter()
            .map(|entry| (entry.id.as_str(), entry.description.as_str()))
            .collect::<Vec<_>>(),
    )?;
    let constraints = score_text(
        &case.expected.constraints,
        &goal
            .constraints
            .iter()
            .map(|entry| (entry.id.as_str(), entry.description.as_str()))
            .collect::<Vec<_>>(),
    )?;
    let deliverables = score_deliverables(case)?;
    let coverage = score_coverage(case)?;
    let completion_checks =
        score_completion_checks(case, &requirements.matches, &constraints.matches)?;
    let run_input = score_run_input(case)?;
    let categories = [
        &requirements,
        &constraints,
        &deliverables,
        &coverage,
        &completion_checks,
        &run_input,
    ];
    let active = categories
        .iter()
        .filter(|score| score.metrics.expected_count != 0 || score.metrics.actual_count != 0)
        .collect::<Vec<_>>();
    let macro_f1 = if active.is_empty() {
        1.0
    } else {
        active.iter().map(|score| score.metrics.f1).sum::<f64>() / active.len() as f64
    };
    let contract_omission = categories
        .iter()
        .any(|score| score.metrics.matched_count < score.metrics.expected_count);
    Ok(ExecutionContractScore {
        case_id: case.case_id.clone(),
        requirements,
        constraints,
        deliverables,
        coverage,
        completion_checks,
        run_input,
        macro_f1,
        contract_omission,
    })
}

/// Validates one contract case independently of corpus-level counts.
pub(crate) fn validate_contract_case(case: &ExecutionContractCase) -> Result<()> {
    if case.schema_version != 1 || case.case_id.trim().is_empty() {
        return Err(invalid_config(format!(
            "execution contract case `{}` has an invalid version or ID",
            case.case_id
        )));
    }
    let requirement_ids = validate_text_expectations("requirement", &case.expected.requirements)?;
    let constraint_ids = validate_text_expectations("constraint", &case.expected.constraints)?;
    validate_expectation_ids(
        "deliverable",
        case.expected
            .deliverables
            .iter()
            .map(|entry| entry.expectation_id.as_str()),
    )?;
    validate_expectation_ids(
        "coverage",
        case.expected
            .coverage
            .iter()
            .map(|entry| entry.expectation_id.as_str()),
    )?;
    let completion_ids = validate_expectation_ids(
        "completion check",
        case.expected
            .completion_checks
            .iter()
            .map(|entry| entry.expectation_id.as_str()),
    )?;
    validate_expectation_ids(
        "run input",
        case.expected
            .run_input
            .iter()
            .map(|entry| entry.expectation_id.as_str()),
    )?;
    for deliverable in &case.expected.deliverables {
        if !valid_json_pointer(&deliverable.output_pointer) {
            return Err(invalid_config(format!(
                "contract case `{}` has invalid deliverable pointer `{}`",
                case.case_id, deliverable.output_pointer
            )));
        }
    }
    for coverage in &case.expected.coverage {
        if coverage.map_node_id.trim().is_empty()
            || coverage.expected_keys.is_empty()
            || coverage
                .expected_keys
                .iter()
                .any(|key| key.trim().is_empty())
            || coverage.expected_keys.iter().collect::<BTreeSet<_>>().len()
                != coverage.expected_keys.len()
        {
            return Err(invalid_config(format!(
                "contract case `{}` has invalid independent coverage expectations",
                case.case_id
            )));
        }
    }
    for check in &case.expected.completion_checks {
        if check
            .requirement_expectation_ids
            .iter()
            .any(|id| !requirement_ids.contains(id.as_str()))
            || check
                .constraint_expectation_ids
                .iter()
                .any(|id| !constraint_ids.contains(id.as_str()))
        {
            return Err(invalid_config(format!(
                "contract case `{}` has a completion check linked to an unknown expectation",
                case.case_id
            )));
        }
    }
    if completion_ids.len() != case.expected.completion_checks.len() {
        return Err(invalid_config(format!(
            "contract case `{}` has duplicate completion expectations",
            case.case_id
        )));
    }
    for predicate in &case.expected.run_input {
        if !valid_json_pointer(&predicate.pointer) {
            return Err(invalid_config(format!(
                "contract case `{}` has invalid run-input pointer `{}`",
                case.case_id, predicate.pointer
            )));
        }
    }
    Ok(())
}

fn score_text(
    expected: &[TextExpectation],
    actual: &[(&str, &str)],
) -> Result<ExecutionContractCategoryScore> {
    let edges = expected
        .iter()
        .map(|gold| {
            actual
                .iter()
                .enumerate()
                .filter_map(|(index, (_, description))| {
                    text_matches(gold, description).then_some(index)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    category_score(
        expected
            .iter()
            .map(|entry| entry.expectation_id.as_str())
            .collect(),
        actual.iter().map(|(id, _)| *id).collect(),
        edges,
    )
}

fn score_deliverables(case: &ExecutionContractCase) -> Result<ExecutionContractCategoryScore> {
    let expected = &case.expected.deliverables;
    let actual = &case.candidate.goal.deliverables;
    let edges = expected
        .iter()
        .map(|gold| {
            actual
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    (entry.output_pointer == gold.output_pointer && entry.schema == gold.schema)
                        .then_some(index)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    category_score(
        expected
            .iter()
            .map(|entry| entry.expectation_id.as_str())
            .collect(),
        actual.iter().map(|entry| entry.id.as_str()).collect(),
        edges,
    )
}

fn score_coverage(case: &ExecutionContractCase) -> Result<ExecutionContractCategoryScore> {
    let expected = &case.expected.coverage;
    let actual = &case.candidate.goal.coverage;
    let edges = expected
        .iter()
        .map(|gold| {
            actual
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let keys = string_set(&entry.expected_items)?;
                    (entry.map_node_id == gold.map_node_id
                        && entry.require_all == gold.require_all
                        && keys == gold.expected_keys.iter().cloned().collect())
                    .then_some(index)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    category_score(
        expected
            .iter()
            .map(|entry| entry.expectation_id.as_str())
            .collect(),
        actual.iter().map(|entry| entry.id.as_str()).collect(),
        edges,
    )
}

fn score_completion_checks(
    case: &ExecutionContractCase,
    requirement_matches: &BTreeMap<String, String>,
    constraint_matches: &BTreeMap<String, String>,
) -> Result<ExecutionContractCategoryScore> {
    let expected = &case.expected.completion_checks;
    let actual = &case.candidate.goal.completion_checks;
    let edges =
        expected
            .iter()
            .map(|gold| {
                let expected_requirements =
                    linked_actual_ids(&gold.requirement_expectation_ids, requirement_matches);
                let expected_constraints =
                    linked_actual_ids(&gold.constraint_expectation_ids, constraint_matches);
                actual
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        (expected_requirements.as_ref().is_some_and(|ids| {
                            ids == &entry.requirement_ids.iter().cloned().collect()
                        }) && expected_constraints.as_ref().is_some_and(|ids| {
                            ids == &entry.constraint_ids.iter().cloned().collect()
                        }) && completion_kind(&entry.kind) == gold.kind)
                            .then_some(index)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
    category_score(
        expected
            .iter()
            .map(|entry| entry.expectation_id.as_str())
            .collect(),
        actual.iter().map(|entry| entry.id.as_str()).collect(),
        edges,
    )
}

fn score_run_input(case: &ExecutionContractCase) -> Result<ExecutionContractCategoryScore> {
    let expected = &case.expected.run_input;
    let matches = expected
        .iter()
        .filter(|entry| case.candidate.run_input.pointer(&entry.pointer) == Some(&entry.value))
        .map(|entry| {
            (
                entry.expectation_id.clone(),
                format!("run_input:{}", entry.pointer),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let missing_expectation_ids = expected
        .iter()
        .filter(|entry| !matches.contains_key(&entry.expectation_id))
        .map(|entry| entry.expectation_id.clone())
        .collect::<Vec<_>>();
    let count = usize_to_u64(expected.len(), "run-input expectation count")?;
    let matched = usize_to_u64(matches.len(), "matched run-input expectation count")?;
    Ok(ExecutionContractCategoryScore {
        metrics: metrics(count, count, matched),
        matches,
        missing_expectation_ids,
        unsupported_actual_ids: Vec::new(),
    })
}

fn category_score(
    expected_ids: Vec<&str>,
    actual_ids: Vec<&str>,
    edges: Vec<Vec<usize>>,
) -> Result<ExecutionContractCategoryScore> {
    let matched_actual = maximum_one_to_one(&edges, actual_ids.len());
    let mut matches = BTreeMap::new();
    for (actual_index, expected_index) in matched_actual.iter().enumerate() {
        if let Some(expected_index) = expected_index {
            matches.insert(
                expected_ids[*expected_index].to_string(),
                actual_ids[actual_index].to_string(),
            );
        }
    }
    let missing_expectation_ids = expected_ids
        .iter()
        .filter(|id| !matches.contains_key(**id))
        .map(|id| (*id).to_string())
        .collect::<Vec<_>>();
    let matched_actual_ids = matches
        .values()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let unsupported_actual_ids = actual_ids
        .iter()
        .filter(|id| !matched_actual_ids.contains(**id))
        .map(|id| (*id).to_string())
        .collect::<Vec<_>>();
    let expected_count = usize_to_u64(expected_ids.len(), "contract expected count")?;
    let actual_count = usize_to_u64(actual_ids.len(), "contract actual count")?;
    let matched_count = usize_to_u64(matches.len(), "contract matched count")?;
    Ok(ExecutionContractCategoryScore {
        metrics: metrics(expected_count, actual_count, matched_count),
        matches,
        missing_expectation_ids,
        unsupported_actual_ids,
    })
}

fn maximum_one_to_one(edges: &[Vec<usize>], actual_count: usize) -> Vec<Option<usize>> {
    let mut actual_to_expected = vec![None; actual_count];
    for expected_index in 0..edges.len() {
        let mut visited = vec![false; actual_count];
        let _ = augment(expected_index, edges, &mut visited, &mut actual_to_expected);
    }
    actual_to_expected
}

fn augment(
    expected_index: usize,
    edges: &[Vec<usize>],
    visited: &mut [bool],
    actual_to_expected: &mut [Option<usize>],
) -> bool {
    for &actual_index in &edges[expected_index] {
        if visited[actual_index] {
            continue;
        }
        visited[actual_index] = true;
        let available = match actual_to_expected[actual_index] {
            None => true,
            Some(owner) => augment(owner, edges, visited, actual_to_expected),
        };
        if available {
            actual_to_expected[actual_index] = Some(expected_index);
            return true;
        }
    }
    false
}

fn text_matches(expectation: &TextExpectation, actual: &str) -> bool {
    let actual = normalize_text(actual);
    expectation
        .all_terms
        .iter()
        .all(|term| actual.contains(&normalize_text(term)))
        && (expectation.any_terms.is_empty()
            || expectation
                .any_terms
                .iter()
                .any(|term| actual.contains(&normalize_text(term))))
        && expectation
            .forbidden_terms
            .iter()
            .all(|term| !actual.contains(&normalize_text(term)))
}

fn normalize_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn completion_kind(kind: &CompletionCheckKind) -> CompletionCheckKindExpectation {
    match kind {
        CompletionCheckKind::OutputSchema => CompletionCheckKindExpectation::OutputSchema,
        CompletionCheckKind::RequiredNodes { .. } => CompletionCheckKindExpectation::RequiredNodes,
        CompletionCheckKind::MapCoverage { .. } => CompletionCheckKindExpectation::MapCoverage,
        CompletionCheckKind::Citations { .. } => CompletionCheckKindExpectation::Citations,
        CompletionCheckKind::AgentVerifier { .. } => CompletionCheckKindExpectation::AgentVerifier,
    }
}

fn linked_actual_ids(
    expectation_ids: &[String],
    matches: &BTreeMap<String, String>,
) -> Option<BTreeSet<String>> {
    expectation_ids
        .iter()
        .map(|id| matches.get(id).cloned())
        .collect()
}

fn string_set(value: &Value) -> Option<BTreeSet<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn metrics(expected: u64, actual: u64, matched: u64) -> ContractCategoryMetrics {
    let precision = defined_ratio(matched, actual, expected == 0);
    let recall = defined_ratio(matched, expected, actual == 0);
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    ContractCategoryMetrics {
        expected_count: expected,
        actual_count: actual,
        matched_count: matched,
        precision,
        recall,
        f1,
    }
}

fn defined_ratio(numerator: u64, denominator: u64, empty_is_perfect: bool) -> f64 {
    if denominator == 0 {
        f64::from(empty_is_perfect)
    } else {
        numerator as f64 / denominator as f64
    }
}

fn validate_text_expectations<'a>(
    category: &str,
    expectations: &'a [TextExpectation],
) -> Result<BTreeSet<&'a str>> {
    let ids = validate_expectation_ids(
        category,
        expectations
            .iter()
            .map(|entry| entry.expectation_id.as_str()),
    )?;
    for entry in expectations {
        if entry.all_terms.is_empty()
            || entry
                .all_terms
                .iter()
                .chain(&entry.any_terms)
                .chain(&entry.forbidden_terms)
                .any(|term| normalize_text(term).is_empty())
        {
            return Err(invalid_config(format!(
                "{category} expectation `{}` has invalid terms",
                entry.expectation_id
            )));
        }
    }
    Ok(ids)
}

fn validate_expectation_ids<'a>(
    category: &str,
    ids: impl Iterator<Item = &'a str>,
) -> Result<BTreeSet<&'a str>> {
    let mut observed = BTreeSet::new();
    for id in ids {
        if id.trim().is_empty() || !observed.insert(id) {
            return Err(invalid_config(format!(
                "{category} expectation ID `{id}` is empty or duplicated"
            )));
        }
    }
    Ok(observed)
}

fn valid_json_pointer(pointer: &str) -> bool {
    pointer.is_empty() || pointer.starts_with('/')
}

fn usize_to_u64(value: usize, context: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| invalid_config(format!("{context} exceeds u64")))
}

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}
