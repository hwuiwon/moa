//! Strict, deterministic human calibration contracts for external-memory judges.

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
