//! External-memory benchmark controls.
//!
//! The harness already renders a `NoMemory` control envelope, but an empty
//! envelope only bounds the *evidence* path. What bounds the *metric* is a
//! predictor: answering a multiple-choice benchmark without reading the question.
//!
//! Two nulls, both scored by the same exact-answer comparison the suite uses:
//!
//! - **no memory** answers from nothing. Offline, the honest deterministic stand-in
//!   for "a reader with no evidence" is a seeded uniform choice among the case's
//!   own options, which is exactly the information state the mode describes.
//! - **query-independent answer** ignores the question and returns the option that
//!   is most often correct in the authoring split — the answer-distribution prior.
//!
//! The oracle answers from the dataset's gold answer, which proves the comparison
//! can score 1.0.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::controls::authoring::AuthoringSplit;
use crate::external_memory::dataset::PreparedExternalMemoryCase;
use crate::kernel::controls::{NullCeiling, NullSeedRun, SliceEvidence};

/// Which negative control to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalMemoryNull {
    /// A reader holding no evidence: a seeded choice among the case's options.
    NoMemory,
    /// The authoring split's most frequently correct option, question ignored.
    QueryIndependentAnswer,
}

impl ExternalMemoryNull {
    /// Returns the registered control id.
    #[must_use]
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::NoMemory => "no_memory",
            Self::QueryIndependentAnswer => "query_independent_answer",
        }
    }
}

/// Fits the answer-distribution prior on the authoring split only.
///
/// Returns gold answers ordered by frequency, ties broken lexically. Fitting this
/// on the gated split would let the null learn the very labels it is bounding.
#[must_use]
pub fn answer_prior(cases: &[PreparedExternalMemoryCase], split: &AuthoringSplit) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for case in cases
        .iter()
        .filter(|case| split.is_authoring(&case.case.isolation_key))
    {
        *counts.entry(case.case.answer.as_str()).or_insert(0) += 1;
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    ranked
        .into_iter()
        .map(|(answer, _)| answer.to_string())
        .collect()
}

/// Returns one predicted answer per case for a negative control.
#[must_use]
pub fn control_answers(
    control: ExternalMemoryNull,
    cases: &[PreparedExternalMemoryCase],
    split: &AuthoringSplit,
    seed: u64,
) -> Vec<String> {
    match control {
        ExternalMemoryNull::NoMemory => cases
            .iter()
            .enumerate()
            .map(|(index, case)| {
                if case.case.options.is_empty() {
                    return String::new();
                }
                let draw = crate::controls::splitmix64(seed ^ (index as u64).wrapping_mul(0x9E37))
                    % case.case.options.len() as u64;
                case.case.options[draw as usize].clone()
            })
            .collect(),
        ExternalMemoryNull::QueryIndependentAnswer => {
            let prior = answer_prior(cases, split);
            cases
                .iter()
                .map(|case| {
                    prior
                        .iter()
                        .find(|answer| {
                            case.case.options.is_empty() || case.case.options.contains(answer)
                        })
                        .cloned()
                        .or_else(|| case.case.options.first().cloned())
                        .unwrap_or_default()
                })
                .collect()
        }
    }
}

/// Returns the oracle answer per case: the dataset's own gold answer.
#[must_use]
pub fn oracle_answers(cases: &[PreparedExternalMemoryCase]) -> Vec<String> {
    cases.iter().map(|case| case.case.answer.clone()).collect()
}

/// Scores exact-answer accuracy per dataset category.
#[must_use]
pub fn accuracy_by_category(
    cases: &[PreparedExternalMemoryCase],
    answers: &[String],
) -> BTreeMap<String, f64> {
    let mut totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (case, answer) in cases.iter().zip(answers) {
        let entry = totals.entry(case.case.category.clone()).or_insert((0, 0));
        entry.1 += 1;
        if answer == &case.case.answer {
            entry.0 += 1;
        }
    }
    totals
        .into_iter()
        .map(|(slice, (correct, total))| (slice, correct as f64 / total as f64))
        .collect()
}

/// Builds repeated null seed runs for one negative control.
#[must_use]
pub fn null_seed_runs(
    control: ExternalMemoryNull,
    cases: &[PreparedExternalMemoryCase],
    split: &AuthoringSplit,
    seeds: &[u64],
) -> Vec<NullSeedRun> {
    seeds
        .iter()
        .map(|seed| {
            let answers = control_answers(control, cases, split, *seed);
            NullSeedRun::new(*seed, accuracy_by_category(cases, &answers))
        })
        .collect()
}

/// Assembles per-category control evidence for observed candidate answers.
#[must_use]
pub fn accuracy_evidence(
    cases: &[PreparedExternalMemoryCase],
    candidate_answers: &[String],
    null_answers: &[String],
    ceilings: &BTreeMap<String, NullCeiling>,
    oracle_floor: f64,
) -> Vec<SliceEvidence> {
    let candidate = accuracy_by_category(cases, candidate_answers);
    let null = accuracy_by_category(cases, null_answers);
    let oracle = accuracy_by_category(cases, &oracle_answers(cases));
    candidate
        .iter()
        .filter_map(|(slice, value)| {
            let ceiling = ceilings.get(slice)?;
            Some(SliceEvidence {
                slice: slice.clone(),
                candidate: *value,
                null_observed: null.get(slice).copied().unwrap_or(0.0),
                null_ceiling: ceiling.clone(),
                oracle_observed: oracle.get(slice).copied().unwrap_or(0.0),
                oracle_floor,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    use crate::controls::authoring::DEFAULT_AUTHORING_FRACTION;
    use crate::external_memory::dataset::{
        EvidenceLabels, ExternalMemoryCaseV1, ExternalMemorySession, ExternalMemoryTurn,
        validate_case,
    };
    use crate::kernel::controls::{DEFAULT_CONTROL_ALPHA, derive_null_ceilings};

    fn case(
        key: &str,
        category: &str,
        answer: &str,
        options: &[&str],
    ) -> PreparedExternalMemoryCase {
        validate_case(ExternalMemoryCaseV1 {
            schema_version: 1,
            isolation_key: key.to_string(),
            sessions: vec![ExternalMemorySession {
                source_id: format!("{key}-session"),
                occurred_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                turns: vec![ExternalMemoryTurn {
                    source_id: format!("{key}-turn"),
                    occurred_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 1).unwrap(),
                    role: "user".to_string(),
                    text: format!("remember {answer}"),
                }],
            }],
            question: format!("what did {key} record?"),
            options: options.iter().map(|option| (*option).to_string()).collect(),
            answer: answer.to_string(),
            category: category.to_string(),
            evidence_labels: EvidenceLabels::default(),
        })
        .expect("fixture case is valid")
    }

    fn cases() -> Vec<PreparedExternalMemoryCase> {
        let options = ["blue", "green", "red", "amber"];
        (0..12)
            .map(|index| {
                let answer = if index % 3 == 0 {
                    "blue"
                } else {
                    options[index % 4]
                };
                case(
                    &format!("case-{index:02}"),
                    if index % 2 == 0 {
                        "single-session-user"
                    } else {
                        "multi-session"
                    },
                    answer,
                    &options,
                )
            })
            .collect()
    }

    fn split(cases: &[PreparedExternalMemoryCase]) -> AuthoringSplit {
        AuthoringSplit::derive(
            "external_memory",
            cases.iter().map(|case| case.case.isolation_key.as_str()),
            DEFAULT_AUTHORING_FRACTION,
        )
    }

    #[test]
    fn the_oracle_control_scores_perfectly_in_every_category() {
        let cases = cases();
        let accuracy = accuracy_by_category(&cases, &oracle_answers(&cases));

        assert!(
            accuracy.values().all(|value| *value == 1.0),
            "oracle accuracy {accuracy:?}"
        );
        assert_eq!(accuracy.len(), 2, "both categories are sliced");
    }

    #[test]
    fn the_answer_prior_is_fitted_only_on_the_authoring_split() {
        // Pins: the query-independent null learns from non-gating cases only.
        let cases = cases();
        let split = split(&cases);
        let prior = answer_prior(&cases, &split);
        let authoring_answers = cases
            .iter()
            .filter(|case| split.is_authoring(&case.case.isolation_key))
            .map(|case| case.case.answer.clone())
            .collect::<std::collections::BTreeSet<_>>();

        assert!(!prior.is_empty());
        for answer in &prior {
            assert!(
                authoring_answers.contains(answer),
                "prior answer {answer} is not an authoring label"
            );
        }
    }

    #[test]
    fn the_query_independent_null_ignores_the_question_entirely() {
        // Pins: every case receives the same predicted answer, which is what makes
        // the control a null rather than a weak reader.
        let cases = cases();
        let split = split(&cases);
        let answers = control_answers(
            ExternalMemoryNull::QueryIndependentAnswer,
            &cases,
            &split,
            17,
        );

        let unique = answers.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), 1, "answers {answers:?}");
    }

    #[test]
    fn the_no_memory_null_varies_by_seed_so_its_ceiling_has_real_variance() {
        // Pins: a chance-level control produces different runs per seed, so the
        // ceiling is a prediction bound rather than a restated constant.
        let cases = cases();
        let split = split(&cases);
        let runs = null_seed_runs(
            ExternalMemoryNull::NoMemory,
            &cases,
            &split,
            &[11, 12, 13, 14, 15],
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");

        assert_eq!(ceilings.len(), 2);
        assert!(
            ceilings.values().any(|ceiling| !ceiling.is_degenerate()),
            "a uniform-choice null should vary across seeds: {ceilings:?}"
        );
        assert!(
            ceilings.values().all(|ceiling| ceiling.ceiling < 1.0),
            "ceilings {ceilings:?}"
        );
    }

    #[test]
    fn a_candidate_that_only_matches_the_answer_prior_is_flagged() {
        use crate::kernel::controls::{
            ControlledMetric, DEFAULT_ORACLE_FLOOR, SuiteVerdict, audit_controlled_metric,
        };

        // Pins: a reader that reproduces the class prior does not clear its own
        // null ceiling, and the audit says so instead of quietly crediting it.
        let cases = cases();
        let split = split(&cases);
        let null_answers = control_answers(
            ExternalMemoryNull::QueryIndependentAnswer,
            &cases,
            &split,
            1,
        );
        let runs = null_seed_runs(
            ExternalMemoryNull::QueryIndependentAnswer,
            &cases,
            &split,
            &[1, 2, 3, 4, 5],
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");
        let slices = accuracy_evidence(
            &cases,
            &null_answers,
            &null_answers,
            &ceilings,
            DEFAULT_ORACLE_FLOOR,
        );

        let report = audit_controlled_metric(&ControlledMetric {
            suite: crate::controls::SUITE_EXTERNAL_MEMORY.to_string(),
            metric: "answer_accuracy".to_string(),
            candidate_overall: 0.33,
            slices,
        });

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(report.headline_score(), None);
    }
}
