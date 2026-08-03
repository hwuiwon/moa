//! Strict deterministic V2 external-memory reports.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::Result;
use super::answer::{
    AbsoluteJudgeResponse, AnswerScore, ExternalMemoryMode, ReaderResponse, SupportStatus,
    TOKEN_ESTIMATOR_CHARS_DIV_4,
};
use super::cost::{StageCostRecord, StageName};
use super::dataset::DatasetPackage;
use super::formation::ResolvedFormationConfig;
use super::longmemeval::LongMemEvalRetrievalMetrics;
use super::personamem::PersonaMemAccuracyReport;

/// Failure class retained in partial reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Provider timeout.
    Timeout,
    /// Cumulative budget exhaustion.
    Budget,
    /// Provider transport or API failure.
    Provider,
    /// Provider response parse failure.
    Parse,
    /// Backend formation or retrieval failure.
    Backend,
}

/// One per-case failure retained in the report denominator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseFailure {
    /// Case isolation key.
    pub isolation_key: String,
    /// Failure class.
    pub kind: FailureKind,
    /// Stable failure detail.
    pub message: String,
}

/// Per-case V2 report artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseReportV2 {
    /// Case isolation key.
    pub isolation_key: String,
    /// Dataset category.
    pub category: String,
    /// Benchmark mode.
    pub mode: ExternalMemoryMode,
    /// Whether this mode can run for the case.
    pub mode_support: SupportStatus,
    /// Exact rendered evidence, including empty evidence.
    pub rendered_evidence: String,
    /// Evidence-only estimate under the persisted reader estimator.
    pub rendered_evidence_tokens: u64,
    /// Reader response and normalized provider usage, when generated.
    pub reader: Option<ReaderResponse>,
    /// Whether deterministic dataset scoring applies.
    pub answer_score_support: SupportStatus,
    /// Dataset-owned answer score, when supported.
    pub answer_score: Option<AnswerScore>,
    /// Absolute judge response, when the dataset uses one.
    pub absolute_judge: Option<AbsoluteJudgeResponse>,
    /// Failure, when a supported case did not complete.
    pub failure: Option<CaseFailure>,
}

/// Transitional internal spelling used by the execution harness.
pub type CaseReport = CaseReportV2;

impl CaseReportV2 {
    /// Creates a retained failed primary-mode case.
    #[must_use]
    pub fn failed(
        isolation_key: impl Into<String>,
        category: impl Into<String>,
        kind: FailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self::failed_for_mode(
            isolation_key,
            category,
            ExternalMemoryMode::Primary,
            kind,
            message,
        )
    }

    /// Creates a retained failed case for an explicit mode.
    #[must_use]
    pub fn failed_for_mode(
        isolation_key: impl Into<String>,
        category: impl Into<String>,
        mode: ExternalMemoryMode,
        kind: FailureKind,
        message: impl Into<String>,
    ) -> Self {
        let isolation_key = isolation_key.into();
        let message = message.into();
        Self {
            failure: Some(CaseFailure {
                isolation_key: isolation_key.clone(),
                kind,
                message: message.clone(),
            }),
            isolation_key,
            category: category.into(),
            mode,
            mode_support: SupportStatus::Supported,
            rendered_evidence: String::new(),
            rendered_evidence_tokens: 0,
            reader: None,
            answer_score_support: SupportStatus::Unsupported { reason: message },
            answer_score: None,
            absolute_judge: None,
        }
    }

    /// Creates a completed primary-mode case.
    #[must_use]
    pub fn completed(
        isolation_key: impl Into<String>,
        category: impl Into<String>,
        rendered_evidence: impl Into<String>,
        answer_score_support: SupportStatus,
    ) -> Self {
        Self::completed_for_mode(
            isolation_key,
            category,
            ExternalMemoryMode::Primary,
            rendered_evidence,
            0,
            answer_score_support,
        )
    }

    /// Creates a completed case for an explicit mode.
    #[must_use]
    pub fn completed_for_mode(
        isolation_key: impl Into<String>,
        category: impl Into<String>,
        mode: ExternalMemoryMode,
        rendered_evidence: impl Into<String>,
        rendered_evidence_tokens: u64,
        answer_score_support: SupportStatus,
    ) -> Self {
        Self {
            isolation_key: isolation_key.into(),
            category: category.into(),
            mode,
            mode_support: SupportStatus::Supported,
            rendered_evidence: rendered_evidence.into(),
            rendered_evidence_tokens,
            reader: None,
            answer_score_support,
            answer_score: None,
            absolute_judge: None,
            failure: None,
        }
    }

    /// Creates a terminal unsupported case without a provider attempt.
    #[must_use]
    pub fn unsupported(
        isolation_key: impl Into<String>,
        category: impl Into<String>,
        mode: ExternalMemoryMode,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            isolation_key: isolation_key.into(),
            category: category.into(),
            mode,
            mode_support: SupportStatus::Unsupported {
                reason: reason.clone(),
            },
            rendered_evidence: String::new(),
            rendered_evidence_tokens: 0,
            reader: None,
            answer_score_support: SupportStatus::Unsupported { reason },
            answer_score: None,
            absolute_judge: None,
            failure: None,
        }
    }

    /// Attaches the generated answer, dataset score, and optional absolute judge artifact.
    #[must_use]
    pub fn with_generated_answer(
        mut self,
        reader: ReaderResponse,
        answer_score: AnswerScore,
        absolute_judge: Option<AbsoluteJudgeResponse>,
    ) -> Self {
        self.reader = Some(reader);
        self.answer_score_support = SupportStatus::Supported;
        self.answer_score = Some(answer_score);
        self.absolute_judge = absolute_judge;
        self
    }

    /// Attaches a generated answer with explicit deterministic-score support.
    #[must_use]
    pub fn with_generated_answer_outcome(
        mut self,
        reader: ReaderResponse,
        support_status: SupportStatus,
        answer_score: Option<AnswerScore>,
        absolute_judge: Option<AbsoluteJudgeResponse>,
    ) -> Self {
        self.answer_score_support = support_status;
        self.reader = Some(reader);
        self.answer_score = answer_score;
        self.absolute_judge = absolute_judge;
        self
    }

    /// Replaces retained evidence and its exact estimator count.
    #[must_use]
    pub fn with_rendered_evidence(
        mut self,
        rendered_evidence: impl Into<String>,
        rendered_evidence_tokens: u64,
    ) -> Self {
        self.rendered_evidence = rendered_evidence.into();
        self.rendered_evidence_tokens = rendered_evidence_tokens;
        self
    }

    /// Sets the estimate for already-retained evidence.
    #[must_use]
    pub fn with_rendered_evidence_tokens(mut self, rendered_evidence_tokens: u64) -> Self {
        self.rendered_evidence_tokens = rendered_evidence_tokens;
        self
    }
}

/// One measured stage occurrence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageObservation {
    /// Attributed stage.
    pub stage: StageName,
    /// Benchmark mode, or null for formation and embedding.
    pub mode: Option<ExternalMemoryMode>,
    /// Deterministic measured latency input.
    pub latency_ms: u64,
    /// Model-aware cost accounting, when applicable.
    pub accounting: Option<StageCostRecord>,
}

/// Aggregated stage latency and usage artifacts keyed by `(stage, mode)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageMetricsV2 {
    /// Attributed stage.
    pub stage: StageName,
    /// Benchmark mode, or null for formation and embedding.
    pub mode: Option<ExternalMemoryMode>,
    /// Number of observed stage attempts.
    pub denominator: usize,
    /// Nearest-rank p50 latency.
    pub p50_latency_ms: u64,
    /// Nearest-rank p95 latency.
    pub p95_latency_ms: u64,
    /// Actual-versus-estimated per-call records.
    pub accounting: Vec<StageCostRecord>,
}

/// Explicit per-mode case and provider-attempt denominators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeDenominatorsV2 {
    /// Every dataset case.
    pub total_cases: usize,
    /// Supported cases that reached a terminal non-failure artifact.
    pub completed_cases: usize,
    /// Supported cases with retained failures.
    pub failed_cases: usize,
    /// Cases excluded before provider invocation.
    pub unsupported_cases: usize,
    /// Reader calls actually attempted.
    pub reader_attempts: usize,
    /// Absolute-judge calls actually attempted.
    pub judge_attempts: usize,
}

/// Correct-answer numerator and failure-retaining denominator for one LongMemEval slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalAnswerSlice {
    /// Judge-supported answers in the slice.
    pub numerator: usize,
    /// Every case in the slice, including all terminal failures.
    pub denominator: usize,
}

/// Supported or intentionally unsupported retrieval metric artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum RetrievalMetricsV2 {
    /// Primary-mode retrieval metrics.
    Supported {
        /// Dataset-owned metric payload.
        metrics: Box<LongMemEvalRetrievalMetrics>,
    },
    /// Retrieval does not apply to the mode or dataset.
    Unsupported {
        /// Stable exclusion reason.
        reason: String,
    },
}

/// PersonaMem metrics for one report mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemModeMetricsV2 {
    /// Label-only answer accuracy and clustered slices.
    pub answer: PersonaMemAccuracyReport,
    /// PersonaMem supplies no authoritative retrieval labels.
    pub retrieval: SupportStatus,
}

/// LongMemEval metrics for one report mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalModeMetricsV2 {
    /// Metric payload schema version.
    pub schema_version: u32,
    /// Accuracy over all 500 answer cases.
    pub answers: LongMemEvalAnswerSlice,
    /// Accuracy over the 30 abstention cases.
    pub abstentions: LongMemEvalAnswerSlice,
    /// Answer accuracy for every official question type.
    pub question_type_slices: BTreeMap<String, LongMemEvalAnswerSlice>,
    /// Primary retrieval metrics or the exact control exclusion.
    pub retrieval: RetrievalMetricsV2,
    /// Retained terminal failure counts keyed by stable failure kind.
    pub failure_counts: BTreeMap<String, usize>,
    /// Exact selected absolute-judge model.
    pub judge_model: String,
    /// Versioned absolute-judge prompt.
    pub judge_prompt_version: String,
    /// Vendored upstream rubric contract version.
    pub rubric_version: String,
    /// Canonical digest over all exact rubric templates.
    pub rubric_bundle_sha256: String,
}

/// Dataset-owned per-mode metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    tag = "dataset",
    content = "metrics",
    deny_unknown_fields
)]
pub enum ExternalMemoryDatasetMetricsV2 {
    /// PersonaMem v1 label accuracy and unsupported retrieval recall.
    PersonaMem32k(Box<PersonaMemModeMetricsV2>),
    /// LongMemEval-S Cleaned answer and retrieval metrics.
    LongMemEvalSCleaned(Box<LongMemEvalModeMetricsV2>),
}

/// Exact reader identity and fit contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderContractV2 {
    /// Exact provider/model selector.
    pub model: String,
    /// Versioned shared prompt renderer.
    pub prompt_version: String,
    /// Maximum provider context tokens.
    pub context_window: u64,
    /// Reserved output tokens used by fit checks and dispatch.
    pub output_token_reserve: u64,
    /// Exact persisted estimator identifier.
    pub token_estimator: String,
}

impl ReaderContractV2 {
    /// Constructs the only supported reader estimator contract.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        prompt_version: impl Into<String>,
        context_window: u64,
        output_token_reserve: u64,
    ) -> Self {
        Self {
            model: model.into(),
            prompt_version: prompt_version.into(),
            context_window,
            output_token_reserve,
            token_estimator: TOKEN_ESTIMATOR_CHARS_DIV_4.to_string(),
        }
    }
}

/// Cumulative budget summary across formation and every report mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBudgetV2 {
    /// Configured cumulative ceiling.
    pub ceiling_usd: f64,
    /// Sum of every pre-call forecast.
    pub estimated_committed_usd: f64,
    /// Actual cost where available and forecast otherwise.
    pub actual_or_estimated_committed_usd: f64,
}

/// One independently promotable report authority lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityLaneV2 {
    /// Always false in runner and workflow output.
    pub authoritative: bool,
    /// Stable reason for informational status.
    pub reason: String,
    /// Null until a separate reviewed promotion links calibration.
    pub calibration_manifest_sha256: Option<String>,
    /// Null until a separate reviewed promotion links calibration.
    pub calibration_results_sha256: Option<String>,
}

/// Separate retrieval and answer authority boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportAuthorityV2 {
    /// Retrieval baseline authority.
    pub retrieval: AuthorityLaneV2,
    /// Generated-answer authority.
    pub answer: AuthorityLaneV2,
}

impl ReportAuthorityV2 {
    /// Constructs the only authority state emitted by benchmark execution.
    #[must_use]
    pub fn informational() -> Self {
        Self {
            retrieval: AuthorityLaneV2 {
                authoritative: false,
                reason: "retrieval-baseline-requires-manual-promotion".to_string(),
                calibration_manifest_sha256: None,
                calibration_results_sha256: None,
            },
            answer: AuthorityLaneV2 {
                authoritative: false,
                reason: "answer-baseline-requires-passing-human-calibration".to_string(),
                calibration_manifest_sha256: None,
                calibration_results_sha256: None,
            },
        }
    }
}

/// Complete report for one benchmark mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeReportV2 {
    /// Benchmark mode.
    pub mode: ExternalMemoryMode,
    /// Full ordered dataset denominator.
    pub cases: Vec<CaseReportV2>,
    /// Explicit terminal and provider-attempt denominators.
    pub denominators: ModeDenominatorsV2,
    /// Attempted cases by dataset category.
    pub category_slices: BTreeMap<String, usize>,
    /// Dataset-owned metrics for this mode.
    pub dataset_metrics: Option<ExternalMemoryDatasetMetricsV2>,
}

/// Hard-break external-memory run report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalMemoryReportV2 {
    /// Report schema version, always 2.
    pub schema_version: u32,
    /// Clock-normalized generation instant.
    pub generated_at: String,
    /// Dataset revision and file/package hashes.
    pub dataset_package: DatasetPackage,
    /// Fully resolved formation configuration.
    pub formation: ResolvedFormationConfig,
    /// Domain-separated formation digest.
    pub formation_hash: String,
    /// Exact shared reader and fit contract.
    pub reader_contract: ReaderContractV2,
    /// One cumulative budget summary.
    pub budget: ReportBudgetV2,
    /// Ordered stage metrics keyed by explicit `(stage, mode)` fields.
    pub stage_metrics: Vec<StageMetricsV2>,
    /// Reports ordered primary, no-memory, full-context, oracle-evidence.
    pub modes: Vec<ModeReportV2>,
    /// Separate informational retrieval and answer authority.
    pub authority: ReportAuthorityV2,
}

impl ExternalMemoryReportV2 {
    /// Returns the primary mode case artifacts.
    #[must_use]
    pub fn primary_cases(&self) -> &[CaseReportV2] {
        self.modes
            .iter()
            .find(|mode| mode.mode == ExternalMemoryMode::Primary)
            .map_or(&[], |mode| mode.cases.as_slice())
    }

    /// Serializes a validated report deterministically.
    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    fn validate(&self) -> Result<()> {
        use super::ExternalMemoryError;
        if self.schema_version != 2
            || self.reader_contract.model.trim().is_empty()
            || self.reader_contract.prompt_version.trim().is_empty()
            || self.reader_contract.context_window == 0
            || self.reader_contract.output_token_reserve == 0
            || self.reader_contract.token_estimator != TOKEN_ESTIMATOR_CHARS_DIV_4
        {
            return Err(ExternalMemoryError::InvalidConfig(
                "invalid V2 reader/report contract".to_string(),
            ));
        }
        self.formation.validate()?;
        if self.formation.canonical_hash()? != self.formation_hash {
            return Err(ExternalMemoryError::InvalidConfig(
                "formation hash does not match resolved configuration".to_string(),
            ));
        }
        if self.modes.len() != 4
            || self
                .modes
                .iter()
                .map(|mode| mode.mode)
                .ne(ExternalMemoryMode::ordered())
        {
            return Err(ExternalMemoryError::InvalidConfig(
                "V2 report must contain all modes in canonical order".to_string(),
            ));
        }
        for mode in &self.modes {
            let denominators = &mode.denominators;
            if denominators.total_cases
                != denominators.completed_cases
                    + denominators.failed_cases
                    + denominators.unsupported_cases
                || mode.cases.len() != denominators.total_cases
                || mode.cases.iter().any(|case| case.mode != mode.mode)
            {
                return Err(ExternalMemoryError::InvalidConfig(format!(
                    "invalid {:?} mode denominators",
                    mode.mode
                )));
            }
        }
        if self.authority.retrieval.authoritative
            || self.authority.answer.authoritative
            || self
                .authority
                .retrieval
                .calibration_manifest_sha256
                .is_some()
            || self
                .authority
                .retrieval
                .calibration_results_sha256
                .is_some()
            || self.authority.answer.calibration_manifest_sha256.is_some()
            || self.authority.answer.calibration_results_sha256.is_some()
        {
            return Err(ExternalMemoryError::InvalidConfig(
                "benchmark runner reports must remain informational".to_string(),
            ));
        }
        Ok(())
    }
}

/// Deterministic V2 report builder with injected clock and accounting.
#[derive(Clone)]
pub struct ExternalMemoryReportBuilder {
    generated_at: DateTime<Utc>,
    dataset_package: DatasetPackage,
    formation: ResolvedFormationConfig,
    formation_hash: String,
    reader_contract: ReaderContractV2,
    budget: ReportBudgetV2,
    stages: Vec<StageObservation>,
    cases: BTreeMap<ExternalMemoryMode, Vec<CaseReportV2>>,
    dataset_metrics: BTreeMap<ExternalMemoryMode, ExternalMemoryDatasetMetricsV2>,
}

impl ExternalMemoryReportBuilder {
    /// Creates a builder with all V2 top-level contracts resolved.
    #[must_use]
    pub fn new(
        generated_at: DateTime<Utc>,
        dataset_package: DatasetPackage,
        formation: ResolvedFormationConfig,
        formation_hash: String,
        reader_contract: ReaderContractV2,
        budget: ReportBudgetV2,
    ) -> Self {
        Self {
            generated_at,
            dataset_package,
            formation,
            formation_hash,
            reader_contract,
            budget,
            stages: Vec::new(),
            cases: BTreeMap::new(),
            dataset_metrics: BTreeMap::new(),
        }
    }

    /// Updates the cumulative budget summary used by partial reports.
    pub fn set_budget(&mut self, budget: ReportBudgetV2) {
        self.budget = budget;
    }

    /// Records one attempted stage, including failed call latency/accounting.
    pub fn record_stage(&mut self, observation: StageObservation) {
        self.stages.push(observation);
    }

    /// Records one completed, unsupported, or failed case artifact.
    pub fn record_case(&mut self, report: CaseReportV2) {
        self.cases.entry(report.mode).or_default().push(report);
    }

    /// Attaches one dataset-owned aggregate for an explicit mode.
    pub fn set_dataset_metrics(
        &mut self,
        mode: ExternalMemoryMode,
        metrics: ExternalMemoryDatasetMetricsV2,
    ) {
        self.dataset_metrics.insert(mode, metrics);
    }

    /// Finishes the report while retaining every mode and terminal artifact.
    #[must_use]
    pub fn finish(self) -> ExternalMemoryReportV2 {
        let mut reader_attempts = BTreeMap::<ExternalMemoryMode, usize>::new();
        let mut judge_attempts = BTreeMap::<ExternalMemoryMode, usize>::new();
        for observation in &self.stages {
            if let Some(mode) = observation.mode {
                match observation.stage {
                    StageName::Reader => *reader_attempts.entry(mode).or_default() += 1,
                    StageName::Judge => *judge_attempts.entry(mode).or_default() += 1,
                    _ => {}
                }
            }
        }
        let mut grouped =
            BTreeMap::<(StageName, Option<ExternalMemoryMode>), Vec<StageObservation>>::new();
        for stage in self.stages {
            grouped
                .entry((stage.stage, stage.mode))
                .or_default()
                .push(stage);
        }
        let stage_metrics = grouped
            .into_iter()
            .map(|((stage, mode), observations)| {
                let mut latencies = observations
                    .iter()
                    .map(|observation| observation.latency_ms)
                    .collect::<Vec<_>>();
                latencies.sort_unstable();
                StageMetricsV2 {
                    stage,
                    mode,
                    denominator: latencies.len(),
                    p50_latency_ms: nearest_rank(&latencies, 50),
                    p95_latency_ms: nearest_rank(&latencies, 95),
                    accounting: observations
                        .into_iter()
                        .filter_map(|observation| observation.accounting)
                        .collect(),
                }
            })
            .collect();
        let mut modes = Vec::with_capacity(4);
        for mode in ExternalMemoryMode::ordered() {
            let cases = self.cases.get(&mode).cloned().unwrap_or_default();
            let failed_cases = cases.iter().filter(|case| case.failure.is_some()).count();
            let unsupported_cases = cases
                .iter()
                .filter(|case| matches!(case.mode_support, SupportStatus::Unsupported { .. }))
                .count();
            let completed_cases = cases.len().saturating_sub(failed_cases + unsupported_cases);
            let mut category_slices = BTreeMap::new();
            for case in &cases {
                *category_slices.entry(case.category.clone()).or_insert(0) += 1;
            }
            modes.push(ModeReportV2 {
                mode,
                denominators: ModeDenominatorsV2 {
                    total_cases: cases.len(),
                    completed_cases,
                    failed_cases,
                    unsupported_cases,
                    reader_attempts: reader_attempts.get(&mode).copied().unwrap_or_default(),
                    judge_attempts: judge_attempts.get(&mode).copied().unwrap_or_default(),
                },
                cases,
                category_slices,
                dataset_metrics: self.dataset_metrics.get(&mode).cloned(),
            });
        }
        ExternalMemoryReportV2 {
            schema_version: 2,
            generated_at: self.generated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            dataset_package: self.dataset_package,
            formation: self.formation,
            formation_hash: self.formation_hash,
            reader_contract: self.reader_contract,
            budget: self.budget,
            stage_metrics,
            modes,
            authority: ReportAuthorityV2::informational(),
        }
    }
}

fn nearest_rank(sorted_values: &[u64], percentile: usize) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let rank = (sorted_values.len() * percentile).div_ceil(100).max(1);
    sorted_values[rank - 1]
}
