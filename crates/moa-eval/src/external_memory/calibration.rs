//! Strict, deterministic human calibration contracts for external-memory judges.
//!
//! Two layers live here. [`judge`] holds the judge-agnostic calibration manifest
//! and reporting primitives — identity and staleness, splits, confusion matrices
//! with intervals, kappa with uncertainty, per-stratum recall, selective
//! accuracy, aggregate bias correction, and the per-task authority requirement.
//! The rest of this module is the external-memory absolute judge's own domain
//! contract: its exact 70-case stratified selection, its blinded two-labeler plus
//! adjudicator artifacts, and its byte-exact V1 results wire format.
//!
//! The layering matters because the two answer different questions. The V1
//! results artifact records *what was measured* on a specific sample and is
//! hash-pinned, so it never changes shape. [`judge::JudgeAuthorityRequirement`]
//! records *what would have to be true* for those measurements to make a metric
//! authoritative, and that is per-task policy: a ten-case stratum cannot support
//! the same recall bound as a thousand-case one, so a single `kappa >= 0.80`
//! point threshold applied everywhere would be either unreachable or vacuous.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::{ExternalMemoryError, Result};

/// Calibration wire schema version.
pub const CALIBRATION_SCHEMA_VERSION: u32 = 1;
/// Stable seed string recorded in every calibration manifest.
pub const CALIBRATION_SELECTION_SEED: &str = "moa-longmemeval-calibration-v1";
/// Exact number of cases selected for human calibration.
pub const CALIBRATION_SAMPLE_SIZE: usize = 70;

const CASES_PER_STRATUM: usize = 10;
const SELECTION_DOMAIN: &[u8] = b"moa-longmemeval-calibration-v1\0";
const MANIFEST_HASH_DOMAIN: &[u8] = b"moa.external-memory.calibration.manifest.v1\0";
const RESULTS_HASH_DOMAIN: &[u8] = b"moa.external-memory.calibration.results.v1\0";
const IDENTITY_HASH_DOMAIN: &[u8] = b"moa.external-memory.calibration.identity.v1\0";
const AGREEMENT_THRESHOLD: f64 = 0.90;
const KAPPA_THRESHOLD: f64 = 0.80;
const ACCURACY_THRESHOLD: f64 = 0.85;

pub mod judge {
    //! Judge-agnostic calibration manifest and reporting primitives.
    //!
    //! A model judge is a measuring instrument. These types are what it takes to
    //! claim an instrument is calibrated, kept separate on purpose:
    //!
    //! * [`JudgeIdentity`] — the exact model, prompt text, rubric, output parser,
    //!   and domain a calibration was measured on, reduced to one fingerprint.
    //!   [`calibration_expiry`] refuses to carry a calibration across a change to
    //!   any of them, or across a material shift in class prevalence.
    //! * [`AgreementReliability`] — how well two *humans* agreed with each other.
    //!   This bounds what any judge could be measured against; it says nothing
    //!   about whether the judge is right.
    //! * [`JudgeValidity`] — how well the *judge* agreed with the adjudicated gold
    //!   label, on an untouched [`LabelSplit::Validation`] split, reported per
    //!   class and per stratum with intervals.
    //! * [`correct_aggregate_rate`] — the separate act of correcting an aggregate
    //!   score for the judge's measured sensitivity and specificity, propagating
    //!   both the calibration-set and the evaluation-set uncertainty.
    //!
    //! Conflating the last three is the usual failure. A high human-human kappa is
    //! routinely quoted as if it validated the judge; a judge accuracy measured on
    //! the same cases used to pick the prompt is routinely quoted as if it were
    //! held out; and a corrected aggregate is routinely quoted with only the
    //! evaluation set's uncertainty, which understates it.
    //!
    //! Nothing here creates a judge or makes one authoritative on its own.
    //! [`JudgeAuthorityRequirement::evaluate`] is a per-task declaration, and
    //! [`apply_judge_authority`] is the single point where a judge-derived metric
    //! either keeps its decision or is downgraded for lacking calibration.
    //!
    //! These primitives are judge-agnostic but currently live under the
    //! external-memory module because that is where the only calibrated judge in
    //! the workspace lives. They depend on nothing in this module beyond the
    //! shared binary [`CalibrationLabel`] and the error type.

    use std::collections::BTreeMap;

    use moa_eval_core::decision::{Decision, MetricDecision};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use super::{CalibrationLabel, exact_sha256, invalid, validate_hash};
    use crate::external_memory::Result;

    /// Wire schema version for judge-calibration reporting types.
    pub const JUDGE_CALIBRATION_SCHEMA_VERSION: u32 = 1;
    /// Two-sided 95% standard-normal quantile used for every reported interval.
    pub const NORMAL_QUANTILE_TWO_SIDED_95: f64 = 1.959_963_984_540_054_4;
    /// Absolute change in positive-class prevalence that expires a calibration.
    ///
    /// Sensitivity and specificity are conditional on the true class, but every
    /// aggregate derived from them is not: a judge calibrated at 20% positives and
    /// applied at 60% positives is being asked a different question. Ten points is
    /// the declared line for "material" rather than a measured constant.
    pub const MATERIAL_PREVALENCE_SHIFT: f64 = 0.10;

    const IDENTITY_FINGERPRINT_DOMAIN: &[u8] = b"moa.eval.judge.identity.v1\0";

    /// Exact identity of the judge a calibration was measured on.
    ///
    /// Every field is part of the instrument. A prompt edit, a rubric edit, a new
    /// output parser, or the same prompt pointed at a different domain all produce
    /// a different instrument whose previous calibration no longer describes it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct JudgeIdentity {
        /// Exact model identifier that produced the decisions.
        pub model: String,
        /// Human-facing prompt version label.
        pub prompt_version: String,
        /// SHA-256 of the exact prompt text bytes.
        pub prompt_sha256: String,
        /// SHA-256 of the exact rubric text bytes.
        pub rubric_sha256: String,
        /// Version of the parser that turned model output into a label.
        pub output_parser_version: String,
        /// Task domain the judge was calibrated on.
        pub domain: String,
    }

    impl JudgeIdentity {
        /// Builds a validated judge identity, hashing the exact prompt and rubric bytes.
        ///
        /// # Errors
        ///
        /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`]
        /// when any label is blank or either text is empty.
        pub fn new(
            model: &str,
            prompt_version: &str,
            prompt: &[u8],
            rubric: &[u8],
            output_parser_version: &str,
            domain: &str,
        ) -> Result<Self> {
            if prompt.is_empty() || rubric.is_empty() {
                return Err(invalid(
                    "judge identity requires exact prompt and rubric text",
                ));
            }
            let identity = Self {
                model: model.to_string(),
                prompt_version: prompt_version.to_string(),
                prompt_sha256: exact_sha256(prompt),
                rubric_sha256: exact_sha256(rubric),
                output_parser_version: output_parser_version.to_string(),
                domain: domain.to_string(),
            };
            identity.validate()?;
            Ok(identity)
        }

        /// Validates that every identity component is present and well formed.
        ///
        /// # Errors
        ///
        /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`]
        /// when a label is blank or a digest is not a lowercase 64-hex SHA-256.
        pub fn validate(&self) -> Result<()> {
            for (field, value) in [
                ("model", self.model.as_str()),
                ("prompt_version", self.prompt_version.as_str()),
                ("output_parser_version", self.output_parser_version.as_str()),
                ("domain", self.domain.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(invalid(format!("judge identity {field} must not be blank")));
                }
            }
            validate_hash("prompt_sha256", &self.prompt_sha256)?;
            validate_hash("rubric_sha256", &self.rubric_sha256)?;
            Ok(())
        }

        /// Returns the domain-separated fingerprint of the complete identity.
        ///
        /// Two judges share a fingerprint only when every identity component
        /// matches, so a calibration can be bound to an instrument by one value.
        #[must_use]
        pub fn fingerprint(&self) -> String {
            let mut hasher = Sha256::new();
            hasher.update(IDENTITY_FINGERPRINT_DOMAIN);
            for field in [
                self.model.as_str(),
                self.prompt_version.as_str(),
                self.prompt_sha256.as_str(),
                self.rubric_sha256.as_str(),
                self.output_parser_version.as_str(),
                self.domain.as_str(),
            ] {
                hasher.update(field.as_bytes());
                hasher.update([0_u8]);
            }
            format!("{:x}", hasher.finalize())
        }
    }

    /// Why a calibration no longer describes the judge in use.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ExpiryReason {
        /// A different model answered.
        Model,
        /// The prompt version or its exact text changed.
        Prompt,
        /// The rubric text changed.
        Rubric,
        /// The output parser changed, so the same text can yield a different label.
        OutputParser,
        /// The judge was applied to a different domain.
        Domain,
        /// Positive-class prevalence moved by at least [`MATERIAL_PREVALENCE_SHIFT`].
        ClassDistribution,
    }

    impl ExpiryReason {
        /// Returns the stable wire spelling.
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Model => "model",
                Self::Prompt => "prompt",
                Self::Rubric => "rubric",
                Self::OutputParser => "output_parser",
                Self::Domain => "domain",
                Self::ClassDistribution => "class_distribution",
            }
        }
    }

    /// Whether a calibration still applies to the judge and data in use.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CalibrationExpiry {
        /// The calibrated instrument and the instrument in use are the same.
        Current,
        /// At least one identity component or the class distribution changed.
        Expired {
            /// Every reason that fired, in declaration order.
            reasons: Vec<ExpiryReason>,
        },
    }

    impl CalibrationExpiry {
        /// Returns whether the calibration still applies.
        #[must_use]
        pub fn is_current(&self) -> bool {
            matches!(self, Self::Current)
        }
    }

    /// Reports every reason a calibration has expired, or that it is current.
    ///
    /// The prevalence arguments are the positive-class rate the calibration was
    /// measured at and the rate observed on the data being scored. A non-finite
    /// prevalence is treated as a material shift rather than ignored.
    #[must_use]
    pub fn calibration_expiry(
        calibrated: &JudgeIdentity,
        in_use: &JudgeIdentity,
        calibrated_prevalence: f64,
        observed_prevalence: f64,
    ) -> CalibrationExpiry {
        let mut reasons = Vec::new();
        if calibrated.model != in_use.model {
            reasons.push(ExpiryReason::Model);
        }
        if calibrated.prompt_version != in_use.prompt_version
            || calibrated.prompt_sha256 != in_use.prompt_sha256
        {
            reasons.push(ExpiryReason::Prompt);
        }
        if calibrated.rubric_sha256 != in_use.rubric_sha256 {
            reasons.push(ExpiryReason::Rubric);
        }
        if calibrated.output_parser_version != in_use.output_parser_version {
            reasons.push(ExpiryReason::OutputParser);
        }
        if calibrated.domain != in_use.domain {
            reasons.push(ExpiryReason::Domain);
        }
        let prevalence_shift = observed_prevalence - calibrated_prevalence;
        if !prevalence_shift.is_finite() || prevalence_shift.abs() >= MATERIAL_PREVALENCE_SHIFT {
            reasons.push(ExpiryReason::ClassDistribution);
        }
        if reasons.is_empty() {
            CalibrationExpiry::Current
        } else {
            CalibrationExpiry::Expired { reasons }
        }
    }

    /// Which half of a labeled sample a measurement was taken on.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum LabelSplit {
        /// Cases used to choose the prompt, rubric, or parser.
        ///
        /// Any accuracy measured here is a selection statistic, not validity.
        Calibration,
        /// Cases untouched by any selection decision.
        Validation,
    }

    /// How an interval was constructed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IntervalMethod {
        /// Wilson score interval for a binomial proportion.
        WilsonScore,
        /// Normal interval on the asymptotic kappa variance.
        AsymptoticKappaNormal,
        /// Corner evaluation over the inputs' intervals.
        IntervalArithmetic,
    }

    /// A proportion with its exact counts and a two-sided 95% interval.
    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ProportionEstimate {
        /// Successes observed.
        pub numerator: u64,
        /// Trials observed.
        pub denominator: u64,
        /// Observed proportion.
        pub point: f64,
        /// Lower interval bound.
        pub lower: f64,
        /// Upper interval bound.
        pub upper: f64,
        /// Interval construction used.
        pub method: IntervalMethod,
    }

    /// Builds a Wilson-score proportion estimate, or `None` for an empty denominator.
    ///
    /// The Wilson interval is used rather than the Wald interval because these
    /// denominators are small — ten cases in a stratum — where Wald coverage
    /// collapses and reports a zero-width interval at zero or full success.
    #[must_use]
    pub fn proportion_estimate(numerator: u64, denominator: u64) -> Option<ProportionEstimate> {
        if denominator == 0 || numerator > denominator {
            return None;
        }
        let n = denominator as f64;
        let point = numerator as f64 / n;
        let z = NORMAL_QUANTILE_TWO_SIDED_95;
        let denominator_term = 1.0 + z * z / n;
        let center = (point + z * z / (2.0 * n)) / denominator_term;
        let half_width =
            z * (point * (1.0 - point) / n + z * z / (4.0 * n * n)).sqrt() / denominator_term;
        Some(ProportionEstimate {
            numerator,
            denominator,
            point,
            lower: (center - half_width).clamp(0.0, 1.0),
            upper: (center + half_width).clamp(0.0, 1.0),
            method: IntervalMethod::WilsonScore,
        })
    }

    /// A two-by-two agreement table between a reference rater and a second rater.
    ///
    /// Rows are the reference (gold, or the first human), columns are the rater
    /// being assessed (the judge, or the second human). `Correct` is the positive
    /// class. Used for both roles on purpose: the arithmetic is identical, and the
    /// difference between "two humans agreed" and "the judge was right" lives in
    /// which report the table is attached to, not in the table.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ConfusionMatrix {
        /// Reference positive, rater positive.
        pub true_positive: u64,
        /// Reference positive, rater negative.
        pub false_negative: u64,
        /// Reference negative, rater positive.
        pub false_positive: u64,
        /// Reference negative, rater negative.
        pub true_negative: u64,
    }

    impl ConfusionMatrix {
        /// Counts `(reference, rater)` label pairs into a table.
        #[must_use]
        pub fn from_pairs(
            pairs: impl IntoIterator<Item = (CalibrationLabel, CalibrationLabel)>,
        ) -> Self {
            let mut matrix = Self::default();
            for (reference, rater) in pairs {
                match (reference, rater) {
                    (CalibrationLabel::Correct, CalibrationLabel::Correct) => {
                        matrix.true_positive += 1;
                    }
                    (CalibrationLabel::Correct, CalibrationLabel::Incorrect) => {
                        matrix.false_negative += 1;
                    }
                    (CalibrationLabel::Incorrect, CalibrationLabel::Correct) => {
                        matrix.false_positive += 1;
                    }
                    (CalibrationLabel::Incorrect, CalibrationLabel::Incorrect) => {
                        matrix.true_negative += 1;
                    }
                }
            }
            matrix
        }

        /// Returns the total labeled pairs in the table.
        #[must_use]
        pub const fn total(&self) -> u64 {
            self.true_positive + self.false_negative + self.false_positive + self.true_negative
        }

        /// Returns the reference-positive count.
        #[must_use]
        pub const fn reference_positives(&self) -> u64 {
            self.true_positive + self.false_negative
        }

        /// Returns the reference-negative count.
        #[must_use]
        pub const fn reference_negatives(&self) -> u64 {
            self.false_positive + self.true_negative
        }

        /// Returns the positive-class prevalence in the reference labels.
        #[must_use]
        pub fn prevalence(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.reference_positives(), self.total())
        }

        /// Returns raw agreement, the fraction of pairs where both labels matched.
        #[must_use]
        pub fn raw_agreement(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.true_positive + self.true_negative, self.total())
        }

        /// Returns recall on the positive class.
        #[must_use]
        pub fn sensitivity(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.true_positive, self.reference_positives())
        }

        /// Returns recall on the negative class.
        #[must_use]
        pub fn specificity(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.true_negative, self.reference_negatives())
        }

        /// Returns precision on the positive class.
        #[must_use]
        pub fn precision(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.true_positive, self.true_positive + self.false_positive)
        }

        /// Returns precision on the negative class.
        #[must_use]
        pub fn negative_predictive_value(&self) -> Option<ProportionEstimate> {
            proportion_estimate(self.true_negative, self.true_negative + self.false_negative)
        }

        /// Returns class-specific recall for one class.
        ///
        /// Reported per class rather than pooled because a judge that never says
        /// `Incorrect` scores well on a mostly-correct sample while being useless
        /// at the only decision that matters.
        #[must_use]
        pub fn class_recall(&self, class: CalibrationLabel) -> Option<ProportionEstimate> {
            match class {
                CalibrationLabel::Correct => self.sensitivity(),
                CalibrationLabel::Incorrect => self.specificity(),
            }
        }
    }

    /// Cohen's kappa with its asymptotic standard error and interval.
    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct KappaEstimate {
        /// Point estimate.
        pub point: f64,
        /// Asymptotic standard error.
        pub standard_error: f64,
        /// Lower interval bound.
        pub lower: f64,
        /// Upper interval bound.
        pub upper: f64,
        /// Interval construction used.
        pub method: IntervalMethod,
    }

    /// Human-human reliability: what any judge could be measured against.
    ///
    /// A judge cannot be shown to agree with a gold label more reliably than two
    /// humans agree with each other, so this report bounds the whole exercise. It
    /// is deliberately a separate type from [`JudgeValidity`] so a high kappa here
    /// can never be quoted as evidence that the judge is right.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct AgreementReliability {
        /// Wire schema version.
        pub schema_version: u32,
        /// First-labeler rows against second-labeler columns.
        pub matrix: ConfusionMatrix,
        /// Observed agreement with its interval.
        pub raw_agreement: Option<ProportionEstimate>,
        /// Chance-expected agreement from the observed marginals.
        pub expected_agreement: f64,
        /// Kappa with uncertainty, `None` when `1 - expected_agreement` is zero.
        pub kappa: Option<KappaEstimate>,
    }

    /// Measures human-human reliability from paired blinded labels.
    #[must_use]
    pub fn human_agreement(matrix: ConfusionMatrix) -> AgreementReliability {
        let (expected_agreement, kappa) = cohen_kappa(&matrix);
        AgreementReliability {
            schema_version: JUDGE_CALIBRATION_SCHEMA_VERSION,
            matrix,
            raw_agreement: matrix.raw_agreement(),
            expected_agreement,
            kappa,
        }
    }

    /// Returns chance agreement and Cohen's kappa with its asymptotic interval.
    ///
    /// The variance is the Fleiss-Cohen-Everitt large-sample form
    ///
    /// ```text
    /// var = 1 / (n (1 - pe)^4) * [ SUM_i  p_ii [(1 - pe) - (p_i. + p_.i)(1 - po)]^2
    ///                            + (1 - po)^2 SUM_{i!=j} p_ij (p_.i + p_j.)^2
    ///                            - (po pe - 2 pe + po)^2 ]
    /// ```
    ///
    /// which agrees with the delta method on the multinomial cell counts and
    /// collapses to zero at perfect agreement. The `(1 - pe)^2` denominator that
    /// circulates alongside it understates the standard error by a factor of
    /// `1 / (1 - pe)`, which is more than two at typical marginals.
    ///
    /// Reporting kappa without this interval is the specific failure this function
    /// exists to prevent: at seventy pairs the standard error is around 0.07, so a
    /// point estimate of 0.80 is not distinguishable from 0.66.
    #[must_use]
    pub fn cohen_kappa(matrix: &ConfusionMatrix) -> (f64, Option<KappaEstimate>) {
        let total = matrix.total();
        if total == 0 {
            return (0.0, None);
        }
        let n = total as f64;
        // Row 0 / column 0 are the positive class.
        let cells = [
            [
                matrix.true_positive as f64 / n,
                matrix.false_negative as f64 / n,
            ],
            [
                matrix.false_positive as f64 / n,
                matrix.true_negative as f64 / n,
            ],
        ];
        let row = [cells[0][0] + cells[0][1], cells[1][0] + cells[1][1]];
        let column = [cells[0][0] + cells[1][0], cells[0][1] + cells[1][1]];
        let observed = cells[0][0] + cells[1][1];
        let expected = row[0] * column[0] + row[1] * column[1];
        let slack = 1.0 - expected;
        if slack == 0.0 {
            return (expected, None);
        }
        let point = (observed - expected) / slack;

        let mut diagonal_term = 0.0;
        let mut off_diagonal_term = 0.0;
        for i in 0..2 {
            for j in 0..2 {
                if i == j {
                    let inner = slack - (row[i] + column[i]) * (1.0 - observed);
                    diagonal_term += cells[i][i] * inner * inner;
                } else {
                    let inner = column[i] + row[j];
                    off_diagonal_term += cells[i][j] * inner * inner;
                }
            }
        }
        let trailing = observed * expected - 2.0 * expected + observed;
        let variance = (diagonal_term + (1.0 - observed) * (1.0 - observed) * off_diagonal_term
            - trailing * trailing)
            / (n * slack * slack * slack * slack);
        let standard_error = variance.max(0.0).sqrt();
        let half_width = NORMAL_QUANTILE_TWO_SIDED_95 * standard_error;
        (
            expected,
            Some(KappaEstimate {
                point,
                standard_error,
                lower: (point - half_width).max(-1.0),
                upper: (point + half_width).min(1.0),
                method: IntervalMethod::AsymptoticKappaNormal,
            }),
        )
    }

    /// One judged case with its adjudicated gold label.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct JudgeLabelPair {
        /// Stable case identity.
        pub case_id: String,
        /// Reporting stratum this case belongs to.
        pub stratum: String,
        /// Adjudicated gold label.
        pub gold: CalibrationLabel,
        /// Judge label, or `None` when the judge abstained or failed to parse.
        pub judge: Option<CalibrationLabel>,
    }

    /// Coverage and accuracy-on-covered-cases when a judge may abstain.
    ///
    /// A judge that abstains on everything hard reports a flattering accuracy on
    /// what is left. Coverage is therefore reported next to it, and abstentions
    /// are excluded from the confusion matrix rather than scored as wrong.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SelectivePerformance {
        /// Cases the judge produced a label for.
        pub covered: u64,
        /// Cases offered to the judge.
        pub total: u64,
        /// Covered over total.
        pub coverage: f64,
        /// Accuracy restricted to covered cases.
        pub selective_accuracy: Option<ProportionEstimate>,
    }

    /// One stratum's judge validity.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct StratumValidity {
        /// Stratum identity.
        pub stratum: String,
        /// Covered cases in this stratum.
        pub matrix: ConfusionMatrix,
        /// Positive-class prevalence in this stratum.
        pub prevalence: Option<ProportionEstimate>,
        /// Recall on the positive class within this stratum.
        pub positive_recall: Option<ProportionEstimate>,
        /// Recall on the negative class within this stratum.
        pub negative_recall: Option<ProportionEstimate>,
        /// The weaker of the two class recalls, by interval lower bound.
        ///
        /// Reported per stratum because that is exactly what a pooled accuracy
        /// hides. An abstention stratum is almost entirely `Correct`, so its
        /// sensitivity is uninformative while its specificity is the whole point;
        /// taking the weaker class avoids having to guess which one that is, and
        /// avoids an arbitrary tie-break when the two classes are balanced.
        pub weakest_class_recall: Option<ProportionEstimate>,
    }

    /// Judge-versus-adjudicated-gold validity for one instrument and split.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct JudgeValidity {
        /// Wire schema version.
        pub schema_version: u32,
        /// Instrument the measurement describes.
        pub identity: JudgeIdentity,
        /// Split the measurement was taken on.
        pub split: LabelSplit,
        /// Covered cases across every stratum.
        pub overall: ConfusionMatrix,
        /// Positive-class prevalence in the gold labels.
        pub prevalence: Option<ProportionEstimate>,
        /// Raw judge-gold agreement.
        pub raw_agreement: Option<ProportionEstimate>,
        /// Positive-class recall.
        pub sensitivity: Option<ProportionEstimate>,
        /// Negative-class recall.
        pub specificity: Option<ProportionEstimate>,
        /// Positive-class precision.
        pub precision: Option<ProportionEstimate>,
        /// Negative-class precision.
        pub negative_predictive_value: Option<ProportionEstimate>,
        /// Per-stratum breakdown in stratum order.
        pub strata: Vec<StratumValidity>,
        /// Stratum with the lowest rare-class recall lower bound.
        pub worst_stratum: Option<String>,
        /// That stratum's rare-class recall.
        pub worst_stratum_recall: Option<ProportionEstimate>,
        /// Coverage and selective accuracy.
        pub selective: SelectivePerformance,
    }

    impl JudgeValidity {
        /// Measures validity from adjudicated pairs.
        ///
        /// # Errors
        ///
        /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`]
        /// when the identity is invalid, when there are no pairs, when a case
        /// identity repeats, or when a stratum label is blank.
        pub fn measure(
            identity: JudgeIdentity,
            split: LabelSplit,
            pairs: &[JudgeLabelPair],
        ) -> Result<Self> {
            identity.validate()?;
            if pairs.is_empty() {
                return Err(invalid("judge validity requires at least one labeled case"));
            }
            let mut seen = std::collections::HashSet::new();
            let mut by_stratum: BTreeMap<&str, Vec<(CalibrationLabel, CalibrationLabel)>> =
                BTreeMap::new();
            let mut covered_pairs = Vec::with_capacity(pairs.len());
            for pair in pairs {
                if pair.case_id.trim().is_empty() || pair.stratum.trim().is_empty() {
                    return Err(invalid(
                        "judge validity case identity and stratum must not be blank",
                    ));
                }
                if !seen.insert(pair.case_id.as_str()) {
                    return Err(invalid(format!(
                        "judge validity repeats case `{}`",
                        pair.case_id
                    )));
                }
                let entry = by_stratum.entry(pair.stratum.as_str()).or_default();
                if let Some(judge) = pair.judge {
                    entry.push((pair.gold, judge));
                    covered_pairs.push((pair.gold, judge));
                }
            }

            let overall = ConfusionMatrix::from_pairs(covered_pairs.iter().copied());
            let strata = by_stratum
                .into_iter()
                .map(|(stratum, pairs)| {
                    let matrix = ConfusionMatrix::from_pairs(pairs);
                    StratumValidity {
                        stratum: stratum.to_string(),
                        matrix,
                        prevalence: matrix.prevalence(),
                        positive_recall: matrix.sensitivity(),
                        negative_recall: matrix.specificity(),
                        weakest_class_recall: weakest_class_recall(&matrix),
                    }
                })
                .collect::<Vec<_>>();
            let worst = strata
                .iter()
                .filter_map(|stratum| {
                    stratum
                        .weakest_class_recall
                        .map(|recall| (stratum.stratum.clone(), recall))
                })
                .min_by(|left, right| left.1.lower.total_cmp(&right.1.lower));

            let total = pairs.len() as u64;
            let covered = overall.total();
            Ok(Self {
                schema_version: JUDGE_CALIBRATION_SCHEMA_VERSION,
                identity,
                split,
                overall,
                prevalence: overall.prevalence(),
                raw_agreement: overall.raw_agreement(),
                sensitivity: overall.sensitivity(),
                specificity: overall.specificity(),
                precision: overall.precision(),
                negative_predictive_value: overall.negative_predictive_value(),
                strata,
                worst_stratum: worst.as_ref().map(|(stratum, _)| stratum.clone()),
                worst_stratum_recall: worst.map(|(_, recall)| recall),
                selective: SelectivePerformance {
                    covered,
                    total,
                    coverage: covered as f64 / total as f64,
                    selective_accuracy: overall.raw_agreement(),
                },
            })
        }
    }

    /// Returns the weaker of the two class recalls, by interval lower bound.
    ///
    /// A class with no reference cases has no recall to report, so a
    /// single-class stratum yields the recall of the class it does contain rather
    /// than `None`.
    fn weakest_class_recall(matrix: &ConfusionMatrix) -> Option<ProportionEstimate> {
        [
            matrix.class_recall(CalibrationLabel::Correct),
            matrix.class_recall(CalibrationLabel::Incorrect),
        ]
        .into_iter()
        .flatten()
        .min_by(|left, right| {
            left.lower
                .total_cmp(&right.lower)
                .then_with(|| left.point.total_cmp(&right.point))
        })
    }

    /// An aggregate rate corrected for the judge's measured error rates.
    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct AggregateBiasCorrection {
        /// Judge-positive rate observed on the evaluation set.
        pub apparent: ProportionEstimate,
        /// Sensitivity measured on the calibration set.
        pub sensitivity: ProportionEstimate,
        /// Specificity measured on the calibration set.
        pub specificity: ProportionEstimate,
        /// Corrected point estimate.
        pub point: f64,
        /// Lower bound over the joint uncertainty.
        pub lower: f64,
        /// Upper bound over the joint uncertainty.
        pub upper: f64,
        /// `sensitivity + specificity - 1` at the point estimates.
        pub youden_j: f64,
        /// Interval construction used.
        pub method: IntervalMethod,
    }

    /// Corrects an apparent rate for measured judge sensitivity and specificity.
    ///
    /// This is the Rogan-Gladen estimator
    ///
    /// ```text
    /// true_rate = (apparent + specificity - 1) / (sensitivity + specificity - 1)
    /// ```
    ///
    /// The interval is taken over the corners of all three input intervals, so it
    /// carries the calibration set's uncertainty about the judge as well as the
    /// evaluation set's uncertainty about the apparent rate. Reporting only the
    /// latter understates the total, sometimes badly: seventy calibration cases
    /// leave sensitivity uncertain to several points, and that uncertainty divides
    /// the whole estimate.
    ///
    /// # Errors
    ///
    /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`] when
    /// the denominator's lower bound is not strictly positive. A judge whose
    /// interval admits `sensitivity + specificity <= 1` is not distinguishable
    /// from coin flipping, and the corrected value is then unbounded rather than
    /// merely wide.
    pub fn correct_aggregate_rate(
        apparent: ProportionEstimate,
        sensitivity: ProportionEstimate,
        specificity: ProportionEstimate,
    ) -> Result<AggregateBiasCorrection> {
        let youden_j = sensitivity.point + specificity.point - 1.0;
        if sensitivity.lower + specificity.lower - 1.0 <= 0.0 {
            return Err(invalid(format!(
                "aggregate bias correction is undefined: sensitivity+specificity-1 lower bound is \
                 {:.6}, so the corrected rate is unbounded",
                sensitivity.lower + specificity.lower - 1.0
            )));
        }
        let rogan_gladen = |apparent: f64, sensitivity: f64, specificity: f64| {
            ((apparent + specificity - 1.0) / (sensitivity + specificity - 1.0)).clamp(0.0, 1.0)
        };
        let mut lower = f64::INFINITY;
        let mut upper = f64::NEG_INFINITY;
        for apparent_bound in [apparent.lower, apparent.upper] {
            for sensitivity_bound in [sensitivity.lower, sensitivity.upper] {
                for specificity_bound in [specificity.lower, specificity.upper] {
                    let corner = rogan_gladen(apparent_bound, sensitivity_bound, specificity_bound);
                    lower = lower.min(corner);
                    upper = upper.max(corner);
                }
            }
        }
        Ok(AggregateBiasCorrection {
            apparent,
            sensitivity,
            specificity,
            point: rogan_gladen(apparent.point, sensitivity.point, specificity.point),
            lower,
            upper,
            youden_j,
            method: IntervalMethod::IntervalArithmetic,
        })
    }

    /// A slice whose judge behavior must be reported before authority is granted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum JudgeSlice {
        /// Candidate answers carrying instructions aimed at the judge.
        PromptInjection,
        /// The same comparison with the presentation order swapped.
        PositionSwap,
        /// The minority reference class in isolation.
        RareClass,
        /// A domain the judge was not calibrated on.
        CrossDomain,
    }

    impl JudgeSlice {
        /// Every slice a calibrated judge must report.
        pub const ALL: [Self; 4] = [
            Self::PromptInjection,
            Self::PositionSwap,
            Self::RareClass,
            Self::CrossDomain,
        ];

        /// Returns the stable wire spelling.
        #[must_use]
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::PromptInjection => "prompt_injection",
                Self::PositionSwap => "position_swap",
                Self::RareClass => "rare_class",
                Self::CrossDomain => "cross_domain",
            }
        }
    }

    /// Held-out validity measured for one required judge-behavior slice.
    ///
    /// This replaces caller-asserted slice labels with the measurement the
    /// authority gate actually needs. Construction always measures on the
    /// validation split, so a required slice cannot be satisfied by naming it
    /// without supplying labeled cases.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct JudgeSliceValidity {
        /// Slice whose cases were measured.
        slice: JudgeSlice,
        /// Held-out judge-versus-gold validity for this slice.
        validity: JudgeValidity,
    }

    impl JudgeSliceValidity {
        /// Measures one authority slice against adjudicated gold labels.
        ///
        /// # Errors
        ///
        /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`]
        /// when [`JudgeValidity::measure`] refuses the identity or labeled cases.
        pub fn measure(
            slice: JudgeSlice,
            identity: JudgeIdentity,
            pairs: &[JudgeLabelPair],
        ) -> Result<Self> {
            Ok(Self {
                slice,
                validity: JudgeValidity::measure(identity, LabelSplit::Validation, pairs)?,
            })
        }

        /// Returns the measured slice.
        #[must_use]
        pub const fn slice(&self) -> JudgeSlice {
            self.slice
        }

        /// Returns the held-out validity measurement.
        #[must_use]
        pub const fn validity(&self) -> &JudgeValidity {
            &self.validity
        }
    }

    /// What one task requires before a judge-derived metric may be authoritative.
    ///
    /// Every bound is per task on purpose. A shared `kappa >= 0.80` point
    /// threshold is either unreachable or vacuous depending on the sample: with
    /// ten cases in a stratum, the Wilson lower bound on a flawless ten-for-ten
    /// recall is only 0.72, so a 0.80 bound on that stratum can never be met by
    /// any judge, however perfect. Declaring the numbers next to the design that
    /// has to meet them is the only way they stay meaningful.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct JudgeAuthorityRequirement {
        /// Task this declaration governs.
        pub task: String,
        /// Minimum untouched validation cases the judge must have been scored on.
        pub min_validation_cases: u64,
        /// Minimum human-human raw agreement point estimate.
        pub min_human_raw_agreement: f64,
        /// Minimum human-human kappa *lower bound*, not point estimate.
        pub min_kappa_lower_bound: f64,
        /// Minimum judge sensitivity lower bound.
        pub min_sensitivity_lower_bound: f64,
        /// Minimum judge specificity lower bound.
        pub min_specificity_lower_bound: f64,
        /// Minimum weakest-class recall *point estimate* in the worst stratum.
        ///
        /// A point estimate carries the weight here because a small stratum's
        /// interval cannot. With a three-case minority class, a flawless recall
        /// still has a Wilson lower bound of only 0.44, so any lower-bound floor
        /// above that would be unmeetable by construction rather than by judge
        /// quality. Both are declared: the point floor is the real bar and the
        /// lower-bound floor only rules out a stratum whose interval is so wide it
        /// says nothing.
        pub min_worst_stratum_recall_point: f64,
        /// Minimum weakest-class recall lower bound in the worst stratum.
        pub min_worst_stratum_recall_lower_bound: f64,
        /// Minimum fraction of cases the judge must label rather than abstain on.
        pub min_coverage: f64,
        /// Slices whose behavior must be reported.
        pub required_slices: Vec<JudgeSlice>,
    }

    impl JudgeAuthorityRequirement {
        /// Returns whether the declared error-rate floors keep bias correction defined.
        ///
        /// [`correct_aggregate_rate`] refuses when the sensitivity and specificity
        /// lower bounds cannot exceed chance. A requirement whose own floors admit
        /// that case would grant authority to a judge whose aggregates cannot be
        /// corrected, so the declaration is checked rather than assumed.
        #[must_use]
        pub fn keeps_bias_correction_defined(&self) -> bool {
            self.min_sensitivity_lower_bound + self.min_specificity_lower_bound > 1.0
        }

        /// Grants or refuses metric authority, naming every unmet condition.
        #[must_use]
        pub fn evaluate(&self, evidence: &JudgeAuthorityEvidence<'_>) -> JudgeAuthority {
            let mut reasons = Vec::new();
            let in_use_fingerprint = evidence.in_use.fingerprint();
            let calibrated_fingerprint = evidence.calibrated.fingerprint();
            if in_use_fingerprint != calibrated_fingerprint {
                reasons.push(
                    "the judge in use is not the judge the calibration was measured on".to_string(),
                );
            }
            if evidence.validity.identity.fingerprint() != calibrated_fingerprint {
                reasons.push(
                    "judge validity was measured on a different judge than the calibration"
                        .to_string(),
                );
            }
            if let CalibrationExpiry::Expired { reasons: expired } = calibration_expiry(
                evidence.calibrated,
                evidence.in_use,
                evidence.calibrated_prevalence,
                evidence.observed_prevalence,
            ) {
                reasons.push(format!(
                    "calibration expired: {}",
                    expired
                        .iter()
                        .map(|reason| reason.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if evidence.validity.split != LabelSplit::Validation {
                reasons.push(
                    "judge validity was measured on the calibration split, which is a selection \
                     statistic rather than held-out validity"
                        .to_string(),
                );
            }
            if evidence.validity.selective.total < self.min_validation_cases {
                reasons.push(format!(
                    "validation split has {} cases, {} required",
                    evidence.validity.selective.total, self.min_validation_cases
                ));
            }
            check_point(
                &mut reasons,
                "human-human raw agreement",
                evidence.reliability.raw_agreement.map(|value| value.point),
                self.min_human_raw_agreement,
            );
            match evidence.reliability.kappa {
                None => reasons.push(
                    "human-human kappa is undefined, so reliability was never established"
                        .to_string(),
                ),
                Some(kappa) => check_point(
                    &mut reasons,
                    "human-human kappa lower bound",
                    Some(kappa.lower),
                    self.min_kappa_lower_bound,
                ),
            }
            check_point(
                &mut reasons,
                "judge sensitivity lower bound",
                evidence.validity.sensitivity.map(|value| value.lower),
                self.min_sensitivity_lower_bound,
            );
            check_point(
                &mut reasons,
                "judge specificity lower bound",
                evidence.validity.specificity.map(|value| value.lower),
                self.min_specificity_lower_bound,
            );
            check_point(
                &mut reasons,
                "worst-stratum weakest-class recall",
                evidence
                    .validity
                    .worst_stratum_recall
                    .map(|value| value.point),
                self.min_worst_stratum_recall_point,
            );
            check_point(
                &mut reasons,
                "worst-stratum weakest-class recall lower bound",
                evidence
                    .validity
                    .worst_stratum_recall
                    .map(|value| value.lower),
                self.min_worst_stratum_recall_lower_bound,
            );
            check_point(
                &mut reasons,
                "judge coverage",
                Some(evidence.validity.selective.coverage),
                self.min_coverage,
            );
            for required in &self.required_slices {
                let mut measured = evidence
                    .slice_validity
                    .iter()
                    .filter(|measurement| measurement.slice == *required);
                let Some(measurement) = measured.next() else {
                    reasons.push(format!("unmeasured required slice: {}", required.as_str()));
                    continue;
                };
                if measured.next().is_some() {
                    reasons.push(format!(
                        "required slice was measured more than once: {}",
                        required.as_str()
                    ));
                    continue;
                }
                let validity = &measurement.validity;
                if validity.identity.fingerprint() != in_use_fingerprint {
                    reasons.push(format!(
                        "{} slice validity was measured on a different judge",
                        required.as_str()
                    ));
                }
                if validity.split != LabelSplit::Validation {
                    reasons.push(format!(
                        "{} slice validity was not measured on the held-out validation split",
                        required.as_str()
                    ));
                }
                check_point(
                    &mut reasons,
                    &format!("{} slice coverage", required.as_str()),
                    Some(validity.selective.coverage),
                    self.min_coverage,
                );
                check_point(
                    &mut reasons,
                    &format!("{} slice weakest-class recall", required.as_str()),
                    validity.worst_stratum_recall.map(|value| value.point),
                    self.min_worst_stratum_recall_point,
                );
                check_point(
                    &mut reasons,
                    &format!(
                        "{} slice weakest-class recall lower bound",
                        required.as_str()
                    ),
                    validity.worst_stratum_recall.map(|value| value.lower),
                    self.min_worst_stratum_recall_lower_bound,
                );
            }
            if reasons.is_empty() {
                JudgeAuthority::Authoritative {
                    task: self.task.clone(),
                    judge_fingerprint: in_use_fingerprint,
                }
            } else {
                JudgeAuthority::Informational { reasons }
            }
        }
    }

    /// Everything [`JudgeAuthorityRequirement::evaluate`] reads.
    #[derive(Debug, Clone, Copy)]
    pub struct JudgeAuthorityEvidence<'a> {
        /// Judge that produced the decisions being reported.
        pub in_use: &'a JudgeIdentity,
        /// Judge the calibration was measured on.
        pub calibrated: &'a JudgeIdentity,
        /// Positive-class prevalence on the calibration sample.
        pub calibrated_prevalence: f64,
        /// Positive-class prevalence on the data being scored.
        pub observed_prevalence: f64,
        /// Human-human reliability.
        pub reliability: &'a AgreementReliability,
        /// Judge-versus-gold validity.
        pub validity: &'a JudgeValidity,
        /// Held-out measurements for required behavior slices.
        pub slice_validity: &'a [JudgeSliceValidity],
    }

    /// Whether a judge's output may back an authoritative metric.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum JudgeAuthority {
        /// Every declared condition held.
        Authoritative {
            /// Task the authority was granted for.
            task: String,
            /// Fingerprint of the judge the grant is bound to.
            judge_fingerprint: String,
        },
        /// At least one condition did not hold; the output is informational.
        Informational {
            /// Every unmet condition.
            reasons: Vec<String>,
        },
    }

    impl JudgeAuthority {
        /// Returns whether the judge may back an authoritative metric.
        #[must_use]
        pub fn is_authoritative(&self) -> bool {
            matches!(self, Self::Authoritative { .. })
        }

        /// Returns the unmet conditions, empty when authoritative.
        #[must_use]
        pub fn reasons(&self) -> &[String] {
            match self {
                Self::Authoritative { .. } => &[],
                Self::Informational { reasons } => reasons,
            }
        }
    }

    /// Downgrades a judge-derived metric decision that lacks calibration authority.
    ///
    /// An uncalibrated or stale judge does not produce a weaker `PASS`; it
    /// produces no population claim at all, so the decision becomes
    /// [`Decision::Inconclusive`] and the rationale names why. Applying this at
    /// the metric boundary is what makes "an uncalibrated judge cannot produce an
    /// authoritative metric" a property of the code rather than of the process.
    #[must_use]
    pub fn apply_judge_authority(
        mut decision: MetricDecision,
        authority: &JudgeAuthority,
    ) -> MetricDecision {
        if authority.is_authoritative() {
            return decision;
        }
        decision.rationale = format!(
            "judge-derived metric is not authoritative: {}",
            authority.reasons().join("; ")
        );
        decision.decision = Decision::Inconclusive;
        decision.regression_p_value = None;
        decision
    }

    /// Shape of a semantic judge's prompt.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PromptShape {
        /// One structured multi-label call.
        Holistic,
        /// One isolated call per dimension.
        Decomposed,
    }

    /// Which prompt shape a held-out comparison selected, and why.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct PromptShapeDecision {
        /// Selected shape.
        pub chosen: PromptShape,
        /// Statement of the measured comparison behind the choice.
        pub rationale: String,
    }

    /// Chooses between a holistic and a decomposed judge prompt on held-out data.
    ///
    /// Decomposition costs one provider call per dimension, so it has to earn its
    /// keep: it is selected only when its accuracy interval clears the holistic
    /// point estimate, which is a measured gain rather than a difference inside
    /// the noise. The default is the single structured call.
    ///
    /// # Errors
    ///
    /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`] when
    /// either measurement came from [`LabelSplit::Calibration`], when the two were
    /// measured on different case counts, or when either has no accuracy at all.
    /// Deciding on the split that selected the prompts is the mistake this refuses.
    pub fn decide_prompt_shape(
        holistic: &JudgeValidity,
        decomposed: &JudgeValidity,
    ) -> Result<PromptShapeDecision> {
        for validity in [holistic, decomposed] {
            if validity.split != LabelSplit::Validation {
                return Err(invalid(
                    "prompt-shape selection requires held-out validation measurements, not the \
                     calibration split that chose the prompts",
                ));
            }
        }
        if holistic.selective.total != decomposed.selective.total {
            return Err(invalid(
                "prompt-shape comparison requires both shapes on the same held-out case set",
            ));
        }
        let holistic_accuracy = holistic
            .raw_agreement
            .ok_or_else(|| invalid("holistic judge produced no scored cases"))?;
        let decomposed_accuracy = decomposed
            .raw_agreement
            .ok_or_else(|| invalid("decomposed judge produced no scored cases"))?;
        if decomposed_accuracy.lower > holistic_accuracy.point {
            Ok(PromptShapeDecision {
                chosen: PromptShape::Decomposed,
                rationale: format!(
                    "decomposed accuracy lower bound {:.4} exceeds holistic point {:.4} on {} \
                     held-out cases",
                    decomposed_accuracy.lower, holistic_accuracy.point, holistic.selective.total
                ),
            })
        } else {
            Ok(PromptShapeDecision {
                chosen: PromptShape::Holistic,
                rationale: format!(
                    "decomposed accuracy lower bound {:.4} does not exceed holistic point {:.4}, \
                     so the extra per-dimension calls are not justified",
                    decomposed_accuracy.lower, holistic_accuracy.point
                ),
            })
        }
    }

    /// Authorization a live, billed calibration run must present.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct LiveCalibrationRequest {
        /// Whether an operator explicitly asked for a live run.
        pub explicitly_enabled: bool,
        /// Whether judge credentials resolved.
        pub credentials_present: bool,
        /// Authorized budget for the run.
        pub budget_usd: f64,
    }

    /// Admits a live calibration run only on an explicit, funded, credentialed request.
    ///
    /// The default request is refused, which is what "ignored by default" has to
    /// mean in code: a run that forgot to ask for live calibration gets no live
    /// calls rather than a surprise bill.
    ///
    /// # Errors
    ///
    /// Returns [`crate::external_memory::ExternalMemoryError::InvalidConfig`]
    /// naming every missing precondition.
    pub fn admit_live_calibration(request: &LiveCalibrationRequest) -> Result<()> {
        let mut missing = Vec::new();
        if !request.explicitly_enabled {
            missing.push("an explicit live-calibration flag");
        }
        if !request.credentials_present {
            missing.push("resolved judge credentials");
        }
        if !(request.budget_usd.is_finite() && request.budget_usd > 0.0) {
            missing.push("a positive authorized budget");
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(invalid(format!(
                "live judge calibration is ignored by default; it requires {}",
                missing.join(", ")
            )))
        }
    }

    fn check_point(reasons: &mut Vec<String>, label: &str, observed: Option<f64>, minimum: f64) {
        match observed {
            None => reasons.push(format!("{label} was not measured")),
            Some(value) if !value.is_finite() => {
                reasons.push(format!("{label} is not a finite number"));
            }
            Some(value) if value < minimum => {
                reasons.push(format!("{label} is {value:.4}, {minimum:.4} required"));
            }
            Some(_) => {}
        }
    }
}

/// One ordered calibration stratum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationStratum {
    /// A newer fact replaces earlier information.
    KnowledgeUpdate,
    /// Evidence spans multiple sessions.
    MultiSession,
    /// Answer evidence came from the assistant.
    SingleSessionAssistant,
    /// The answer is a personalized preference rubric.
    SingleSessionPreference,
    /// Answer evidence came from the user.
    SingleSessionUser,
    /// The answer requires temporal reasoning.
    TemporalReasoning,
    /// An `_abs` question whose correct behavior is abstention.
    Abstention,
}

impl CalibrationStratum {
    /// All strata in the required calibration concatenation order.
    pub const ALL: [Self; 7] = [
        Self::KnowledgeUpdate,
        Self::MultiSession,
        Self::SingleSessionAssistant,
        Self::SingleSessionPreference,
        Self::SingleSessionUser,
        Self::TemporalReasoning,
        Self::Abstention,
    ];

    /// Returns the exact wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KnowledgeUpdate => "knowledge-update",
            Self::MultiSession => "multi-session",
            Self::SingleSessionAssistant => "single-session-assistant",
            Self::SingleSessionPreference => "single-session-preference",
            Self::SingleSessionUser => "single-session-user",
            Self::TemporalReasoning => "temporal-reasoning",
            Self::Abstention => "abstention",
        }
    }
}

impl fmt::Display for CalibrationStratum {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Report-and-dataset projection needed to prepare and score one calibration case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationSourceCase {
    /// Stable upstream question ID.
    pub question_id: String,
    /// Ordered calibration stratum.
    pub stratum: CalibrationStratum,
    /// Exact question shown to both blinded labelers.
    pub question: String,
    /// Exact reference answer shown to both blinded labelers.
    pub reference_answer: String,
    /// Primary-mode reader answer, when the reader completed.
    pub candidate_answer: Option<String>,
    /// Primary-mode reader failure kind, when no candidate exists.
    pub reader_failure_kind: Option<String>,
    /// Primary-mode absolute-judge binary decision; `None` means no valid judge outcome.
    pub judge_outcome: Option<bool>,
}

/// One ordered manifest selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationSampleItemV1 {
    /// Stable upstream question ID.
    pub question_id: String,
    /// Calibration stratum.
    pub stratum: CalibrationStratum,
}

/// Strict, self-hashed calibration selection manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationManifestV1 {
    /// Wire schema version.
    pub schema_version: u32,
    /// Exact dataset registry identifier.
    pub dataset: String,
    /// Immutable dataset revision.
    pub dataset_revision: String,
    /// SHA-256 of the exact package file bytes supplied to `prepare`.
    pub package_sha256: String,
    /// SHA-256 of the exact report bytes supplied to `prepare`.
    pub report_sha256: String,
    /// Versioned deterministic selection seed.
    pub selection_seed: String,
    /// Exactly 70 unique ordered selections.
    pub sample: Vec<CalibrationSampleItemV1>,
    /// Domain-separated canonical self-hash.
    pub manifest_sha256: String,
}

impl CalibrationManifestV1 {
    /// Validates the complete manifest, including deterministic ordering and its self-hash.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CALIBRATION_SCHEMA_VERSION
            || self.dataset != "longmemeval-s-cleaned"
            || self.dataset_revision.trim().is_empty()
            || self.selection_seed != CALIBRATION_SELECTION_SEED
        {
            return Err(invalid("invalid calibration manifest metadata"));
        }
        validate_hash("package_sha256", &self.package_sha256)?;
        validate_hash("report_sha256", &self.report_sha256)?;
        validate_hash("manifest_sha256", &self.manifest_sha256)?;
        validate_sample(&self.sample)?;
        let expected = canonical_self_hash(MANIFEST_HASH_DOMAIN, self, "manifest_sha256")?;
        if self.manifest_sha256 != expected {
            return Err(invalid(format!(
                "calibration manifest SHA-256 mismatch: expected {expected}, got {}",
                self.manifest_sha256
            )));
        }
        Ok(())
    }
}

/// Binary human or judge correctness label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationLabel {
    /// The candidate answer is correct.
    Correct,
    /// The candidate answer is incorrect.
    Incorrect,
}

impl CalibrationLabel {
    const fn bit(self) -> usize {
        match self {
            Self::Correct => 1,
            Self::Incorrect => 0,
        }
    }

    const fn from_judge(value: bool) -> Self {
        if value {
            Self::Correct
        } else {
            Self::Incorrect
        }
    }
}

/// Role carried by a manual calibration artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationRole {
    /// First blinded labeler.
    LabelerA,
    /// Second blinded labeler.
    LabelerB,
    /// Adjudicator who resolves the final gold label.
    Adjudicator,
}

/// Lifecycle status of a blinded label artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationArtifactStatus {
    /// Prepared template with no identity or labels.
    Template,
    /// Completed artifact with one identity hash and every label.
    Completed,
}

/// One blinded label item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationLabelItemV1 {
    /// Stable upstream question ID.
    pub question_id: String,
    /// Calibration stratum.
    pub stratum: CalibrationStratum,
    /// Exact question.
    pub question: String,
    /// Exact reference answer.
    pub reference_answer: String,
    /// Primary reader answer, or null on reader failure.
    pub candidate_answer: Option<String>,
    /// Stable reader failure kind, or null when a candidate exists.
    pub reader_failure_kind: Option<String>,
    /// Human binary label, null only in a template.
    pub label: Option<CalibrationLabel>,
}

/// Strict blinded labeler artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationLabelArtifactV1 {
    /// Wire schema version.
    pub schema_version: u32,
    /// Exact manifest self-hash.
    pub manifest_sha256: String,
    /// Labeler role.
    pub role: CalibrationRole,
    /// Template or completed status.
    pub status: CalibrationArtifactStatus,
    /// Domain-separated identity hash; null in templates.
    pub identity_sha256: Option<String>,
    /// Exactly 70 ordered label items.
    pub items: Vec<CalibrationLabelItemV1>,
}

/// One adjudicated gold label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationAdjudicationItemV1 {
    /// Stable upstream question ID.
    pub question_id: String,
    /// Final adjudicated label.
    pub label: CalibrationLabel,
}

/// Strict ordered adjudication artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationAdjudicationV1 {
    /// Wire schema version.
    pub schema_version: u32,
    /// Exact manifest self-hash.
    pub manifest_sha256: String,
    /// Must be `adjudicator`.
    pub role: CalibrationRole,
    /// Domain-separated adjudicator identity hash.
    pub identity_sha256: String,
    /// Exactly 70 labels in manifest order.
    pub labels: Vec<CalibrationAdjudicationItemV1>,
}

/// Whether Cohen's kappa had a defined denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KappaStatus {
    /// Kappa is a finite number.
    Defined,
    /// Both marginals made `1 - p_e` exactly zero.
    UndefinedZeroDenominator,
}

/// Conjunctive calibration gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationVerdict {
    /// Every threshold passed.
    Pass,
    /// At least one threshold failed or was undefined.
    Fail,
}

impl CalibrationVerdict {
    /// Returns the exact wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// Strict, self-hashed judge calibration results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationResultsV1 {
    /// Wire schema version.
    pub schema_version: u32,
    /// Manifest self-hash.
    pub manifest_sha256: String,
    /// SHA-256 of the exact report input bytes.
    pub report_sha256: String,
    /// SHA-256 of exact labeler-A file bytes.
    pub labeler_a_sha256: String,
    /// SHA-256 of exact labeler-B file bytes.
    pub labeler_b_sha256: String,
    /// SHA-256 of exact adjudication file bytes.
    pub adjudication_sha256: String,
    /// A=incorrect, B=incorrect pairs.
    pub n00: usize,
    /// A=incorrect, B=correct pairs.
    pub n01: usize,
    /// A=correct, B=incorrect pairs.
    pub n10: usize,
    /// A=correct, B=correct pairs.
    pub n11: usize,
    /// Exact human-pair denominator.
    pub pair_denominator: usize,
    /// Observed human agreement.
    pub agreement: f64,
    /// Kappa denominator status.
    pub kappa_status: KappaStatus,
    /// Unweighted Cohen's kappa, null when undefined.
    pub kappa: Option<f64>,
    /// Valid judge outcomes exactly matching adjudicated labels.
    pub judge_correct_count: usize,
    /// Exact judge denominator.
    pub judge_denominator: usize,
    /// Judge accuracy over the complete sample.
    pub judge_accuracy: f64,
    /// Whether agreement reached 0.90.
    pub agreement_pass: bool,
    /// Whether defined kappa reached 0.80.
    pub kappa_pass: bool,
    /// Whether judge accuracy reached 0.85.
    pub accuracy_pass: bool,
    /// Conjunctive gate verdict.
    pub verdict: CalibrationVerdict,
    /// Domain-separated canonical self-hash.
    pub results_sha256: String,
}

impl CalibrationResultsV1 {
    /// Validates hashes, denominators, metrics, threshold flags, and the canonical self-hash.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CALIBRATION_SCHEMA_VERSION {
            return Err(invalid("invalid calibration results schema version"));
        }
        if !self.agreement.is_finite()
            || !self.judge_accuracy.is_finite()
            || self.kappa.is_some_and(|value| !value.is_finite())
        {
            return Err(invalid("calibration result metrics must be finite"));
        }
        for (name, digest) in [
            ("manifest_sha256", &self.manifest_sha256),
            ("report_sha256", &self.report_sha256),
            ("labeler_a_sha256", &self.labeler_a_sha256),
            ("labeler_b_sha256", &self.labeler_b_sha256),
            ("adjudication_sha256", &self.adjudication_sha256),
            ("results_sha256", &self.results_sha256),
        ] {
            validate_hash(name, digest)?;
        }
        if self.pair_denominator != CALIBRATION_SAMPLE_SIZE
            || self.judge_denominator != CALIBRATION_SAMPLE_SIZE
            || self.n00 + self.n01 + self.n10 + self.n11 != CALIBRATION_SAMPLE_SIZE
            || self.judge_correct_count > CALIBRATION_SAMPLE_SIZE
        {
            return Err(invalid("invalid calibration result denominators"));
        }
        let recomputed = compute_metrics(
            self.n00,
            self.n01,
            self.n10,
            self.n11,
            self.judge_correct_count,
        );
        if self.agreement != recomputed.agreement
            || self.kappa_status != recomputed.kappa_status
            || self.kappa != recomputed.kappa
            || self.judge_accuracy != recomputed.judge_accuracy
            || self.agreement_pass != recomputed.agreement_pass
            || self.kappa_pass != recomputed.kappa_pass
            || self.accuracy_pass != recomputed.accuracy_pass
            || self.verdict != recomputed.verdict
        {
            return Err(invalid("calibration result metrics are inconsistent"));
        }
        let expected = canonical_self_hash(RESULTS_HASH_DOMAIN, self, "results_sha256")?;
        if self.results_sha256 != expected {
            return Err(invalid(format!(
                "calibration results SHA-256 mismatch: expected {expected}, got {}",
                self.results_sha256
            )));
        }
        Ok(())
    }
}

/// Prepared manifest and two blinded templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCalibrationV1 {
    /// Self-hashed selection manifest.
    pub manifest: CalibrationManifestV1,
    /// First blinded labeler template.
    pub labeler_a: CalibrationLabelArtifactV1,
    /// Second blinded labeler template.
    pub labeler_b: CalibrationLabelArtifactV1,
}

/// Selects exactly ten deterministic question IDs per ordered stratum.
pub fn select_question_ids(
    cases: &[CalibrationSourceCase],
) -> Result<Vec<CalibrationSampleItemV1>> {
    validate_source_cases(cases)?;
    let mut selected = Vec::with_capacity(CALIBRATION_SAMPLE_SIZE);
    for stratum in CalibrationStratum::ALL {
        let mut candidates = cases
            .iter()
            .filter(|case| case.stratum == stratum)
            .map(|case| {
                let mut hasher = Sha256::new();
                hasher.update(SELECTION_DOMAIN);
                hasher.update(case.question_id.as_bytes());
                (format!("{:x}", hasher.finalize()), case.question_id.clone())
            })
            .collect::<Vec<_>>();
        if candidates.len() < CASES_PER_STRATUM {
            return Err(invalid(format!(
                "calibration stratum {stratum} requires at least {CASES_PER_STRATUM} cases"
            )));
        }
        candidates.sort_unstable();
        selected.extend(
            candidates
                .into_iter()
                .take(CASES_PER_STRATUM)
                .map(|(_, question_id)| CalibrationSampleItemV1 {
                    question_id,
                    stratum,
                }),
        );
    }
    Ok(selected)
}

/// Prepares one self-hashed manifest and two blinded label templates.
pub fn prepare_calibration(
    dataset_revision: &str,
    cases: &[CalibrationSourceCase],
    package_bytes: &[u8],
    report_bytes: &[u8],
) -> Result<PreparedCalibrationV1> {
    if dataset_revision.trim().is_empty() {
        return Err(invalid("dataset revision must not be blank"));
    }
    let sample = select_question_ids(cases)?;
    let by_id = cases
        .iter()
        .map(|case| (case.question_id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let mut manifest = CalibrationManifestV1 {
        schema_version: CALIBRATION_SCHEMA_VERSION,
        dataset: "longmemeval-s-cleaned".to_string(),
        dataset_revision: dataset_revision.to_string(),
        package_sha256: exact_sha256(package_bytes),
        report_sha256: exact_sha256(report_bytes),
        selection_seed: CALIBRATION_SELECTION_SEED.to_string(),
        sample,
        manifest_sha256: String::new(),
    };
    manifest.manifest_sha256 =
        canonical_self_hash(MANIFEST_HASH_DOMAIN, &manifest, "manifest_sha256")?;
    manifest.validate()?;

    let items = manifest
        .sample
        .iter()
        .map(|sample| {
            let source = by_id.get(sample.question_id.as_str()).ok_or_else(|| {
                invalid(format!(
                    "selected calibration question {} is missing",
                    sample.question_id
                ))
            })?;
            Ok(CalibrationLabelItemV1 {
                question_id: source.question_id.clone(),
                stratum: source.stratum,
                question: source.question.clone(),
                reference_answer: source.reference_answer.clone(),
                candidate_answer: source.candidate_answer.clone(),
                reader_failure_kind: source.reader_failure_kind.clone(),
                label: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let labeler_a = label_template(&manifest, CalibrationRole::LabelerA, items.clone());
    let labeler_b = label_template(&manifest, CalibrationRole::LabelerB, items);
    Ok(PreparedCalibrationV1 {
        manifest,
        labeler_a,
        labeler_b,
    })
}

/// Scores completed labels and adjudication against primary-mode judge outcomes.
pub fn score_calibration(
    manifest_bytes: &[u8],
    report_bytes: &[u8],
    labeler_a_bytes: &[u8],
    labeler_b_bytes: &[u8],
    adjudication_bytes: &[u8],
    judge_outcomes: &BTreeMap<String, Option<bool>>,
) -> Result<CalibrationResultsV1> {
    let manifest: CalibrationManifestV1 = serde_json::from_slice(manifest_bytes)?;
    manifest.validate()?;
    let actual_report_sha256 = exact_sha256(report_bytes);
    if actual_report_sha256 != manifest.report_sha256 {
        return Err(invalid(format!(
            "report SHA-256 mismatch: expected {}, got {actual_report_sha256}",
            manifest.report_sha256
        )));
    }
    let labeler_a: CalibrationLabelArtifactV1 = serde_json::from_slice(labeler_a_bytes)?;
    let labeler_b: CalibrationLabelArtifactV1 = serde_json::from_slice(labeler_b_bytes)?;
    validate_completed_labels(&labeler_a, &manifest, CalibrationRole::LabelerA)?;
    validate_completed_labels(&labeler_b, &manifest, CalibrationRole::LabelerB)?;
    validate_label_content_equality(&labeler_a, &labeler_b)?;
    let adjudication: CalibrationAdjudicationV1 = serde_json::from_slice(adjudication_bytes)?;
    validate_adjudication(&adjudication, &manifest)?;

    let identity_a = labeler_a
        .identity_sha256
        .as_ref()
        .ok_or_else(|| invalid("labeler A identity hash is required"))?;
    let identity_b = labeler_b
        .identity_sha256
        .as_ref()
        .ok_or_else(|| invalid("labeler B identity hash is required"))?;
    if identity_a == identity_b
        || identity_a == &adjudication.identity_sha256
        || identity_b == &adjudication.identity_sha256
    {
        return Err(invalid(
            "labeler A, labeler B, and adjudicator identity hashes must be pairwise distinct",
        ));
    }

    let mut counts = [0_usize; 4];
    for (a, b) in labeler_a.items.iter().zip(&labeler_b.items) {
        let a = a
            .label
            .ok_or_else(|| invalid("labeler A has a missing label"))?
            .bit();
        let b = b
            .label
            .ok_or_else(|| invalid("labeler B has a missing label"))?
            .bit();
        counts[a * 2 + b] += 1;
    }

    let judge_correct_count = adjudication
        .labels
        .iter()
        .filter(|gold| {
            judge_outcomes
                .get(&gold.question_id)
                .copied()
                .flatten()
                .map(CalibrationLabel::from_judge)
                == Some(gold.label)
        })
        .count();
    let metrics = compute_metrics(
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        judge_correct_count,
    );
    let mut results = CalibrationResultsV1 {
        schema_version: CALIBRATION_SCHEMA_VERSION,
        manifest_sha256: manifest.manifest_sha256,
        report_sha256: actual_report_sha256,
        labeler_a_sha256: exact_sha256(labeler_a_bytes),
        labeler_b_sha256: exact_sha256(labeler_b_bytes),
        adjudication_sha256: exact_sha256(adjudication_bytes),
        n00: counts[0],
        n01: counts[1],
        n10: counts[2],
        n11: counts[3],
        pair_denominator: CALIBRATION_SAMPLE_SIZE,
        agreement: metrics.agreement,
        kappa_status: metrics.kappa_status,
        kappa: metrics.kappa,
        judge_correct_count,
        judge_denominator: CALIBRATION_SAMPLE_SIZE,
        judge_accuracy: metrics.judge_accuracy,
        agreement_pass: metrics.agreement_pass,
        kappa_pass: metrics.kappa_pass,
        accuracy_pass: metrics.accuracy_pass,
        verdict: metrics.verdict,
        results_sha256: String::new(),
    };
    results.results_sha256 = canonical_self_hash(RESULTS_HASH_DOMAIN, &results, "results_sha256")?;
    results.validate()?;
    Ok(results)
}

/// Reads the human-human reliability out of a scored V1 calibration.
///
/// The V1 artifact stores the two blinded labelers' agreement as four counts.
/// This projects them into the shared [`judge::ConfusionMatrix`] — labeler A as
/// rows, labeler B as columns — so the same kappa-with-uncertainty reporting
/// applies here as anywhere else. It deliberately reports the *interval*, which
/// the V1 `kappa` field does not carry: at seventy pairs a point kappa of 0.80
/// has a standard error near 0.07, so the V1 `kappa_pass` flag alone cannot
/// distinguish substantial agreement from moderate agreement.
#[must_use]
pub fn calibration_reliability(results: &CalibrationResultsV1) -> judge::AgreementReliability {
    judge::human_agreement(judge::ConfusionMatrix {
        true_positive: results.n11 as u64,
        false_negative: results.n10 as u64,
        false_positive: results.n01 as u64,
        true_negative: results.n00 as u64,
    })
}

/// What the LongMemEval absolute answer judge must clear to be authoritative.
///
/// The numbers are this task's declaration, sized to its actual design of seven
/// strata with ten cases each:
///
/// * `min_validation_cases` is half the seventy-case sample. The V1 results
///   artifact scores the judge over all seventy, which is a calibration-split
///   statistic; a promotion therefore needs a separate held-out measurement, and
///   the V1 verdict alone can never satisfy this requirement.
/// * `min_kappa_lower_bound` is 0.60 — the conventional "substantial agreement"
///   line — applied to the *lower bound* rather than the point estimate. The V1
///   artifact's own `kappa >= 0.80` point threshold stays in force as a separate
///   condition; requiring 0.80 of the lower bound would demand a point estimate
///   near 0.93 at this sample size, which is a different and much stricter bar
///   than the one that was declared.
/// * The worst-stratum recall bars are split, because a ten-case stratum whose
///   minority class holds three or four cases cannot support an interval bar at
///   all: a flawless three-for-three recall has a Wilson lower bound of only
///   0.44. So the real bar is the point estimate at 0.80, and the lower-bound bar
///   sits at 0.40 purely to exclude a stratum whose interval is too wide to mean
///   anything. This is the concrete reason a single shared point threshold cannot
///   be reused across tasks: the same number is unmeetable here and trivial on a
///   thousand-case suite.
/// * The sensitivity and specificity floors sum to more than one so
///   [`judge::correct_aggregate_rate`] is always defined for a judge this
///   requirement admits; [`judge::JudgeAuthorityRequirement::keeps_bias_correction_defined`]
///   checks that property rather than trusting it.
#[must_use]
pub fn external_memory_answer_judge_requirement() -> judge::JudgeAuthorityRequirement {
    judge::JudgeAuthorityRequirement {
        task: "longmemeval-absolute-answer-judge".to_string(),
        min_validation_cases: (CALIBRATION_SAMPLE_SIZE / 2) as u64,
        min_human_raw_agreement: AGREEMENT_THRESHOLD,
        min_kappa_lower_bound: 0.60,
        min_sensitivity_lower_bound: 0.70,
        min_specificity_lower_bound: 0.70,
        min_worst_stratum_recall_point: 0.80,
        min_worst_stratum_recall_lower_bound: 0.40,
        min_coverage: 0.95,
        required_slices: judge::JudgeSlice::ALL.to_vec(),
    }
}

/// Hashes a human identity after trimming and NFC normalization.
pub fn hash_identity(identity: &str) -> Result<String> {
    let normalized = identity.trim().nfc().collect::<String>();
    if normalized.is_empty() {
        return Err(invalid("calibration identity must not be blank"));
    }
    let mut hasher = Sha256::new();
    hasher.update(IDENTITY_HASH_DOMAIN);
    hasher.update(normalized.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

fn label_template(
    manifest: &CalibrationManifestV1,
    role: CalibrationRole,
    items: Vec<CalibrationLabelItemV1>,
) -> CalibrationLabelArtifactV1 {
    CalibrationLabelArtifactV1 {
        schema_version: CALIBRATION_SCHEMA_VERSION,
        manifest_sha256: manifest.manifest_sha256.clone(),
        role,
        status: CalibrationArtifactStatus::Template,
        identity_sha256: None,
        items,
    }
}

fn validate_source_cases(cases: &[CalibrationSourceCase]) -> Result<()> {
    let mut question_ids = HashSet::new();
    for case in cases {
        if case.question_id.trim().is_empty()
            || case.question.trim().is_empty()
            || case.reference_answer.trim().is_empty()
        {
            return Err(invalid("calibration source fields must not be blank"));
        }
        if !question_ids.insert(case.question_id.as_str()) {
            return Err(invalid(format!(
                "duplicate calibration question ID `{}`",
                case.question_id
            )));
        }
        match (&case.candidate_answer, &case.reader_failure_kind) {
            (Some(answer), None) if !answer.trim().is_empty() => {}
            (None, Some(kind)) if !kind.trim().is_empty() => {}
            _ => {
                return Err(invalid(format!(
                    "calibration question {} must contain exactly one candidate answer or reader failure kind",
                    case.question_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_sample(sample: &[CalibrationSampleItemV1]) -> Result<()> {
    if sample.len() != CALIBRATION_SAMPLE_SIZE {
        return Err(invalid(format!(
            "calibration sample must contain exactly {CALIBRATION_SAMPLE_SIZE} items"
        )));
    }
    let mut seen = HashSet::new();
    for (stratum_index, stratum) in CalibrationStratum::ALL.into_iter().enumerate() {
        let start = stratum_index * CASES_PER_STRATUM;
        let end = start + CASES_PER_STRATUM;
        for item in &sample[start..end] {
            if item.stratum != stratum {
                return Err(invalid("calibration sample stratum order is invalid"));
            }
            if item.question_id.trim().is_empty() || !seen.insert(item.question_id.as_str()) {
                return Err(invalid(
                    "calibration sample question IDs must be nonblank and unique",
                ));
            }
        }
    }
    Ok(())
}

fn validate_completed_labels(
    artifact: &CalibrationLabelArtifactV1,
    manifest: &CalibrationManifestV1,
    role: CalibrationRole,
) -> Result<()> {
    if artifact.schema_version != CALIBRATION_SCHEMA_VERSION
        || artifact.manifest_sha256 != manifest.manifest_sha256
        || artifact.role != role
        || artifact.status != CalibrationArtifactStatus::Completed
    {
        return Err(invalid(
            "invalid completed calibration label artifact metadata",
        ));
    }
    let identity = artifact
        .identity_sha256
        .as_deref()
        .ok_or_else(|| invalid("completed calibration label artifact requires identity hash"))?;
    validate_hash("identity_sha256", identity)?;
    if artifact.items.len() != CALIBRATION_SAMPLE_SIZE {
        return Err(invalid(
            "completed label artifact must contain exactly 70 items",
        ));
    }
    for (item, sample) in artifact.items.iter().zip(&manifest.sample) {
        if item.question_id != sample.question_id
            || item.stratum != sample.stratum
            || item.question.trim().is_empty()
            || item.reference_answer.trim().is_empty()
            || item.label.is_none()
        {
            return Err(invalid("label artifact sample/content order is invalid"));
        }
        match (&item.candidate_answer, &item.reader_failure_kind) {
            (Some(answer), None) if !answer.trim().is_empty() => {}
            (None, Some(kind)) if !kind.trim().is_empty() => {}
            _ => {
                return Err(invalid(
                    "label artifact candidate/failure content is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn validate_label_content_equality(
    a: &CalibrationLabelArtifactV1,
    b: &CalibrationLabelArtifactV1,
) -> Result<()> {
    for (a, b) in a.items.iter().zip(&b.items) {
        if a.question_id != b.question_id
            || a.stratum != b.stratum
            || a.question != b.question
            || a.reference_answer != b.reference_answer
            || a.candidate_answer != b.candidate_answer
            || a.reader_failure_kind != b.reader_failure_kind
        {
            return Err(invalid(
                "labeler artifacts must have exact sample/content equality",
            ));
        }
    }
    Ok(())
}

fn validate_adjudication(
    adjudication: &CalibrationAdjudicationV1,
    manifest: &CalibrationManifestV1,
) -> Result<()> {
    if adjudication.schema_version != CALIBRATION_SCHEMA_VERSION
        || adjudication.manifest_sha256 != manifest.manifest_sha256
        || adjudication.role != CalibrationRole::Adjudicator
        || adjudication.labels.len() != CALIBRATION_SAMPLE_SIZE
    {
        return Err(invalid("invalid calibration adjudication metadata"));
    }
    validate_hash("adjudicator identity_sha256", &adjudication.identity_sha256)?;
    for (label, sample) in adjudication.labels.iter().zip(&manifest.sample) {
        if label.question_id != sample.question_id {
            return Err(invalid("adjudication labels must follow manifest order"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ComputedMetrics {
    agreement: f64,
    kappa_status: KappaStatus,
    kappa: Option<f64>,
    judge_accuracy: f64,
    agreement_pass: bool,
    kappa_pass: bool,
    accuracy_pass: bool,
    verdict: CalibrationVerdict,
}

fn compute_metrics(
    n00: usize,
    n01: usize,
    n10: usize,
    n11: usize,
    judge_correct_count: usize,
) -> ComputedMetrics {
    let denominator = CALIBRATION_SAMPLE_SIZE as f64;
    let agreement = (n00 + n11) as f64 / denominator;
    let p_a1 = (n10 + n11) as f64 / denominator;
    let p_b1 = (n01 + n11) as f64 / denominator;
    let p_e = p_a1 * p_b1 + (1.0 - p_a1) * (1.0 - p_b1);
    let kappa_denominator = 1.0 - p_e;
    let (kappa_status, kappa) = if kappa_denominator == 0.0 {
        (KappaStatus::UndefinedZeroDenominator, None)
    } else {
        (
            KappaStatus::Defined,
            Some((agreement - p_e) / kappa_denominator),
        )
    };
    let judge_accuracy = judge_correct_count as f64 / denominator;
    let agreement_pass = agreement >= AGREEMENT_THRESHOLD;
    let kappa_pass = kappa.is_some_and(|value| value >= KAPPA_THRESHOLD);
    let accuracy_pass = judge_accuracy >= ACCURACY_THRESHOLD;
    let verdict = if agreement_pass && kappa_pass && accuracy_pass {
        CalibrationVerdict::Pass
    } else {
        CalibrationVerdict::Fail
    };
    ComputedMetrics {
        agreement,
        kappa_status,
        kappa,
        judge_accuracy,
        agreement_pass,
        kappa_pass,
        accuracy_pass,
        verdict,
    }
}

fn canonical_self_hash<T: Serialize>(domain: &[u8], value: &T, field: &str) -> Result<String> {
    let mut value = serde_json::to_value(value)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("calibration self-hash value must be a JSON object"))?;
    if object.remove(field).is_none() {
        return Err(invalid(format!(
            "calibration self-hash field `{field}` is missing"
        )));
    }
    let mut canonical = Vec::new();
    write_canonical_json(&value, &mut canonical)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(canonical);
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_canonical_json(value: &serde_json::Value, output: &mut Vec<u8>) -> Result<()> {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            output.extend_from_slice(if *value { &b"true"[..] } else { &b"false"[..] });
        }
        serde_json::Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => {
            output.extend_from_slice(serde_json::to_string(value)?.as_bytes());
        }
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        serde_json::Value::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                output.push(b':');
                write_canonical_json(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn exact_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_hash(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "{name} must be a lowercase 64-hex SHA-256 digest"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ExternalMemoryError {
    ExternalMemoryError::InvalidConfig(message.into())
}

#[cfg(test)]
mod judge_tests {
    use super::judge::{
        AgreementReliability, CalibrationExpiry, ConfusionMatrix, ExpiryReason, JudgeAuthority,
        JudgeAuthorityEvidence, JudgeAuthorityRequirement, JudgeIdentity, JudgeLabelPair,
        JudgeSlice, JudgeSliceValidity, JudgeValidity, LabelSplit, LiveCalibrationRequest,
        PromptShape, admit_live_calibration, apply_judge_authority, calibration_expiry,
        cohen_kappa, correct_aggregate_rate, decide_prompt_shape, human_agreement,
        proportion_estimate,
    };
    use super::{CalibrationLabel, external_memory_answer_judge_requirement};
    use moa_eval_core::decision::{Decision, MetricDecision, SupportSummary};
    use moa_eval_core::metric::{GateKind, HypothesisFamily};

    fn identity() -> JudgeIdentity {
        JudgeIdentity::new(
            "judge-model-1",
            "v3",
            b"exact judge prompt text",
            b"exact rubric text",
            "strict-json-v1",
            "longmemeval",
        )
        .expect("valid judge identity")
    }

    /// Deterministic uniform source for the bootstrap cross-check below.
    struct Lcg(u64);

    impl Lcg {
        fn unit(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1_u64 << 53) as f64
        }
    }

    #[test]
    fn wilson_intervals_are_what_the_small_stratum_thresholds_were_sized_against() {
        // Pins: the arithmetic the per-task thresholds are justified by. A ten-case
        // stratum cannot support a 0.80 recall bound even at a flawless ten for ten,
        // and Wilson does not collapse to a zero-width interval at the boundary the
        // way Wald does.
        let perfect = proportion_estimate(10, 10).expect("ten of ten");
        assert!((perfect.lower - 0.722_47).abs() < 1e-4, "{perfect:?}");
        assert_eq!(perfect.upper, 1.0);
        assert!(perfect.lower < 0.80, "a 0.80 bound would be unreachable");

        let one_miss = proportion_estimate(8, 10).expect("eight of ten");
        assert!((one_miss.lower - 0.490_15).abs() < 1e-4, "{one_miss:?}");
        assert!(one_miss.lower >= 0.50 - 1e-2);

        assert_eq!(proportion_estimate(0, 0), None);
        assert_eq!(proportion_estimate(3, 2), None);
    }

    #[test]
    fn kappa_standard_error_matches_a_resampled_distribution_of_the_same_table() {
        // Pins: the asymptotic variance is the Fleiss-Cohen-Everitt form, not the
        // simplified po(1-po)/(n(1-pe)^2) expression that circulates with it. A
        // seeded nonparametric bootstrap of the same table is the independent
        // check: the two must agree, and the simplified form does not.
        let matrix = ConfusionMatrix {
            true_positive: 120,
            false_negative: 15,
            false_positive: 10,
            true_negative: 55,
        };
        let (expected_agreement, kappa) = cohen_kappa(&matrix);
        let kappa = kappa.expect("defined kappa");
        assert!(kappa.standard_error > 0.0);

        let total = matrix.total();
        let cells = [
            matrix.true_positive,
            matrix.false_negative,
            matrix.false_positive,
            matrix.true_negative,
        ];
        let cumulative = cells
            .iter()
            .scan(0.0, |running, count| {
                *running += *count as f64 / total as f64;
                Some(*running)
            })
            .collect::<Vec<_>>();
        let mut rng = Lcg(0x5eed_1234_5678_9abc);
        let mut points = Vec::with_capacity(2_000);
        for _ in 0..2_000 {
            let mut resampled = [0_u64; 4];
            for _ in 0..total {
                let draw = rng.unit();
                let cell = cumulative
                    .iter()
                    .position(|bound| draw < *bound)
                    .unwrap_or(3);
                resampled[cell] += 1;
            }
            if let (_, Some(estimate)) = cohen_kappa(&ConfusionMatrix {
                true_positive: resampled[0],
                false_negative: resampled[1],
                false_positive: resampled[2],
                true_negative: resampled[3],
            }) {
                points.push(estimate.point);
            }
        }
        let mean = points.iter().sum::<f64>() / points.len() as f64;
        let bootstrap_error = (points
            .iter()
            .map(|point| (point - mean) * (point - mean))
            .sum::<f64>()
            / (points.len() - 1) as f64)
            .sqrt();
        let relative = (kappa.standard_error - bootstrap_error).abs() / bootstrap_error;
        assert!(
            relative < 0.15,
            "analytic SE {} should match bootstrap SE {bootstrap_error}",
            kappa.standard_error
        );

        let simplified = (kappa.point * (1.0 - kappa.point)
            / (total as f64 * (1.0 - expected_agreement) * (1.0 - expected_agreement)))
            .sqrt();
        assert!(
            (simplified - kappa.standard_error).abs() > 1e-4,
            "the simplified variance must not be what is reported"
        );

        // Perfect agreement is exactly kappa = 1 with no uncertainty, which the
        // simplified form also gets right but by coincidence.
        let (_, perfect) = cohen_kappa(&ConfusionMatrix {
            true_positive: 40,
            false_negative: 0,
            false_positive: 0,
            true_negative: 30,
        });
        let perfect = perfect.expect("defined kappa");
        assert!((perfect.point - 1.0).abs() < 1e-12);
        assert!(perfect.standard_error < 1e-12);

        // Unanimous marginals leave no chance-corrected room at all.
        assert!(
            cohen_kappa(&ConfusionMatrix {
                true_positive: 70,
                ..ConfusionMatrix::default()
            })
            .1
            .is_none()
        );
    }

    #[test]
    fn a_calibration_expires_on_every_instrument_change_and_on_a_prevalence_shift() {
        // Pins: each expiry trigger fires on its own, so the staleness check cannot
        // be weakened to require several components to change at once.
        let calibrated = identity();
        assert_eq!(
            calibration_expiry(&calibrated, &identity(), 0.5, 0.55),
            CalibrationExpiry::Current
        );

        type Change = (ExpiryReason, fn(&mut JudgeIdentity));
        let changes: [Change; 5] = [
            (ExpiryReason::Model, |identity| {
                identity.model = "judge-model-2".to_string();
            }),
            (ExpiryReason::Prompt, |identity| {
                identity.prompt_sha256 = "b".repeat(64);
            }),
            (ExpiryReason::Rubric, |identity| {
                identity.rubric_sha256 = "c".repeat(64);
            }),
            (ExpiryReason::OutputParser, |identity| {
                identity.output_parser_version = "lenient-v2".to_string();
            }),
            (ExpiryReason::Domain, |identity| {
                identity.domain = "personamem".to_string();
            }),
        ];
        for (expected, mutate) in changes {
            let mut in_use = identity();
            mutate(&mut in_use);
            assert_eq!(
                calibration_expiry(&calibrated, &in_use, 0.5, 0.5),
                CalibrationExpiry::Expired {
                    reasons: vec![expected]
                },
                "changing {} must expire the calibration",
                expected.as_str()
            );
        }

        // A prompt version bump alone is an expiry: the label is the bookkeeping
        // that a report is read through.
        let mut relabeled = identity();
        relabeled.prompt_version = "v4".to_string();
        assert!(!calibration_expiry(&calibrated, &relabeled, 0.5, 0.5).is_current());

        assert_eq!(
            calibration_expiry(&calibrated, &identity(), 0.2, 0.35),
            CalibrationExpiry::Expired {
                reasons: vec![ExpiryReason::ClassDistribution]
            }
        );
        assert!(!calibration_expiry(&calibrated, &identity(), 0.2, f64::NAN).is_current());
    }

    #[test]
    fn judge_validity_separates_abstentions_classes_and_the_worst_stratum() {
        // Pins: abstentions lower coverage instead of scoring as wrong, per-class
        // recall is reported separately, and the worst stratum is the one with the
        // weakest rare-class recall bound rather than the weakest pooled accuracy.
        let mut pairs = Vec::new();
        for index in 0..10 {
            pairs.push(JudgeLabelPair {
                case_id: format!("easy-{index}"),
                stratum: "easy".to_string(),
                gold: CalibrationLabel::Correct,
                judge: Some(CalibrationLabel::Correct),
            });
        }
        // The hard stratum is mostly Correct with two Incorrect cases the judge
        // misses, so pooled accuracy stays high while rare-class recall collapses.
        for index in 0..8 {
            pairs.push(JudgeLabelPair {
                case_id: format!("hard-{index}"),
                stratum: "hard".to_string(),
                gold: CalibrationLabel::Correct,
                judge: Some(CalibrationLabel::Correct),
            });
        }
        for index in 8..10 {
            pairs.push(JudgeLabelPair {
                case_id: format!("hard-{index}"),
                stratum: "hard".to_string(),
                gold: CalibrationLabel::Incorrect,
                judge: Some(CalibrationLabel::Correct),
            });
        }
        pairs.push(JudgeLabelPair {
            case_id: "abstained".to_string(),
            stratum: "hard".to_string(),
            gold: CalibrationLabel::Incorrect,
            judge: None,
        });

        let validity = JudgeValidity::measure(identity(), LabelSplit::Validation, &pairs)
            .expect("measurable validity");

        assert_eq!(validity.selective.total, 21);
        assert_eq!(validity.selective.covered, 20);
        assert!((validity.selective.coverage - 20.0 / 21.0).abs() < 1e-12);
        assert_eq!(validity.overall.false_positive, 2);
        assert_eq!(validity.overall.false_negative, 0);
        assert_eq!(
            validity.sensitivity.map(|estimate| estimate.point),
            Some(1.0)
        );
        assert_eq!(
            validity.specificity.map(|estimate| estimate.point),
            Some(0.0),
            "the judge never identified an incorrect answer"
        );
        assert!(
            validity
                .raw_agreement
                .is_some_and(|estimate| estimate.point > 0.85),
            "pooled accuracy stays flattering, which is why it cannot be the gate"
        );
        assert_eq!(validity.worst_stratum.as_deref(), Some("hard"));
        assert_eq!(
            validity
                .worst_stratum_recall
                .map(|estimate| estimate.numerator),
            Some(0),
            "the worst stratum is the one whose weakest class recall is zero"
        );
        let hard = validity
            .strata
            .iter()
            .find(|stratum| stratum.stratum == "hard")
            .expect("hard stratum is reported");
        assert_eq!(
            hard.positive_recall.map(|estimate| estimate.point),
            Some(1.0)
        );
        assert_eq!(
            hard.negative_recall.map(|estimate| estimate.point),
            Some(0.0)
        );
        let easy = validity
            .strata
            .iter()
            .find(|stratum| stratum.stratum == "easy")
            .expect("easy stratum is reported");
        assert_eq!(
            easy.negative_recall, None,
            "a stratum with no negative cases has no negative recall to report"
        );
        assert_eq!(
            easy.weakest_class_recall.map(|estimate| estimate.numerator),
            Some(10),
            "a single-class stratum falls back to the class it does contain"
        );

        let duplicate = JudgeValidity::measure(
            identity(),
            LabelSplit::Validation,
            &[pairs[0].clone(), pairs[0].clone()],
        );
        assert!(
            duplicate.is_err(),
            "a repeated case identity must be refused"
        );
        assert!(JudgeValidity::measure(identity(), LabelSplit::Validation, &[]).is_err());
    }

    #[test]
    fn bias_correction_carries_calibration_uncertainty_and_refuses_a_chance_level_judge() {
        // Pins: the corrected interval is wider than the evaluation set's own
        // interval because the judge's error rates are themselves estimates, and a
        // judge whose interval admits chance performance produces no correction at
        // all rather than an arbitrarily wide one.
        let apparent = proportion_estimate(60, 200).expect("apparent rate");
        let sensitivity = proportion_estimate(63, 70).expect("sensitivity");
        let specificity = proportion_estimate(66, 70).expect("specificity");
        let corrected =
            correct_aggregate_rate(apparent, sensitivity, specificity).expect("defined correction");

        assert!(corrected.youden_j > 0.0);
        assert!(
            corrected.upper - corrected.lower > apparent.upper - apparent.lower,
            "calibration-set uncertainty must widen the interval: {corrected:?}"
        );
        assert!(corrected.lower <= corrected.point && corrected.point <= corrected.upper);
        assert!((0.0..=1.0).contains(&corrected.point));

        let chance = proportion_estimate(35, 70).expect("coin-flip sensitivity");
        let error = correct_aggregate_rate(apparent, chance, chance)
            .expect_err("a chance-level judge must not be corrected");
        assert!(error.to_string().contains("undefined"), "{error}");
    }

    #[test]
    fn an_uncalibrated_or_stale_judge_cannot_produce_an_authoritative_metric() {
        // Pins: the authority gate and its effect on a metric decision. Every
        // condition is load-bearing on its own, and a failed condition downgrades a
        // PASS to INCONCLUSIVE rather than reporting a weaker pass.
        let requirement = external_memory_answer_judge_requirement();
        assert!(
            requirement.keeps_bias_correction_defined(),
            "the declared error-rate floors must keep aggregate correction defined"
        );

        let calibrated = identity();
        let reliability = human_agreement(ConfusionMatrix {
            true_positive: 100,
            false_negative: 4,
            false_positive: 4,
            true_negative: 92,
        });
        let validity = strong_validity(LabelSplit::Validation);
        let slices = measured_slices(calibrated.clone());
        let evidence = JudgeAuthorityEvidence {
            in_use: &calibrated,
            calibrated: &calibrated,
            calibrated_prevalence: 0.5,
            observed_prevalence: 0.52,
            reliability: &reliability,
            validity: &validity,
            slice_validity: &slices,
        };
        let authority = requirement.evaluate(&evidence);
        assert!(authority.is_authoritative(), "{authority:?}");

        let unrelated_validity = JudgeValidity::measure(
            identity_with_other_model(),
            LabelSplit::Validation,
            &labeled_pairs(40, 2, 2),
        )
        .expect("measurable validity");
        let unrelated = requirement.evaluate(&JudgeAuthorityEvidence {
            validity: &unrelated_validity,
            ..evidence
        });
        assert!(matches!(
            &unrelated,
            JudgeAuthority::Informational { reasons }
                if reasons.iter().any(|reason| reason.contains("different judge than the calibration"))
        ));

        let passing = judged_decision(Decision::Pass);
        assert_eq!(
            apply_judge_authority(passing.clone(), &authority).decision,
            Decision::Pass
        );

        // Each way of losing authority, checked separately.
        let stale = identity_with_other_model();
        let calibration_split = strong_validity(LabelSplit::Calibration);
        let thin = thin_validity();
        let weak_reliability = human_agreement(ConfusionMatrix {
            true_positive: 20,
            false_negative: 8,
            false_positive: 7,
            true_negative: 5,
        });
        let missing_slices = [slices[1].clone()];
        let cases: [(&str, JudgeAuthorityEvidence<'_>); 5] = [
            (
                "a different judge",
                JudgeAuthorityEvidence {
                    in_use: &stale,
                    ..evidence
                },
            ),
            (
                "a prevalence shift",
                JudgeAuthorityEvidence {
                    observed_prevalence: 0.9,
                    ..evidence
                },
            ),
            (
                "the calibration split",
                JudgeAuthorityEvidence {
                    validity: &calibration_split,
                    ..evidence
                },
            ),
            (
                "too few held-out cases",
                JudgeAuthorityEvidence {
                    validity: &thin,
                    ..evidence
                },
            ),
            (
                "weak human reliability",
                JudgeAuthorityEvidence {
                    reliability: &weak_reliability,
                    ..evidence
                },
            ),
        ];
        for (label, broken) in cases {
            let authority = requirement.evaluate(&broken);
            assert!(
                !authority.is_authoritative(),
                "{label} must refuse authority"
            );
            let downgraded = apply_judge_authority(passing.clone(), &authority);
            assert_eq!(downgraded.decision, Decision::Inconclusive, "{label}");
            assert_eq!(downgraded.regression_p_value, None, "{label}");
            assert!(
                downgraded.rationale.contains("not authoritative"),
                "{label}: {}",
                downgraded.rationale
            );
        }

        let unreported = requirement.evaluate(&JudgeAuthorityEvidence {
            slice_validity: &missing_slices,
            ..evidence
        });
        assert!(matches!(
            &unreported,
            JudgeAuthority::Informational { reasons }
                if reasons.iter().any(|reason| reason.contains("prompt_injection"))
                    && reasons.iter().any(|reason| reason.contains("rare_class"))
                    && reasons.iter().any(|reason| reason.contains("cross_domain"))
        ));

        let other_judge_slices = measured_slices(identity_with_other_model());
        let mismatched_slice_evidence = requirement.evaluate(&JudgeAuthorityEvidence {
            slice_validity: &other_judge_slices,
            ..evidence
        });
        assert!(matches!(
            &mismatched_slice_evidence,
            JudgeAuthority::Informational { reasons }
                if reasons.iter().any(|reason| reason.contains("slice validity was measured on a different judge"))
        ));
    }

    #[test]
    fn prompt_shape_is_decided_on_held_out_cases_and_defaults_to_one_call() {
        // Pins: the selection split can never decide the prompt shape, and
        // decomposition is chosen only on a measured gain rather than a difference
        // inside the interval.
        let holistic = validity(LabelSplit::Validation, 40, 2, 12);
        let error = decide_prompt_shape(&validity(LabelSplit::Calibration, 40, 2, 12), &holistic)
            .expect_err("the calibration split must not decide the prompt shape");
        assert!(error.to_string().contains("held-out"), "{error}");

        let decision =
            decide_prompt_shape(&holistic, &holistic).expect("equal shapes decide holistic");
        assert_eq!(decision.chosen, PromptShape::Holistic);

        // A two-case advantage on forty cases is inside the interval, so the extra
        // per-dimension calls are refused; a twelve-case advantage clears it.
        let marginal = validity(LabelSplit::Validation, 40, 2, 10);
        assert_eq!(
            decide_prompt_shape(&holistic, &marginal)
                .expect("marginal comparison")
                .chosen,
            PromptShape::Holistic
        );
        let decisive = validity(LabelSplit::Validation, 40, 2, 0);
        let decision = decide_prompt_shape(&holistic, &decisive).expect("measured gain");
        assert_eq!(decision.chosen, PromptShape::Decomposed);
        assert!(decision.rationale.contains("held-out"));

        let mut different_cases = holistic.clone();
        different_cases.selective.total += 1;
        assert!(
            decide_prompt_shape(&holistic, &different_cases).is_err(),
            "the two shapes must be compared on one case set"
        );
    }

    #[test]
    fn live_calibration_is_ignored_by_default_and_needs_flag_credentials_and_budget() {
        // Pins: the default request is refused and each precondition is separately
        // required, so a forgotten flag cannot spend provider credit.
        let error = admit_live_calibration(&LiveCalibrationRequest::default())
            .expect_err("the default must be refused");
        assert!(error.to_string().contains("ignored by default"), "{error}");

        let complete = LiveCalibrationRequest {
            explicitly_enabled: true,
            credentials_present: true,
            budget_usd: 5.0,
        };
        admit_live_calibration(&complete).expect("a complete request is admitted");

        for mutate in [
            (|request: &mut LiveCalibrationRequest| request.explicitly_enabled = false)
                as fn(&mut _),
            |request: &mut LiveCalibrationRequest| request.credentials_present = false,
            |request: &mut LiveCalibrationRequest| request.budget_usd = 0.0,
            |request: &mut LiveCalibrationRequest| request.budget_usd = f64::NAN,
        ] {
            let mut request = complete;
            mutate(&mut request);
            assert!(admit_live_calibration(&request).is_err());
        }
    }

    fn identity_with_other_model() -> JudgeIdentity {
        let mut identity = identity();
        identity.model = "judge-model-2".to_string();
        identity
    }

    /// Builds a balanced labeled set whose first `errors` cases the judge gets wrong.
    ///
    /// Gold alternates by case so every stratum is class-balanced, and the errors
    /// land in the first stratum, which makes that stratum the worst one by
    /// construction.
    fn labeled_pairs(cases: usize, strata: usize, errors: usize) -> Vec<JudgeLabelPair> {
        (0..cases)
            .map(|index| {
                let gold = if index % 2 == 0 {
                    CalibrationLabel::Correct
                } else {
                    CalibrationLabel::Incorrect
                };
                let judge = if index < errors {
                    match gold {
                        CalibrationLabel::Correct => CalibrationLabel::Incorrect,
                        CalibrationLabel::Incorrect => CalibrationLabel::Correct,
                    }
                } else {
                    gold
                };
                JudgeLabelPair {
                    case_id: format!("case-{index}"),
                    stratum: format!("stratum-{}", index * strata / cases),
                    gold,
                    judge: Some(judge),
                }
            })
            .collect()
    }

    fn validity(split: LabelSplit, cases: usize, strata: usize, errors: usize) -> JudgeValidity {
        JudgeValidity::measure(identity(), split, &labeled_pairs(cases, strata, errors))
            .expect("measurable validity")
    }

    fn strong_validity(split: LabelSplit) -> JudgeValidity {
        validity(split, 40, 2, 2)
    }

    fn measured_slices(identity: JudgeIdentity) -> Vec<JudgeSliceValidity> {
        let pairs = labeled_pairs(40, 2, 2);
        JudgeSlice::ALL
            .into_iter()
            .map(|slice| {
                JudgeSliceValidity::measure(slice, identity.clone(), &pairs)
                    .expect("measurable slice validity")
            })
            .collect()
    }

    fn thin_validity() -> JudgeValidity {
        validity(LabelSplit::Validation, 4, 1, 0)
    }

    fn judged_decision(decision: Decision) -> MetricDecision {
        MetricDecision {
            metric_id: "answer_support_rate".to_string(),
            decision,
            utility_delta: 0.01,
            lower_bound: -0.005,
            upper_bound: 0.025,
            practical_margin: 0.02,
            alpha: 0.025,
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
            support: SupportSummary {
                independent_units: 40,
                observations: 40,
                required_independent_units: 12,
            },
            regression_p_value: Some(0.9),
            rationale: "lower_bound -0.005000 >= -margin -0.020000".to_string(),
        }
    }

    #[test]
    fn the_reliability_projection_keeps_labeler_a_as_rows() {
        // Pins: the V1 four-count projection is not transposed. A labeler pair that
        // disagrees asymmetrically must keep its direction, otherwise sensitivity
        // and specificity swap silently.
        let results_matrix = |n00: usize, n01: usize, n10: usize, n11: usize| ConfusionMatrix {
            true_positive: n11 as u64,
            false_negative: n10 as u64,
            false_positive: n01 as u64,
            true_negative: n00 as u64,
        };
        let matrix = results_matrix(28, 4, 3, 35);
        assert_eq!(matrix.total(), 70);
        assert_eq!(matrix.reference_positives(), 38);
        let reliability: AgreementReliability = human_agreement(matrix);
        assert_eq!(
            reliability.raw_agreement.map(|estimate| estimate.numerator),
            Some(63)
        );
        let kappa = reliability.kappa.expect("defined kappa");
        assert!((kappa.point - 0.798_021_434_460_016_5).abs() < 1e-12);
        assert!(
            kappa.lower < 0.70,
            "the interval must show that a 0.80 point estimate is not a 0.80 claim: {kappa:?}"
        );
    }

    #[test]
    fn the_declared_requirement_cannot_be_met_by_the_v1_whole_sample_alone() {
        // Pins: the V1 artifact scores the judge over the same seventy cases used to
        // select it, so a promotion needs a separate held-out measurement. The
        // requirement names that as a missing condition rather than accepting it.
        let requirement: JudgeAuthorityRequirement = external_memory_answer_judge_requirement();
        assert_eq!(requirement.min_validation_cases, 35);
        let calibrated = identity();
        let reliability = human_agreement(ConfusionMatrix {
            true_positive: 35,
            false_negative: 0,
            false_positive: 0,
            true_negative: 35,
        });
        let validity = strong_validity(LabelSplit::Calibration);
        let slices = measured_slices(calibrated.clone());
        let authority = requirement.evaluate(&JudgeAuthorityEvidence {
            in_use: &calibrated,
            calibrated: &calibrated,
            calibrated_prevalence: 0.5,
            observed_prevalence: 0.5,
            reliability: &reliability,
            validity: &validity,
            slice_validity: &slices,
        });
        assert!(
            authority
                .reasons()
                .iter()
                .any(|reason| reason.contains("calibration split")),
            "{authority:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_memory_calibration_canonical_json_sorts_nested_keys_and_renders_floats() {
        // Pins: calibration self-hashes use compact recursive scalar-key ordering, serde_json
        // escaping, and deterministic shortest finite-number text.
        let value = serde_json::json!({
            "z": [0.9, {"zebra": "quoted \"value\"", "alpha": 1.25}],
            "alpha": "line\nbreak"
        });
        let mut bytes = Vec::new();
        write_canonical_json(&value, &mut bytes).expect("canonicalize finite nested JSON");
        assert_eq!(
            bytes,
            br#"{"alpha":"line\nbreak","z":[0.9,{"alpha":1.25,"zebra":"quoted \"value\""}]}"#
        );
    }
}
