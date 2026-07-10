//! Backend-neutral chronological formation and retrieval harness.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use super::answer::{
    AbsoluteAnswerJudge, AbsoluteJudgeRequest, AbsoluteJudgeResponse, AnswerScore,
    AnswerScoreOutcome, AnswerScorer, ExternalMemoryMode, Reader, ReaderRequest, ReaderResponse,
    SupportStatus,
};
use super::cost::{
    BudgetLedger, NormalizedUsage, PricingSnapshotV1, StageCostRecord, StageName, UsageProvenance,
};
use super::dataset::{ChronologicalTurn, PreparedExternalMemoryCase};
use super::report::{CaseFailure, CaseReport, FailureKind, StageObservation};
use super::{ExternalMemoryError, Result};

/// Largest ranked source-occurrence depth accepted by the benchmark harness.
pub const MAX_RANKED_OCCURRENCE_DEPTH: usize = 50;

/// One occurrence-level source identity used by retrieval metrics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceOccurrenceRef {
    /// External session occurrence ID.
    pub session_source_id: String,
    /// External turn occurrence ID.
    pub turn_source_id: String,
}

/// One reversible external source reference returned with rendered evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSourceRef {
    /// External session occurrence ID.
    pub session_source_id: String,
    /// External turn occurrence ID.
    pub turn_source_id: String,
    /// Exact excerpt rendered into evidence.
    pub evidence: String,
}

/// Evidence exported from one backend under an explicit token budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceExport {
    /// Exact evidence string passed to the reader.
    pub rendered_evidence: String,
    /// Evidence tokens consumed under the backend's documented estimator.
    pub tokens_used: usize,
    /// Unique external source occurrences in authoritative retrieval rank order.
    pub ranked_source_refs: Vec<EvidenceOccurrenceRef>,
    /// Per-hit external source references whose excerpts were rendered for the reader.
    pub rendered_source_refs: Vec<EvidenceSourceRef>,
}

/// Backend contract implemented by MOA and reusable competitors.
#[async_trait]
pub trait ExternalMemoryBackend: Send {
    /// Resets all state for one hard isolation key.
    async fn reset(&mut self, isolation_key: &str) -> std::result::Result<(), String>;
    /// Ingests one validated chronological turn through the backend's production path.
    async fn ingest(&mut self, turn: &ChronologicalTurn) -> std::result::Result<(), String>;
    /// Settles or consolidates all ingested turns.
    async fn settle(&mut self) -> std::result::Result<(), String>;
    /// Retrieves and renders evidence under an explicit token budget.
    async fn retrieve(
        &mut self,
        query: &str,
        evidence_token_budget: usize,
        ranked_occurrence_depth: usize,
    ) -> std::result::Result<EvidenceExport, String>;
}

/// Runs reset, chronological ingest, settle, and retrieval for one case.
pub async fn run_retrieval_case<B: ExternalMemoryBackend>(
    backend: &mut B,
    case: &PreparedExternalMemoryCase,
    evidence_token_budget: usize,
    ranked_occurrence_depth: usize,
) -> Result<EvidenceExport> {
    if evidence_token_budget == 0 {
        return Err(ExternalMemoryError::InvalidConfig(
            "evidence-token-budget must be positive".to_string(),
        ));
    }
    validate_ranked_occurrence_depth(ranked_occurrence_depth)?;
    backend
        .reset(&case.case.isolation_key)
        .await
        .map_err(ExternalMemoryError::Backend)?;
    for turn in &case.chronological_turns {
        backend
            .ingest(turn)
            .await
            .map_err(ExternalMemoryError::Backend)?;
    }
    backend
        .settle()
        .await
        .map_err(ExternalMemoryError::Backend)?;
    let evidence = backend
        .retrieve(
            &case.case.question,
            evidence_token_budget,
            ranked_occurrence_depth,
        )
        .await
        .map_err(ExternalMemoryError::Backend)?;
    validate_evidence_export(&evidence, evidence_token_budget, ranked_occurrence_depth)?;
    Ok(evidence)
}

fn validate_ranked_occurrence_depth(depth: usize) -> Result<()> {
    if !(1..=MAX_RANKED_OCCURRENCE_DEPTH).contains(&depth) {
        return Err(ExternalMemoryError::InvalidConfig(format!(
            "ranked-occurrence-depth must be in 1..={MAX_RANKED_OCCURRENCE_DEPTH}"
        )));
    }
    Ok(())
}

/// Validates the independent ranked and rendered views returned by a backend.
pub fn validate_evidence_export(
    evidence: &EvidenceExport,
    evidence_token_budget: usize,
    ranked_occurrence_depth: usize,
) -> Result<()> {
    validate_ranked_occurrence_depth(ranked_occurrence_depth)?;
    if evidence.tokens_used > evidence_token_budget {
        return Err(ExternalMemoryError::Backend(format!(
            "backend used {} evidence tokens with budget {evidence_token_budget}",
            evidence.tokens_used
        )));
    }
    if evidence.ranked_source_refs.len() > ranked_occurrence_depth {
        return Err(ExternalMemoryError::Backend(format!(
            "backend returned {} ranked source occurrences with requested ranked occurrence depth {ranked_occurrence_depth}",
            evidence.ranked_source_refs.len()
        )));
    }

    let mut ranked_positions = HashMap::new();
    for (rank, occurrence) in evidence.ranked_source_refs.iter().enumerate() {
        if ranked_positions.insert(occurrence, rank).is_some() {
            return Err(ExternalMemoryError::Backend(format!(
                "duplicate ranked source occurrence {}/{}",
                occurrence.session_source_id, occurrence.turn_source_id
            )));
        }
    }

    let mut first_rendered = HashSet::new();
    let mut previous_rank = None;
    for rendered in &evidence.rendered_source_refs {
        let occurrence = EvidenceOccurrenceRef {
            session_source_id: rendered.session_source_id.clone(),
            turn_source_id: rendered.turn_source_id.clone(),
        };
        let Some(rank) = ranked_positions.get(&occurrence).copied() else {
            return Err(ExternalMemoryError::Backend(format!(
                "rendered source occurrence {}/{} is absent from ranked source occurrences",
                occurrence.session_source_id, occurrence.turn_source_id
            )));
        };
        if first_rendered.insert(occurrence)
            && previous_rank.is_some_and(|previous| rank <= previous)
        {
            return Err(ExternalMemoryError::Backend(
                "rendered source occurrence order is inconsistent with ranked order".to_string(),
            ));
        }
        previous_rank = Some(previous_rank.map_or(rank, |previous| previous.max(rank)));
    }
    Ok(())
}

/// Forecast inputs for one paid reader or judge call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaidStagePlan {
    /// Immutable model/date pricing used by the budget ledger.
    pub pricing: PricingSnapshotV1,
    /// Provider-neutral usage forecast recorded before the call.
    pub estimated_usage: NormalizedUsage,
}

/// Per-run execution settings shared across external-memory cases.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalMemoryExecutionSettings {
    /// Explicit cap passed to backend retrieval and evidence rendering.
    pub evidence_token_budget: usize,
    /// Unique ranked source-occurrence depth retained for retrieval metrics.
    pub ranked_occurrence_depth: usize,
    /// Maximum duration of each reader or absolute-judge call.
    pub paid_call_timeout: Duration,
    /// Versioned reader prompt placed in every reader request.
    pub reader_prompt_version: String,
    /// Dataset-independent rubric placed in every absolute-judge request.
    pub absolute_judge_rubric: String,
    /// Versioned absolute-judge prompt placed in every judge request.
    pub absolute_judge_prompt_version: String,
    /// Forecast and pricing for each reader call.
    pub reader: PaidStagePlan,
    /// Forecast and pricing for each absolute-judge call.
    pub absolute_judge: PaidStagePlan,
}

impl ExternalMemoryExecutionSettings {
    fn validate(&self) -> Result<()> {
        if self.evidence_token_budget == 0 {
            return Err(ExternalMemoryError::InvalidConfig(
                "evidence-token-budget must be positive".to_string(),
            ));
        }
        validate_ranked_occurrence_depth(self.ranked_occurrence_depth)?;
        if self.paid_call_timeout.is_zero() {
            return Err(ExternalMemoryError::InvalidConfig(
                "paid-call timeout must be positive".to_string(),
            ));
        }
        for (name, value) in [
            ("reader prompt version", self.reader_prompt_version.as_str()),
            ("absolute judge rubric", self.absolute_judge_rubric.as_str()),
            (
                "absolute judge prompt version",
                self.absolute_judge_prompt_version.as_str(),
            ),
        ] {
            if value.trim().is_empty() {
                return Err(ExternalMemoryError::InvalidConfig(format!(
                    "{name} must not be blank"
                )));
            }
        }

        // A validation-only ledger reuses the authoritative pricing/usage
        // validation without consuming the real run budget.
        let mut validation_ledger = BudgetLedger::new(f64::MAX)?;
        validation_ledger.forecast(
            StageName::Reader,
            Some(ExternalMemoryMode::Primary),
            self.reader.pricing.clone(),
            self.reader.estimated_usage.clone(),
        )?;
        validation_ledger.forecast(
            StageName::Judge,
            Some(ExternalMemoryMode::Primary),
            self.absolute_judge.pricing.clone(),
            self.absolute_judge.estimated_usage.clone(),
        )?;
        Ok(())
    }
}

/// Retained artifacts from one completed or failed case execution.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseExecutionOutcome {
    /// Partial or completed per-case report.
    pub case_report: CaseReport,
    /// Every paid stage that was actually attempted.
    pub stage_observations: Vec<StageObservation>,
}

/// Generic backend-neutral generated-answer executor.
///
/// The reader, dataset-owned scorer, and dataset-independent absolute judge are
/// distinct type parameters and fields. The cumulative ledger is owned by the
/// executor so one budget applies across every case in a run.
pub struct ExternalMemoryExecutor<R, S, J> {
    reader: R,
    scorer: S,
    absolute_judge: J,
    settings: ExternalMemoryExecutionSettings,
    ledger: BudgetLedger,
    budget_exhausted: bool,
}

impl<R, S, J> ExternalMemoryExecutor<R, S, J> {
    /// Creates an executor after validating all non-case-specific configuration.
    pub fn new(
        reader: R,
        scorer: S,
        absolute_judge: J,
        settings: ExternalMemoryExecutionSettings,
        budget_usd: f64,
    ) -> Result<Self> {
        settings.validate()?;
        Ok(Self {
            reader,
            scorer,
            absolute_judge,
            settings,
            ledger: BudgetLedger::new(budget_usd)?,
            budget_exhausted: false,
        })
    }

    /// Returns immutable forecast/actual accounting accumulated for the run.
    #[must_use]
    pub fn accounting_records(&self) -> &[StageCostRecord] {
        self.ledger.records()
    }

    /// Reports whether a forecast or actual response exhausted the run budget.
    #[must_use]
    pub fn budget_exhausted(&self) -> bool {
        self.budget_exhausted
    }
}

impl<R, S, J> ExternalMemoryExecutor<R, S, J>
where
    R: Reader,
    S: AnswerScorer,
    J: AbsoluteAnswerJudge,
{
    /// Runs retrieval, reader, dataset scoring, and absolute judging for one case.
    ///
    /// Provider, parse, timeout, backend, and budget failures are returned as
    /// retained partial case artifacts. Once the cumulative budget is exhausted,
    /// later calls return budget failures without invoking any backend or paid
    /// component.
    pub async fn run_case<B: ExternalMemoryBackend>(
        &mut self,
        backend: &mut B,
        case: &PreparedExternalMemoryCase,
    ) -> CaseExecutionOutcome {
        if self.budget_exhausted {
            return failed_outcome(
                case,
                String::new(),
                None,
                None,
                None,
                FailureKind::Budget,
                "run budget was already exhausted",
                Vec::new(),
            );
        }

        let evidence = match run_retrieval_case(
            backend,
            case,
            self.settings.evidence_token_budget,
            self.settings.ranked_occurrence_depth,
        )
        .await
        {
            Ok(evidence) => evidence,
            Err(error) => {
                return failed_outcome(
                    case,
                    String::new(),
                    None,
                    None,
                    None,
                    FailureKind::Backend,
                    error.to_string(),
                    Vec::new(),
                );
            }
        };
        let rendered_evidence = evidence.rendered_evidence;
        let mut stage_observations = Vec::with_capacity(2);

        let reader_record_id = match self.ledger.forecast(
            StageName::Reader,
            Some(ExternalMemoryMode::Primary),
            self.settings.reader.pricing.clone(),
            self.settings.reader.estimated_usage.clone(),
        ) {
            Ok(record_id) => record_id,
            Err(error) => {
                self.budget_exhausted = true;
                return failed_outcome(
                    case,
                    rendered_evidence,
                    None,
                    None,
                    None,
                    FailureKind::Budget,
                    error.to_string(),
                    stage_observations,
                );
            }
        };
        let reader_request = ReaderRequest {
            isolation_key: case.case.isolation_key.clone(),
            question: case.case.question.clone(),
            options: case.case.options.clone(),
            rendered_evidence: rendered_evidence.clone(),
            prompt_version: self.settings.reader_prompt_version.clone(),
        };
        let reader_response = match timeout(
            self.settings.paid_call_timeout,
            self.reader.answer(reader_request),
        )
        .await
        {
            Err(_) => {
                stage_observations.push(self.stage_observation(
                    StageName::Reader,
                    reader_record_id,
                    timeout_latency_ms(self.settings.paid_call_timeout),
                ));
                return failed_outcome(
                    case,
                    rendered_evidence,
                    None,
                    None,
                    None,
                    FailureKind::Timeout,
                    "reader call timed out",
                    stage_observations,
                );
            }
            Ok(Err(error)) => {
                stage_observations.push(self.stage_observation(
                    StageName::Reader,
                    reader_record_id,
                    0,
                ));
                return failed_outcome(
                    case,
                    rendered_evidence,
                    None,
                    None,
                    None,
                    FailureKind::Provider,
                    format!("reader provider failure: {error}"),
                    stage_observations,
                );
            }
            Ok(Ok(response)) => response,
        };
        if let Err(error) =
            validate_reader_response(&reader_response, &self.settings.reader_prompt_version)
        {
            stage_observations.push(self.stage_observation(
                StageName::Reader,
                reader_record_id,
                reader_response.latency_ms,
            ));
            return failed_outcome(
                case,
                rendered_evidence,
                None,
                None,
                None,
                FailureKind::Parse,
                error,
                stage_observations,
            );
        }
        let reader_accounting = self
            .ledger
            .record_actual(reader_record_id, reader_response.usage.clone());
        stage_observations.push(self.stage_observation(
            StageName::Reader,
            reader_record_id,
            reader_response.latency_ms,
        ));
        if let Err(error) = reader_accounting {
            self.budget_exhausted = true;
            return failed_outcome(
                case,
                rendered_evidence,
                Some(reader_response),
                None,
                None,
                FailureKind::Budget,
                error.to_string(),
                stage_observations,
            );
        }

        let (support_status, answer_score) = match self.scorer.score(&case.case, &reader_response) {
            Ok(AnswerScoreOutcome::Supported(score)) if answer_score_is_valid(&score) => {
                (SupportStatus::Supported, Some(score))
            }
            Ok(AnswerScoreOutcome::Supported(_)) => {
                return failed_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    None,
                    None,
                    FailureKind::Parse,
                    "dataset scorer returned an invalid answer score",
                    stage_observations,
                );
            }
            Ok(AnswerScoreOutcome::Unsupported { reason }) if !reason.trim().is_empty() => {
                (SupportStatus::Unsupported { reason }, None)
            }
            Ok(AnswerScoreOutcome::Unsupported { .. }) => {
                return failed_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    None,
                    None,
                    FailureKind::Parse,
                    "dataset scorer returned a blank unsupported reason",
                    stage_observations,
                );
            }
            Err(error) => {
                return failed_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    None,
                    None,
                    FailureKind::Parse,
                    format!("dataset scoring failure: {error}"),
                    stage_observations,
                );
            }
        };

        let judge_record_id = match self.ledger.forecast(
            StageName::Judge,
            Some(ExternalMemoryMode::Primary),
            self.settings.absolute_judge.pricing.clone(),
            self.settings.absolute_judge.estimated_usage.clone(),
        ) {
            Ok(record_id) => record_id,
            Err(error) => {
                self.budget_exhausted = true;
                return failed_scored_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    answer_score,
                    None,
                    support_status,
                    FailureKind::Budget,
                    error.to_string(),
                    stage_observations,
                );
            }
        };
        let judge_request = AbsoluteJudgeRequest {
            question: case.case.question.clone(),
            reference_answer: case.case.answer.clone(),
            candidate_answer: reader_response.answer.clone(),
            rubric: self.settings.absolute_judge_rubric.clone(),
            prompt_version: self.settings.absolute_judge_prompt_version.clone(),
        };
        let judge_response = match timeout(
            self.settings.paid_call_timeout,
            self.absolute_judge.judge(judge_request),
        )
        .await
        {
            Err(_) => {
                stage_observations.push(self.stage_observation(
                    StageName::Judge,
                    judge_record_id,
                    timeout_latency_ms(self.settings.paid_call_timeout),
                ));
                return failed_scored_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    answer_score,
                    None,
                    support_status,
                    FailureKind::Timeout,
                    "absolute judge call timed out",
                    stage_observations,
                );
            }
            Ok(Err(error)) => {
                stage_observations.push(self.stage_observation(
                    StageName::Judge,
                    judge_record_id,
                    0,
                ));
                return failed_scored_outcome(
                    case,
                    rendered_evidence,
                    Some(reader_response),
                    answer_score,
                    None,
                    support_status,
                    FailureKind::Provider,
                    format!("absolute judge provider failure: {error}"),
                    stage_observations,
                );
            }
            Ok(Ok(response)) => response,
        };
        if let Err(error) = validate_judge_response(
            &judge_response,
            &self.settings.absolute_judge_prompt_version,
        ) {
            stage_observations.push(self.stage_observation(
                StageName::Judge,
                judge_record_id,
                judge_response.latency_ms,
            ));
            return failed_scored_outcome(
                case,
                rendered_evidence,
                Some(reader_response),
                answer_score,
                None,
                support_status,
                FailureKind::Parse,
                error,
                stage_observations,
            );
        }
        let judge_accounting = self
            .ledger
            .record_actual(judge_record_id, judge_response.usage.clone());
        stage_observations.push(self.stage_observation(
            StageName::Judge,
            judge_record_id,
            judge_response.latency_ms,
        ));
        if let Err(error) = judge_accounting {
            self.budget_exhausted = true;
            return failed_scored_outcome(
                case,
                rendered_evidence,
                Some(reader_response),
                answer_score,
                Some(judge_response),
                support_status,
                FailureKind::Budget,
                error.to_string(),
                stage_observations,
            );
        }

        CaseExecutionOutcome {
            case_report: CaseReport::completed(
                &case.case.isolation_key,
                &case.case.category,
                rendered_evidence,
                support_status.clone(),
            )
            .with_generated_answer_outcome(
                reader_response,
                support_status,
                answer_score,
                Some(judge_response),
            ),
            stage_observations,
        }
    }

    fn stage_observation(
        &self,
        stage: StageName,
        record_id: usize,
        latency_ms: u64,
    ) -> StageObservation {
        StageObservation {
            stage,
            mode: Some(ExternalMemoryMode::Primary),
            latency_ms,
            accounting: self.ledger.records().get(record_id).cloned(),
        }
    }
}

fn validate_reader_response(
    response: &ReaderResponse,
    prompt_version: &str,
) -> std::result::Result<(), String> {
    if response.model.trim().is_empty() {
        return Err("reader response omitted its model".to_string());
    }
    if response.prompt_version != prompt_version {
        return Err("reader response prompt version did not match the request".to_string());
    }
    if response.usage.provenance != UsageProvenance::Actual {
        return Err("reader response usage was not normalized as actual".to_string());
    }
    Ok(())
}

fn validate_judge_response(
    response: &AbsoluteJudgeResponse,
    prompt_version: &str,
) -> std::result::Result<(), String> {
    if response.model.trim().is_empty() {
        return Err("absolute judge response omitted its model".to_string());
    }
    if response.prompt_version != prompt_version {
        return Err("absolute judge response prompt version did not match the request".to_string());
    }
    if response.usage.provenance != UsageProvenance::Actual {
        return Err("absolute judge response usage was not normalized as actual".to_string());
    }
    Ok(())
}

fn answer_score_is_valid(score: &AnswerScore) -> bool {
    !score.metric.trim().is_empty() && score.value.is_finite() && score.denominator > 0
}

#[allow(clippy::too_many_arguments)]
fn failed_scored_outcome(
    case: &PreparedExternalMemoryCase,
    rendered_evidence: String,
    reader: Option<ReaderResponse>,
    answer_score: Option<AnswerScore>,
    absolute_judge: Option<AbsoluteJudgeResponse>,
    support_status: SupportStatus,
    kind: FailureKind,
    message: impl Into<String>,
    stage_observations: Vec<StageObservation>,
) -> CaseExecutionOutcome {
    let mut outcome = failed_outcome(
        case,
        rendered_evidence,
        reader,
        answer_score,
        absolute_judge,
        kind,
        message,
        stage_observations,
    );
    outcome.case_report.answer_score_support = support_status;
    outcome
}

#[allow(clippy::too_many_arguments)]
fn failed_outcome(
    case: &PreparedExternalMemoryCase,
    rendered_evidence: String,
    reader: Option<ReaderResponse>,
    answer_score: Option<AnswerScore>,
    absolute_judge: Option<AbsoluteJudgeResponse>,
    kind: FailureKind,
    message: impl Into<String>,
    stage_observations: Vec<StageObservation>,
) -> CaseExecutionOutcome {
    let message = message.into();
    CaseExecutionOutcome {
        case_report: CaseReport {
            isolation_key: case.case.isolation_key.clone(),
            category: case.case.category.clone(),
            mode: ExternalMemoryMode::Primary,
            mode_support: SupportStatus::Supported,
            rendered_evidence,
            rendered_evidence_tokens: 0,
            answer_score_support: SupportStatus::Unsupported {
                reason: message.clone(),
            },
            reader,
            answer_score,
            absolute_judge,
            failure: Some(CaseFailure {
                isolation_key: case.case.isolation_key.clone(),
                kind,
                message,
            }),
        },
        stage_observations,
    }
}

fn timeout_latency_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod ranked_occurrence {
    use super::{
        EvidenceExport, EvidenceOccurrenceRef, EvidenceSourceRef, validate_evidence_export,
    };

    fn occurrence(session: &str, turn: &str) -> EvidenceOccurrenceRef {
        EvidenceOccurrenceRef {
            session_source_id: session.to_string(),
            turn_source_id: turn.to_string(),
        }
    }

    fn rendered(session: &str, turn: &str) -> EvidenceSourceRef {
        EvidenceSourceRef {
            session_source_id: session.to_string(),
            turn_source_id: turn.to_string(),
            evidence: format!("{session}/{turn}"),
        }
    }

    fn export() -> EvidenceExport {
        EvidenceExport {
            rendered_evidence: "one two three".to_string(),
            tokens_used: 3,
            ranked_source_refs: vec![
                occurrence("s1", "t1"),
                occurrence("s1", "t2"),
                occurrence("s2", "t3"),
            ],
            rendered_source_refs: vec![rendered("s1", "t1"), rendered("s2", "t3")],
        }
    }

    #[test]
    fn ranked_occurrence_accepts_separate_ranked_and_rendered_views() {
        // Pins: reader evidence may be a strict prefix/subset of ranked metric occurrences.
        validate_evidence_export(&export(), 8, 3).expect("valid evidence views");
    }

    #[test]
    fn ranked_occurrence_rejects_duplicate_unknown_and_reordered_rendered_refs() {
        // Pins: metric inputs are unique and rendered first-occurrence order follows ranking.
        let mut duplicate = export();
        duplicate.ranked_source_refs.push(occurrence("s1", "t1"));
        assert!(
            validate_evidence_export(&duplicate, 8, 4)
                .expect_err("duplicate ranked occurrence")
                .to_string()
                .contains("duplicate ranked source occurrence")
        );

        let mut unknown = export();
        unknown.rendered_source_refs.push(rendered("s9", "t9"));
        assert!(
            validate_evidence_export(&unknown, 8, 3)
                .expect_err("unknown rendered occurrence")
                .to_string()
                .contains("absent from ranked source occurrences")
        );

        let mut reordered = export();
        reordered.rendered_source_refs = vec![rendered("s2", "t3"), rendered("s1", "t1")];
        assert!(
            validate_evidence_export(&reordered, 8, 3)
                .expect_err("reordered rendered occurrences")
                .to_string()
                .contains("rendered source occurrence order")
        );
    }

    #[test]
    fn ranked_occurrence_enforces_requested_depth_and_evidence_budget() {
        // Pins: the backend may return fewer, but never more, occurrences than requested.
        assert!(
            validate_evidence_export(&export(), 8, 2)
                .expect_err("ranked depth overrun")
                .to_string()
                .contains("requested ranked occurrence depth 2")
        );
        assert!(
            validate_evidence_export(&export(), 2, 3)
                .expect_err("evidence budget overrun")
                .to_string()
                .contains("used 3 evidence tokens with budget 2")
        );
    }
}
