//! Lane classification and fixed-corpus leakage scanning.
//!
//! Three different problems get called "contamination" and they need different
//! controls:
//!
//! - **package leakage** — the evaluated corpus itself contains eval artifacts
//!   (a question paired with its answer key, a label file, dataset metadata).
//!   This applies to every fixed-corpus lane.
//! - **corpus freshness** — a frozen corpus stops representing current data.
//!   That is a cohort-versioning problem, handled in
//!   [`crate::kernel::cohorts`].
//!
//! The distinction that matters most in a RAG lane: retrieving a legitimate
//! benchmark *source document* is the entire point and must pass. Retrieving a
//! question/answer pair, a label file, or accidentally indexed eval metadata is
//! leakage and must fail. This module separates the two by requiring a
//! *pairing* signal — an object that restates the question and also carries the
//! gold answer — rather than by punishing answer overlap, which every correct
//! source document has by construction.
//!
//! Every check here fails closed: a missing content hash or missing provenance
//! is an error, never a skipped check.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Minimum shingle overlap for two texts to count as near-duplicates.
pub const DEFAULT_NEAR_DUPLICATE_JACCARD: f64 = 0.80;
/// Minimum fraction of a question's shingles an object must contain to count as
/// restating that question.
///
/// Containment, not Jaccard: a leaked page pairs the question *with* its answer,
/// so symmetric overlap is diluted by the answer text while containment stays
/// high. A legitimate source document does not restate the question at all.
pub const DEFAULT_QUESTION_RESTATEMENT_CONTAINMENT: f64 = 0.60;
/// Minimum answer-shingle containment for an object to count as answer-bearing.
pub const DEFAULT_ANSWER_CONTAINMENT: f64 = 0.90;
/// Shingle width used by every similarity measure in this module.
pub const SHINGLE_WIDTH: usize = 3;

/// How a lane obtains the material it is scored on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneClass {
    /// Retrieval or RAG over a pinned, closed corpus.
    FixedCorpusRetrieval,
    /// Answer generation over a public benchmark dataset.
    PublicAnswerGeneration,
    /// Checked-in fixtures with no retrieval corpus and no external source.
    ClosedFixtureSuite,
}

impl LaneClass {
    /// Returns the stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FixedCorpusRetrieval => "fixed_corpus_retrieval",
            Self::PublicAnswerGeneration => "public_answer_generation",
            Self::ClosedFixtureSuite => "closed_fixture_suite",
        }
    }

    /// Returns whether the class requires pinned corpus content hashes.
    #[must_use]
    pub const fn requires_pinned_corpus(self) -> bool {
        matches!(
            self,
            Self::FixedCorpusRetrieval | Self::PublicAnswerGeneration
        )
    }
}

/// One lane's contamination classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneClassification {
    /// Stable lane identifier.
    pub lane: &'static str,
    /// How the lane obtains scored material.
    pub class: LaneClass,
    /// Whether network access is denied for the duration of evaluation.
    pub network_denied: bool,
    /// Why this classification is correct for this lane.
    pub rationale: &'static str,
}

impl LaneClassification {
    /// Returns whether this lane must run a package-leakage scan.
    #[must_use]
    pub const fn requires_leakage_scan(&self) -> bool {
        self.class.requires_pinned_corpus()
    }

    /// Validates the internal consistency of one classification.
    ///
    /// A fixed corpus that has not denied network is a contradiction rather
    /// than a policy.
    pub fn validate(&self) -> Result<(), ContaminationError> {
        if self.lane.trim().is_empty() {
            return Err(ContaminationError::InvalidClassification {
                lane: self.lane.to_string(),
                reason: "lane id must not be blank".to_string(),
            });
        }
        if self.class.requires_pinned_corpus() && !self.network_denied {
            return Err(ContaminationError::InvalidClassification {
                lane: self.lane.to_string(),
                reason: "a pinned-corpus lane must deny network during evaluation".to_string(),
            });
        }
        if self.rationale.trim().is_empty() {
            return Err(ContaminationError::InvalidClassification {
                lane: self.lane.to_string(),
                reason: "classification must carry a rationale".to_string(),
            });
        }
        Ok(())
    }
}

/// What a corpus object is, as declared by whoever packaged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Legitimate benchmark source material. The only kind a corpus may hold.
    SourceDocument,
    /// A benchmark question.
    Question,
    /// A gold answer or answer key.
    AnswerKey,
    /// A label or relevance-judgment file.
    Label,
    /// Eval harness metadata, splits, or scoring configuration.
    EvalMetadata,
}

impl ArtifactKind {
    /// Returns whether this kind is allowed inside an evaluated corpus.
    #[must_use]
    pub const fn is_admissible_in_corpus(self) -> bool {
        matches!(self, Self::SourceDocument)
    }

    /// Returns the stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceDocument => "source_document",
            Self::Question => "question",
            Self::AnswerKey => "answer_key",
            Self::Label => "label",
            Self::EvalMetadata => "eval_metadata",
        }
    }
}

/// Retained provenance for one corpus object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProvenance {
    /// Where the object came from.
    pub source_uri: String,
    /// Immutable upstream revision or snapshot identifier.
    pub upstream_revision: String,
    /// When the object was captured.
    pub retrieved_at: DateTime<Utc>,
}

/// One object in an evaluated corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusObject {
    /// Stable object identifier.
    pub object_id: String,
    /// Declared artifact kind.
    pub declared_kind: ArtifactKind,
    /// Producer-declared lowercase SHA-256 of the object content; `None` fails closed.
    ///
    /// The scanner does not trust this value: it hashes [`Self::text`] itself and
    /// compares those bytes with the pinned manifest.
    pub content_sha256: Option<String>,
    /// Retained provenance; absent or blank identifiers fail closed.
    pub provenance: Option<SourceProvenance>,
    /// Object text used for duplicate and pairing analysis.
    pub text: String,
}

/// Which split an eval case belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSplit {
    /// Cases authors may inspect and tune against.
    Authoring,
    /// Cases that gate; never used to derive adaptive controls.
    GatedTest,
}

/// One eval case's scored text, used for leakage analysis only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCaseText {
    /// Stable case identifier.
    pub case_id: String,
    /// Split this case belongs to.
    pub split: CaseSplit,
    /// Question text.
    pub question: String,
    /// Gold answer text.
    pub answer: String,
}

/// Pinned allow-list of corpus object content hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedCorpus {
    /// Stable corpus identity.
    pub corpus_id: String,
    /// Allowed object id to lowercase SHA-256 mapping.
    pub object_hashes: BTreeMap<String, String>,
}

impl PinnedCorpus {
    /// Builds a pinned corpus from object id and hash pairs.
    #[must_use]
    pub fn new(
        corpus_id: impl Into<String>,
        hashes: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            corpus_id: corpus_id.into(),
            object_hashes: hashes.into_iter().collect(),
        }
    }
}

/// Thresholds for duplicate and pairing detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeakageThresholds {
    /// Shingle Jaccard above which two texts are near-duplicates.
    pub near_duplicate_jaccard: f64,
    /// Question-shingle containment above which an object restates a question.
    pub question_restatement_containment: f64,
    /// Answer-shingle containment above which an object is answer-bearing.
    pub answer_containment: f64,
}

impl Default for LeakageThresholds {
    fn default() -> Self {
        Self {
            near_duplicate_jaccard: DEFAULT_NEAR_DUPLICATE_JACCARD,
            question_restatement_containment: DEFAULT_QUESTION_RESTATEMENT_CONTAINMENT,
            answer_containment: DEFAULT_ANSWER_CONTAINMENT,
        }
    }
}

/// One leakage or provenance defect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum LeakageFinding {
    /// An object carries no content hash, so it cannot be pinned.
    MissingContentHash {
        /// Offending object.
        object_id: String,
    },
    /// A producer-declared hash does not describe the supplied object bytes.
    DeclaredContentHashMismatch {
        /// Offending object.
        object_id: String,
        /// Hash supplied by the corpus package.
        declared: String,
        /// Hash recomputed from the exact object bytes.
        actual: String,
    },
    /// An object carries no usable source provenance.
    MissingProvenance {
        /// Offending object.
        object_id: String,
    },
    /// An object is absent from the pinned allow-list.
    UnpinnedObject {
        /// Offending object.
        object_id: String,
    },
    /// An object's content hash differs from its pinned hash.
    ContentHashMismatch {
        /// Offending object.
        object_id: String,
        /// Hash the manifest pins.
        pinned: String,
        /// Hash the object reports.
        actual: String,
    },
    /// A pinned object never appeared in the scanned corpus.
    PinnedObjectMissing {
        /// Missing object.
        object_id: String,
    },
    /// An object declares an artifact kind a corpus may not contain.
    ForbiddenArtifactKind {
        /// Offending object.
        object_id: String,
        /// Declared kind.
        kind: ArtifactKind,
    },
    /// An object's text advertises itself as eval material.
    EvalArtifactMarker {
        /// Offending object.
        object_id: String,
        /// Marker phrase that matched.
        marker: String,
    },
    /// An object restates a question and also carries its gold answer.
    QuestionAnswerPairLeak {
        /// Offending object.
        object_id: String,
        /// Case whose pair leaked.
        case_id: String,
        /// Fraction of the question's shingles the object contains.
        question_containment: f64,
        /// Answer containment.
        answer_containment: f64,
    },
    /// An authoring case and a gated case are the same case.
    SplitDuplicate {
        /// Authoring-split case.
        authoring_case_id: String,
        /// Gated-split case.
        gated_case_id: String,
        /// Question similarity between the two.
        similarity: f64,
    },
    /// A corpus object legitimately covers a question's subject matter.
    ///
    /// Informational: this is what a working RAG corpus looks like.
    SourceDocumentOverlap {
        /// Overlapping object.
        object_id: String,
        /// Case the object can answer.
        case_id: String,
        /// Answer containment observed.
        answer_containment: f64,
    },
}

impl LeakageFinding {
    /// Returns whether this finding must fail the scan.
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        !matches!(self, Self::SourceDocumentOverlap { .. })
    }
}

/// Errors raised by lane classification and leakage scanning.
#[derive(Debug, Error, PartialEq)]
pub enum ContaminationError {
    /// A lane classification contradicts itself.
    #[error("lane `{lane}` has an invalid classification: {reason}")]
    InvalidClassification {
        /// Offending lane.
        lane: String,
        /// Why the classification is invalid.
        reason: String,
    },
    /// Blocking leakage or provenance findings were detected.
    #[error("corpus `{corpus_id}` failed leakage scanning with {} blocking finding(s): {summary}", findings.len())]
    LeakageDetected {
        /// Corpus that failed.
        corpus_id: String,
        /// Every blocking finding, retained for review.
        findings: Vec<LeakageFinding>,
        /// Short human summary.
        summary: String,
    },
}

/// Successful scan result, retaining informational findings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeakageScanReport {
    /// Corpus that was scanned.
    pub corpus_id: String,
    /// Objects scanned.
    pub objects_scanned: usize,
    /// Cases compared against the corpus.
    pub cases_scanned: usize,
    /// Non-blocking findings, kept so a reviewer sees what was allowed.
    pub informational: Vec<LeakageFinding>,
}

/// Fixed-corpus package leakage scanner.
#[derive(Debug, Clone, Default)]
pub struct LeakageScanner {
    thresholds: LeakageThresholds,
}

impl LeakageScanner {
    /// Creates a scanner with the default thresholds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a scanner with explicit thresholds.
    #[must_use]
    pub const fn with_thresholds(thresholds: LeakageThresholds) -> Self {
        Self { thresholds }
    }

    /// Scans a corpus against its pinned manifest and its eval cases.
    ///
    /// Fails closed: any blocking finding — including a missing hash or missing
    /// provenance — returns [`ContaminationError::LeakageDetected`] carrying
    /// every blocking finding, so nothing is scored against an unverified
    /// corpus.
    pub fn scan(
        &self,
        pinned: &PinnedCorpus,
        objects: &[CorpusObject],
        cases: &[EvalCaseText],
    ) -> Result<LeakageScanReport, ContaminationError> {
        let mut findings = Vec::new();
        let mut seen_objects = BTreeSet::new();
        let analyzed_cases = cases
            .iter()
            .map(|case| AnalyzedCase {
                case,
                question_shingles: shingles(&case.question),
                answer_shingles: shingles(&case.answer),
            })
            .collect::<Vec<_>>();

        for object in objects {
            seen_objects.insert(object.object_id.as_str());
            if !object.declared_kind.is_admissible_in_corpus() {
                findings.push(LeakageFinding::ForbiddenArtifactKind {
                    object_id: object.object_id.clone(),
                    kind: object.declared_kind,
                });
            }
            let actual = sha256_text(&object.text);
            match &object.content_sha256 {
                None => findings.push(LeakageFinding::MissingContentHash {
                    object_id: object.object_id.clone(),
                }),
                Some(declared) if declared != &actual => {
                    findings.push(LeakageFinding::DeclaredContentHashMismatch {
                        object_id: object.object_id.clone(),
                        declared: declared.clone(),
                        actual: actual.clone(),
                    });
                }
                Some(_) => {}
            }
            match pinned.object_hashes.get(&object.object_id) {
                None => findings.push(LeakageFinding::UnpinnedObject {
                    object_id: object.object_id.clone(),
                }),
                Some(expected) if expected != &actual => {
                    findings.push(LeakageFinding::ContentHashMismatch {
                        object_id: object.object_id.clone(),
                        pinned: expected.clone(),
                        actual,
                    });
                }
                Some(_) => {}
            }
            if object.provenance.as_ref().is_none_or(|provenance| {
                provenance.source_uri.trim().is_empty()
                    || provenance.upstream_revision.trim().is_empty()
            }) {
                findings.push(LeakageFinding::MissingProvenance {
                    object_id: object.object_id.clone(),
                });
            }
            if let Some(marker) = eval_artifact_marker(&object.text) {
                findings.push(LeakageFinding::EvalArtifactMarker {
                    object_id: object.object_id.clone(),
                    marker: marker.to_string(),
                });
            }
        }

        for object_id in pinned.object_hashes.keys() {
            if !seen_objects.contains(object_id.as_str()) {
                findings.push(LeakageFinding::PinnedObjectMissing {
                    object_id: object_id.clone(),
                });
            }
        }

        for object in objects {
            let object_shingles = shingles(&object.text);
            for analyzed in &analyzed_cases {
                let case = analyzed.case;
                let question_containment =
                    containment_shingles(&object_shingles, &analyzed.question_shingles);
                let answer_containment =
                    containment_shingles(&object_shingles, &analyzed.answer_shingles);
                let restates_question =
                    question_containment >= self.thresholds.question_restatement_containment;
                let carries_answer = answer_containment >= self.thresholds.answer_containment;
                if restates_question && carries_answer {
                    findings.push(LeakageFinding::QuestionAnswerPairLeak {
                        object_id: object.object_id.clone(),
                        case_id: case.case_id.clone(),
                        question_containment,
                        answer_containment,
                    });
                } else if carries_answer {
                    // Expected: a source document that answers the question.
                    findings.push(LeakageFinding::SourceDocumentOverlap {
                        object_id: object.object_id.clone(),
                        case_id: case.case_id.clone(),
                        answer_containment,
                    });
                }
            }
        }

        for authoring in analyzed_cases
            .iter()
            .filter(|case| case.case.split == CaseSplit::Authoring)
        {
            for gated in analyzed_cases
                .iter()
                .filter(|case| case.case.split == CaseSplit::GatedTest)
            {
                let similarity = jaccard(&authoring.question_shingles, &gated.question_shingles);
                if normalize(&authoring.case.question) == normalize(&gated.case.question)
                    || similarity >= self.thresholds.near_duplicate_jaccard
                {
                    findings.push(LeakageFinding::SplitDuplicate {
                        authoring_case_id: authoring.case.case_id.clone(),
                        gated_case_id: gated.case.case_id.clone(),
                        similarity,
                    });
                }
            }
        }

        let (blocking, informational): (Vec<_>, Vec<_>) =
            findings.into_iter().partition(LeakageFinding::is_blocking);
        if !blocking.is_empty() {
            let summary = blocking
                .iter()
                .take(3)
                .map(|finding| {
                    serde_json::to_string(finding).unwrap_or_else(|_| "unserializable".to_string())
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ContaminationError::LeakageDetected {
                corpus_id: pinned.corpus_id.clone(),
                findings: blocking,
                summary,
            });
        }
        Ok(LeakageScanReport {
            corpus_id: pinned.corpus_id.clone(),
            objects_scanned: objects.len(),
            cases_scanned: cases.len(),
            informational,
        })
    }
}

struct AnalyzedCase<'a> {
    case: &'a EvalCaseText,
    question_shingles: BTreeSet<String>,
    answer_shingles: BTreeSet<String>,
}

/// Phrases that identify an object as eval material rather than source content.
const EVAL_ARTIFACT_MARKERS: &[&str] = &[
    "answer key",
    "answer_key",
    "ground truth",
    "ground_truth",
    "gold label",
    "gold_label",
    "gold answer",
    "gold_answer",
    "relevance judgment",
    "qrels",
    "eval metadata",
    "eval_metadata",
    "test split",
    "held-out split",
];

fn eval_artifact_marker(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    EVAL_ARTIFACT_MARKERS
        .iter()
        .find(|marker| lowered.contains(**marker))
        .copied()
}

/// Normalizes text to lowercase alphanumeric tokens joined by single spaces.
#[must_use]
pub fn normalize(text: &str) -> String {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the [`SHINGLE_WIDTH`]-token shingles of a text.
///
/// Texts shorter than the shingle width fall back to single tokens so a short
/// question is still comparable.
#[must_use]
pub fn shingles(text: &str) -> BTreeSet<String> {
    let tokens = normalize(text)
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tokens.len() < SHINGLE_WIDTH {
        return tokens.into_iter().collect();
    }
    tokens
        .windows(SHINGLE_WIDTH)
        .map(|window| window.join(" "))
        .collect()
}

/// Returns the Jaccard similarity of two shingle sets.
#[must_use]
pub fn jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Returns the fraction of a needle's shingles present in a haystack.
#[must_use]
pub fn containment(haystack: &BTreeSet<String>, needle: &str) -> f64 {
    let needle_shingles = shingles(needle);
    containment_shingles(haystack, &needle_shingles)
}

fn containment_shingles(haystack: &BTreeSet<String>, needle_shingles: &BTreeSet<String>) -> f64 {
    if needle_shingles.is_empty() {
        return 0.0;
    }
    let present = needle_shingles
        .iter()
        .filter(|shingle| haystack.contains(*shingle))
        .count() as f64;
    present / needle_shingles.len() as f64
}

/// Returns the lowercase SHA-256 of exact UTF-8 text bytes.
///
/// Corpus adapters use the same function as [`LeakageScanner`] so declared and
/// recomputed digests cannot drift onto different hash algorithms.
#[must_use]
pub fn sha256_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn provenance() -> Option<SourceProvenance> {
        Some(SourceProvenance {
            source_uri: "https://example.test/kb/1".to_string(),
            upstream_revision: "rev-1".to_string(),
            retrieved_at: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
        })
    }

    fn source_object(object_id: &str, text: &str) -> CorpusObject {
        CorpusObject {
            object_id: object_id.to_string(),
            declared_kind: ArtifactKind::SourceDocument,
            content_sha256: Some(hash_of(text)),
            provenance: provenance(),
            text: text.to_string(),
        }
    }

    fn hash_of(text: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    fn pinned_for(objects: &[CorpusObject]) -> PinnedCorpus {
        PinnedCorpus::new(
            "kb-v1",
            objects.iter().map(|object| {
                (
                    object.object_id.clone(),
                    object.content_sha256.clone().unwrap_or_default(),
                )
            }),
        )
    }

    const LEGIT_ARTICLE: &str = "To rotate the signing key open the console, choose security, \
        then choose rotate. The rotation window is twenty four hours and the \
        rotation window cannot be shortened.";

    fn rotation_case(split: CaseSplit) -> EvalCaseText {
        EvalCaseText {
            case_id: "case-rotate".to_string(),
            split,
            question: "How long is the signing key rotation window?".to_string(),
            answer: "The rotation window is twenty four hours".to_string(),
        }
    }

    #[test]
    fn a_legitimate_source_document_near_match_passes() {
        // Pins: a source document that answers the question is what a working
        // RAG corpus looks like; it is recorded as overlap, not leakage.
        let objects = vec![source_object("kb-1", LEGIT_ARTICLE)];
        let report = LeakageScanner::new()
            .scan(
                &pinned_for(&objects),
                &objects,
                &[rotation_case(CaseSplit::GatedTest)],
            )
            .expect("a legitimate source document must pass");

        assert_eq!(report.objects_scanned, 1);
        assert_eq!(
            report.informational,
            vec![LeakageFinding::SourceDocumentOverlap {
                object_id: "kb-1".to_string(),
                case_id: "case-rotate".to_string(),
                answer_containment: 1.0,
            }]
        );
    }

    #[test]
    fn a_seeded_answer_key_leak_fails_closed() {
        // Pins: an object that restates the question and carries the gold answer
        // is leakage even though it is declared a source document.
        let leak = "How long is the signing key rotation window? \
            The rotation window is twenty four hours";
        let objects = vec![
            source_object("kb-1", LEGIT_ARTICLE),
            source_object("kb-2", leak),
        ];
        let error = LeakageScanner::new()
            .scan(
                &pinned_for(&objects),
                &objects,
                &[rotation_case(CaseSplit::GatedTest)],
            )
            .expect_err("a seeded answer-key leak must fail");

        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LeakageFinding::QuestionAnswerPairLeak { object_id, case_id, .. }
                    if object_id == "kb-2" && case_id == "case-rotate"
            )),
            "findings {findings:?}"
        );
    }

    #[test]
    fn a_declared_answer_key_artifact_is_refused_outright() {
        // Pins: kind alone is enough; the scanner does not need to prove pairing
        // when the package admits what the object is.
        let mut object = source_object("labels", "arbitrary");
        object.declared_kind = ArtifactKind::AnswerKey;
        let objects = vec![object];
        let error = LeakageScanner::new()
            .scan(&pinned_for(&objects), &objects, &[])
            .expect_err("declared answer key must fail");

        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert_eq!(
            findings,
            &vec![LeakageFinding::ForbiddenArtifactKind {
                object_id: "labels".to_string(),
                kind: ArtifactKind::AnswerKey,
            }]
        );
    }

    #[test]
    fn eval_metadata_text_markers_are_detected_inside_source_documents() {
        // Pins: accidentally indexed harness metadata trips the scan even when
        // the package declares it as ordinary source material.
        let objects = vec![source_object(
            "kb-1",
            "internal notes: ground truth for the rotation question is 24h",
        )];
        let error = LeakageScanner::new()
            .scan(&pinned_for(&objects), &objects, &[])
            .expect_err("marker must fail");

        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert_eq!(
            findings,
            &vec![LeakageFinding::EvalArtifactMarker {
                object_id: "kb-1".to_string(),
                marker: "ground truth".to_string(),
            }]
        );
    }

    #[test]
    fn missing_source_hashes_and_provenance_fail_closed() {
        // Pins: an unverifiable object never gets scored.
        let mut object = source_object("kb-1", LEGIT_ARTICLE);
        let pinned = pinned_for(&[object.clone()]);
        object.content_sha256 = None;
        object.provenance = None;
        let error = LeakageScanner::new()
            .scan(&pinned, &[object], &[])
            .expect_err("missing hash must fail");

        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert!(findings.contains(&LeakageFinding::MissingContentHash {
            object_id: "kb-1".to_string()
        }));
        assert!(findings.contains(&LeakageFinding::MissingProvenance {
            object_id: "kb-1".to_string()
        }));
    }

    #[test]
    fn blank_source_provenance_fails_closed() {
        // Pins: a structurally present provenance record cannot bypass the
        // scanner without identifying both its source and immutable revision.
        let mut object = source_object("kb-1", LEGIT_ARTICLE);
        object.provenance = Some(SourceProvenance {
            source_uri: "  ".to_string(),
            upstream_revision: String::new(),
            retrieved_at: Utc::now(),
        });
        let error = LeakageScanner::new()
            .scan(&pinned_for(std::slice::from_ref(&object)), &[object], &[])
            .expect_err("blank provenance must fail");

        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert_eq!(
            findings,
            &vec![LeakageFinding::MissingProvenance {
                object_id: "kb-1".to_string(),
            }]
        );
    }

    #[test]
    fn an_unpinned_or_mutated_object_fails_closed() {
        // Pins: content that is not exactly what the manifest pinned cannot be
        // silently evaluated.
        let objects = vec![source_object("kb-1", LEGIT_ARTICLE)];
        let unpinned = PinnedCorpus::new("kb-v1", [("kb-other".to_string(), hash_of("x"))]);
        let error = LeakageScanner::new()
            .scan(&unpinned, &objects, &[])
            .expect_err("unpinned must fail");
        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert!(findings.contains(&LeakageFinding::UnpinnedObject {
            object_id: "kb-1".to_string()
        }));
        assert!(findings.contains(&LeakageFinding::PinnedObjectMissing {
            object_id: "kb-other".to_string()
        }));

        let mut mutated = objects.clone();
        mutated[0].content_sha256 = Some(hash_of("tampered"));
        let error = LeakageScanner::new()
            .scan(&pinned_for(&objects), &mutated, &[])
            .expect_err("mutation must fail");
        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LeakageFinding::DeclaredContentHashMismatch { .. }
            ))
        );
    }

    #[test]
    fn changed_text_with_a_reused_claimed_hash_fails_closed() {
        // Pins: the scanner hashes the bytes it receives. A package cannot keep
        // an old trusted digest beside changed text and bypass corpus pinning.
        let original = source_object("kb-1", LEGIT_ARTICLE);
        let pinned = pinned_for(std::slice::from_ref(&original));
        let mut tampered = original;
        tampered.text.push_str(" silently changed");

        let error = LeakageScanner::new()
            .scan(&pinned, &[tampered], &[])
            .expect_err("changed bytes with the old claimed hash must fail");
        let ContaminationError::LeakageDetected { findings, .. } = error else {
            panic!("expected leakage, got {error}");
        };
        assert!(findings.iter().any(|finding| matches!(
            finding,
            LeakageFinding::ContentHashMismatch { object_id, actual, .. }
                if object_id == "kb-1" && actual == &hash_of(&format!("{LEGIT_ARTICLE} silently changed"))
        )));
    }

    #[test]
    fn an_exact_or_near_duplicate_across_splits_fails_closed() {
        // Pins: a gated case that also sits in the authoring split is a
        // train/test leak, whether copied verbatim or lightly edited.
        let objects = vec![source_object("kb-1", "unrelated text about billing")];
        let cases = vec![
            rotation_case(CaseSplit::Authoring),
            EvalCaseText {
                case_id: "gated-1".to_string(),
                split: CaseSplit::GatedTest,
                question: "How long is the signing key rotation window?".to_string(),
                answer: "twenty four hours".to_string(),
            },
        ];
        let error = LeakageScanner::new()
            .scan(&pinned_for(&objects), &objects, &cases)
            .expect_err("split duplicate must fail");
        let ContaminationError::LeakageDetected { findings, .. } = &error else {
            panic!("expected leakage, got {error}");
        };
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                LeakageFinding::SplitDuplicate { gated_case_id, .. } if gated_case_id == "gated-1"
            )),
            "findings {findings:?}"
        );
    }

    #[test]
    fn distinct_split_questions_are_not_flagged_as_duplicates() {
        // Pins: the duplicate detector is not a blanket topical-overlap alarm.
        let objects = vec![source_object("kb-1", "unrelated text about billing")];
        let cases = vec![
            rotation_case(CaseSplit::Authoring),
            EvalCaseText {
                case_id: "gated-2".to_string(),
                split: CaseSplit::GatedTest,
                question: "Which console page lists retained audit exports?".to_string(),
                answer: "the compliance page".to_string(),
            },
        ];

        LeakageScanner::new()
            .scan(&pinned_for(&objects), &objects, &cases)
            .expect("distinct questions must pass");
    }

    #[test]
    fn similarity_helpers_behave_at_their_edges() {
        // Pins: the primitives the whole scanner rests on. Two empty texts are not
        // "identical", a short text still produces comparable tokens, and
        // containment is directional.
        assert_eq!(jaccard(&shingles(""), &shingles("")), 0.0);
        assert_eq!(
            jaccard(&shingles("alpha beta gamma"), &shingles("alpha beta gamma")),
            1.0
        );
        assert_eq!(
            jaccard(&shingles("alpha beta gamma"), &shingles("x y z")),
            0.0
        );

        assert_eq!(normalize("  Alpha, BETA!  gamma "), "alpha beta gamma");
        assert_eq!(
            shingles("alpha beta").len(),
            2,
            "short text falls back to tokens"
        );
        assert_eq!(shingles("alpha beta gamma delta").len(), 2);

        let haystack = shingles("the rotation window is twenty four hours long");
        assert_eq!(containment(&haystack, "rotation window is"), 1.0);
        assert_eq!(containment(&haystack, ""), 0.0);
        assert!(containment(&haystack, "billing invoices are issued monthly") < 0.5);
    }

    #[test]
    fn contamination_wire_labels_are_stable() {
        // Pins: the labels persisted classification and finding reports are read by.
        assert_eq!(
            LaneClass::FixedCorpusRetrieval.as_str(),
            "fixed_corpus_retrieval"
        );
        assert_eq!(
            LaneClass::PublicAnswerGeneration.as_str(),
            "public_answer_generation"
        );
        assert_eq!(
            LaneClass::ClosedFixtureSuite.as_str(),
            "closed_fixture_suite"
        );
        assert_eq!(ArtifactKind::SourceDocument.as_str(), "source_document");
        assert_eq!(ArtifactKind::Question.as_str(), "question");
        assert_eq!(ArtifactKind::AnswerKey.as_str(), "answer_key");
        assert_eq!(ArtifactKind::Label.as_str(), "label");
        assert_eq!(ArtifactKind::EvalMetadata.as_str(), "eval_metadata");
    }

    #[test]
    fn only_source_documents_are_admissible_and_only_overlap_is_advisory() {
        // Pins: both predicates that decide whether a finding blocks a run.
        assert!(ArtifactKind::SourceDocument.is_admissible_in_corpus());
        for kind in [
            ArtifactKind::Question,
            ArtifactKind::AnswerKey,
            ArtifactKind::Label,
            ArtifactKind::EvalMetadata,
        ] {
            assert!(!kind.is_admissible_in_corpus(), "{kind:?}");
        }

        assert!(
            !LeakageFinding::SourceDocumentOverlap {
                object_id: "kb-1".to_string(),
                case_id: "case-1".to_string(),
                answer_containment: 1.0,
            }
            .is_blocking()
        );
        assert!(
            LeakageFinding::MissingProvenance {
                object_id: "kb-1".to_string()
            }
            .is_blocking()
        );
        assert!(LaneClass::FixedCorpusRetrieval.requires_pinned_corpus());
        assert!(LaneClass::PublicAnswerGeneration.requires_pinned_corpus());
        assert!(!LaneClass::ClosedFixtureSuite.requires_pinned_corpus());
    }

    #[test]
    fn a_consistent_classification_validates() {
        // Pins: validate is not a constant error.
        LaneClassification {
            lane: "closed",
            class: LaneClass::FixedCorpusRetrieval,
            network_denied: true,
            rationale: "seeded corpus",
        }
        .validate()
        .expect("a consistent classification validates");

        assert!(matches!(
            LaneClassification {
                lane: "closed",
                class: LaneClass::FixedCorpusRetrieval,
                network_denied: true,
                rationale: "  ",
            }
            .validate(),
            Err(ContaminationError::InvalidClassification { .. })
        ));
    }

    #[test]
    fn a_closed_corpus_lane_must_deny_network() {
        // Pins: classification contradictions are refused instead of documented.
        let inconsistent = LaneClassification {
            lane: "closed",
            class: LaneClass::FixedCorpusRetrieval,
            network_denied: false,
            rationale: "seeded corpus",
        };
        assert!(matches!(
            inconsistent.validate(),
            Err(ContaminationError::InvalidClassification { .. })
        ));
    }
}
