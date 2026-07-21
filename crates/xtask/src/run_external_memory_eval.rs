//! Strict argument and authorization gate for external-memory benchmark runs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use moa_config::MoaConfig;
use moa_core::traits::{EmbeddingProvider, LLMProvider};
use moa_core::types::provider::ProviderId;
use moa_core::{
    error::MoaError, types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::context::ContextMessage, types::context::estimate_text_tokens,
    types::identifiers::ModelId,
};
use moa_crypto::LocalKmsProvider;
use moa_eval::external_memory::answer::{
    AbsoluteJudgeResponse, AnswerScore, AnswerScoreOutcome, ExternalMemoryMode, ReaderResponse,
    SupportStatus, reader_fit_support, render_control_evidence, render_reader_prompt,
};
use moa_eval::external_memory::cost::{
    BudgetLedger, NormalizedUsage, PricingSnapshotV1, StageName, UsageProvenance,
};
use moa_eval::external_memory::dataset::{
    DatasetPackageRegistry, DatasetPackageV1, PreparedExternalMemoryCase, VerifiedFetchSummaryV1,
};
use moa_eval::external_memory::formation::{
    ComponentConfig, ConsolidationSettings, EmbeddingConfig, EntityBlockingConfig, FormationMode,
    RecordedFormationManifestV1, ResolvedFormationConfig,
};
use moa_eval::external_memory::harness::{
    EvidenceExport, ExternalMemoryBackend, validate_evidence_export,
};
use moa_eval::external_memory::longmemeval::{
    LONGMEMEVAL_DATASET, LONGMEMEVAL_RUBRIC_BUNDLE_SHA256, LONGMEMEVAL_RUBRIC_VERSION,
    LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON, LongMemEvalOccurrenceRef, LongMemEvalRubricKind,
    PreparedLongMemEvalCase, aggregate_retrieval_metrics, load_full_longmemeval_package,
    parse_absolute_judge_label,
};
use moa_eval::external_memory::moa_backend::MoaMemoryBackend;
use moa_eval::external_memory::personamem::{
    PERSONAMEM_DATASET, PersonaMemAnswerOutcome, PreparedPersonaMemCase, build_accuracy_report,
    load_full_personamem_package,
};
use moa_eval::external_memory::report::{
    CaseReport, ExternalMemoryDatasetMetricsV2, ExternalMemoryReportBuilder, FailureKind,
    LongMemEvalAnswerSliceV1, LongMemEvalModeMetricsV2, PersonaMemModeMetricsV2, ReaderContractV2,
    ReportBudgetV2, RetrievalMetricsV2, StageObservation,
};
use moa_memory_ingest::{
    DeterministicEntityMergeVerifier, EntityMergeFixtureRecord, EntityMergeVerifier,
    EntityResolver, Error, ExtractionFixtureRecord, HeuristicFactExtractor, ModelCallObserver,
    ModelEntityMergeVerifier, ModelFactExtractor, RecordedEntityMergeStore,
    RecordedEntityMergeVerifier, RecordedExtractionStore, RecordedFactExtractor,
    RrfPlusJudgeDetector,
};
use moa_memory_lifecycle::ConsolidationOptions;
use moa_memory_pii::HeuristicPiiClassifier;
use moa_memory_vector::VECTOR_DIMENSION;
use moa_providers::{
    EmbedderConstructionRole, build_embedder_from_config, build_provider_from_selection,
    provider_descriptor_by_name, resolve_provider_selection,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tokio::time::timeout;

const LIVE_FLAG: &str = "MOA_RUN_LIVE_MEMORY_BENCHMARKS";
const PRICING_AS_OF: &str = "2026-07-09";
const PAID_CALL_TIMEOUT: Duration = Duration::from_secs(120);
const LONGMEMEVAL_READER_PROMPT_VERSION: &str = "longmemeval-evidence-reader-v1";
const LONGMEMEVAL_JUDGE_PROMPT_VERSION: &str = "longmemeval-absolute-judge-v1";

#[derive(Clone)]
struct LiveRunBudget {
    state: Arc<tokio::sync::Mutex<LiveRunBudgetState>>,
}

struct LiveRunBudgetState {
    ledger: BudgetLedger,
    observations: Vec<StageObservation>,
    terminal_error: Option<String>,
}

impl LiveRunBudget {
    fn new(budget_usd: f64) -> Result<Self> {
        Ok(Self {
            state: Arc::new(tokio::sync::Mutex::new(LiveRunBudgetState {
                ledger: BudgetLedger::new(budget_usd).map_err(anyhow::Error::from)?,
                observations: Vec::new(),
                terminal_error: None,
            })),
        })
    }

    async fn forecast(
        &self,
        stage: StageName,
        mode: Option<ExternalMemoryMode>,
        pricing: PricingSnapshotV1,
        usage: NormalizedUsage,
    ) -> std::result::Result<usize, String> {
        let mut state = self.state.lock().await;
        if let Some(error) = &state.terminal_error {
            return Err(error.clone());
        }
        match state.ledger.forecast(stage, mode, pricing, usage) {
            Ok(id) => Ok(id),
            Err(error) => {
                let error = error.to_string();
                state.terminal_error = Some(error.clone());
                Err(error)
            }
        }
    }

    async fn record_actual(
        &self,
        record_id: usize,
        usage: NormalizedUsage,
        latency_ms: u64,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let result = state.ledger.record_actual(record_id, usage);
        let record = state.ledger.records().get(record_id).cloned();
        if let Some(record) = record {
            state.observations.push(StageObservation {
                stage: record.stage,
                mode: record.mode,
                latency_ms,
                accounting: Some(record),
            });
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let error = error.to_string();
                state.terminal_error = Some(error.clone());
                Err(error)
            }
        }
    }

    async fn record_failure(&self, record_id: usize, latency_ms: u64) {
        let mut state = self.state.lock().await;
        if let Some(record) = state.ledger.records().get(record_id).cloned() {
            state.observations.push(StageObservation {
                stage: record.stage,
                mode: record.mode,
                latency_ms,
                accounting: Some(record),
            });
        }
    }

    async fn drain_observations(&self) -> Vec<StageObservation> {
        let mut state = self.state.lock().await;
        std::mem::take(&mut state.observations)
    }

    async fn terminal_error(&self) -> Option<String> {
        self.state.lock().await.terminal_error.clone()
    }

    async fn report_budget(&self) -> ReportBudgetV2 {
        let state = self.state.lock().await;
        ReportBudgetV2 {
            ceiling_usd: state.ledger.ceiling_usd(),
            estimated_committed_usd: state.ledger.estimated_committed_cost_usd(),
            actual_or_estimated_committed_usd: state.ledger.committed_cost_usd(),
        }
    }

    #[cfg(test)]
    async fn records(&self) -> Vec<moa_eval::external_memory::cost::StageCostRecord> {
        self.state.lock().await.ledger.records().to_vec()
    }
}

struct AccountingModelObserver {
    stage: StageName,
    pricing: PricingSnapshotV1,
    budget: LiveRunBudget,
    pending: tokio::sync::Mutex<Vec<(usize, Instant)>>,
}

impl AccountingModelObserver {
    fn new(stage: StageName, model_selector: &str, budget: LiveRunBudget) -> Result<Self> {
        Ok(Self {
            stage,
            pricing: chat_pricing(model_selector)?,
            budget,
            pending: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    async fn take_pending(&self) -> Option<(usize, Instant)> {
        self.pending.lock().await.pop()
    }
}

#[async_trait]
impl ModelCallObserver for AccountingModelObserver {
    async fn before_call(&self, request: &CompletionRequest) -> std::result::Result<(), Error> {
        let accounting_id = self
            .budget
            .forecast(
                self.stage,
                None,
                self.pricing.clone(),
                estimated_completion_usage(request),
            )
            .await
            .map_err(Error::ModelInference)?;
        self.pending
            .lock()
            .await
            .push((accounting_id, Instant::now()));
        Ok(())
    }

    async fn after_response(
        &self,
        response: &CompletionResponse,
    ) -> std::result::Result<(), Error> {
        let (accounting_id, started) = self.take_pending().await.ok_or_else(|| {
            Error::ModelInference(
                "memory benchmark observer received an unreserved response".to_string(),
            )
        })?;
        self.budget
            .record_actual(
                accounting_id,
                normalized_response_usage(response),
                elapsed_ms(started),
            )
            .await
            .map_err(Error::ModelInference)
    }

    async fn after_failure(&self) {
        if let Some((accounting_id, started)) = self.take_pending().await {
            self.budget
                .record_failure(accounting_id, elapsed_ms(started))
                .await;
        }
    }
}

struct AccountingEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    pricing: PricingSnapshotV1,
    budget: LiveRunBudget,
}

impl AccountingEmbeddingProvider {
    fn new(
        inner: Arc<dyn EmbeddingProvider>,
        selector: &str,
        budget: LiveRunBudget,
    ) -> Result<Self> {
        let pricing = embedding_pricing(selector, inner.model_id())?;
        Ok(Self {
            inner,
            pricing,
            budget,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for AccountingEmbeddingProvider {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.inner.model_version()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        let input_tokens = inputs.iter().map(|input| estimate_text_tokens(input)).sum();
        let accounting_id = self
            .budget
            .forecast(
                StageName::Embedding,
                None,
                self.pricing.clone(),
                NormalizedUsage {
                    input_tokens_uncached: input_tokens,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 0,
                    provenance: UsageProvenance::Estimated,
                },
            )
            .await
            .map_err(MoaError::BudgetExhausted)?;
        let started = Instant::now();
        let vectors = match self.inner.embed(inputs).await {
            Ok(vectors) => vectors,
            Err(error) => {
                self.budget
                    .record_failure(accounting_id, elapsed_ms(started))
                    .await;
                return Err(error);
            }
        };
        self.budget
            .record_actual(
                accounting_id,
                NormalizedUsage {
                    input_tokens_uncached: input_tokens,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 0,
                    provenance: UsageProvenance::Actual,
                },
                elapsed_ms(started),
            )
            .await
            .map_err(MoaError::BudgetExhausted)?;
        Ok(vectors)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RunExternalMemoryEvalArgs {
    dataset: String,
    data: PathBuf,
    package_manifest: PathBuf,
    fetch_summary: Option<PathBuf>,
    migrate_database: bool,
    output: PathBuf,
    formation_mode: FormationMode,
    embedding_selector: String,
    reader_model: String,
    judge_model: Option<String>,
    reader_context_window: u64,
    reader_output_token_reserve: u64,
    controls: String,
    evidence_token_budget: usize,
    budget_usd: f64,
    recorded_manifest: Option<PathBuf>,
    extractor_model: Option<String>,
    merge_verifier_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedLongMemEvalModels {
    reader_family: ProviderId,
    reader_model: ModelId,
    judge_family: ProviderId,
    judge_model: ModelId,
}

fn validate_longmemeval_model_policy(
    args: &RunExternalMemoryEvalArgs,
) -> Result<ResolvedLongMemEvalModels> {
    let (reader_family, reader_model) =
        resolve_explicit_model(&args.reader_model, "--reader-model")?;
    let judge_selector = args
        .judge_model
        .as_deref()
        .context("LongMemEval requires explicit --judge-model provider:model")?;
    let (judge_family, judge_model) = resolve_explicit_model(judge_selector, "--judge-model")?;
    if reader_family == judge_family {
        bail!("LongMemEval reader and judge must use different provider families");
    }
    Ok(ResolvedLongMemEvalModels {
        reader_family,
        reader_model,
        judge_family,
        judge_model,
    })
}

fn resolve_explicit_model(selector: &str, flag: &str) -> Result<(ProviderId, ModelId)> {
    let (provider, model) = selector.split_once(':').with_context(|| {
        format!("{flag} must use an explicit recognized provider:model selector")
    })?;
    let descriptor = provider_descriptor_by_name(provider)
        .with_context(|| format!("{flag} has unknown provider family `{provider}`"))?;
    let model = model.trim();
    if model.is_empty() {
        bail!("{flag} must include a non-empty model after the provider family");
    }
    Ok((descriptor.id, ModelId::new(model)))
}

fn ranked_occurrence_depth(dataset: &str) -> usize {
    if dataset == LONGMEMEVAL_DATASET {
        50
    } else {
        4
    }
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let parsed = parse_args(args)?;
    let live_authorized = env::var(LIVE_FLAG).as_deref() == Ok("1");
    validate_args(&parsed, live_authorized)?;

    let package_bytes = std::fs::read(&parsed.package_manifest).with_context(|| {
        format!(
            "read external-memory package manifest {}",
            parsed.package_manifest.display()
        )
    })?;
    let package: DatasetPackageV1 =
        serde_json::from_slice(&package_bytes).context("parse external-memory package manifest")?;
    package.validate().map_err(anyhow::Error::from)?;
    if parsed.dataset != package.manifest.dataset {
        bail!("--dataset does not match package.json dataset");
    }
    let (cases, personamem_cases, longmemeval_cases, loaded_counts) =
        if package.manifest.dataset == PERSONAMEM_DATASET {
            let dataset = load_full_personamem_package(&package, &parsed.data)
                .map_err(anyhow::Error::from)?;
            let counts = LoadedDatasetCounts::PersonaMem {
                questions: dataset.cases.len(),
                personas: dataset.persona_count(),
                contexts: dataset.context_count,
            };
            let cases = dataset
                .cases
                .iter()
                .map(|case| case.prepared.clone())
                .collect();
            (cases, Some(dataset.cases), None, Some(counts))
        } else if package.manifest.dataset == LONGMEMEVAL_DATASET {
            validate_longmemeval_model_policy(&parsed)?;
            let dataset = load_full_longmemeval_package(&package, &parsed.data)
                .map_err(anyhow::Error::from)?;
            let counts = LoadedDatasetCounts::LongMemEval {
                questions: dataset.cases.len(),
                abstentions: dataset.abstention_count(),
                retrieval: dataset.retrieval_count(),
            };
            let cases = dataset
                .cases
                .iter()
                .map(|case| case.prepared.clone())
                .collect();
            (cases, None, Some(dataset.cases), Some(counts))
        } else {
            verify_data_provenance(&parsed.data, &package)?;
            let cases = DatasetPackageRegistry::task_9()
                .load(&package, &parsed.data)
                .map_err(anyhow::Error::from)?;
            (cases, None, None, None)
        };
    validate_fetch_summary_before_runtime(
        parsed.fetch_summary.as_deref(),
        &package,
        loaded_counts.as_ref(),
    )?;
    if !live_authorized {
        bail!("provider-backed benchmark execution requires {LIVE_FLAG}=1");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for external-memory eval")?;
    runtime.block_on(run_validated(
        parsed,
        package,
        cases,
        personamem_cases,
        longmemeval_cases,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadedDatasetCounts {
    PersonaMem {
        questions: usize,
        personas: usize,
        contexts: usize,
    },
    LongMemEval {
        questions: usize,
        abstentions: usize,
        retrieval: usize,
    },
}

fn validate_fetch_summary_before_runtime(
    path: Option<&Path>,
    package: &DatasetPackageV1,
    counts: Option<&LoadedDatasetCounts>,
) -> Result<()> {
    let Some(path) = path else {
        if counts.is_some() {
            bail!("external benchmark package requires --fetch-summary");
        }
        return Ok(());
    };
    let bytes = std::fs::read(path)
        .with_context(|| format!("read verified fetch summary {}", path.display()))?;
    let summary: VerifiedFetchSummaryV1 =
        serde_json::from_slice(&bytes).context("parse strict verified fetch summary")?;
    summary
        .validate_package(package)
        .map_err(anyhow::Error::from)?;
    let matches = match (&summary, counts) {
        (
            VerifiedFetchSummaryV1::PersonaMem(summary),
            Some(LoadedDatasetCounts::PersonaMem {
                questions,
                personas,
                contexts,
            }),
        ) => {
            summary.question_count == *questions
                && summary.persona_count == *personas
                && summary.context_count == *contexts
        }
        (
            VerifiedFetchSummaryV1::LongMemEval(summary),
            Some(LoadedDatasetCounts::LongMemEval {
                questions,
                abstentions,
                retrieval,
            }),
        ) => {
            summary.question_count == *questions
                && summary.abstention_count == *abstentions
                && summary.retrieval_count == *retrieval
        }
        _ => false,
    };
    if !matches {
        bail!("fetch summary dataset counts do not match the loaded package");
    }
    Ok(())
}

async fn run_validated(
    args: RunExternalMemoryEvalArgs,
    package: DatasetPackageV1,
    cases: Vec<PreparedExternalMemoryCase>,
    personamem_cases: Option<Vec<PreparedPersonaMemCase>>,
    longmemeval_cases: Option<Vec<PreparedLongMemEvalCase>>,
) -> Result<()> {
    let database_url = env::var("MOA_DATABASE_URL")
        .context("MOA_DATABASE_URL is required for the MOA external-memory backend")?;
    if !args.migrate_database {
        bail!("external-memory execution requires --migrate-database");
    }
    moa_migrations::run(&database_url)
        .await
        .context("run external-memory database migrations before provider construction")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .context("connect external-memory backend to migrated Postgres")?;

    let mut config = MoaConfig::load().context("load MOA config")?;
    config.memory.vector.embedder.name = args.embedding_selector.clone();
    config.memory.vector.embedder.output_dim = VECTOR_DIMENSION;
    config.memory.embedding_model = args.embedding_selector.clone();

    let budget = LiveRunBudget::new(args.budget_usd)?;
    let formation_inputs = resolve_formation_inputs(&args, &mut config, budget.clone())?;
    let raw_embedder = build_embedder_from_config(&config, EmbedderConstructionRole::Retrieval)
        .context("construct explicit external-memory embedder")?;
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(AccountingEmbeddingProvider::new(
        raw_embedder,
        &args.embedding_selector,
        budget.clone(),
    )?);
    let (reader_family, reader_model_id) =
        resolve_provider_selection(&config, Some(&args.reader_model))
            .context("resolve external-memory reader")?;
    let reader = build_provider_from_selection(&config, reader_family, &reader_model_id)
        .context("construct external-memory reader without failover")?;
    let judge_selection = if package.manifest.dataset == LONGMEMEVAL_DATASET {
        let resolved = validate_longmemeval_model_policy(&args)?;
        Some((resolved.judge_family, resolved.judge_model))
    } else {
        None
    };
    let judge = judge_selection
        .as_ref()
        .map(|(family, model)| {
            build_provider_from_selection(&config, *family, model)
                .context("construct LongMemEval judge without failover")
        })
        .transpose()?;
    let formation = resolved_formation_config(&args, &formation_inputs, embedder.as_ref());
    let formation_hash = formation
        .canonical_hash()
        .map_err(anyhow::Error::from)
        .context("hash resolved formation config")?;
    let consolidation = ConsolidationOptions::default();
    let mut backend = MoaMemoryBackend::new_with_dependencies(
        pool.clone(),
        Arc::new(LocalKmsProvider::new()),
        embedder,
        formation_inputs.extractor,
        Arc::new(HeuristicPiiClassifier),
        Arc::new(RrfPlusJudgeDetector::default()),
        Arc::new(EntityResolver::for_app_role(
            formation_inputs.merge_verifier,
        )),
        false,
        consolidation,
    )
    .map_err(anyhow::Error::from)?;
    let mut report = ExternalMemoryReportBuilder::new(
        Utc::now(),
        package.clone(),
        formation,
        formation_hash,
        ReaderContractV2::new(
            &args.reader_model,
            reader_prompt_version(&package.manifest.dataset),
            args.reader_context_window,
            args.reader_output_token_reserve,
        ),
        ReportBudgetV2 {
            ceiling_usd: args.budget_usd,
            estimated_committed_usd: 0.0,
            actual_or_estimated_committed_usd: 0.0,
        },
    );
    let reader_stage_pricing = reader_pricing(reader.as_ref(), &args.reader_model);
    let judge_pricing = judge
        .as_ref()
        .zip(args.judge_model.as_deref())
        .map(|(provider, model)| reader_pricing(provider.as_ref(), model));
    let mut terminal_error = None;
    let mut terminal_tail_start = None;
    let mut personamem_outcomes = BTreeMap::new();
    let mut longmemeval_rankings = BTreeMap::new();

    for (case_index, case) in cases.iter().enumerate() {
        let formation_started = Instant::now();
        if let Err(error) = form_case(&mut backend, case).await {
            record_budget_observations(&mut report, &budget).await;
            let budget_error = budget.terminal_error().await;
            report.record_case(CaseReport::failed(
                &case.case.isolation_key,
                &case.case.category,
                if budget_error.is_some() {
                    FailureKind::Budget
                } else {
                    FailureKind::Backend
                },
                budget_error.clone().unwrap_or(error),
            ));
            record_personamem_failure(case, &personamem_cases, &mut personamem_outcomes, false);
            write_partial_report(&report, &args.output)?;
            if let Some(error) = budget_error {
                terminal_error = Some(anyhow::Error::msg(error));
                terminal_tail_start = Some(case_index.saturating_add(1));
                break;
            }
            continue;
        }
        let formation_observations = record_budget_observations(&mut report, &budget).await;
        if formation_observations == 0 {
            report.record_stage(StageObservation {
                stage: StageName::FormationExtraction,
                mode: None,
                latency_ms: elapsed_ms(formation_started),
                accounting: None,
            });
        }
        let retrieval_started = Instant::now();
        let depth = ranked_occurrence_depth(&package.manifest.dataset);
        let evidence = match retrieve_case(&mut backend, case, args.evidence_token_budget, depth)
            .await
        {
            Ok(evidence) => evidence,
            Err(error) => {
                record_budget_observations(&mut report, &budget).await;
                let budget_error = budget.terminal_error().await;
                report.record_stage(StageObservation {
                    stage: StageName::Retrieval,
                    mode: Some(ExternalMemoryMode::Primary),
                    latency_ms: elapsed_ms(retrieval_started),
                    accounting: None,
                });
                report.record_case(CaseReport::failed(
                    &case.case.isolation_key,
                    &case.case.category,
                    if budget_error.is_some() {
                        FailureKind::Budget
                    } else {
                        FailureKind::Backend
                    },
                    budget_error.clone().unwrap_or(error),
                ));
                record_personamem_failure(case, &personamem_cases, &mut personamem_outcomes, false);
                write_partial_report(&report, &args.output)?;
                if let Some(error) = budget_error {
                    terminal_error = Some(anyhow::Error::msg(error));
                    terminal_tail_start = Some(case_index.saturating_add(1));
                    break;
                }
                continue;
            }
        };
        record_budget_observations(&mut report, &budget).await;
        report.record_stage(StageObservation {
            stage: StageName::Retrieval,
            mode: Some(ExternalMemoryMode::Primary),
            latency_ms: elapsed_ms(retrieval_started),
            accounting: None,
        });
        if let Some(longmemeval_case) = longmemeval_case(case, &longmemeval_cases) {
            longmemeval_rankings.insert(
                longmemeval_case.metadata.question_id.clone(),
                evidence
                    .ranked_source_refs
                    .iter()
                    .map(|source| {
                        LongMemEvalOccurrenceRef::new(
                            source.session_source_id.clone(),
                            source.turn_source_id.clone(),
                        )
                    })
                    .collect(),
            );
        }

        let reader_prompt_version = reader_prompt_version(&package.manifest.dataset);
        let rendered_prompt = render_reader_prompt(
            case,
            &evidence.rendered_evidence,
            reader_prompt_version,
            &package.manifest.dataset,
        );
        if let SupportStatus::Unsupported { reason } = reader_fit_support(
            &rendered_prompt,
            args.reader_context_window,
            args.reader_output_token_reserve,
        ) {
            let mut unsupported = CaseReport::unsupported(
                &case.case.isolation_key,
                &case.case.category,
                ExternalMemoryMode::Primary,
                reason,
            );
            unsupported.rendered_evidence = evidence.rendered_evidence;
            unsupported.rendered_evidence_tokens = evidence.tokens_used as u64;
            report.record_case(unsupported);
            record_personamem_failure(case, &personamem_cases, &mut personamem_outcomes, false);
            write_partial_report(&report, &args.output)?;
            continue;
        }
        let paid_reader = execute_paid_completion(
            reader.as_ref(),
            reader_request(
                case,
                &evidence.rendered_evidence,
                reader_model_id.as_str(),
                &package.manifest.dataset,
                args.reader_output_token_reserve,
            ),
            StageName::Reader,
            ExternalMemoryMode::Primary,
            reader_stage_pricing.clone(),
            &budget,
            PAID_CALL_TIMEOUT,
        )
        .await;
        record_budget_observations(&mut report, &budget).await;
        let paid_reader = match paid_reader {
            Ok(paid_reader) => paid_reader,
            Err(failure) => {
                let mut failed = CaseReport::failed(
                    &case.case.isolation_key,
                    &case.case.category,
                    failure.kind,
                    failure.message.clone(),
                );
                failed.rendered_evidence = evidence.rendered_evidence;
                failed.rendered_evidence_tokens = evidence.tokens_used as u64;
                report.record_case(failed);
                record_personamem_failure(
                    case,
                    &personamem_cases,
                    &mut personamem_outcomes,
                    failure.kind == FailureKind::Provider,
                );
                write_partial_report(&report, &args.output)?;
                if failure.kind == FailureKind::Budget {
                    terminal_error = Some(anyhow::Error::msg(failure.message));
                    terminal_tail_start = Some(case_index.saturating_add(1));
                    break;
                }
                continue;
            }
        };
        let response = paid_reader.response;
        let reader_response = ReaderResponse {
            answer: response.text,
            model: response.model.to_string(),
            prompt_version: reader_prompt_version.to_string(),
            usage: paid_reader.actual_usage,
            latency_ms: response.duration_ms,
        };
        if let Err(error) = paid_reader.post_response_budget {
            let mut failed = CaseReport::failed(
                &case.case.isolation_key,
                &case.case.category,
                FailureKind::Budget,
                error.clone(),
            );
            failed.rendered_evidence = evidence.rendered_evidence;
            failed.rendered_evidence_tokens = evidence.tokens_used as u64;
            failed.reader = Some(reader_response);
            report.record_case(failed);
            record_personamem_failure(case, &personamem_cases, &mut personamem_outcomes, false);
            write_partial_report(&report, &args.output)?;
            terminal_error = Some(anyhow::Error::msg(error));
            terminal_tail_start = Some(case_index.saturating_add(1));
            break;
        }
        if let Some(question_id) = personamem_question_id(case, &personamem_cases) {
            personamem_outcomes.insert(
                question_id.to_string(),
                PersonaMemAnswerOutcome::Answer(reader_response.answer.clone()),
            );
        }
        let (support_status, score) = match answer_score_for_dataset(
            &package.manifest.dataset,
            &case.case.answer,
            &reader_response.answer,
        ) {
            AnswerScoreOutcome::Supported(score) => (SupportStatus::Supported, Some(score)),
            AnswerScoreOutcome::Unsupported { reason } => {
                (SupportStatus::Unsupported { reason }, None)
            }
        };

        if package.manifest.dataset == LONGMEMEVAL_DATASET {
            let longmemeval_case = longmemeval_case(case, &longmemeval_cases)
                .context("LongMemEval runner lost typed case metadata")?;
            let rubric = LongMemEvalRubricKind::for_question(
                longmemeval_case.metadata.question_type,
                longmemeval_case.is_abstention,
            );
            let judge_provider = judge
                .as_ref()
                .context("LongMemEval judge was not constructed")?;
            let judge_model = args
                .judge_model
                .as_deref()
                .context("LongMemEval judge model was not retained")?;
            let judge_request =
                longmemeval_judge_request(longmemeval_case, &reader_response, rubric, judge_model)?;
            let paid_judge = execute_paid_completion(
                judge_provider.as_ref(),
                judge_request,
                StageName::Judge,
                ExternalMemoryMode::Primary,
                judge_pricing
                    .clone()
                    .context("LongMemEval judge pricing was not resolved")?,
                &budget,
                PAID_CALL_TIMEOUT,
            )
            .await;
            record_budget_observations(&mut report, &budget).await;
            let paid_judge = match paid_judge {
                Ok(paid_judge) => paid_judge,
                Err(failure) => {
                    let failed = failed_after_reader(
                        case,
                        evidence.rendered_evidence,
                        evidence.tokens_used as u64,
                        reader_response,
                        support_status,
                        score,
                        None,
                        failure.kind,
                        failure.message.clone(),
                    );
                    report.record_case(failed);
                    write_partial_report(&report, &args.output)?;
                    if failure.kind == FailureKind::Budget {
                        terminal_error = Some(anyhow::Error::msg(failure.message));
                        terminal_tail_start = Some(case_index.saturating_add(1));
                        break;
                    }
                    continue;
                }
            };
            let raw_label = paid_judge.response.text.trim().to_string();
            let parsed_label = parse_absolute_judge_label(&raw_label);
            let absolute_judge = AbsoluteJudgeResponse {
                supported: parsed_label.unwrap_or(false),
                rationale: raw_label,
                model: paid_judge.response.model.to_string(),
                prompt_version: longmemeval_judge_prompt_version(rubric),
                usage: paid_judge.actual_usage,
                latency_ms: paid_judge.response.duration_ms,
            };
            if let Err(error) = paid_judge.post_response_budget {
                report.record_case(failed_after_reader(
                    case,
                    evidence.rendered_evidence,
                    evidence.tokens_used as u64,
                    reader_response,
                    support_status,
                    score,
                    Some(absolute_judge),
                    FailureKind::Budget,
                    error.clone(),
                ));
                write_partial_report(&report, &args.output)?;
                terminal_error = Some(anyhow::Error::msg(error));
                terminal_tail_start = Some(case_index.saturating_add(1));
                break;
            }
            if parsed_label.is_none() {
                report.record_case(failed_after_reader(
                    case,
                    evidence.rendered_evidence,
                    evidence.tokens_used as u64,
                    reader_response,
                    support_status,
                    score,
                    Some(absolute_judge),
                    FailureKind::Parse,
                    "LongMemEval judge returned neither exact yes nor exact no",
                ));
                write_partial_report(&report, &args.output)?;
                continue;
            }
            report.record_case(
                CaseReport::completed(
                    &case.case.isolation_key,
                    &case.case.category,
                    evidence.rendered_evidence,
                    support_status.clone(),
                )
                .with_rendered_evidence_tokens(evidence.tokens_used as u64)
                .with_generated_answer_outcome(
                    reader_response,
                    support_status,
                    score,
                    Some(absolute_judge),
                ),
            );
        } else {
            report.record_case(
                CaseReport::completed(
                    &case.case.isolation_key,
                    &case.case.category,
                    evidence.rendered_evidence,
                    support_status.clone(),
                )
                .with_rendered_evidence_tokens(evidence.tokens_used as u64)
                .with_generated_answer_outcome(
                    reader_response,
                    support_status,
                    score,
                    None,
                ),
            );
        }
        write_partial_report(&report, &args.output)?;
    }

    if let Some(start) = terminal_tail_start {
        let error = terminal_error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "run budget exhausted".to_string());
        for (case, failed) in cases[start..]
            .iter()
            .zip(budget_tail_reports(&cases[start..], &error))
        {
            report.record_case(failed);
            record_personamem_failure(case, &personamem_cases, &mut personamem_outcomes, false);
        }
    }
    let control_personamem_outcomes = run_control_modes(
        &mut report,
        &cases,
        &package.manifest.dataset,
        &personamem_cases,
        &longmemeval_cases,
        reader.as_ref(),
        reader_model_id.as_str(),
        reader_stage_pricing.clone(),
        judge.as_deref(),
        judge_pricing.as_ref(),
        &budget,
        &args,
    )
    .await?;
    record_budget_observations(&mut report, &budget).await;
    if terminal_error.is_none() {
        terminal_error = budget.terminal_error().await.map(anyhow::Error::msg);
    }
    if let Some(personamem_cases) = &personamem_cases {
        let metrics = build_accuracy_report(personamem_cases, &personamem_outcomes)
            .map_err(anyhow::Error::from)?;
        report.set_dataset_metrics(
            ExternalMemoryMode::Primary,
            ExternalMemoryDatasetMetricsV2::PersonaMem32k(Box::new(PersonaMemModeMetricsV2 {
                answer: metrics,
                retrieval: SupportStatus::Unsupported {
                    reason: "persona-mem-does-not-provide-retrieval-labels".to_string(),
                },
            })),
        );
        for mode in [
            ExternalMemoryMode::NoMemory,
            ExternalMemoryMode::FullContext,
            ExternalMemoryMode::OracleEvidence,
        ] {
            let outcomes = control_personamem_outcomes
                .get(&mode)
                .context("control outcome map is complete")?;
            let metrics =
                build_accuracy_report(personamem_cases, outcomes).map_err(anyhow::Error::from)?;
            report.set_dataset_metrics(
                mode,
                ExternalMemoryDatasetMetricsV2::PersonaMem32k(Box::new(PersonaMemModeMetricsV2 {
                    answer: metrics,
                    retrieval: SupportStatus::Unsupported {
                        reason: "persona-mem-does-not-provide-retrieval-labels".to_string(),
                    },
                })),
            );
        }
    }
    if let Some(longmemeval_cases) = &longmemeval_cases {
        let snapshot = report.clone().finish();
        let metrics = build_longmemeval_report(
            longmemeval_cases,
            &longmemeval_rankings,
            snapshot.primary_cases(),
            args.judge_model
                .as_deref()
                .context("LongMemEval judge model missing from aggregate")?,
        )?;
        report.set_dataset_metrics(
            ExternalMemoryMode::Primary,
            ExternalMemoryDatasetMetricsV2::LongMemEvalSCleaned(Box::new(metrics)),
        );
        let snapshot = report.clone().finish();
        for mode in [
            ExternalMemoryMode::NoMemory,
            ExternalMemoryMode::FullContext,
            ExternalMemoryMode::OracleEvidence,
        ] {
            let mode_cases = snapshot
                .modes
                .iter()
                .find(|report| report.mode == mode)
                .context("V2 report retains every control mode")?;
            let mut metrics = build_longmemeval_report(
                longmemeval_cases,
                &BTreeMap::new(),
                &mode_cases.cases,
                args.judge_model
                    .as_deref()
                    .context("LongMemEval judge model missing from aggregate")?,
            )?;
            metrics.retrieval = RetrievalMetricsV2::Unsupported {
                reason: "retrieval-metrics-apply-to-primary-only".to_string(),
            };
            report.set_dataset_metrics(
                mode,
                ExternalMemoryDatasetMetricsV2::LongMemEvalSCleaned(Box::new(metrics)),
            );
        }
    }
    write_partial_report(&report, &args.output)?;
    pool.close().await;
    if let Some(error) = terminal_error {
        return Err(error.context("external-memory run stopped after writing a partial report"));
    }
    println!(
        "wrote external-memory report: output={} cases={}",
        args.output.display(),
        cases.len()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_control_modes(
    report: &mut ExternalMemoryReportBuilder,
    cases: &[PreparedExternalMemoryCase],
    dataset: &str,
    personamem_cases: &Option<Vec<PreparedPersonaMemCase>>,
    longmemeval_cases: &Option<Vec<PreparedLongMemEvalCase>>,
    reader: &dyn LLMProvider,
    reader_model: &str,
    reader_pricing: PricingSnapshotV1,
    judge: Option<&dyn LLMProvider>,
    judge_pricing: Option<&PricingSnapshotV1>,
    budget: &LiveRunBudget,
    args: &RunExternalMemoryEvalArgs,
) -> Result<BTreeMap<ExternalMemoryMode, BTreeMap<String, PersonaMemAnswerOutcome>>> {
    let mut outcomes_by_mode = BTreeMap::new();
    for mode in [
        ExternalMemoryMode::NoMemory,
        ExternalMemoryMode::FullContext,
        ExternalMemoryMode::OracleEvidence,
    ] {
        let mut outcomes = BTreeMap::new();
        for case in cases {
            let mode_evidence = render_control_evidence(case, mode, dataset)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("render {mode:?} control evidence"))?;
            if let SupportStatus::Unsupported { reason } = mode_evidence.support {
                report.record_case(CaseReport::unsupported(
                    &case.case.isolation_key,
                    &case.case.category,
                    mode,
                    reason,
                ));
                record_personamem_failure(case, personamem_cases, &mut outcomes, false);
                continue;
            }
            let prompt_version = reader_prompt_version(dataset);
            let rendered_prompt = render_reader_prompt(
                case,
                &mode_evidence.rendered_evidence,
                prompt_version,
                dataset,
            );
            if let SupportStatus::Unsupported { reason } = reader_fit_support(
                &rendered_prompt,
                args.reader_context_window,
                args.reader_output_token_reserve,
            ) {
                let mut unsupported = CaseReport::unsupported(
                    &case.case.isolation_key,
                    &case.case.category,
                    mode,
                    reason,
                );
                unsupported.rendered_evidence = mode_evidence.rendered_evidence;
                unsupported.rendered_evidence_tokens = mode_evidence.rendered_evidence_tokens;
                report.record_case(unsupported);
                record_personamem_failure(case, personamem_cases, &mut outcomes, false);
                continue;
            }
            if let Some(message) = budget.terminal_error().await {
                report.record_case(
                    CaseReport::failed_for_mode(
                        &case.case.isolation_key,
                        &case.case.category,
                        mode,
                        FailureKind::Budget,
                        message,
                    )
                    .with_rendered_evidence(
                        mode_evidence.rendered_evidence,
                        mode_evidence.rendered_evidence_tokens,
                    ),
                );
                record_personamem_failure(case, personamem_cases, &mut outcomes, false);
                continue;
            }
            let paid_reader = execute_paid_completion(
                reader,
                reader_request(
                    case,
                    &mode_evidence.rendered_evidence,
                    reader_model,
                    dataset,
                    args.reader_output_token_reserve,
                ),
                StageName::Reader,
                mode,
                reader_pricing.clone(),
                budget,
                PAID_CALL_TIMEOUT,
            )
            .await;
            let paid_reader = match paid_reader {
                Ok(paid) => paid,
                Err(failure) => {
                    report.record_case(
                        CaseReport::failed_for_mode(
                            &case.case.isolation_key,
                            &case.case.category,
                            mode,
                            failure.kind,
                            failure.message,
                        )
                        .with_rendered_evidence(
                            mode_evidence.rendered_evidence,
                            mode_evidence.rendered_evidence_tokens,
                        ),
                    );
                    record_personamem_failure(
                        case,
                        personamem_cases,
                        &mut outcomes,
                        failure.kind == FailureKind::Provider,
                    );
                    continue;
                }
            };
            let reader_response = ReaderResponse {
                answer: paid_reader.response.text,
                model: paid_reader.response.model.to_string(),
                prompt_version: prompt_version.to_string(),
                usage: paid_reader.actual_usage,
                latency_ms: paid_reader.response.duration_ms,
            };
            if let Err(message) = paid_reader.post_response_budget {
                let mut failed = CaseReport::failed_for_mode(
                    &case.case.isolation_key,
                    &case.case.category,
                    mode,
                    FailureKind::Budget,
                    message,
                )
                .with_rendered_evidence(
                    mode_evidence.rendered_evidence,
                    mode_evidence.rendered_evidence_tokens,
                );
                failed.reader = Some(reader_response);
                report.record_case(failed);
                record_personamem_failure(case, personamem_cases, &mut outcomes, false);
                continue;
            }
            if let Some(question_id) = personamem_question_id(case, personamem_cases) {
                outcomes.insert(
                    question_id.to_string(),
                    PersonaMemAnswerOutcome::Answer(reader_response.answer.clone()),
                );
            }
            let (answer_score_support, answer_score) =
                match answer_score_for_dataset(dataset, &case.case.answer, &reader_response.answer)
                {
                    AnswerScoreOutcome::Supported(score) => (SupportStatus::Supported, Some(score)),
                    AnswerScoreOutcome::Unsupported { reason } => {
                        (SupportStatus::Unsupported { reason }, None)
                    }
                };
            if dataset != LONGMEMEVAL_DATASET {
                report.record_case(
                    CaseReport::completed_for_mode(
                        &case.case.isolation_key,
                        &case.case.category,
                        mode,
                        mode_evidence.rendered_evidence,
                        mode_evidence.rendered_evidence_tokens,
                        answer_score_support.clone(),
                    )
                    .with_generated_answer_outcome(
                        reader_response,
                        answer_score_support,
                        answer_score,
                        None,
                    ),
                );
                continue;
            }
            let typed_case = longmemeval_case(case, longmemeval_cases)
                .context("LongMemEval control lost typed metadata")?;
            let rubric = LongMemEvalRubricKind::for_question(
                typed_case.metadata.question_type,
                typed_case.is_abstention,
            );
            let judge = judge.context("LongMemEval control judge was not constructed")?;
            let judge_model = args
                .judge_model
                .as_deref()
                .context("LongMemEval control judge model missing")?;
            let paid_judge = execute_paid_completion(
                judge,
                longmemeval_judge_request(typed_case, &reader_response, rubric, judge_model)?,
                StageName::Judge,
                mode,
                judge_pricing
                    .cloned()
                    .context("LongMemEval control judge pricing missing")?,
                budget,
                PAID_CALL_TIMEOUT,
            )
            .await;
            let paid_judge = match paid_judge {
                Ok(paid) => paid,
                Err(failure) => {
                    let mut failed = CaseReport::failed_for_mode(
                        &case.case.isolation_key,
                        &case.case.category,
                        mode,
                        failure.kind,
                        failure.message,
                    )
                    .with_rendered_evidence(
                        mode_evidence.rendered_evidence,
                        mode_evidence.rendered_evidence_tokens,
                    );
                    failed.reader = Some(reader_response);
                    failed.answer_score_support = answer_score_support;
                    failed.answer_score = answer_score;
                    report.record_case(failed);
                    continue;
                }
            };
            let raw_label = paid_judge.response.text.trim().to_string();
            let parsed_label = parse_absolute_judge_label(&raw_label);
            let absolute_judge = AbsoluteJudgeResponse {
                supported: parsed_label.unwrap_or(false),
                rationale: raw_label,
                model: paid_judge.response.model.to_string(),
                prompt_version: longmemeval_judge_prompt_version(rubric),
                usage: paid_judge.actual_usage,
                latency_ms: paid_judge.response.duration_ms,
            };
            let failure = paid_judge
                .post_response_budget
                .err()
                .map(|message| (FailureKind::Budget, message))
                .or_else(|| {
                    parsed_label.is_none().then(|| {
                        (
                            FailureKind::Parse,
                            "LongMemEval judge returned neither exact yes nor exact no".to_string(),
                        )
                    })
                });
            let mut case_report = if let Some((kind, message)) = failure {
                CaseReport::failed_for_mode(
                    &case.case.isolation_key,
                    &case.case.category,
                    mode,
                    kind,
                    message,
                )
                .with_rendered_evidence(
                    mode_evidence.rendered_evidence,
                    mode_evidence.rendered_evidence_tokens,
                )
            } else {
                CaseReport::completed_for_mode(
                    &case.case.isolation_key,
                    &case.case.category,
                    mode,
                    mode_evidence.rendered_evidence,
                    mode_evidence.rendered_evidence_tokens,
                    answer_score_support.clone(),
                )
            };
            case_report.reader = Some(reader_response);
            case_report.answer_score_support = answer_score_support;
            case_report.answer_score = answer_score;
            case_report.absolute_judge = Some(absolute_judge);
            report.record_case(case_report);
        }
        outcomes_by_mode.insert(mode, outcomes);
    }
    Ok(outcomes_by_mode)
}

async fn form_case(
    backend: &mut MoaMemoryBackend,
    case: &PreparedExternalMemoryCase,
) -> std::result::Result<(), String> {
    backend.reset(&case.case.isolation_key).await?;
    for turn in &case.chronological_turns {
        backend.ingest(turn).await?;
    }
    backend.settle().await
}

async fn retrieve_case(
    backend: &mut MoaMemoryBackend,
    case: &PreparedExternalMemoryCase,
    evidence_token_budget: usize,
    ranked_occurrence_depth: usize,
) -> std::result::Result<EvidenceExport, String> {
    let evidence = backend
        .retrieve(
            &case.case.question,
            evidence_token_budget,
            ranked_occurrence_depth,
        )
        .await?;
    validate_evidence_export(&evidence, evidence_token_budget, ranked_occurrence_depth)
        .map_err(|error| error.to_string())?;
    Ok(evidence)
}

struct FormationInputs {
    extractor: Arc<dyn moa_memory_ingest::FactExtractor>,
    merge_verifier: Arc<dyn EntityMergeVerifier>,
    extractor_identity: ComponentConfig,
    merge_identity: ComponentConfig,
}

#[derive(Clone)]
struct ExtractionMap(HashMap<String, ExtractionFixtureRecord>);

impl RecordedExtractionStore for ExtractionMap {
    fn get_optional(&self, key: &str) -> Option<&ExtractionFixtureRecord> {
        self.0.get(key)
    }
}

#[derive(Clone)]
struct MergeMap(HashMap<String, EntityMergeFixtureRecord>);

impl RecordedEntityMergeStore for MergeMap {
    fn get_optional(&self, key: &str) -> Option<&EntityMergeFixtureRecord> {
        self.0.get(key)
    }
}

fn resolve_formation_inputs(
    args: &RunExternalMemoryEvalArgs,
    config: &mut MoaConfig,
    budget: LiveRunBudget,
) -> Result<FormationInputs> {
    match args.formation_mode {
        FormationMode::Heuristic => Ok(FormationInputs {
            extractor: Arc::new(HeuristicFactExtractor),
            merge_verifier: Arc::new(DeterministicEntityMergeVerifier),
            extractor_identity: component("heuristic-v1", None, None),
            merge_identity: component("deterministic-v1", None, None),
        }),
        FormationMode::Recorded => {
            let manifest_path = args
                .recorded_manifest
                .as_ref()
                .expect("recorded manifest was validated");
            let manifest: RecordedFormationManifestV1 =
                serde_json::from_slice(&std::fs::read(manifest_path).with_context(|| {
                    format!("read recorded manifest {}", manifest_path.display())
                })?)
                .context("parse recorded formation manifest")?;
            manifest.validate().map_err(anyhow::Error::from)?;
            let root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
            let extraction_path = root.join(&manifest.extraction_fixture_path);
            let merge_path = root.join(&manifest.merge_fixture_path);
            verify_sha256(&extraction_path, &manifest.extraction_fixture_sha256)?;
            verify_sha256(&merge_path, &manifest.merge_fixture_sha256)?;
            let extraction_records: Vec<ExtractionFixtureRecord> =
                load_json_records(&extraction_path)?;
            let merge_records: Vec<EntityMergeFixtureRecord> = load_json_records(&merge_path)?;
            let extraction_model = uniform_field(
                &extraction_records,
                |record| record.model.as_str(),
                "recorded extraction model",
            )?;
            let extraction_prompt = uniform_field(
                &extraction_records,
                |record| record.prompt_version.as_str(),
                "recorded extraction prompt",
            )?;
            let merge_prompt = uniform_field(
                &merge_records,
                |record| record.prompt_version.as_str(),
                "recorded merge prompt",
            )?;
            let extraction_map = extraction_records
                .into_iter()
                .map(|record| (record.chunk_hash.clone(), record))
                .collect();
            let merge_map = merge_records
                .into_iter()
                .map(|record| (record.key.clone(), record))
                .collect();
            Ok(FormationInputs {
                extractor: Arc::new(RecordedFactExtractor::new(
                    ExtractionMap(extraction_map),
                    "cargo xtask record-memory-extractions",
                )),
                merge_verifier: Arc::new(RecordedEntityMergeVerifier::new(
                    MergeMap(merge_map),
                    "cargo xtask record-memory-merges",
                )),
                extractor_identity: component(
                    "recorded-v1",
                    Some(extraction_model),
                    Some(extraction_prompt),
                ),
                merge_identity: component("recorded-v1", None, Some(merge_prompt)),
            })
        }
        FormationMode::Live => {
            let extractor_model = args
                .extractor_model
                .clone()
                .expect("live extractor was validated");
            let merge_model = args
                .merge_verifier_model
                .clone()
                .expect("live merge verifier was validated");
            config.memory.extraction.enabled = true;
            config.memory.extraction.model = extractor_model.clone();
            let extraction_observer = Arc::new(AccountingModelObserver::new(
                StageName::FormationExtraction,
                &extractor_model,
                budget.clone(),
            )?);
            let extractor =
                ModelFactExtractor::from_config_with_observer(config, extraction_observer)
                    .context("construct explicit live fact extractor")?;
            config.memory.extraction.model = merge_model.clone();
            let merge_observer = Arc::new(AccountingModelObserver::new(
                StageName::FormationMerge,
                &merge_model,
                budget,
            )?);
            let merge = ModelEntityMergeVerifier::from_config_with_observer(config, merge_observer)
                .context("construct explicit live merge verifier")?;
            Ok(FormationInputs {
                extractor: Arc::new(extractor),
                merge_verifier: Arc::new(merge),
                extractor_identity: component(
                    "model-v1",
                    Some(extractor_model),
                    Some(
                        moa_memory_ingest::model_fact_extractor::EXTRACTION_PROMPT_VERSION
                            .to_string(),
                    ),
                ),
                merge_identity: component(
                    "model-verifier-v1",
                    Some(merge_model),
                    Some(moa_memory_ingest::model_entity_merge::MERGE_PROMPT_VERSION.to_string()),
                ),
            })
        }
    }
}

fn resolved_formation_config(
    args: &RunExternalMemoryEvalArgs,
    inputs: &FormationInputs,
    embedder: &dyn EmbeddingProvider,
) -> ResolvedFormationConfig {
    let consolidation = ConsolidationOptions::default();
    ResolvedFormationConfig {
        schema_version: 1,
        mode: args.formation_mode,
        extractor: inputs.extractor_identity.clone(),
        merge: inputs.merge_identity.clone(),
        embedding: EmbeddingConfig {
            provider: args
                .embedding_selector
                .split_once(':')
                .map_or("explicit", |(provider, _)| provider)
                .to_string(),
            model: embedder.model_id().to_string(),
            version: embedder.model_version(),
            dimensions: embedder.dimensions(),
        },
        entity_blocking: EntityBlockingConfig {
            enabled: false,
            cosine_threshold: "0.91".to_string(),
        },
        pii_classifier: component("heuristic-v1", None, None),
        contradiction_detector: component("rrf-plus-heuristic-v1", None, None),
        consolidation: ConsolidationSettings {
            decay_idle_days: consolidation.decay_idle_days,
            decay_half_life_days: consolidation.decay_half_life_days.to_string(),
            decay_floor: consolidation.decay_floor.to_string(),
            expire_idle_days: consolidation.expire_idle_days,
            digest_enabled: consolidation.digest.enabled,
            digest_max_tokens: consolidation.digest.max_tokens,
            digest_rebuild_min_interval_hours: consolidation.digest.rebuild_min_interval_hours,
        },
    }
}

fn component(
    implementation: &str,
    model: Option<String>,
    prompt_version: Option<String>,
) -> ComponentConfig {
    ComponentConfig {
        implementation: implementation.to_string(),
        model,
        prompt_version,
    }
}

fn reader_request(
    case: &PreparedExternalMemoryCase,
    evidence: &str,
    model: &str,
    dataset: &str,
    output_token_reserve: u64,
) -> CompletionRequest {
    let prompt = render_reader_prompt(case, evidence, reader_prompt_version(dataset), dataset);
    CompletionRequest {
        model: Some(model.into()),
        messages: vec![
            ContextMessage::system(prompt.system),
            ContextMessage::user(prompt.user),
        ],
        tools: Vec::new(),
        max_output_tokens: usize::try_from(output_token_reserve).ok(),
        temperature: Some(0.0),
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    }
}

fn reader_prompt_version(dataset: &str) -> &'static str {
    match dataset {
        PERSONAMEM_DATASET => "personamem-label-v1",
        LONGMEMEVAL_DATASET => LONGMEMEVAL_READER_PROMPT_VERSION,
        _ => "common-json-reader-v1",
    }
}

fn longmemeval_case<'a>(
    case: &PreparedExternalMemoryCase,
    longmemeval_cases: &'a Option<Vec<PreparedLongMemEvalCase>>,
) -> Option<&'a PreparedLongMemEvalCase> {
    longmemeval_cases.as_ref().and_then(|cases| {
        cases
            .iter()
            .find(|candidate| candidate.prepared.case.isolation_key == case.case.isolation_key)
    })
}

fn longmemeval_judge_request(
    case: &PreparedLongMemEvalCase,
    reader: &ReaderResponse,
    rubric: LongMemEvalRubricKind,
    judge_model: &str,
) -> Result<CompletionRequest> {
    let prompt = rubric
        .render(
            &case.prepared.case.question,
            &case.prepared.case.answer,
            &reader.answer,
        )
        .map_err(anyhow::Error::from)?;
    Ok(CompletionRequest {
        model: Some(ModelId::new(
            judge_model
                .split_once(':')
                .map_or(judge_model, |(_, model)| model),
        )),
        messages: vec![ContextMessage::user(prompt)],
        tools: Vec::new(),
        max_output_tokens: Some(8),
        temperature: Some(0.0),
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    })
}

fn longmemeval_judge_prompt_version(_rubric: LongMemEvalRubricKind) -> String {
    LONGMEMEVAL_JUDGE_PROMPT_VERSION.to_string()
}

#[allow(clippy::too_many_arguments)]
fn failed_after_reader(
    case: &PreparedExternalMemoryCase,
    rendered_evidence: String,
    rendered_evidence_tokens: u64,
    reader: ReaderResponse,
    support_status: SupportStatus,
    answer_score: Option<AnswerScore>,
    absolute_judge: Option<AbsoluteJudgeResponse>,
    kind: FailureKind,
    message: impl Into<String>,
) -> CaseReport {
    let message = message.into();
    let mut report = CaseReport::failed(
        &case.case.isolation_key,
        &case.case.category,
        kind,
        &message,
    );
    report.rendered_evidence = rendered_evidence;
    report.rendered_evidence_tokens = rendered_evidence_tokens;
    report.answer_score_support = support_status;
    report.reader = Some(reader);
    report.answer_score = answer_score;
    report.absolute_judge = absolute_judge;
    report
}

fn reader_pricing(provider: &dyn LLMProvider, model: &str) -> PricingSnapshotV1 {
    let pricing = provider.capabilities().pricing;
    PricingSnapshotV1 {
        model: model.to_string(),
        effective_date: Utc::now().date_naive().to_string(),
        input_per_million_usd: pricing.input_per_mtok,
        output_per_million_usd: pricing.output_per_mtok,
        cache_read_per_million_usd: pricing
            .cached_input_per_mtok
            .unwrap_or(pricing.input_per_mtok),
        cache_write_per_million_usd: pricing.cache_write_per_mtok(),
    }
}

fn chat_pricing(model_selector: &str) -> Result<PricingSnapshotV1> {
    let model = model_selector
        .split_once(':')
        .map_or(model_selector, |(_, model)| model);
    let pricing = moa_providers::pricing_for_model(model).with_context(|| {
        format!("no model-aware pricing is registered for live model `{model_selector}`")
    })?;
    Ok(PricingSnapshotV1 {
        model: model_selector.to_string(),
        effective_date: PRICING_AS_OF.to_string(),
        input_per_million_usd: pricing.input_per_mtok,
        output_per_million_usd: pricing.output_per_mtok,
        cache_read_per_million_usd: pricing
            .cached_input_per_mtok
            .unwrap_or(pricing.input_per_mtok),
        cache_write_per_million_usd: pricing.cache_write_per_mtok(),
    })
}

fn embedding_pricing(selector: &str, actual_model: &str) -> Result<PricingSnapshotV1> {
    let (provider, configured_model) = selector.split_once(':').with_context(|| {
        format!("embedding selector `{selector}` must use explicit provider:model syntax")
    })?;
    if configured_model != actual_model {
        bail!(
            "embedding selector model `{configured_model}` resolved to unexpected model `{actual_model}`"
        );
    }
    let input_per_million_usd = match (provider, configured_model) {
        ("gemini" | "google", "gemini-embedding-2") => 0.20,
        ("cohere", "embed-v4.0") => 0.12,
        ("openai", "text-embedding-3-small") => 0.02,
        _ => {
            bail!("no dated input-token pricing is registered for embedding selector `{selector}`")
        }
    };
    Ok(PricingSnapshotV1 {
        model: selector.to_string(),
        effective_date: PRICING_AS_OF.to_string(),
        input_per_million_usd,
        output_per_million_usd: 0.0,
        cache_read_per_million_usd: input_per_million_usd,
        cache_write_per_million_usd: input_per_million_usd,
    })
}

fn estimated_completion_usage(request: &CompletionRequest) -> NormalizedUsage {
    let message_tokens = request
        .messages
        .iter()
        .map(|message| estimate_text_tokens(&message.content))
        .sum::<usize>();
    let tool_tokens = if request.tools.is_empty() {
        0
    } else {
        serde_json::to_string(&request.tools).map_or(0, |tools| estimate_text_tokens(&tools))
    };
    NormalizedUsage {
        input_tokens_uncached: message_tokens.saturating_add(tool_tokens),
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: request.max_output_tokens.unwrap_or(0),
        provenance: UsageProvenance::Estimated,
    }
}

fn normalized_response_usage(response: &CompletionResponse) -> NormalizedUsage {
    NormalizedUsage {
        input_tokens_uncached: response.usage.input_tokens_uncached,
        input_tokens_cache_write: response.usage.input_tokens_cache_write,
        input_tokens_cache_read: response.usage.input_tokens_cache_read,
        output_tokens: response.usage.output_tokens,
        provenance: UsageProvenance::Actual,
    }
}

#[derive(Debug)]
struct PaidCompletion {
    response: CompletionResponse,
    actual_usage: NormalizedUsage,
    post_response_budget: std::result::Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaidCallFailure {
    kind: FailureKind,
    message: String,
}

async fn execute_paid_completion(
    provider: &dyn LLMProvider,
    request: CompletionRequest,
    stage: StageName,
    mode: ExternalMemoryMode,
    pricing: PricingSnapshotV1,
    budget: &LiveRunBudget,
    call_timeout: Duration,
) -> std::result::Result<PaidCompletion, PaidCallFailure> {
    let accounting_id = budget
        .forecast(
            stage,
            Some(mode),
            pricing,
            estimated_completion_usage(&request),
        )
        .await
        .map_err(|message| PaidCallFailure {
            kind: FailureKind::Budget,
            message,
        })?;
    let started = Instant::now();
    let response = match timeout(call_timeout, async {
        let stream = provider.complete(request).await?;
        stream.into_response().await
    })
    .await
    {
        Err(_) => {
            budget
                .record_failure(accounting_id, elapsed_ms(started))
                .await;
            return Err(PaidCallFailure {
                kind: FailureKind::Timeout,
                message: format!("{} call timed out", stage_name(stage)),
            });
        }
        Ok(Err(error)) => {
            budget
                .record_failure(accounting_id, elapsed_ms(started))
                .await;
            return Err(PaidCallFailure {
                kind: FailureKind::Provider,
                message: format!("{} provider failure: {error}", stage_name(stage)),
            });
        }
        Ok(Ok(response)) => response,
    };
    let actual_usage = normalized_response_usage(&response);
    let post_response_budget = budget
        .record_actual(accounting_id, actual_usage.clone(), elapsed_ms(started))
        .await;
    Ok(PaidCompletion {
        response,
        actual_usage,
        post_response_budget,
    })
}

const fn stage_name(stage: StageName) -> &'static str {
    match stage {
        StageName::Reader => "reader",
        StageName::Judge => "judge",
        StageName::FormationExtraction => "formation extraction",
        StageName::FormationMerge => "formation merge",
        StageName::Embedding => "embedding",
        StageName::Retrieval => "retrieval",
    }
}

fn budget_tail_reports(cases: &[PreparedExternalMemoryCase], message: &str) -> Vec<CaseReport> {
    cases
        .iter()
        .map(|case| {
            CaseReport::failed(
                &case.case.isolation_key,
                &case.case.category,
                FailureKind::Budget,
                message,
            )
        })
        .collect()
}

fn build_longmemeval_report(
    cases: &[PreparedLongMemEvalCase],
    rankings: &BTreeMap<String, Vec<LongMemEvalOccurrenceRef>>,
    reports: &[CaseReport],
    judge_model: &str,
) -> Result<LongMemEvalModeMetricsV2> {
    let retrieval = aggregate_retrieval_metrics(cases, rankings).map_err(anyhow::Error::from)?;
    let correct = reports
        .iter()
        .filter(|report| {
            report.failure.is_none()
                && report
                    .absolute_judge
                    .as_ref()
                    .is_some_and(|judge| judge.supported)
        })
        .map(|report| report.isolation_key.as_str())
        .collect::<HashSet<_>>();
    let answers = LongMemEvalAnswerSliceV1 {
        numerator: cases
            .iter()
            .filter(|case| correct.contains(case.prepared.case.isolation_key.as_str()))
            .count(),
        denominator: cases.len(),
    };
    let abstentions = answer_slice(cases.iter().filter(|case| case.is_abstention), &correct);
    let question_type_slices = moa_eval::external_memory::longmemeval::LongMemEvalQuestionType::ALL
        .into_iter()
        .map(|question_type| {
            (
                question_type.as_str().to_string(),
                answer_slice(
                    cases
                        .iter()
                        .filter(|case| case.metadata.question_type == question_type),
                    &correct,
                ),
            )
        })
        .collect();
    let mut failure_counts = BTreeMap::new();
    for failure in reports.iter().filter_map(|report| report.failure.as_ref()) {
        *failure_counts
            .entry(failure_kind_name(failure.kind).to_string())
            .or_default() += 1;
    }
    Ok(LongMemEvalModeMetricsV2 {
        schema_version: 1,
        judge_model: judge_model.to_string(),
        judge_prompt_version: LONGMEMEVAL_JUDGE_PROMPT_VERSION.to_string(),
        rubric_version: LONGMEMEVAL_RUBRIC_VERSION.to_string(),
        rubric_bundle_sha256: LONGMEMEVAL_RUBRIC_BUNDLE_SHA256.to_string(),
        answers,
        abstentions,
        question_type_slices,
        retrieval: RetrievalMetricsV2::Supported {
            metrics: Box::new(retrieval),
        },
        failure_counts,
    })
}

fn answer_slice<'a>(
    cases: impl Iterator<Item = &'a PreparedLongMemEvalCase>,
    correct: &HashSet<&str>,
) -> LongMemEvalAnswerSliceV1 {
    let mut slice = LongMemEvalAnswerSliceV1 {
        numerator: 0,
        denominator: 0,
    };
    for case in cases {
        slice.denominator += 1;
        slice.numerator += usize::from(correct.contains(case.prepared.case.isolation_key.as_str()));
    }
    slice
}

const fn failure_kind_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Timeout => "timeout",
        FailureKind::Budget => "budget",
        FailureKind::Provider => "provider",
        FailureKind::Parse => "parse",
        FailureKind::Backend => "backend",
    }
}

async fn record_budget_observations(
    report: &mut ExternalMemoryReportBuilder,
    budget: &LiveRunBudget,
) -> usize {
    let observations = budget.drain_observations().await;
    let count = observations.len();
    for observation in observations {
        report.record_stage(observation);
    }
    report.set_budget(budget.report_budget().await);
    count
}

fn answer_score_for_dataset(dataset: &str, reference: &str, candidate: &str) -> AnswerScoreOutcome {
    if dataset == LONGMEMEVAL_DATASET {
        return AnswerScoreOutcome::Unsupported {
            reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
        };
    }
    if dataset == PERSONAMEM_DATASET {
        return AnswerScoreOutcome::Supported(AnswerScore {
            metric: "personamem_label_accuracy_v1".to_string(),
            value: moa_eval::external_memory::personamem::PersonaMemLabelScorerV1::score_text(
                reference, candidate,
            ),
            denominator: 1,
        });
    }
    AnswerScoreOutcome::Supported(AnswerScore {
        metric: "common_json_exact_match".to_string(),
        value: f64::from(normalize_answer(reference) == normalize_answer(candidate)),
        denominator: 1,
    })
}

fn personamem_question_id<'a>(
    case: &PreparedExternalMemoryCase,
    personamem_cases: &'a Option<Vec<PreparedPersonaMemCase>>,
) -> Option<&'a str> {
    personamem_cases.as_ref().and_then(|cases| {
        cases
            .iter()
            .find(|candidate| candidate.prepared.case.isolation_key == case.case.isolation_key)
            .map(|candidate| candidate.metadata.question_id.as_str())
    })
}

fn record_personamem_failure(
    case: &PreparedExternalMemoryCase,
    personamem_cases: &Option<Vec<PreparedPersonaMemCase>>,
    outcomes: &mut BTreeMap<String, PersonaMemAnswerOutcome>,
    provider_failure: bool,
) {
    if let Some(question_id) = personamem_question_id(case, personamem_cases) {
        outcomes.insert(
            question_id.to_string(),
            if provider_failure {
                PersonaMemAnswerOutcome::ProviderFailure
            } else {
                PersonaMemAnswerOutcome::ParseFailure
            },
        );
    }
}

fn normalize_answer(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn write_partial_report(report: &ExternalMemoryReportBuilder, output: &Path) -> Result<()> {
    let finished = report.clone().finish();
    let bytes = finished.canonical_json().map_err(anyhow::Error::from)?;
    let parent = output
        .parent()
        .context("external-memory output has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "create external-memory output directory {}",
            parent.display()
        )
    })?;
    let temp = output.with_extension("json.tmp");
    std::fs::write(&temp, bytes)
        .with_context(|| format!("write partial external-memory report {}", temp.display()))?;
    std::fs::rename(&temp, output)
        .with_context(|| format!("publish external-memory report {}", output.display()))?;
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read fixture {}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!(
            "fixture {} SHA-256 mismatch: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn verify_data_provenance(path: &Path, package: &DatasetPackageV1) -> Result<()> {
    let provenance = package
        .manifest
        .files
        .iter()
        .find(|file| path.ends_with(&file.path))
        .with_context(|| {
            format!(
                "dataset path {} is not pinned by the package manifest",
                path.display()
            )
        })?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("read dataset package file {}", path.display()))?;
    let actual_bytes =
        u64::try_from(bytes.len()).context("dataset file length does not fit u64")?;
    if actual_bytes != provenance.size_bytes {
        bail!(
            "dataset file {} length mismatch: expected {}, got {actual_bytes}",
            path.display(),
            provenance.size_bytes
        );
    }
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if actual_sha256 != provenance.sha256 {
        bail!(
            "dataset file {} SHA-256 mismatch: expected {}, got {actual_sha256}",
            path.display(),
            provenance.sha256
        );
    }
    Ok(())
}

fn load_json_records<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read JSON fixture {}", path.display()))?;
    if let Ok(records) = serde_json::from_str::<Vec<T>>(&text) {
        return Ok(records);
    }
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parse JSONL fixture record"))
        .collect()
}

fn uniform_field<T>(records: &[T], field: impl Fn(&T) -> &str, name: &str) -> Result<String> {
    let Some(first) = records.first() else {
        bail!("{name} fixture set must not be empty");
    };
    let expected = field(first);
    if expected.trim().is_empty() || records.iter().any(|record| field(record) != expected) {
        bail!("{name} must be non-empty and uniform across fixtures");
    }
    Ok(expected.to_string())
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<RunExternalMemoryEvalArgs> {
    let mut values = HashMap::<String, String>::new();
    let mut migrate_database = false;
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        if !flag.starts_with("--") {
            bail!("unexpected positional argument `{flag}`");
        }
        if flag == "--migrate-database" {
            if migrate_database {
                bail!("duplicate argument `{flag}`");
            }
            migrate_database = true;
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("missing value for {flag}"))?;
        if value.starts_with("--") {
            bail!("missing value for {flag}");
        }
        if values.insert(flag.clone(), value).is_some() {
            bail!("duplicate argument `{flag}`");
        }
    }

    let formation_mode = match required(&values, "--formation-mode")?.as_str() {
        "heuristic" => FormationMode::Heuristic,
        "recorded" => FormationMode::Recorded,
        "live" => FormationMode::Live,
        value => bail!("unsupported --formation-mode `{value}`"),
    };
    Ok(RunExternalMemoryEvalArgs {
        dataset: required(&values, "--dataset")?,
        data: required(&values, "--data")?.into(),
        package_manifest: required(&values, "--package-manifest")?.into(),
        fetch_summary: optional(&values, "--fetch-summary").map(PathBuf::from),
        migrate_database,
        output: required(&values, "--output")?.into(),
        formation_mode,
        embedding_selector: required(&values, "--embedding-selector")?,
        reader_model: required(&values, "--reader-model")?,
        judge_model: optional(&values, "--judge-model"),
        reader_context_window: required(&values, "--reader-context-window")?
            .parse()
            .context("parse --reader-context-window")?,
        reader_output_token_reserve: required(&values, "--reader-output-token-reserve")?
            .parse()
            .context("parse --reader-output-token-reserve")?,
        controls: required(&values, "--controls")?,
        evidence_token_budget: required(&values, "--evidence-token-budget")?
            .parse()
            .context("parse --evidence-token-budget")?,
        budget_usd: required(&values, "--budget-usd")?
            .parse()
            .context("parse --budget-usd")?,
        recorded_manifest: optional(&values, "--recorded-manifest").map(PathBuf::from),
        extractor_model: optional(&values, "--extractor-model"),
        merge_verifier_model: optional(&values, "--merge-verifier-model"),
    })
}

fn validate_args(args: &RunExternalMemoryEvalArgs, live_authorized: bool) -> Result<()> {
    if args.embedding_selector.trim().is_empty() || args.reader_model.trim().is_empty() {
        bail!("embedding-selector and reader-model must not be blank");
    }
    if !matches!(args.evidence_token_budget, 512 | 1024 | 2048) {
        bail!("evidence-token-budget must be exactly 512, 1024, or 2048");
    }
    if args.reader_context_window == 0 || args.reader_output_token_reserve == 0 {
        bail!("reader context window and output-token reserve must be positive");
    }
    if args.controls != "no-memory,full-context,oracle-evidence" {
        bail!("controls must normalize exactly to no-memory,full-context,oracle-evidence");
    }
    if matches!(
        args.dataset.as_str(),
        PERSONAMEM_DATASET | LONGMEMEVAL_DATASET
    ) {
        if args.fetch_summary.is_none() {
            bail!("PersonaMem and LongMemEval require --fetch-summary");
        }
        if !args.migrate_database {
            bail!("PersonaMem and LongMemEval require --migrate-database");
        }
    }
    if args.judge_model.as_deref().is_some_and(str::is_empty) {
        bail!("judge-model must not be blank when provided");
    }
    BudgetLedger::new(args.budget_usd).map_err(anyhow::Error::from)?;
    validate_target_output(&args.output)?;
    match args.formation_mode {
        FormationMode::Heuristic => {
            if args.recorded_manifest.is_some()
                || args.extractor_model.is_some()
                || args.merge_verifier_model.is_some()
            {
                bail!("heuristic formation rejects recorded/live-only inputs");
            }
        }
        FormationMode::Recorded => {
            if args.recorded_manifest.is_none() {
                bail!("recorded formation requires --recorded-manifest");
            }
            if args.extractor_model.is_some() || args.merge_verifier_model.is_some() {
                bail!("recorded formation rejects live model selectors");
            }
        }
        FormationMode::Live => {
            if !live_authorized {
                bail!("live formation requires {LIVE_FLAG}=1 before provider construction");
            }
            if args.extractor_model.as_deref().is_none_or(str::is_empty)
                || args
                    .merge_verifier_model
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                bail!("live formation requires --extractor-model and --merge-verifier-model");
            }
            if args.recorded_manifest.is_some() {
                bail!("live formation rejects --recorded-manifest");
            }
        }
    }
    Ok(())
}

fn required(values: &HashMap<String, String>, flag: &str) -> Result<String> {
    values
        .get(flag)
        .cloned()
        .with_context(|| format!("missing required argument {flag}"))
}

fn optional(values: &HashMap<String, String>, flag: &str) -> Option<String> {
    values.get(flag).cloned()
}

fn validate_target_output(path: &Path) -> Result<()> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name == "target")
        || components.any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("--output must be a relative path beneath target/");
    }
    if path == Path::new("target") {
        bail!("--output must name a file beneath target/");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use moa_core::{
        types::completion::CompletionContent, types::completion::CompletionStream,
        types::completion::StopReason, types::completion::TokenUsage, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    };

    use super::*;

    fn base(mode: &str) -> Vec<String> {
        [
            "--dataset",
            "common-json",
            "--data",
            "crates/moa-eval/tests/fixtures/external_memory/common_cases.json",
            "--package-manifest",
            "target/memory-benchmarks/package.json",
            "--output",
            "target/memory-benchmarks/report.json",
            "--formation-mode",
            mode,
            "--embedding-selector",
            "gemini:gemini-embedding-2",
            "--reader-model",
            "openai:gpt-5.4-mini",
            "--reader-context-window",
            "128000",
            "--reader-output-token-reserve",
            "256",
            "--controls",
            "no-memory,full-context,oracle-evidence",
            "--evidence-token-budget",
            "1024",
            "--budget-usd",
            "25",
            "--migrate-database",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn task_11_run_external_memory_eval_requires_exact_controls_reader_limits_and_migration() {
        // Pins: all workflow safety inputs fail in pure validation before runtime construction.
        let args = parse_args(base("heuristic").into_iter()).expect("base args parse");
        validate_args(&args, true).expect("complete base args validate");

        let mut invalid_controls = args.clone();
        invalid_controls.controls = "full-context,no-memory,oracle-evidence".to_string();
        assert!(validate_args(&invalid_controls, true).is_err());
        let mut invalid_window = args.clone();
        invalid_window.reader_context_window = 0;
        assert!(validate_args(&invalid_window, true).is_err());
        let mut invalid_reserve = args.clone();
        invalid_reserve.reader_output_token_reserve = 0;
        assert!(validate_args(&invalid_reserve, true).is_err());
        let mut invalid_evidence = args.clone();
        invalid_evidence.evidence_token_budget = 513;
        assert!(validate_args(&invalid_evidence, true).is_err());
        let mut external = args;
        external.dataset = PERSONAMEM_DATASET.to_string();
        external.fetch_summary = Some("target/summary.json".into());
        external.migrate_database = false;
        assert!(validate_args(&external, true).is_err());
    }

    #[test]
    fn task_11_run_external_memory_eval_orders_migration_before_provider_construction() {
        // Pins: source order makes migration failure a closed gate before every provider factory.
        let source = include_str!("run_external_memory_eval.rs");
        let run_start = source
            .find("async fn run_validated(")
            .expect("runner function");
        let source = &source[run_start..];
        let migration = source
            .find("moa_migrations::run(&database_url)")
            .expect("migration call");
        let embedder = source
            .find("build_embedder_from_config")
            .expect("embedder construction");
        let reader = source
            .find("build_provider_from_selection")
            .expect("reader construction");
        assert!(migration < embedder);
        assert!(migration < reader);
    }

    #[test]
    fn task_11_run_external_memory_eval_validates_fetch_summary_before_runtime() {
        // Pins: the run command shares the strict summary wire and rejects count/provenance drift.
        let package = DatasetPackageV1::new(
            moa_eval::external_memory::dataset::DatasetPackageManifestV1 {
                schema_version: 1,
                dataset: PERSONAMEM_DATASET.to_string(),
                source: moa_eval::external_memory::dataset::DatasetPackageSourceV1 {
                    repository: "fixture/personamem".to_string(),
                    revision: "fixture-v1".to_string(),
                },
                files: vec![moa_eval::external_memory::dataset::DatasetFileProvenance {
                    path: "questions.csv".to_string(),
                    size_bytes: 1,
                    sha256: "a".repeat(64),
                }],
            },
        )
        .expect("package hashes");
        let summary = VerifiedFetchSummaryV1::PersonaMem(
            moa_eval::external_memory::dataset::PersonaMemFetchSummaryV1 {
                schema_version: 1,
                dataset: package.manifest.dataset.clone(),
                repository: package.manifest.source.repository.clone(),
                revision: package.manifest.source.revision.clone(),
                package_sha256: package.package_sha256.clone(),
                question_count: 2,
                persona_count: 1,
                context_count: 1,
                verified: true,
            },
        );
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("summary.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&summary).expect("serialize summary"),
        )
        .expect("write summary");
        validate_fetch_summary_before_runtime(
            Some(&path),
            &package,
            Some(&LoadedDatasetCounts::PersonaMem {
                questions: 2,
                personas: 1,
                contexts: 1,
            }),
        )
        .expect("matching summary validates");
        assert!(
            validate_fetch_summary_before_runtime(
                Some(&path),
                &package,
                Some(&LoadedDatasetCounts::PersonaMem {
                    questions: 3,
                    personas: 1,
                    contexts: 1,
                }),
            )
            .is_err()
        );
    }

    #[test]
    fn run_external_memory_eval_requires_every_core_input() {
        // Pins: data/package/output/formation/embedder/reader/evidence/budget are all explicit.
        let flags = [
            "--data",
            "--package-manifest",
            "--output",
            "--formation-mode",
            "--embedding-selector",
            "--reader-model",
            "--evidence-token-budget",
            "--budget-usd",
        ];
        for flag in flags {
            let mut args = base("heuristic");
            let position = args.iter().position(|value| value == flag).expect("flag");
            args.drain(position..=position + 1);
            let error = parse_args(args.into_iter()).expect_err("missing flag must fail");
            assert!(error.to_string().contains(flag), "{flag}: {error}");
        }
    }

    #[test]
    fn run_external_memory_eval_rejects_invalid_budget_and_output() {
        // Pins: spend is positive/finite and artifacts cannot escape target/.
        for value in ["0", "-1", "NaN", "inf"] {
            let mut args = parse_args(base("heuristic").into_iter()).expect("parse base");
            args.budget_usd = value.parse().expect("f64 value");
            assert!(validate_args(&args, false).is_err(), "budget {value}");
        }
        let mut args = parse_args(base("heuristic").into_iter()).expect("parse base");
        args.output = "docs/report.json".into();
        assert!(validate_args(&args, false).is_err());
    }

    #[test]
    fn run_external_memory_eval_requires_separate_recorded_manifest() {
        // Pins: recorded extraction/merge provenance is a required versioned input.
        let args = parse_args(base("recorded").into_iter()).expect("parse base");
        assert!(
            validate_args(&args, false)
                .expect_err("missing recorded manifest")
                .to_string()
                .contains("--recorded-manifest")
        );
        let mut values = base("recorded");
        values.extend([
            "--recorded-manifest".to_string(),
            "fixtures/recorded.json".to_string(),
        ]);
        let args = parse_args(values.into_iter()).expect("parse recorded args");
        validate_args(&args, false).expect("recorded args should validate");
    }

    #[test]
    fn run_external_memory_eval_checks_live_gate_before_provider_construction() {
        // Pins: missing authorization/selectors fail before any provider factory can run.
        let provider_constructions = AtomicUsize::new(0);
        let mut values = base("live");
        values.extend([
            "--extractor-model".to_string(),
            "openai:gpt-5.4-mini".to_string(),
            "--merge-verifier-model".to_string(),
            "openai:gpt-5.4-mini".to_string(),
        ]);
        let args = parse_args(values.into_iter()).expect("parse live args");
        let result = validate_args(&args, false).map(|()| {
            provider_constructions.fetch_add(1, Ordering::SeqCst);
        });
        assert!(result.is_err());
        assert_eq!(provider_constructions.load(Ordering::SeqCst), 0);
        validate_args(&args, true).expect("authorized live args should validate");
    }

    fn completion_request(max_output_tokens: usize) -> CompletionRequest {
        CompletionRequest {
            model: Some(ModelId::new("gpt-5.4-mini")),
            messages: vec![ContextMessage::user("remember this preference")],
            tools: Vec::new(),
            max_output_tokens: Some(max_output_tokens),
            temperature: Some(0.0),
            response_format: None,
            native_web_search: Default::default(),
            metadata: Default::default(),
        }
    }

    fn completion_response(usage: TokenUsage) -> CompletionResponse {
        CompletionResponse {
            text: "[]".to_string(),
            content: Vec::<CompletionContent>::new(),
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("gpt-5.4-mini"),
            usage,
            duration_ms: 7,
            thought_signature: None,
        }
    }

    #[tokio::test]
    async fn run_external_memory_eval_live_model_observer_forecasts_then_records_provider_usage() {
        // Pins: extraction/merge reserve against the shared ledger before dispatch and retain the
        // exact normalized provider usage returned after the paid call.
        let budget = LiveRunBudget::new(1.0).expect("budget");
        let observer = AccountingModelObserver::new(
            StageName::FormationExtraction,
            "openai:gpt-5.4-mini",
            budget.clone(),
        )
        .expect("observer");
        let request = completion_request(32);
        observer.before_call(&request).await.expect("forecast");
        let actual = TokenUsage {
            input_tokens_uncached: 17,
            input_tokens_cache_write: 3,
            input_tokens_cache_read: 5,
            output_tokens: 11,
        };
        observer
            .after_response(&completion_response(actual))
            .await
            .expect("actual usage");

        let records = budget.records().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stage, StageName::FormationExtraction);
        assert_eq!(
            records[0].estimated_usage.provenance,
            UsageProvenance::Estimated
        );
        assert_eq!(
            records[0].actual_usage,
            Some(NormalizedUsage {
                input_tokens_uncached: 17,
                input_tokens_cache_write: 3,
                input_tokens_cache_read: 5,
                output_tokens: 11,
                provenance: UsageProvenance::Actual,
            })
        );
        let observations = budget.drain_observations().await;
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].accounting, Some(records[0].clone()));
    }

    #[tokio::test]
    async fn run_external_memory_eval_actual_overage_is_sticky_and_skips_next_paid_stage() {
        // Pins: a provider response can exceed its forecast, and that overage prevents every later
        // provider reservation while preserving the completed call's actual usage.
        let budget = LiveRunBudget::new(0.000_02).expect("budget");
        let observer = AccountingModelObserver::new(
            StageName::FormationMerge,
            "openai:gpt-5.4-mini",
            budget.clone(),
        )
        .expect("observer");
        let request = completion_request(1);
        observer.before_call(&request).await.expect("forecast fits");
        let error = observer
            .after_response(&completion_response(TokenUsage {
                input_tokens_uncached: 1,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 100,
            }))
            .await
            .expect_err("actual usage must exceed budget");
        assert!(
            error
                .to_string()
                .contains("actual provider usage exceeds budget")
        );
        let next_error = observer
            .before_call(&request)
            .await
            .expect_err("terminal overage must reject later stages");
        assert_eq!(next_error.to_string(), error.to_string());
        let records = budget.records().await;
        assert_eq!(records.len(), 1, "no later reservation may be admitted");
        assert_eq!(
            records[0]
                .actual_usage
                .as_ref()
                .expect("completed usage")
                .output_tokens,
            100
        );
    }

    struct FakeEmbeddingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EmbeddingProvider for FakeEmbeddingProvider {
        fn model_id(&self) -> &str {
            "gemini-embedding-2"
        }

        fn dimensions(&self) -> usize {
            2
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![vec![0.0, 1.0]; inputs.len()])
        }
    }

    #[tokio::test]
    async fn run_external_memory_eval_embedder_accounts_completed_input_batch() {
        // Pins: formation and query embeddings share the cumulative ledger, forecast before the
        // real provider, and record the actual completed input batch when no usage is returned.
        let calls = Arc::new(AtomicUsize::new(0));
        let budget = LiveRunBudget::new(1.0).expect("budget");
        let embedder = AccountingEmbeddingProvider::new(
            Arc::new(FakeEmbeddingProvider {
                calls: calls.clone(),
            }),
            "gemini:gemini-embedding-2",
            budget.clone(),
        )
        .expect("accounting embedder");
        let inputs = vec!["first fact".to_string(), "second fact".to_string()];
        embedder.embed(&inputs).await.expect("embed batch");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let records = budget.records().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stage, StageName::Embedding);
        assert_eq!(records[0].pricing.model, "gemini:gemini-embedding-2");
        assert_eq!(
            records[0]
                .actual_usage
                .as_ref()
                .expect("completed batch usage")
                .input_tokens_uncached,
            inputs
                .iter()
                .map(|input| estimate_text_tokens(input))
                .sum::<usize>()
        );
    }

    #[tokio::test]
    async fn run_external_memory_eval_embedding_forecast_blocks_provider_dispatch() {
        // Pins: exhausted cumulative budget prevents the embedding provider from being called.
        let calls = Arc::new(AtomicUsize::new(0));
        let budget = LiveRunBudget::new(0.000_000_01).expect("budget");
        let embedder = AccountingEmbeddingProvider::new(
            Arc::new(FakeEmbeddingProvider {
                calls: calls.clone(),
            }),
            "gemini:gemini-embedding-2",
            budget,
        )
        .expect("accounting embedder");
        let error = embedder
            .embed(&["this input cannot fit in the budget".to_string()])
            .await
            .expect_err("forecast must reject provider call");

        assert!(error.to_string().contains("forecast"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn run_external_memory_eval_longmemeval_requires_explicit_different_provider_families() {
        // Pins: LongMemEval resolves explicit known reader/judge families before any provider is
        // constructed, and the judge cannot share the reader's provider family.
        let mut args = parse_args(base("heuristic").into_iter()).expect("parse base");
        let missing = validate_longmemeval_model_policy(&args)
            .expect_err("LongMemEval requires an explicit judge model");
        assert!(missing.to_string().contains("--judge-model"));

        args.judge_model = Some("openai:gpt-5.4".to_string());
        let same_family = validate_longmemeval_model_policy(&args)
            .expect_err("reader and judge must use different families");
        assert!(
            same_family
                .to_string()
                .contains("different provider families")
        );

        args.judge_model = Some("unknown:judge-v1".to_string());
        let unknown = validate_longmemeval_model_policy(&args)
            .expect_err("unknown provider families must fail before construction");
        assert!(unknown.to_string().contains("unknown provider family"));

        args.judge_model = Some("anthropic:claude-sonnet-4-6".to_string());
        let resolved = validate_longmemeval_model_policy(&args)
            .expect("different recognized provider families should validate");
        assert_eq!(resolved.reader_family.as_str(), "openai");
        assert_eq!(resolved.judge_family.as_str(), "anthropic");
        assert_eq!(resolved.reader_model.as_str(), "gpt-5.4-mini");
        assert_eq!(resolved.judge_model.as_str(), "claude-sonnet-4-6");
    }

    #[test]
    fn run_external_memory_eval_longmemeval_requests_ranked_depth_fifty() {
        // Pins: official turn metrics at 50 receive the requested production rank prefix rather
        // than the ordinary stage-7 default of four.
        assert_eq!(ranked_occurrence_depth(LONGMEMEVAL_DATASET), 50);
        assert_eq!(ranked_occurrence_depth(PERSONAMEM_DATASET), 4);
    }

    struct FixedCompletionProvider {
        calls: Arc<AtomicUsize>,
        text: String,
        usage: TokenUsage,
    }

    #[async_trait]
    impl LLMProvider for FixedCompletionProvider {
        fn name(&self) -> &str {
            "fixed-completion"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new("claude-sonnet-4-6"),
                context_window: 32_000,
                max_output: 256,
                supports_tools: false,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::OpenAiCompatible,
                pricing: TokenPricing {
                    input_per_mtok: 3.0,
                    output_per_mtok: 15.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionStream::from_response(CompletionResponse {
                text: self.text.clone(),
                content: Vec::new(),
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("claude-sonnet-4-6"),
                usage: self.usage,
                duration_ms: 9,
                thought_signature: None,
            }))
        }
    }

    #[tokio::test]
    async fn run_external_memory_eval_longmemeval_paid_helper_accounts_before_strict_parse() {
        // Pins: malformed judge output remains a parse failure only after its actual provider
        // usage has replaced the forecast in the one cumulative ledger.
        let calls = Arc::new(AtomicUsize::new(0));
        let actual = TokenUsage {
            input_tokens_uncached: 31,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 4,
        };
        let provider = FixedCompletionProvider {
            calls: calls.clone(),
            text: "yes, because".to_string(),
            usage: actual,
        };
        let budget = LiveRunBudget::new(1.0).expect("budget");
        let request = completion_request(16);
        let reader_paid = execute_paid_completion(
            &provider,
            request.clone(),
            StageName::Reader,
            ExternalMemoryMode::Primary,
            chat_pricing("anthropic:claude-sonnet-4-6").expect("reader pricing"),
            &budget,
            Duration::from_secs(1),
        )
        .await
        .expect("reader response");
        assert_eq!(reader_paid.post_response_budget, Ok(()));
        let paid = execute_paid_completion(
            &provider,
            request,
            StageName::Judge,
            ExternalMemoryMode::Primary,
            chat_pricing("anthropic:claude-sonnet-4-6").expect("judge pricing"),
            &budget,
            Duration::from_secs(1),
        )
        .await
        .expect("provider response");

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(parse_absolute_judge_label(&paid.response.text), None);
        assert_eq!(paid.actual_usage, normalized_response_usage(&paid.response));
        assert_eq!(paid.post_response_budget, Ok(()));
        let records = budget.records().await;
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(|record| record.stage)
                .collect::<Vec<_>>(),
            vec![StageName::Reader, StageName::Judge]
        );
        assert_eq!(records[1].actual_usage, Some(paid.actual_usage));
    }

    #[test]
    fn run_external_memory_eval_longmemeval_renders_category_rubric_for_judge() {
        // Pins: the live runner sends the exact dataset-owned category rubric and never a generic
        // scorer prompt for LongMemEval.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../moa-eval/tests/fixtures/external_memory/longmemeval/longmemeval_s_cleaned_tiny.json",
        );
        let dataset = moa_eval::external_memory::longmemeval::load_longmemeval_file(&fixture)
            .expect("load tiny LongMemEval fixture");
        for (question_id, expected_kind) in [
            ("q-knowledge", LongMemEvalRubricKind::KnowledgeUpdate),
            ("q-multi", LongMemEvalRubricKind::General),
            (
                "q-preference",
                LongMemEvalRubricKind::SingleSessionPreference,
            ),
            ("q-temporal", LongMemEvalRubricKind::TemporalReasoning),
            ("q-user_abs", LongMemEvalRubricKind::Abstention),
        ] {
            let case = dataset.case(question_id).expect("fixture case");
            let reader = ReaderResponse {
                answer: "candidate".to_string(),
                model: "gpt-5.4-mini".to_string(),
                prompt_version: LONGMEMEVAL_READER_PROMPT_VERSION.to_string(),
                usage: NormalizedUsage {
                    input_tokens_uncached: 1,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 1,
                    provenance: UsageProvenance::Actual,
                },
                latency_ms: 1,
            };
            let request = longmemeval_judge_request(
                case,
                &reader,
                expected_kind,
                "anthropic:claude-sonnet-4-6",
            )
            .expect("render judge request");
            assert_eq!(request.messages.len(), 1);
            assert_eq!(
                request.messages[0].content,
                expected_kind
                    .render(
                        &case.prepared.case.question,
                        &case.prepared.case.answer,
                        &reader.answer,
                    )
                    .expect("render exact rubric")
            );
            assert_eq!(
                LongMemEvalRubricKind::for_question(
                    case.metadata.question_type,
                    case.is_abstention,
                ),
                expected_kind
            );
        }
    }

    #[test]
    fn run_external_memory_eval_longmemeval_budget_tail_preserves_every_denominator() {
        // Pins: terminal exhaustion prevents later work while still emitting one explicit failed
        // artifact for every unvisited case.
        let cases = vec![
            prepared_case("q-1", "knowledge-update"),
            prepared_case("q-2", "temporal-reasoning"),
            prepared_case("q-3_abs", "single-session-user"),
        ];
        let tail = budget_tail_reports(&cases, "reader forecast exceeded budget");

        assert_eq!(tail.len(), 3);
        assert_eq!(
            tail.iter()
                .map(|case| case.isolation_key.as_str())
                .collect::<Vec<_>>(),
            vec!["q-1", "q-2", "q-3_abs"]
        );
        assert!(tail.iter().all(|case| {
            case.rendered_evidence.is_empty()
                && case.reader.is_none()
                && case.absolute_judge.is_none()
                && matches!(
                    case.failure.as_ref(),
                    Some(failure) if failure.kind == FailureKind::Budget
                )
        }));
    }

    #[test]
    fn run_external_memory_eval_longmemeval_aggregate_retains_all_tiny_denominators() {
        // Pins: answer, abstention, six type, and retrieval aggregates are
        // complete even when no retrieval ranking is available.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../moa-eval/tests/fixtures/external_memory/longmemeval/longmemeval_s_cleaned_tiny.json",
        );
        let dataset = moa_eval::external_memory::longmemeval::load_longmemeval_file(&fixture)
            .expect("load tiny LongMemEval fixture");
        let reports = dataset
            .cases
            .iter()
            .map(|case| {
                CaseReport::completed(
                    &case.prepared.case.isolation_key,
                    &case.prepared.case.category,
                    "fixture evidence",
                    SupportStatus::Unsupported {
                        reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
                    },
                )
                .with_generated_answer_outcome(
                    ReaderResponse {
                        answer: "fixture answer".to_string(),
                        model: "gpt-5.4-mini".to_string(),
                        prompt_version: LONGMEMEVAL_READER_PROMPT_VERSION.to_string(),
                        usage: NormalizedUsage {
                            input_tokens_uncached: 1,
                            input_tokens_cache_write: 0,
                            input_tokens_cache_read: 0,
                            output_tokens: 1,
                            provenance: UsageProvenance::Actual,
                        },
                        latency_ms: 1,
                    },
                    SupportStatus::Unsupported {
                        reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
                    },
                    None,
                    Some(AbsoluteJudgeResponse {
                        supported: true,
                        rationale: "yes".to_string(),
                        model: "claude-sonnet-4-6".to_string(),
                        prompt_version: LONGMEMEVAL_JUDGE_PROMPT_VERSION.to_string(),
                        usage: NormalizedUsage {
                            input_tokens_uncached: 1,
                            input_tokens_cache_write: 0,
                            input_tokens_cache_read: 0,
                            output_tokens: 1,
                            provenance: UsageProvenance::Actual,
                        },
                        latency_ms: 1,
                    }),
                )
            })
            .collect::<Vec<_>>();

        let aggregate = build_longmemeval_report(
            &dataset.cases,
            &BTreeMap::new(),
            &reports,
            "anthropic:claude-sonnet-4-6",
        )
        .expect("build typed aggregate");

        assert_eq!(aggregate.answers.denominator, 7);
        assert_eq!(aggregate.answers.numerator, 7);
        assert_eq!(aggregate.abstentions.denominator, 1);
        assert_eq!(aggregate.question_type_slices.len(), 6);
        let RetrievalMetricsV2::Supported { metrics } = &aggregate.retrieval else {
            panic!("primary retrieval metrics should be supported");
        };
        assert_eq!(metrics.denominator, 6);
        assert!(aggregate.failure_counts.is_empty());
    }

    #[test]
    fn run_external_memory_eval_longmemeval_parse_failure_retains_reader_and_judge_usage() {
        // Pins: malformed judge output is incorrect and failed, while preserving the evidence,
        // reader response, raw judge text, and both normalized usage artifacts.
        let case = prepared_case("q-parse", "temporal-reasoning");
        let reader = ReaderResponse {
            answer: "42 days".to_string(),
            model: "gpt-5.4-mini".to_string(),
            prompt_version: LONGMEMEVAL_READER_PROMPT_VERSION.to_string(),
            usage: NormalizedUsage {
                input_tokens_uncached: 11,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 2,
                provenance: UsageProvenance::Actual,
            },
            latency_ms: 3,
        };
        let judge = AbsoluteJudgeResponse {
            supported: false,
            rationale: "yes, because".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            prompt_version: LONGMEMEVAL_JUDGE_PROMPT_VERSION.to_string(),
            usage: NormalizedUsage {
                input_tokens_uncached: 17,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 3,
                provenance: UsageProvenance::Actual,
            },
            latency_ms: 5,
        };
        let failed = failed_after_reader(
            &case,
            "rendered evidence".to_string(),
            4,
            reader.clone(),
            SupportStatus::Unsupported {
                reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
            },
            None,
            Some(judge.clone()),
            FailureKind::Parse,
            "invalid judge label",
        );

        assert_eq!(failed.rendered_evidence, "rendered evidence");
        assert_eq!(failed.rendered_evidence_tokens, 4);
        assert_eq!(failed.reader, Some(reader));
        assert_eq!(failed.absolute_judge, Some(judge));
        assert_eq!(failed.answer_score, None);
        assert!(matches!(
            failed.failure,
            Some(failure) if failure.kind == FailureKind::Parse
        ));
    }

    fn prepared_case(isolation_key: &str, category: &str) -> PreparedExternalMemoryCase {
        PreparedExternalMemoryCase {
            case: moa_eval::external_memory::dataset::ExternalMemoryCaseV1 {
                schema_version: 1,
                isolation_key: isolation_key.to_string(),
                category: category.to_string(),
                question: "question".to_string(),
                answer: "answer".to_string(),
                options: Vec::new(),
                sessions: Vec::new(),
                evidence_labels: Default::default(),
            },
            chronological_turns: Vec::new(),
        }
    }
}
