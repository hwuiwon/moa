//! Eval execution engine for running suites against isolated agent configurations.
//!
//! The engine is an admission-controlled scheduler, not a post-hoc reporter. A
//! run passes through three gates before any paid work happens:
//!
//! 1. [`EvalAdmissionPolicy::admit`] hard-rejects an oversized or invalid
//!    `(suite, configs, parallelism)` matrix and returns the versioned
//!    [`ResourceEnvelope`] the run must fit inside;
//! 2. every case reserves its worst case from the shared
//!    [`SharedResourceLedger`] *before* an environment, provider, or tool is
//!    touched, and scheduling stops for the rest of the run the moment a
//!    reservation is refused;
//! 3. dispatch runs under a [`DeadlineGuard`], so an expired per-case or
//!    whole-run deadline cancels the token the brain turn, provider call, and
//!    tool router are all holding instead of merely dropping the outer future.
//!
//! Actual usage is reconciled back into the ledger after each case, which frees
//! the unused part of the reservation for the cases still queued.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use futures_util::{StreamExt, stream};
use moa_brain::runtime_events::RuntimeEvent;
use moa_brain::{BrainTurnRequest, StreamedTurnRequest, StreamedTurnResult, run_streamed_turn};
use moa_config::MoaConfig;
use moa_core::types::resource::{
    DeadlineGuard, ResourceBudget, ResourceError, ResourceReservation, SharedResourceLedger,
};
use moa_core::{events::Event, traits::LLMProvider, types::events_stream::EventRange};
use moa_eval_core::admission::{AdmittedRun, EvalAdmissionPolicy};
use moa_eval_core::engine::{EngineOptions, EvalRun, RunSummary};
use moa_eval_core::evidence::EvidenceSubject;
use moa_eval_core::plan::EvalPlan;
use moa_eval_core::resource_report::{RunResourceReport, usage_from_metrics};
use moa_eval_core::{
    AgentConfig, Error, EvalMetrics, EvalResult, EvalStatus, Result, TestCase, TestCaseKind,
    TestSuite,
};
use moa_providers::CancellableLLMProvider;
use opentelemetry::trace::TraceContextExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::collector::{CollectedExecution, TrajectoryCollector};
use crate::long_conversation::transcript_runner::run_scenario_in_environment;
use crate::plan::build_eval_plan;
use crate::setup::{
    AgentEnvironment, build_agent_environment_with_provider, resolve_agent_llm_provider,
};

const DEFAULT_SINGLE_TIMEOUT_SECONDS: u64 = 300;

/// Executes eval suites against one or more agent configurations.
#[derive(Debug, Clone)]
pub struct EvalEngine {
    base_config: MoaConfig,
    options: EngineOptions,
    admission: EvalAdmissionPolicy,
}

impl EvalEngine {
    /// Creates a new eval engine from a base MOA config and execution options.
    ///
    /// Options outside the admission bounds are rejected here rather than
    /// reduced, so a run never executes at a concurrency the caller did not ask
    /// for.
    pub fn new(base_config: MoaConfig, options: EngineOptions) -> Result<Self> {
        if options.content_max_bytes == 0 {
            return Err(Error::InvalidConfig(
                "content_max_bytes must be at least 1".to_string(),
            ));
        }
        let admission = EvalAdmissionPolicy::new(options.admission.clone());
        admission.check_parallelism(options.parallel)?;

        Ok(Self {
            base_config,
            options,
            admission,
        })
    }

    /// Returns the dry-run plan for one suite/config matrix.
    pub fn plan(&self, suite: &TestSuite, configs: &[AgentConfig]) -> EvalPlan {
        build_eval_plan(&self.base_config, suite, configs)
    }

    /// Returns the admission policy this engine enforces.
    #[must_use]
    pub const fn admission(&self) -> &EvalAdmissionPolicy {
        &self.admission
    }

    /// Runs all test cases in a suite against all provided configs.
    pub async fn run_suite(&self, suite: &TestSuite, configs: &[AgentConfig]) -> Result<EvalRun> {
        self.run_suite_inner(suite, configs, None).await
    }

    /// Runs all test cases in a suite against all provided configs using one provider instance.
    pub async fn run_suite_with_provider(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        llm_provider: Arc<dyn LLMProvider>,
    ) -> Result<EvalRun> {
        self.run_suite_inner(suite, configs, Some(llm_provider))
            .await
    }

    /// Runs one test case against one agent config under the same admission and
    /// reservation path as a full suite.
    pub async fn run_single(&self, case: &TestCase, config: &AgentConfig) -> Result<EvalResult> {
        let suite = TestSuite {
            name: format!("single:{}", case.name),
            cases: vec![case.clone()],
            default_timeout_seconds: DEFAULT_SINGLE_TIMEOUT_SECONDS,
            ..TestSuite::default()
        };
        let run = self
            .run_suite_inner(&suite, std::slice::from_ref(config), None)
            .await?;
        run.results.into_iter().next().ok_or_else(|| {
            Error::InvalidConfig(format!("single run produced no result for '{}'", case.name))
        })
    }

    async fn run_suite_inner(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> Result<EvalRun> {
        let started_at = Utc::now();
        let admitted = self
            .admission
            .admit(suite, configs, self.options.parallel, started_at)?;
        let ledger = SharedResourceLedger::from_envelope(admitted.envelope.clone())?;
        let mut report = RunResourceReport::new(
            admitted.limits_version,
            &ledger,
            admitted.per_case,
            admitted.worst_case_projection,
            admitted.parallel,
            admitted.total_runs,
        );

        if self.options.dry_run {
            let results = configs
                .iter()
                .flat_map(|config| {
                    suite.cases.iter().map(move |case| EvalResult {
                        test_case: case.name.clone(),
                        agent_config: config.name.clone(),
                        status: EvalStatus::Skipped,
                        started_at,
                        completed_at: started_at,
                        ..EvalResult::default()
                    })
                })
                .collect::<Vec<_>>();
            let summary = RunSummary::from_results(&results);
            return Ok(EvalRun {
                suite_name: suite.name.clone(),
                started_at,
                completed_at: Utc::now(),
                results,
                summary,
                resources: Some(report),
            });
        }

        let prepared_cases = suite
            .cases
            .iter()
            .map(|case| {
                Ok((
                    Arc::new(case.clone()),
                    self.admission.effective_case_seconds(case, suite)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut indexed_pairs: Vec<ScheduledCase> = Vec::with_capacity(admitted.total_runs);
        for (config_index, config) in configs.iter().enumerate() {
            let arc_config = Arc::new(config.clone());
            for (case_index, (case, timeout_seconds)) in prepared_cases.iter().enumerate() {
                indexed_pairs.push(ScheduledCase {
                    config_index,
                    case_index,
                    config: Arc::clone(&arc_config),
                    case: Arc::clone(case),
                    timeout_seconds: *timeout_seconds,
                });
            }
        }

        // Cancelling this token stops the scheduler: cases that have not yet
        // reserved are never dispatched.
        let stop = CancellationToken::new();
        let run_guard = DeadlineGuard::new(CancellationToken::new(), admitted.envelope.deadline);

        let mut outcomes: Vec<(usize, usize, CaseOutcome)> = if admitted.parallel <= 1 {
            let mut outcomes = Vec::with_capacity(indexed_pairs.len());
            for scheduled in indexed_pairs {
                let outcome = self
                    .execute_case(
                        &scheduled,
                        &admitted,
                        &ledger,
                        &run_guard,
                        &stop,
                        llm_provider.clone(),
                    )
                    .await;
                outcomes.push((scheduled.config_index, scheduled.case_index, outcome));
            }
            outcomes
        } else {
            stream::iter(indexed_pairs)
                .map(|scheduled| {
                    let engine = self.clone();
                    let admitted = admitted.clone();
                    let ledger = ledger.clone();
                    let run_guard = run_guard.clone();
                    let stop = stop.clone();
                    let llm_provider = llm_provider.clone();
                    async move {
                        let outcome = engine
                            .execute_case(
                                &scheduled,
                                &admitted,
                                &ledger,
                                &run_guard,
                                &stop,
                                llm_provider,
                            )
                            .await;
                        (scheduled.config_index, scheduled.case_index, outcome)
                    }
                })
                .buffer_unordered(admitted.parallel)
                .collect::<Vec<_>>()
                .await
        };
        outcomes.sort_by_key(|(config_index, case_index, _)| (*config_index, *case_index));

        let mut results = Vec::with_capacity(outcomes.len());
        for (_, _, outcome) in outcomes {
            match outcome {
                CaseOutcome::Dispatched(result) => {
                    report.record_dispatched();
                    results.push(result);
                }
                CaseOutcome::Unreserved { result, reason } => {
                    report.record_unreserved(&reason);
                    results.push(result);
                }
            }
        }
        report.refresh(&ledger);

        let summary = RunSummary::from_results(&results);
        Ok(EvalRun {
            suite_name: suite.name.clone(),
            started_at,
            completed_at: Utc::now(),
            results,
            summary,
            resources: Some(report),
        })
    }

    /// Reserves capacity for one case and dispatches it only if the reservation
    /// is granted.
    async fn execute_case(
        &self,
        scheduled: &ScheduledCase,
        admitted: &AdmittedRun,
        ledger: &SharedResourceLedger,
        run_guard: &DeadlineGuard,
        stop: &CancellationToken,
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> CaseOutcome {
        let case = scheduled.case.as_ref();
        let config = scheduled.config.as_ref();
        let started_at = Utc::now();

        if stop.is_cancelled() {
            return CaseOutcome::unreserved(
                case,
                config,
                started_at,
                "scheduling stopped after the resource envelope was exhausted".to_string(),
            );
        }

        let reservation = match ledger.try_reserve(admitted.per_case, started_at) {
            Ok(reservation) => reservation,
            Err(error) => {
                // Stop the whole run: the envelope cannot cover another case, so
                // no further case may dispatch.
                stop.cancel();
                tracing::warn!(
                    case = %case.name,
                    config = %config.name,
                    error = %error,
                    "eval case refused a resource reservation; scheduling stopped"
                );
                return CaseOutcome::unreserved(case, config, started_at, error.to_string());
            }
        };

        let case_deadline = started_at
            .checked_add_signed(Duration::seconds(
                i64::try_from(scheduled.timeout_seconds).unwrap_or(i64::MAX),
            ))
            .unwrap_or(started_at);
        let guard = run_guard.child(Some(case_deadline));

        let dispatch = self
            .dispatch_case(case, config, &guard, admitted, llm_provider, started_at)
            .await;

        settle(ledger, reservation, &dispatch, case);
        CaseOutcome::Dispatched(dispatch.into_result())
    }

    async fn dispatch_case(
        &self,
        case: &TestCase,
        config: &AgentConfig,
        guard: &DeadlineGuard,
        admitted: &AdmittedRun,
        llm_provider: Option<Arc<dyn LLMProvider>>,
        started_at: DateTime<Utc>,
    ) -> Dispatch {
        if case.kind == TestCaseKind::Long {
            return self
                .dispatch_long_case(case, config, guard, admitted, llm_provider, started_at)
                .await;
        }

        let environment = match self.build_environment(config, guard, llm_provider).await {
            Ok(environment) => environment,
            Err(error) => {
                return Dispatch::NotStarted(build_error_result(
                    case,
                    config,
                    started_at,
                    error,
                    EvalStatus::Error,
                ));
            }
        };
        let run_root = run_root_of(&environment);
        let span = tracing::info_span!(
            "eval_run",
            moa.eval.case = %case.name,
            moa.eval.config = %config.name,
            moa.session.id = %environment.session_id,
        );
        let trace_id = extract_trace_id(&span);
        let engine_options = self.options.clone();
        let case_input = case.input.clone();
        let execution = run_environment(
            case_input,
            &environment,
            &engine_options,
            guard.token(),
            ResourceBudget::new(guard.deadline(), Some(admitted.per_case)),
        )
        .instrument(span);

        let mut dispatch = match guard.run(execution).await {
            // Evidence is built here, before `cleanup_run_resources` below tears
            // the workspace and database down. Nothing after this point can
            // re-derive what the run did.
            Ok(Ok(execution)) => Dispatch::Completed(EvalResult {
                test_case: case.name.clone(),
                agent_config: config.name.clone(),
                status: EvalStatus::Passed,
                evidence: Some(execution.to_evidence(EvidenceSubject {
                    case: case.name.clone(),
                    case_schema_version: case.schema_version,
                    agent_config: config.name.clone(),
                    run_label: environment.session_id.to_string(),
                })),
                response: execution.response,
                trajectory: execution.trajectory,
                metrics: execution.metrics,
                trace_id: None,
                error: None,
                started_at,
                completed_at: Utc::now(),
                ..EvalResult::default()
            }),
            Ok(Err(error)) => Dispatch::Indeterminate(build_error_result(
                case,
                config,
                started_at,
                error.to_string(),
                EvalStatus::Error,
            )),
            Err(error) => Dispatch::Indeterminate(build_error_result(
                case,
                config,
                started_at,
                error.to_string(),
                deadline_status(&error),
            )),
        };
        dispatch.set_trace_id(trace_id);

        let cleanup_errors = cleanup_run_resources(&run_root, environment).await;
        dispatch.apply_cleanup_errors(cleanup_errors);
        dispatch
    }

    async fn dispatch_long_case(
        &self,
        case: &TestCase,
        config: &AgentConfig,
        guard: &DeadlineGuard,
        admitted: &AdmittedRun,
        llm_provider: Option<Arc<dyn LLMProvider>>,
        started_at: DateTime<Utc>,
    ) -> Dispatch {
        let Some(llm_provider) = llm_provider else {
            return Dispatch::NotStarted(build_error_result(
                case,
                config,
                started_at,
                "long conversation cases require an explicit provider".to_string(),
                EvalStatus::Error,
            ));
        };
        let llm_provider = case_provider(llm_provider, guard);
        let environment = match build_agent_environment_with_provider(
            &self.base_config,
            config,
            &self.options.temp_dir,
            llm_provider,
        )
        .await
        {
            Ok(environment) => environment,
            Err(error) => {
                return Dispatch::NotStarted(build_error_result(
                    case,
                    config,
                    started_at,
                    error.to_string(),
                    EvalStatus::Error,
                ));
            }
        };
        let run_root = run_root_of(&environment);
        let scenario = run_scenario_in_environment(
            config,
            &self.options,
            case,
            &environment,
            ResourceBudget::new(guard.deadline(), Some(admitted.per_case)),
            guard.token(),
        );

        let mut dispatch = match guard.run(scenario).await {
            Ok(Ok(report)) => Dispatch::Completed(report.result),
            Ok(Err(error)) => Dispatch::Indeterminate(build_error_result(
                case,
                config,
                started_at,
                error.to_string(),
                EvalStatus::Error,
            )),
            Err(error) => Dispatch::Indeterminate(build_error_result(
                case,
                config,
                started_at,
                error.to_string(),
                deadline_status(&error),
            )),
        };

        let cleanup_errors = cleanup_run_resources(&run_root, environment).await;
        dispatch.apply_cleanup_errors(cleanup_errors);
        dispatch
    }

    async fn build_environment(
        &self,
        config: &AgentConfig,
        guard: &DeadlineGuard,
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> std::result::Result<AgentEnvironment, String> {
        let llm_provider = match llm_provider {
            Some(llm_provider) => llm_provider,
            None => resolve_agent_llm_provider(&self.base_config, config)
                .map_err(|error| error.to_string())?,
        };
        let built = build_agent_environment_with_provider(
            &self.base_config,
            config,
            &self.options.temp_dir,
            case_provider(llm_provider, guard),
        )
        .await;
        built.map_err(|error| error.to_string())
    }
}

fn case_provider(
    llm_provider: Arc<dyn LLMProvider>,
    guard: &DeadlineGuard,
) -> Arc<dyn LLMProvider> {
    Arc::new(CancellableLLMProvider::new(llm_provider, guard.clone()))
}

/// One `(config, case)` pair with its already-validated wall-clock budget.
#[derive(Debug, Clone)]
struct ScheduledCase {
    config_index: usize,
    case_index: usize,
    config: Arc<AgentConfig>,
    case: Arc<TestCase>,
    timeout_seconds: u64,
}

/// Whether a case reached dispatch at all.
#[derive(Debug)]
enum CaseOutcome {
    /// The case reserved capacity and ran.
    Dispatched(EvalResult),
    /// The case never dispatched because the ledger refused a reservation.
    Unreserved {
        /// Skipped placeholder result recorded for reporting.
        result: EvalResult,
        /// Why the reservation was refused.
        reason: String,
    },
}

impl CaseOutcome {
    fn unreserved(
        case: &TestCase,
        config: &AgentConfig,
        started_at: DateTime<Utc>,
        reason: String,
    ) -> Self {
        let mut result = build_error_result(
            case,
            config,
            started_at,
            reason.clone(),
            EvalStatus::Skipped,
        );
        result.completed_at = started_at;
        Self::Unreserved { result, reason }
    }
}

/// What a dispatch attempt proved about actual spend.
#[derive(Debug)]
enum Dispatch {
    /// Setup failed before any provider, tool, or sandbox was touched.
    NotStarted(EvalResult),
    /// The case ran to completion and its metrics are authoritative.
    Completed(EvalResult),
    /// The case started but its final spend is unknown (timeout or cancellation).
    Indeterminate(EvalResult),
}

impl Dispatch {
    fn result_mut(&mut self) -> &mut EvalResult {
        match self {
            Self::NotStarted(result) | Self::Completed(result) | Self::Indeterminate(result) => {
                result
            }
        }
    }

    fn into_result(self) -> EvalResult {
        match self {
            Self::NotStarted(result) | Self::Completed(result) | Self::Indeterminate(result) => {
                result
            }
        }
    }

    fn set_trace_id(&mut self, trace_id: Option<String>) {
        if trace_id.is_some() {
            self.result_mut().trace_id = trace_id;
        }
    }

    fn apply_cleanup_errors(&mut self, cleanup_errors: Vec<String>) {
        if cleanup_errors.is_empty() {
            return;
        }
        let cleanup_error = cleanup_errors.join("; ");
        let result = self.result_mut();
        if result.status == EvalStatus::Passed {
            result.status = EvalStatus::Error;
            result.error = Some(cleanup_error);
        } else {
            tracing::warn!(
                error = %cleanup_error,
                status = ?result.status,
                "eval run cleanup failed after a non-passing outcome"
            );
        }
    }
}

/// Returns or commits a reservation according to what the dispatch proved about
/// actual spend.
///
/// Setup failures release the whole reservation, completed cases reconcile their
/// measured usage, and an indeterminate case (timeout or cancellation) keeps the
/// worst case committed because its real spend is unknowable.
fn settle(
    ledger: &SharedResourceLedger,
    reservation: ResourceReservation,
    dispatch: &Dispatch,
    case: &TestCase,
) {
    let reserved = reservation.reserved();
    let settlement = match dispatch {
        Dispatch::NotStarted(_) => {
            if let Err(error) = ledger.release(reservation) {
                tracing::warn!(case = %case.name, error = %error, "failed to release an eval reservation");
            }
            return;
        }
        Dispatch::Completed(result) => usage_from_metrics(&result.metrics),
        Dispatch::Indeterminate(_) => Ok(reserved),
    };

    let actual = match settlement {
        Ok(actual) => actual,
        Err(error) => {
            tracing::warn!(
                case = %case.name,
                error = %error,
                "eval metrics could not be converted to resource usage; committing the reservation"
            );
            reserved
        }
    };

    match ledger.reconcile(reservation, actual) {
        Ok(outcome) => {
            if let Some(overrun) = outcome.overrun() {
                tracing::warn!(
                    case = %case.name,
                    overrun_cost_micro_usd = overrun.cost_micro_usd,
                    overrun_tokens = overrun.tokens,
                    overrun_tool_calls = overrun.tool_calls,
                    "eval case exceeded its resource reservation"
                );
            }
        }
        Err(error) => {
            tracing::warn!(case = %case.name, error = %error, "failed to reconcile an eval reservation");
        }
    }
}

/// Maps a deadline or cancellation failure onto an eval status.
fn deadline_status(error: &ResourceError) -> EvalStatus {
    match error {
        ResourceError::DeadlineExceeded { .. } => EvalStatus::Timeout,
        _ => EvalStatus::Error,
    }
}

fn run_root_of(environment: &AgentEnvironment) -> PathBuf {
    environment
        .workspace_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| environment.workspace_dir.clone())
}

/// Drives one agent session to completion under a caller-owned cancellation
/// token and reserved resource budget.
async fn run_environment(
    input: String,
    environment: &crate::AgentEnvironment,
    options: &EngineOptions,
    cancel_token: &CancellationToken,
    mut resource_budget: ResourceBudget,
) -> Result<CollectedExecution> {
    environment
        .session_store
        .emit_event(
            environment.session_id,
            Event::UserMessage {
                text: input,
                attachments: Vec::new(),
            },
        )
        .await?;

    let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);
    let max_turns = resource_budget
        .remaining
        .map_or(0, |remaining| remaining.turns);

    for turn_index in 0..max_turns {
        if cancel_token.is_cancelled() {
            return Err(Error::Moa(moa_core::error::MoaError::Cancelled));
        }

        let outcome = run_streamed_turn(StreamedTurnRequest {
            turn: BrainTurnRequest {
                identity: environment.identity.clone(),
                session_id: environment.session_id,
                session_store: environment.session_store.clone(),
                llm_provider: environment.llm_provider.clone(),
                pipeline: &environment.pipeline,
                tool_router: Some(environment.tool_router.clone()),
            },
            runtime_tx: &runtime_tx,
            event_tx: None,
            cancel_token: Some(cancel_token.clone()),
            // The case deadline cancels this token. Reusing it as the hard
            // token makes in-sandbox work observe the same terminal stop.
            hard_cancel_token: Some(cancel_token.clone()),
            resource_budget: &mut resource_budget,
            signal_state: None,
            lineage: environment.lineage.clone(),
        })
        .await?;

        match outcome {
            StreamedTurnResult::Complete => break,
            StreamedTurnResult::Continue => {
                if turn_index + 1 == max_turns {
                    return Err(Error::InvalidConfig(format!(
                        "agent exceeded its reserved budget of {max_turns} turns"
                    )));
                }
                continue;
            }
            StreamedTurnResult::Cancelled => {
                return Err(Error::Moa(moa_core::error::MoaError::Cancelled));
            }
        }
    }

    let events = environment
        .session_store
        .get_events(environment.session_id, EventRange::all())
        .await?;
    let mut collector = TrajectoryCollector::new(
        Some(environment.llm_provider.capabilities().pricing.clone()),
        options.capture_content,
        options.content_max_bytes,
    );
    collector.process_events(&events);
    Ok(collector.finish())
}

async fn cleanup_run_resources(run_root: &Path, environment: AgentEnvironment) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = cleanup_workspace(run_root).await {
        errors.push(format!("workspace cleanup failed: {error}"));
    }
    if let Err(error) = environment.cleanup().await {
        errors.push(format!("database cleanup failed: {error}"));
    }
    errors
}

async fn cleanup_workspace(path: &Path) -> Result<()> {
    if fs_try_exists(path).await? {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|source| Error::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

async fn fs_try_exists(path: &Path) -> Result<bool> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn extract_trace_id(span: &tracing::Span) -> Option<String> {
    let trace_id = span.context().span().span_context().trace_id();
    let value = trace_id.to_string();
    if value.chars().all(|character| character == '0') {
        None
    } else {
        Some(value)
    }
}

fn build_error_result(
    case: &TestCase,
    config: &AgentConfig,
    started_at: DateTime<Utc>,
    error: String,
    status: EvalStatus,
) -> EvalResult {
    EvalResult {
        // An error or timeout captured no evidence and evaluated no assertion, so
        // both are stated as absent rather than defaulted into a passing shape.
        evidence: None,
        assertions: Vec::new(),
        test_case: case.name.clone(),
        agent_config: config.name.clone(),
        status,
        response: None,
        trajectory: Vec::new(),
        scores: Vec::new(),
        metrics: EvalMetrics::default(),
        trace_id: None,
        error: Some(error),
        started_at,
        completed_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use chrono::{Duration, Utc};
    use moa_config::MoaConfig;
    use moa_core::{
        error::MoaError, traits::LLMProvider, types::completion::CompletionRequest,
        types::completion::CompletionResponse, types::completion::CompletionStream,
        types::completion::StopReason, types::completion::TokenUsage,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
        types::resource::DeadlineGuard, types::resource::ResourceAmounts,
        types::resource::ResourceBudget,
    };
    use tempfile::tempdir;

    use super::{case_provider, run_environment};
    use crate::{EvalEngine, setup::build_agent_environment_with_provider};
    use moa_eval_core::admission::{AdmissionError, EvalAdmissionLimits};
    use moa_eval_core::{AgentConfig, EngineOptions, Error, EvalStatus, TestCase, TestSuite};
    use tokio_util::sync::CancellationToken;

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

    fn test_moa_config() -> MoaConfig {
        let mut config = MoaConfig::default();
        config.database.url = moa_session::testing::test_database_url();
        config
    }

    fn test_resource_budget(turns: u64) -> ResourceBudget {
        ResourceBudget::new(
            None,
            Some(ResourceAmounts {
                cost_micro_usd: 1_000_000,
                tokens: 1_000_000,
                turns,
                model_calls: turns,
                tool_calls: 100,
            }),
        )
    }

    #[derive(Clone, Default)]
    struct MockProvider {
        calls: Arc<AtomicUsize>,
    }

    impl MockProvider {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: moa_core::types::identifiers::ModelId::new("mock-model"),
                context_window: 32_000,
                max_output: 1_024,
                supports_tools: true,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::Anthropic,
                pricing: TokenPricing {
                    input_per_mtok: 1.0,
                    output_per_mtok: 2.0,
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
                text: "hello from eval".to_string(),
                content: vec![moa_core::types::completion::CompletionContent::Text(
                    "hello from eval".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::types::identifiers::ModelId::new("mock-model"),
                usage: token_usage(42, 7),
                duration_ms: 3,
                thought_signature: None,
            }))
        }
    }

    struct PendingProvider {
        stopped: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for PendingProvider {
        fn name(&self) -> &str {
            "pending"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
            let _sentinel = CompletionDropSentinel(Arc::clone(&self.stopped));
            std::future::pending::<()>().await;
            unreachable!("the case deadline must drop the pending provider future")
        }
    }

    struct CompletionDropSentinel(Arc<AtomicUsize>);

    impl Drop for CompletionDropSentinel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn case_provider_deadline_stops_pending_handshake_offline() {
        // Pins: every provider injected into an eval environment is bound to the
        // case guard. A provider stuck before it returns a stream must be dropped
        // at the case deadline rather than outliving the reported timeout.
        let stopped = Arc::new(AtomicUsize::new(0));
        let guard = DeadlineGuard::new(
            CancellationToken::new(),
            Some(Utc::now() + Duration::milliseconds(25)),
        );
        let provider = case_provider(
            Arc::new(PendingProvider {
                stopped: Arc::clone(&stopped),
            }),
            &guard,
        );

        let result = tokio::time::timeout(
            StdDuration::from_secs(1),
            provider.complete(CompletionRequest::new("never returns")),
        )
        .await
        .expect("the case deadline must stop the pending provider handshake");
        let error = result.expect_err("the case deadline must surface as an error");

        assert!(
            matches!(error, MoaError::BudgetExhausted(_)),
            "got {error:?}"
        );
        assert!(guard.is_cancelled(), "deadline expiry cancels the case");
        assert_eq!(
            stopped.load(Ordering::SeqCst),
            1,
            "the pending provider future must be dropped"
        );
    }

    fn suite_with(cases: Vec<TestCase>) -> TestSuite {
        TestSuite {
            name: "suite".to_string(),
            cases,
            ..TestSuite::default()
        }
    }

    fn case(name: &str) -> TestCase {
        TestCase {
            name: name.to_string(),
            input: format!("input for {name}"),
            ..TestCase::default()
        }
    }

    fn agent(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn dry_run_marks_results_skipped() {
        let temp = tempdir().unwrap();
        let engine = EvalEngine::new(
            MoaConfig::default(),
            EngineOptions {
                dry_run: true,
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
        )
        .unwrap();
        let result = engine
            .run_single(&case("case"), &agent("config"))
            .await
            .unwrap();

        assert_eq!(result.status, EvalStatus::Skipped);
    }

    #[test]
    fn parallelism_above_the_admission_bound_is_rejected_at_construction() {
        // Pins: EngineOptions::parallel is bounded, and the bound rejects rather
        // than silently reducing the requested concurrency.
        let temp = tempdir().unwrap();
        let options = |parallel| EngineOptions {
            parallel,
            temp_dir: temp.path().to_path_buf(),
            admission: EvalAdmissionLimits {
                max_parallel_cases: 4,
                ..EvalAdmissionLimits::default()
            },
            ..EngineOptions::default()
        };

        EvalEngine::new(MoaConfig::default(), options(4)).expect("exactly at the bound is allowed");
        let error = EvalEngine::new(MoaConfig::default(), options(5))
            .expect_err("one over the bound is rejected");
        assert!(matches!(
            error,
            Error::Admission(AdmissionError::ParallelismTooHigh {
                requested: 5,
                limit: 4
            })
        ));
        assert!(matches!(
            EvalEngine::new(MoaConfig::default(), options(0)),
            Err(Error::Admission(AdmissionError::InvalidParallelism))
        ));
    }

    #[tokio::test]
    async fn oversized_matrix_dispatches_no_provider_call() {
        // Pins: admission runs before anything is built or called, so a rejected
        // matrix costs zero provider calls.
        let temp = tempdir().unwrap();
        let provider = Arc::new(MockProvider::default());
        let engine = EvalEngine::new(
            MoaConfig::default(),
            EngineOptions {
                temp_dir: temp.path().to_path_buf(),
                admission: EvalAdmissionLimits {
                    max_total_runs: 1,
                    ..EvalAdmissionLimits::default()
                },
                ..EngineOptions::default()
            },
        )
        .unwrap();

        let error = engine
            .run_suite_with_provider(
                &suite_with(vec![case("a"), case("b")]),
                &[agent("baseline")],
                provider.clone(),
            )
            .await
            .expect_err("two runs exceed the one-run matrix limit");
        assert!(matches!(
            error,
            Error::Admission(AdmissionError::MatrixTooLarge {
                total_runs: 2,
                limit: 1
            })
        ));
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn every_run_carries_the_resource_envelope_it_was_admitted_under() {
        // Pins: the run report exposes the envelope the engine enforced, so a
        // reviewer can see the reservation contract a run actually executed
        // under rather than inferring it from scores.
        let temp = tempdir().unwrap();
        let engine = EvalEngine::new(
            MoaConfig::default(),
            EngineOptions {
                dry_run: true,
                temp_dir: temp.path().to_path_buf(),
                admission: EvalAdmissionLimits {
                    per_case: ResourceAmounts {
                        cost_micro_usd: 1_000,
                        tokens: 10,
                        turns: 2,
                        model_calls: 2,
                        tool_calls: 4,
                    },
                    total: ResourceAmounts {
                        cost_micro_usd: 4_000,
                        tokens: 40,
                        turns: 8,
                        model_calls: 8,
                        tool_calls: 16,
                    },
                    ..EvalAdmissionLimits::default()
                },
                ..EngineOptions::default()
            },
        )
        .unwrap();

        let run = engine
            .run_suite(
                &suite_with(vec![case("a"), case("b")]),
                &[agent("baseline"), agent("variant")],
            )
            .await
            .expect("dry run is admitted");

        let resources = run.resources.expect("resource report");
        assert_eq!(resources.planned_cases, 4);
        assert_eq!(resources.per_case_reservation.cost_micro_usd, 1_000);
        assert_eq!(resources.worst_case_projection.cost_micro_usd, 4_000);
        assert_eq!(resources.ledger.limits.cost_micro_usd, 4_000);
        assert!(resources.ledger.deadline.is_some());
        // A dry run reserves nothing because it dispatches nothing.
        assert_eq!(resources.dispatched_cases, 0);
        assert_eq!(resources.ledger.open_reservations, 0);
    }

    #[tokio::test]
    async fn run_environment_db_collects_response_and_metrics() {
        let temp = tempdir().unwrap();
        let config = test_moa_config();
        let environment = build_agent_environment_with_provider(
            &config,
            &agent("config"),
            temp.path(),
            Arc::new(MockProvider::default()),
        )
        .await
        .unwrap();

        let result = run_environment(
            "the with your".to_string(),
            &environment,
            &EngineOptions {
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
            &CancellationToken::new(),
            test_resource_budget(32),
        )
        .await
        .unwrap();

        assert_eq!(result.response.as_deref(), Some("hello from eval"));
        assert_eq!(result.metrics.total_tokens, 49);
        assert_eq!(result.metrics.turn_count, 1);
        environment
            .cleanup()
            .await
            .expect("cleanup eval engine test database");
    }

    #[tokio::test]
    async fn run_environment_db_stops_on_a_cancelled_token() {
        // Pins: cancellation propagates into the turn loop instead of only
        // dropping the outer future, so no model call is dispatched.
        let temp = tempdir().unwrap();
        let config = test_moa_config();
        let provider = Arc::new(MockProvider::default());
        let environment = build_agent_environment_with_provider(
            &config,
            &agent("config"),
            temp.path(),
            provider.clone(),
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = run_environment(
            "hello".to_string(),
            &environment,
            &EngineOptions {
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
            &cancel,
            test_resource_budget(32),
        )
        .await
        .expect_err("a cancelled token stops the turn loop");

        assert!(error.to_string().contains("cancelled"));
        assert_eq!(provider.calls(), 0);
        environment
            .cleanup()
            .await
            .expect("cleanup eval engine test database");
    }

    #[test]
    fn plan_reports_matrix_size() {
        let temp = tempdir().unwrap();
        let engine = EvalEngine::new(
            MoaConfig::default(),
            EngineOptions {
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
        )
        .unwrap();
        let suite = suite_with(vec![case("a"), case("b")]);
        let configs = vec![agent("baseline"), agent("variant")];

        let plan = engine.plan(&suite, &configs);
        assert_eq!(plan.total_runs, 4);
    }

    #[tokio::test]
    async fn run_suite_preserves_config_case_order_in_parallel_mode() {
        let temp = tempdir().unwrap();
        let engine = EvalEngine::new(
            MoaConfig::default(),
            EngineOptions {
                dry_run: true,
                parallel: 2,
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
        )
        .unwrap();
        let suite = suite_with(vec![case("a"), case("b")]);
        let configs = vec![agent("baseline"), agent("variant")];

        let run = engine.run_suite(&suite, &configs).await.unwrap();
        let observed = run
            .results
            .into_iter()
            .map(|result| (result.agent_config, result.test_case))
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                ("baseline".to_string(), "a".to_string()),
                ("baseline".to_string(), "b".to_string()),
                ("variant".to_string(), "a".to_string()),
                ("variant".to_string(), "b".to_string()),
            ]
        );
    }
}
