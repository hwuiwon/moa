use std::collections::VecDeque;
use std::future::pending;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use moa_eval::external_memory::answer::{
    AbsoluteAnswerJudge, AbsoluteJudgeRequest, AbsoluteJudgeResponse, AnswerScore,
    AnswerScoreOutcome, AnswerScorer, ExternalMemoryMode, Reader, ReaderRequest, ReaderResponse,
    SupportStatus,
};
use moa_eval::external_memory::cost::{
    NormalizedUsage, PricingSnapshot, StageName, UsageProvenance,
};
use moa_eval::external_memory::dataset::{
    ChronologicalTurn, DatasetFileProvenance, DatasetPackage, DatasetPackageManifest,
    DatasetPackageSource, EvidenceLabels, ExternalMemoryCase, ExternalMemorySession,
    ExternalMemoryTurn, PreparedExternalMemoryCase, validate_case,
};
use moa_eval::external_memory::formation::{
    ComponentConfig, ConsolidationSettings, EmbeddingConfig, EntityBlockingConfig, FormationMode,
    ResolvedFormationConfig,
};
use moa_eval::external_memory::harness::{
    EvidenceExport, EvidenceOccurrenceRef, EvidenceSourceRef, ExternalMemoryBackend,
    ExternalMemoryExecutionSettings, ExternalMemoryExecutor, PaidStagePlan,
};
use moa_eval::external_memory::report::{
    ExternalMemoryReportBuilder, FailureKind, ReaderContractV2, ReportBudgetV2,
};

#[derive(Default)]
struct FakeBackend {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ExternalMemoryBackend for FakeBackend {
    async fn reset(&mut self, isolation_key: &str) -> Result<(), String> {
        self.calls
            .lock()
            .expect("backend calls lock")
            .push(format!("reset:{isolation_key}"));
        Ok(())
    }

    async fn ingest(&mut self, turn: &ChronologicalTurn) -> Result<(), String> {
        self.calls
            .lock()
            .expect("backend calls lock")
            .push(format!("ingest:{}", turn.turn_source_id));
        Ok(())
    }

    async fn settle(&mut self) -> Result<(), String> {
        self.calls
            .lock()
            .expect("backend calls lock")
            .push("settle".to_string());
        Ok(())
    }

    async fn retrieve(
        &mut self,
        _query: &str,
        evidence_token_budget: usize,
        ranked_occurrence_depth: usize,
    ) -> Result<EvidenceExport, String> {
        self.calls.lock().expect("backend calls lock").push(format!(
            "retrieve:{evidence_token_budget}:{ranked_occurrence_depth}"
        ));
        Ok(EvidenceExport {
            rendered_evidence: "<knowledge_context>Ada owns it.</knowledge_context>".to_string(),
            tokens_used: 12,
            ranked_source_refs: vec![EvidenceOccurrenceRef {
                session_source_id: "session-1".to_string(),
                turn_source_id: "turn-1".to_string(),
            }],
            rendered_source_refs: vec![EvidenceSourceRef {
                session_source_id: "session-1".to_string(),
                turn_source_id: "turn-1".to_string(),
                evidence: "Ada owns it.".to_string(),
            }],
        })
    }
}

enum ReaderBehavior {
    Response(ReaderResponse),
    ProviderFailure,
    Pending,
}

struct FakeReader {
    behaviors: Mutex<VecDeque<ReaderBehavior>>,
    requests: Arc<Mutex<Vec<ReaderRequest>>>,
}

impl FakeReader {
    fn new(behaviors: impl IntoIterator<Item = ReaderBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors.into_iter().collect()),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Reader for FakeReader {
    async fn answer(&self, request: ReaderRequest) -> Result<ReaderResponse, String> {
        self.requests
            .lock()
            .expect("reader requests lock")
            .push(request);
        let behavior = self
            .behaviors
            .lock()
            .expect("reader behavior lock")
            .pop_front()
            .expect("reader behavior should be scripted");
        match behavior {
            ReaderBehavior::Response(response) => Ok(response),
            ReaderBehavior::ProviderFailure => Err("provider unavailable".to_string()),
            ReaderBehavior::Pending => pending().await,
        }
    }
}

struct FakeScorer {
    calls: Arc<Mutex<usize>>,
    response: Result<AnswerScoreOutcome, String>,
}

impl FakeScorer {
    fn exact() -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            response: Ok(AnswerScoreOutcome::Supported(AnswerScore {
                metric: "exact_match".to_string(),
                value: 1.0,
                denominator: 1,
            })),
        }
    }

    fn unsupported(reason: &str) -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            response: Ok(AnswerScoreOutcome::Unsupported {
                reason: reason.to_string(),
            }),
        }
    }
}

impl AnswerScorer for FakeScorer {
    fn score(
        &self,
        _case: &ExternalMemoryCase,
        _answer: &ReaderResponse,
    ) -> Result<AnswerScoreOutcome, String> {
        *self.calls.lock().expect("scorer calls lock") += 1;
        self.response.clone()
    }
}

enum JudgeBehavior {
    Response(AbsoluteJudgeResponse),
}

struct FakeAbsoluteJudge {
    behaviors: Mutex<VecDeque<JudgeBehavior>>,
    requests: Arc<Mutex<Vec<AbsoluteJudgeRequest>>>,
}

impl FakeAbsoluteJudge {
    fn successful(count: usize) -> Self {
        Self {
            behaviors: Mutex::new(
                (0..count)
                    .map(|_| JudgeBehavior::Response(judge_response()))
                    .collect(),
            ),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl AbsoluteAnswerJudge for FakeAbsoluteJudge {
    async fn judge(&self, request: AbsoluteJudgeRequest) -> Result<AbsoluteJudgeResponse, String> {
        self.requests
            .lock()
            .expect("judge requests lock")
            .push(request);
        match self
            .behaviors
            .lock()
            .expect("judge behavior lock")
            .pop_front()
            .expect("judge behavior should be scripted")
        {
            JudgeBehavior::Response(response) => Ok(response),
        }
    }
}

#[tokio::test]
async fn external_memory_execution_keeps_reader_scorer_and_absolute_judge_distinct() {
    // Pins: reader generation, dataset scoring, and absolute judging are three
    // independent calls, and the judge request has no comparator input.
    let reader = FakeReader::new([ReaderBehavior::Response(reader_response(
        UsageProvenance::Actual,
        20,
    ))]);
    let reader_requests = reader.requests.clone();
    let scorer = FakeScorer::exact();
    let scorer_calls = scorer.calls.clone();
    let judge = FakeAbsoluteJudge::successful(1);
    let judge_requests = judge.requests.clone();
    let mut executor = ExternalMemoryExecutor::new(reader, scorer, judge, settings(), 1.0)
        .expect("valid executor config");
    let mut backend = FakeBackend::default();

    let outcome = executor
        .run_case(&mut backend, &prepared_case("case-1"))
        .await;

    assert!(outcome.case_report.failure.is_none());
    assert_eq!(
        reader_requests.lock().expect("reader requests lock").len(),
        1
    );
    assert_eq!(*scorer_calls.lock().expect("scorer calls lock"), 1);
    let judge_requests = judge_requests.lock().expect("judge requests lock");
    assert_eq!(judge_requests.len(), 1);
    let judge_json = serde_json::to_value(&judge_requests[0]).expect("serialize judge request");
    assert_eq!(judge_json["candidate_answer"], "Ada");
    assert!(judge_json.get("baseline").is_none());
    assert!(judge_json.get("comparator").is_none());
    assert_eq!(outcome.stage_observations.len(), 2);
    assert_eq!(outcome.stage_observations[0].stage, StageName::Reader);
    assert_eq!(outcome.stage_observations[1].stage, StageName::Judge);
    assert_eq!(executor.accounting_records().len(), 2);
    for record in executor.accounting_records() {
        assert_eq!(
            record
                .actual_usage
                .as_ref()
                .expect("successful paid call records actual usage")
                .provenance,
            UsageProvenance::Actual
        );
        assert!(record.actual_cost_usd.is_some());
    }
}

#[tokio::test]
async fn external_memory_execution_unsupported_score_continues_to_absolute_judge() {
    // Pins: LongMemEval's lack of a deterministic answer scorer is typed support
    // state, not a parse failure, and the absolute judge remains authoritative.
    let reader = FakeReader::new([ReaderBehavior::Response(reader_response(
        UsageProvenance::Actual,
        20,
    ))]);
    let scorer = FakeScorer::unsupported("longmemeval-requires-absolute-judge");
    let judge = FakeAbsoluteJudge::successful(1);
    let judge_requests = judge.requests.clone();
    let mut executor = ExternalMemoryExecutor::new(reader, scorer, judge, settings(), 1.0)
        .expect("valid executor config");
    let mut backend = FakeBackend::default();

    let outcome = executor
        .run_case(&mut backend, &prepared_case("unsupported-score"))
        .await;

    assert!(outcome.case_report.failure.is_none());
    assert_eq!(outcome.case_report.answer_score, None);
    assert!(outcome.case_report.absolute_judge.is_some());
    assert_eq!(
        outcome.case_report.answer_score_support,
        SupportStatus::Unsupported {
            reason: "longmemeval-requires-absolute-judge".to_string(),
        }
    );
    assert_eq!(judge_requests.lock().expect("judge requests lock").len(), 1);
}

#[tokio::test]
async fn external_memory_execution_forecasts_before_calls_and_keeps_exhaustion_sticky() {
    // Pins: an over-budget forecast prevents the paid call, and exhaustion
    // prevents later cases from resuming backend or paid work.
    let reader = FakeReader::new([ReaderBehavior::Response(reader_response(
        UsageProvenance::Actual,
        10,
    ))]);
    let reader_requests = reader.requests.clone();
    let scorer = FakeScorer::exact();
    let scorer_calls = scorer.calls.clone();
    let judge = FakeAbsoluteJudge::successful(1);
    let judge_requests = judge.requests.clone();
    let mut executor = ExternalMemoryExecutor::new(reader, scorer, judge, settings(), 0.000_005)
        .expect("positive budget config");
    let mut backend = FakeBackend::default();
    let backend_calls = backend.calls.clone();

    let first = executor
        .run_case(&mut backend, &prepared_case("case-1"))
        .await;
    let calls_after_first = backend_calls.lock().expect("backend calls lock").len();
    let second = executor
        .run_case(&mut backend, &prepared_case("case-2"))
        .await;

    assert_eq!(failure_kind(&first), FailureKind::Budget);
    assert_eq!(failure_kind(&second), FailureKind::Budget);
    assert!(executor.budget_exhausted());
    assert!(executor.accounting_records().is_empty());
    assert_eq!(
        reader_requests.lock().expect("reader requests lock").len(),
        0
    );
    assert_eq!(*scorer_calls.lock().expect("scorer calls lock"), 0);
    assert_eq!(judge_requests.lock().expect("judge requests lock").len(), 0);
    assert_eq!(
        backend_calls.lock().expect("backend calls lock").len(),
        calls_after_first,
        "the exhausted run must skip the next case before backend execution"
    );
}

#[tokio::test]
async fn external_memory_execution_records_actual_overage_and_skips_later_stages() {
    // Pins: actual usage is persisted before the budget recheck fails; judge,
    // scorer, and later cases are skipped after that exhaustion.
    let reader = FakeReader::new([ReaderBehavior::Response(reader_response(
        UsageProvenance::Actual,
        20,
    ))]);
    let reader_requests = reader.requests.clone();
    let scorer = FakeScorer::exact();
    let scorer_calls = scorer.calls.clone();
    let judge = FakeAbsoluteJudge::successful(1);
    let judge_requests = judge.requests.clone();
    let mut executor = ExternalMemoryExecutor::new(reader, scorer, judge, settings(), 0.000_015)
        .expect("positive budget config");
    let mut backend = FakeBackend::default();
    let backend_calls = backend.calls.clone();

    let first = executor
        .run_case(&mut backend, &prepared_case("case-1"))
        .await;
    let calls_after_first = backend_calls.lock().expect("backend calls lock").len();
    let second = executor
        .run_case(&mut backend, &prepared_case("case-2"))
        .await;

    assert_eq!(failure_kind(&first), FailureKind::Budget);
    assert!(first.case_report.reader.is_some());
    assert_eq!(first.stage_observations.len(), 1);
    assert_eq!(executor.accounting_records().len(), 1);
    assert_eq!(
        executor.accounting_records()[0]
            .actual_usage
            .as_ref()
            .expect("over-budget actual usage remains recorded")
            .input_tokens_uncached,
        20
    );
    assert_eq!(failure_kind(&second), FailureKind::Budget);
    assert_eq!(
        reader_requests.lock().expect("reader requests lock").len(),
        1
    );
    assert_eq!(*scorer_calls.lock().expect("scorer calls lock"), 0);
    assert_eq!(judge_requests.lock().expect("judge requests lock").len(), 0);
    assert_eq!(
        backend_calls.lock().expect("backend calls lock").len(),
        calls_after_first
    );
}

#[tokio::test]
async fn external_memory_execution_retains_timeout_provider_and_parse_denominators() {
    // Pins: failures remain explicit case and stage denominator contributions
    // instead of disappearing from a partial report.
    let outcomes = vec![
        failed_reader_outcome(ReaderBehavior::Pending).await,
        failed_reader_outcome(ReaderBehavior::ProviderFailure).await,
        failed_reader_outcome(ReaderBehavior::Response(reader_response(
            UsageProvenance::Estimated,
            10,
        )))
        .await,
    ];
    assert_eq!(failure_kind(&outcomes[0]), FailureKind::Timeout);
    assert_eq!(failure_kind(&outcomes[1]), FailureKind::Provider);
    assert_eq!(failure_kind(&outcomes[2]), FailureKind::Parse);
    assert!(
        outcomes
            .iter()
            .all(|outcome| !outcome.case_report.rendered_evidence.is_empty())
    );

    let mut builder = report_builder();
    for outcome in outcomes {
        for observation in outcome.stage_observations {
            builder.record_stage(observation);
        }
        builder.record_case(outcome.case_report);
    }
    let report = builder.finish();
    let primary = &report.modes[0];
    assert_eq!(primary.denominators.total_cases, 3);
    assert_eq!(primary.denominators.completed_cases, 0);
    assert_eq!(primary.denominators.failed_cases, 3);
    assert_eq!(
        primary
            .cases
            .iter()
            .filter(|case| case.failure.is_some())
            .count(),
        3
    );
    assert_eq!(
        report
            .stage_metrics
            .iter()
            .find(|metrics| {
                metrics.stage == StageName::Reader
                    && metrics.mode == Some(ExternalMemoryMode::Primary)
            })
            .expect("primary reader metrics")
            .denominator,
        3
    );
}

async fn failed_reader_outcome(
    behavior: ReaderBehavior,
) -> moa_eval::external_memory::harness::CaseExecutionOutcome {
    let reader = FakeReader::new([behavior]);
    let scorer = FakeScorer::exact();
    let judge = FakeAbsoluteJudge::successful(1);
    let mut executor = ExternalMemoryExecutor::new(reader, scorer, judge, settings(), 1.0)
        .expect("valid executor config");
    let mut backend = FakeBackend::default();
    executor
        .run_case(&mut backend, &prepared_case("failed-case"))
        .await
}

fn failure_kind(outcome: &moa_eval::external_memory::harness::CaseExecutionOutcome) -> FailureKind {
    outcome
        .case_report
        .failure
        .as_ref()
        .expect("expected failed case")
        .kind
}

fn prepared_case(isolation_key: &str) -> PreparedExternalMemoryCase {
    let occurred_at = Utc
        .with_ymd_and_hms(2026, 7, 9, 10, 0, 0)
        .single()
        .expect("fixed timestamp");
    validate_case(ExternalMemoryCase {
        schema_version: 1,
        isolation_key: isolation_key.to_string(),
        sessions: vec![ExternalMemorySession {
            source_id: "session-1".to_string(),
            occurred_at,
            turns: vec![ExternalMemoryTurn {
                source_id: "turn-1".to_string(),
                occurred_at,
                role: "user".to_string(),
                text: "Ada owns it.".to_string(),
            }],
        }],
        question: "Who owns it?".to_string(),
        options: Vec::new(),
        answer: "Ada".to_string(),
        category: "single_session".to_string(),
        evidence_labels: EvidenceLabels::default(),
    })
    .expect("case should validate")
}

fn settings() -> ExternalMemoryExecutionSettings {
    ExternalMemoryExecutionSettings {
        evidence_token_budget: 64,
        ranked_occurrence_depth: 4,
        paid_call_timeout: Duration::from_millis(5),
        reader_prompt_version: "reader-v1".to_string(),
        absolute_judge_rubric: "Judge correctness against the reference answer.".to_string(),
        absolute_judge_prompt_version: "judge-v1".to_string(),
        reader: paid_plan("reader-model"),
        absolute_judge: paid_plan("judge-model"),
    }
}

fn paid_plan(model: &str) -> PaidStagePlan {
    PaidStagePlan {
        pricing: PricingSnapshot {
            model: model.to_string(),
            effective_date: "2026-07-09".to_string(),
            input_per_million_usd: 1.0,
            output_per_million_usd: 0.0,
            cache_read_per_million_usd: 0.0,
            cache_write_per_million_usd: 0.0,
        },
        estimated_usage: NormalizedUsage {
            input_tokens_uncached: 10,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            provenance: UsageProvenance::Estimated,
        },
    }
}

fn reader_response(provenance: UsageProvenance, input_tokens_uncached: usize) -> ReaderResponse {
    ReaderResponse {
        answer: "Ada".to_string(),
        model: "reader-model".to_string(),
        prompt_version: "reader-v1".to_string(),
        usage: NormalizedUsage {
            input_tokens_uncached,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            provenance,
        },
        latency_ms: 7,
    }
}

fn judge_response() -> AbsoluteJudgeResponse {
    AbsoluteJudgeResponse {
        supported: true,
        rationale: "The answer matches the reference.".to_string(),
        model: "judge-model".to_string(),
        prompt_version: "judge-v1".to_string(),
        usage: NormalizedUsage {
            input_tokens_uncached: 10,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            provenance: UsageProvenance::Actual,
        },
        latency_ms: 9,
    }
}

fn report_builder() -> ExternalMemoryReportBuilder {
    let formation = ResolvedFormationConfig {
        schema_version: 1,
        mode: FormationMode::Heuristic,
        extractor: component("extractor"),
        merge: component("merge"),
        embedding: EmbeddingConfig {
            provider: "fake".to_string(),
            model: "fake-embedding".to_string(),
            version: 1,
            dimensions: 8,
        },
        entity_blocking: EntityBlockingConfig {
            enabled: false,
            cosine_threshold: "0.0".to_string(),
        },
        pii_classifier: component("pii"),
        contradiction_detector: component("contradiction"),
        consolidation: ConsolidationSettings {
            decay_idle_days: 30,
            decay_half_life_days: "180.0".to_string(),
            decay_floor: "0.1".to_string(),
            expire_idle_days: 180,
            digest_enabled: false,
            digest_max_tokens: 0,
            digest_rebuild_min_interval_hours: 1,
        },
    };
    let formation_hash = formation.canonical_hash().expect("formation should hash");
    ExternalMemoryReportBuilder::new(
        Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
            .single()
            .expect("fixed timestamp"),
        DatasetPackage::new(DatasetPackageManifest {
            schema_version: 1,
            dataset: "common-json".to_string(),
            source: DatasetPackageSource {
                repository: "fixtures/common-json".to_string(),
                revision: "fixture-v1".to_string(),
            },
            files: vec![DatasetFileProvenance {
                path: "common_cases.json".to_string(),
                size_bytes: 1,
                sha256: "a".repeat(64),
            }],
        })
        .expect("package should hash"),
        formation,
        formation_hash,
        ReaderContractV2::new("fixture:reader", "reader-v1", 4096, 64),
        ReportBudgetV2 {
            ceiling_usd: 1.0,
            estimated_committed_usd: 0.0,
            actual_or_estimated_committed_usd: 0.0,
        },
    )
}

fn component(implementation: &str) -> ComponentConfig {
    ComponentConfig {
        implementation: implementation.to_string(),
        model: None,
        prompt_version: None,
    }
}
