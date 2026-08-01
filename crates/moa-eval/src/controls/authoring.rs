//! Authoring splits and checked-in-case authoring validators.
//!
//! Two rules live here.
//!
//! **Adaptive controls come from an authoring split.** A majority-class or
//! popularity null has to learn its prior from somewhere. Learning it from the
//! gated test set makes the null artificially strong and the candidate's margin
//! artificially small — the gate would then be tuned by its own control. The
//! split is a deterministic function of case id, so it is stable across runs,
//! independent of file order, and reproducible from the id alone.
//!
//! **Checked-in cases get an authoring validator, not a solution function.**
//! There is no oracle that can re-derive the correct route for a
//! human-adjudicated case, so pretending to "solve" it would be fiction. What
//! *is* checkable is authoring quality: complete provenance, no duplicates, no
//! contradictory labels on the same input, and no case whose input leaks its own
//! label.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::kernel::contamination::{jaccard, normalize, shingles};

/// Fraction of cases assigned to the authoring split by default.
pub const DEFAULT_AUTHORING_FRACTION: f64 = 0.25;

/// Similarity at which two case inputs count as the same authored case.
pub const DUPLICATE_INPUT_JACCARD: f64 = 0.90;

/// Deterministic authoring/gated partition over case identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoringSplit {
    /// Salt that makes the split specific to one suite.
    pub salt: String,
    /// Case ids authors may inspect and derive adaptive controls from.
    pub authoring: BTreeSet<String>,
    /// Case ids that gate and must never inform a control.
    pub gated: BTreeSet<String>,
}

impl AuthoringSplit {
    /// Partitions case ids by a salted hash of the id.
    ///
    /// The hash makes membership a property of the case, not of its position in
    /// a file, so adding a case never reshuffles the existing split.
    #[must_use]
    pub fn derive<'a>(
        salt: &str,
        case_ids: impl IntoIterator<Item = &'a str>,
        authoring_fraction: f64,
    ) -> Self {
        let fraction = authoring_fraction.clamp(0.0, 1.0);
        let threshold = (fraction * u64::MAX as f64) as u64;
        let mut authoring = BTreeSet::new();
        let mut gated = BTreeSet::new();
        for case_id in case_ids {
            if split_hash(salt, case_id) < threshold {
                authoring.insert(case_id.to_string());
            } else {
                gated.insert(case_id.to_string());
            }
        }
        Self {
            salt: salt.to_string(),
            authoring,
            gated,
        }
    }

    /// Returns whether a case may be used to fit an adaptive control.
    #[must_use]
    pub fn is_authoring(&self, case_id: &str) -> bool {
        self.authoring.contains(case_id)
    }

    /// Returns whether a case is gated.
    #[must_use]
    pub fn is_gated(&self, case_id: &str) -> bool {
        self.gated.contains(case_id)
    }

    /// Returns whether the two sides share no case.
    #[must_use]
    pub fn is_disjoint(&self) -> bool {
        self.authoring.is_disjoint(&self.gated)
    }
}

fn split_hash(salt: &str, case_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"moa.eval.authoring-split.v1\0");
    hasher.update(salt.as_bytes());
    hasher.update(b"\0");
    hasher.update(case_id.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("sha256 digest always has eight leading bytes"),
    )
}

/// Who authored a checked-in case and where its label came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseProvenance {
    /// Person or process that authored the case.
    pub authored_by: String,
    /// Where the case material came from.
    pub source: String,
    /// Who adjudicated the expected label.
    pub adjudicated_by: String,
}

impl CaseProvenance {
    /// Returns whether every provenance field is present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.authored_by.trim().is_empty()
            && !self.source.trim().is_empty()
            && !self.adjudicated_by.trim().is_empty()
    }
}

/// One checked-in case reduced to what an authoring validator can check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredCase {
    /// Stable case identifier.
    pub case_id: String,
    /// Exact input the case supplies to production code.
    pub input: String,
    /// Stringified expected label for this case.
    pub expected_label: String,
    /// Authoring provenance; absent provenance is a defect.
    pub provenance: Option<CaseProvenance>,
}

/// Label markers whose presence in an input would make the label guessable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelLeakLexicon {
    markers_by_label: BTreeMap<String, Vec<String>>,
}

impl LabelLeakLexicon {
    /// Builds a lexicon from label to marker-phrase pairs.
    #[must_use]
    pub fn new<L, M>(entries: impl IntoIterator<Item = (L, M)>) -> Self
    where
        L: Into<String>,
        M: IntoIterator,
        M::Item: Into<String>,
    {
        Self {
            markers_by_label: entries
                .into_iter()
                .map(|(label, markers)| {
                    (
                        label.into(),
                        markers
                            .into_iter()
                            .map(|marker| marker.into().to_lowercase())
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// Returns the first marker of a case's own label present in its input.
    #[must_use]
    pub fn leaked_marker(&self, label: &str, input: &str) -> Option<String> {
        let lowered = format!(" {} ", normalize(input));
        self.markers_by_label.get(label).and_then(|markers| {
            markers
                .iter()
                .find(|marker| lowered.contains(&format!(" {} ", normalize(marker))))
                .cloned()
        })
    }
}

/// One authoring defect found in a checked-in corpus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "defect", rename_all = "snake_case")]
pub enum AuthoringDefect {
    /// A case id is blank.
    BlankCaseId,
    /// Two cases share an id.
    DuplicateCaseId {
        /// Repeated id.
        case_id: String,
    },
    /// A case has missing or incomplete authoring provenance.
    IncompleteProvenance {
        /// Offending case.
        case_id: String,
    },
    /// Two cases carry the same input and the same label.
    DuplicateCase {
        /// First case.
        case_id: String,
        /// Duplicate of the first.
        duplicate_of: String,
        /// Input similarity.
        similarity: f64,
    },
    /// Two cases carry the same input and *different* labels, so at most one can
    /// be right and the corpus is unsatisfiable.
    ContradictoryLabels {
        /// First case.
        case_id: String,
        /// Conflicting case.
        conflicts_with: String,
        /// Label of the first case.
        label: String,
        /// Label of the conflicting case.
        other_label: String,
    },
    /// A case input contains a marker for its own expected label.
    LabelLeak {
        /// Offending case.
        case_id: String,
        /// Label that leaked.
        label: String,
        /// Marker phrase found in the input.
        marker: String,
    },
    /// A case is structurally impossible for production code to satisfy.
    ImpossibleCase {
        /// Offending case.
        case_id: String,
        /// Why no correct behavior can satisfy it.
        reason: String,
    },
}

/// Validates authoring quality of a checked-in case set.
///
/// Returns every defect rather than the first, so one review pass sees the whole
/// corpus. An empty result means the corpus is authored well enough to gate.
#[must_use]
pub fn validate_authored_cases(
    cases: &[AuthoredCase],
    lexicon: &LabelLeakLexicon,
) -> Vec<AuthoringDefect> {
    let mut defects = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for case in cases {
        if case.case_id.trim().is_empty() {
            defects.push(AuthoringDefect::BlankCaseId);
        } else if !seen_ids.insert(case.case_id.as_str()) {
            defects.push(AuthoringDefect::DuplicateCaseId {
                case_id: case.case_id.clone(),
            });
        }
        if !case
            .provenance
            .as_ref()
            .is_some_and(CaseProvenance::is_complete)
        {
            defects.push(AuthoringDefect::IncompleteProvenance {
                case_id: case.case_id.clone(),
            });
        }
        if let Some(marker) = lexicon.leaked_marker(&case.expected_label, &case.input) {
            defects.push(AuthoringDefect::LabelLeak {
                case_id: case.case_id.clone(),
                label: case.expected_label.clone(),
                marker,
            });
        }
    }

    let normalized = cases
        .iter()
        .map(|case| (normalize(&case.input), shingles(&case.input)))
        .collect::<Vec<_>>();
    for (left_index, left) in cases.iter().enumerate() {
        for (right_index, right) in cases.iter().enumerate().skip(left_index + 1) {
            let similarity = jaccard(&normalized[left_index].1, &normalized[right_index].1);
            let same_input = normalized[left_index].0 == normalized[right_index].0
                || similarity >= DUPLICATE_INPUT_JACCARD;
            if !same_input {
                continue;
            }
            if left.expected_label == right.expected_label {
                defects.push(AuthoringDefect::DuplicateCase {
                    case_id: right.case_id.clone(),
                    duplicate_of: left.case_id.clone(),
                    similarity,
                });
            } else {
                defects.push(AuthoringDefect::ContradictoryLabels {
                    case_id: left.case_id.clone(),
                    conflicts_with: right.case_id.clone(),
                    label: left.expected_label.clone(),
                    other_label: right.expected_label.clone(),
                });
            }
        }
    }
    defects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Option<CaseProvenance> {
        Some(CaseProvenance {
            authored_by: "release-desk".to_string(),
            source: "adjudication-2026-07".to_string(),
            adjudicated_by: "reviewer-a".to_string(),
        })
    }

    fn case(case_id: &str, input: &str, label: &str) -> AuthoredCase {
        AuthoredCase {
            case_id: case_id.to_string(),
            input: input.to_string(),
            expected_label: label.to_string(),
            provenance: provenance(),
        }
    }

    #[test]
    fn the_split_is_a_stable_property_of_the_case_id() {
        // Pins: membership does not depend on file order or on which other cases
        // exist, so adding a case never reshuffles an existing control's fit set.
        let all = ["a-1", "a-2", "a-3", "a-4", "a-5", "a-6", "a-7", "a-8"];
        let full = AuthoringSplit::derive("routing", all, DEFAULT_AUTHORING_FRACTION);
        let reordered = AuthoringSplit::derive(
            "routing",
            all.iter().rev().copied(),
            DEFAULT_AUTHORING_FRACTION,
        );
        assert_eq!(full, reordered);

        let subset = AuthoringSplit::derive("routing", ["a-3", "a-1"], DEFAULT_AUTHORING_FRACTION);
        assert_eq!(subset.is_authoring("a-1"), full.is_authoring("a-1"));
        assert_eq!(subset.is_authoring("a-3"), full.is_authoring("a-3"));
        assert!(full.is_disjoint());
    }

    #[test]
    fn a_different_salt_produces_a_different_split() {
        // Pins: two suites do not accidentally share one held-out partition.
        let ids = (0..64)
            .map(|index| format!("case-{index:03}"))
            .collect::<Vec<_>>();
        let borrowed = ids.iter().map(String::as_str).collect::<Vec<_>>();
        let routing = AuthoringSplit::derive("routing", borrowed.clone(), 0.25);
        let retrieval = AuthoringSplit::derive("retrieval", borrowed, 0.25);
        assert_ne!(routing.authoring, retrieval.authoring);
    }

    #[test]
    fn the_gated_side_is_the_larger_side_at_the_default_fraction() {
        // Pins: the authoring split stays a minority so the gate keeps its power.
        let ids = (0..200)
            .map(|index| format!("case-{index:03}"))
            .collect::<Vec<_>>();
        let split = AuthoringSplit::derive(
            "routing",
            ids.iter().map(String::as_str),
            DEFAULT_AUTHORING_FRACTION,
        );
        assert!(split.authoring.len() < split.gated.len());
        assert_eq!(split.authoring.len() + split.gated.len(), 200);
    }

    #[test]
    fn incomplete_provenance_is_a_defect() {
        let mut incomplete = case("c-1", "deploy the release", "execute");
        incomplete.provenance = Some(CaseProvenance {
            authored_by: "release-desk".to_string(),
            source: String::new(),
            adjudicated_by: "reviewer-a".to_string(),
        });
        let mut absent = case("c-2", "explain the release process", "respond");
        absent.provenance = None;

        let defects = validate_authored_cases(&[incomplete, absent], &LabelLeakLexicon::default());

        assert_eq!(
            defects,
            vec![
                AuthoringDefect::IncompleteProvenance {
                    case_id: "c-1".to_string()
                },
                AuthoringDefect::IncompleteProvenance {
                    case_id: "c-2".to_string()
                },
            ]
        );
    }

    #[test]
    fn duplicate_and_contradictory_cases_are_separated() {
        // Pins: the same input twice with one label is redundancy; with two
        // labels it is an unsatisfiable corpus, and the report says which.
        let cases = vec![
            case("c-1", "Ship release 2.1 to production now", "execute"),
            case("c-2", "ship release 2.1 to production now", "execute"),
            case("c-3", "Ship release 2.1 to production now!", "needs_input"),
        ];

        let defects = validate_authored_cases(&cases, &LabelLeakLexicon::default());

        assert!(defects.contains(&AuthoringDefect::DuplicateCase {
            case_id: "c-2".to_string(),
            duplicate_of: "c-1".to_string(),
            similarity: 1.0,
        }));
        assert!(
            defects.iter().any(|defect| matches!(
                defect,
                AuthoringDefect::ContradictoryLabels { case_id, conflicts_with, .. }
                    if case_id == "c-1" && conflicts_with == "c-3"
            )),
            "defects {defects:?}"
        );
    }

    #[test]
    fn an_input_that_states_its_own_label_is_a_leak() {
        // Pins: a case whose text says "needs input" measures string matching,
        // not routing.
        let lexicon = LabelLeakLexicon::new([
            ("needs_input", vec!["needs input", "needs_input"]),
            ("execute", vec!["execute"]),
        ]);
        let cases = vec![
            case(
                "c-1",
                "This one needs input from the user first",
                "needs_input",
            ),
            case("c-2", "Ask me for the target environment", "needs_input"),
        ];

        let defects = validate_authored_cases(&cases, &lexicon);

        assert_eq!(
            defects,
            vec![AuthoringDefect::LabelLeak {
                case_id: "c-1".to_string(),
                label: "needs_input".to_string(),
                marker: "needs input".to_string(),
            }]
        );
    }

    #[test]
    fn a_marker_for_another_label_is_not_a_leak() {
        // Pins: the check is about a case revealing *its own* answer, so ordinary
        // vocabulary overlap with other labels stays legal.
        let lexicon = LabelLeakLexicon::new([("execute", vec!["execute"])]);
        let cases = vec![case("c-1", "Explain how execute mode works", "respond")];

        assert!(validate_authored_cases(&cases, &lexicon).is_empty());
    }
}
