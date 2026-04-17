//! Eval execution engine for running suites against isolated agent configurations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_brain::{StreamedTurnResult, run_streamed_turn};
use moa_core::{Event, EventRange, LLMProvider, MoaConfig, RuntimeEvent};
use opentelemetry::trace::TraceContextExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, broadcast};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::collector::{CollectedExecution, TrajectoryCollector};
use crate::plan::{EvalPlan, build_eval_plan};
use crate::setup::{build_agent_environment, build_agent_environment_with_provider};
use crate::{
    AgentConfig, EvalError, EvalMetrics, EvalResult, EvalStatus, Result, TestCase, TestSuite,
};

const DEFAULT_SINGLE_TIMEOUT_SECONDS: u64 = 300;
const MAX_AGENT_TURNS: usize = 32;

/// Options that control eval execution behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineOptions {
    /// Maximum number of cases to execute concurrently.
    pub parallel: usize,
    /// Base directory used for temporary eval workspaces.
    pub temp_dir: PathBuf,
    /// When true, skip execution and mark runs as skipped.
    pub dry_run: bool,
    /// Whether to capture response and tool content in results.
    pub capture_content: bool,
    /// Maximum bytes captured for any text payload.
    pub content_max_bytes: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            parallel: 1,
            temp_dir: std::env::temp_dir().join("moa-eval"),
            dry_run: false,
            capture_content: true,
            content_max_bytes: 32 * 1024,
        }
    }
}

/// Complete suite execution result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    /// Suite name that was executed.
    pub suite_name: String,
    /// Wall-clock start time for the run.
    pub started_at: DateTime<Utc>,
    /// Wall-clock completion time for the run.
    pub completed_at: DateTime<Utc>,
    /// Per `(config, case)` result entries.
    pub results: Vec<EvalResult>,
    /// Aggregate summary across all results.
    pub summary: RunSummary,
}

/// Aggregate counters and resource usage across a suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunSummary {
    /// Total number of `(config, case)` runs.
    pub total_cases: usize,
    /// Number of successful executions.
    pub passed: usize,
    /// Number of failed evals.
    pub failed: usize,
    /// Number of errored runs.
    pub errors: usize,
    /// Number of timed-out runs.
    pub timeouts: usize,
    /// Total tokens consumed across all runs.
    pub total_tokens: usize,
    /// Total estimated dollar cost across all runs.
    pub total_cost_dollars: f64,
    /// Total wall-clock execution duration in milliseconds.
    pub total_duration_ms: u64,
}

impl RunSummary {
    /// Aggregates a summary from a list of results.
    pub fn from_results(results: &[EvalResult]) -> Self {
        let mut summary = Self {
            total_cases: results.len(),
            ..Self::default()
        };

        for result in results {
            match result.status {
                EvalStatus::Passed => summary.passed += 1,
                EvalStatus::Failed => summary.failed += 1,
                EvalStatus::Error => summary.errors += 1,
                EvalStatus::Timeout => summary.timeouts += 1,
                EvalStatus::Skipped => {}
            }
            summary.total_tokens += result.metrics.total_tokens;
            summary.total_cost_dollars += result.metrics.cost_dollars;
            summary.total_duration_ms += (result.completed_at - result.started_at)
                .num_milliseconds()
                .max(0) as u64;
        }

        summary
    }
}

/// Executes eval suites against one or more agent configurations.
#[derive(Debug, Clone)]
pub struct EvalEngine {
    base_config: MoaConfig,
    options: EngineOptions,
}

impl EvalEngine {
    /// Creates a new eval engine from a base MOA config and execution options.
    pub fn new(base_config: MoaConfig, options: EngineOptions) -> Result<Self> {
        if options.parallel == 0 {
            return Err(EvalError::InvalidConfig(
                "engine parallelism must be at least 1".to_string(),
            ));
        }
        if options.content_max_bytes == 0 {
            return Err(EvalError::InvalidConfig(
                "content_max_bytes must be at least 1".to_string(),
            ));
        }

        Ok(Self {
            base_config,
            options,
        })
    }

    /// Returns the dry-run plan for one suite/config matrix.
    pub fn plan(&self, suite: &TestSuite, configs: &[AgentConfig]) -> EvalPlan {
        build_eval_plan(&self.base_config, suite, configs)
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

    /// Runs one test case against one agent config.
    pub async fn run_single(&self, case: &TestCase, config: &AgentConfig) -> Result<EvalResult> {
        self.run_single_with_timeout(case, config, DEFAULT_SINGLE_TIMEOUT_SECONDS, None)
            .await
    }

    /// Runs one test case against one agent config using one provider instance.
    pub async fn run_single_with_provider(
        &self,
        case: &TestCase,
        config: &AgentConfig,
        llm_provider: Arc<dyn LLMProvider>,
    ) -> Result<EvalResult> {
        self.run_single_with_timeout(
            case,
            config,
            DEFAULT_SINGLE_TIMEOUT_SECONDS,
            Some(llm_provider),
        )
        .await
    }

    async fn run_suite_inner(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> Result<EvalRun> {
        let started_at = Utc::now();
        let mut indexed_pairs: Vec<(usize, usize, Arc<AgentConfig>, Arc<TestCase>)> = Vec::new();
        for (config_index, config) in configs.iter().enumerate() {
            let arc_config = Arc::new(config.clone());
            for (case_index, case) in suite.cases.iter().enumerate() {
                indexed_pairs.push((config_index, case_index, Arc::clone(&arc_config), Arc::new(case.clone())));
            }
        }

        let results = if self.options.parallel <= 1 {
            let mut results = Vec::with_capacity(indexed_pairs.len());
            for (_, _, config, case) in indexed_pairs {
                results.push(
                    self.run_single_with_timeout(
                        &case,
                        &config,
                        suite.default_timeout_seconds,
                        llm_provider.clone(),
                    )
                    .await?,
                );
            }
            results
        } else {
            let semaphore = Arc::new(Semaphore::new(self.options.parallel));
            let mut join_set = JoinSet::new();
            for (config_index, case_index, config, case) in indexed_pairs {
                let engine = self.clone();
                let default_timeout = suite.default_timeout_seconds;
                let llm_provider = llm_provider.clone();
                let permit = semaphore.clone().acquire_owned().await.map_err(|_| {
                    EvalError::InvalidConfig("parallel semaphore closed unexpectedly".to_string())
                })?;
                join_set.spawn(async move {
                    let _permit = permit;
                    let result = engine
                        .run_single_with_timeout(&case, &config, default_timeout, llm_provider)
                        .await;
                    (config_index, case_index, result)
                });
            }

            let mut unordered = Vec::new();
            while let Some(joined) = join_set.join_next().await {
                let (config_index, case_index, result) = joined?;
                unordered.push((config_index, case_index, result?));
            }
            unordered.sort_by_key(|(config_index, case_index, _)| (*config_index, *case_index));
            unordered.into_iter().map(|(_, _, result)| result).collect()
        };

        let summary = RunSummary::from_results(&results);
        Ok(EvalRun {
            suite_name: suite.name.clone(),
            started_at,
            completed_at: Utc::now(),
            results,
            summary,
        })
    }

    async fn run_single_with_timeout(
        &self,
        case: &TestCase,
        config: &AgentConfig,
        default_timeout_seconds: u64,
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> Result<EvalResult> {
        let started_at = Utc::now();

        if self.options.dry_run {
            return Ok(EvalResult {
                test_case: case.name.clone(),
                agent_config: config.name.clone(),
                status: EvalStatus::Skipped,
                started_at,
                completed_at: started_at,
                ..EvalResult::default()
            });
        }

        let environment = if let Some(llm_provider) = llm_provider {
            match build_agent_environment_with_provider(
                &self.base_config,
                config,
                &self.options.temp_dir,
                llm_provider,
            )
            .await
            {
                Ok(environment) => environment,
                Err(error) => {
                    return Ok(build_error_result(
                        case,
                        config,
                        started_at,
                        error.to_string(),
                        EvalStatus::Error,
                    ));
                }
            }
        } else {
            match build_agent_environment(&self.base_config, config, &self.options.temp_dir).await {
                Ok(environment) => environment,
                Err(error) => {
                    return Ok(build_error_result(
                        case,
                        config,
                        started_at,
                        error.to_string(),
                        EvalStatus::Error,
                    ));
                }
            }
        };
        let run_root = environment
            .workspace_dir
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| environment.workspace_dir.clone());
        let timeout = Duration::from_secs(case.timeout_seconds.unwrap_or(default_timeout_seconds));
        let span = tracing::info_span!(
            "eval_run",
            moa.eval.case = %case.name,
            moa.eval.config = %config.name,
            langfuse.session.id = %environment.session_id,
        );
        let trace_id = extract_trace_id(&span);
        let engine_options = self.options.clone();
        let case_input = case.input.clone();
        let execution =
            async move { run_environment(case_input, environment, &engine_options).await }
                .instrument(span);
        let mut handle = tokio::spawn(execution);

        let run_outcome = tokio::select! {
            joined = &mut handle => {
                match joined {
                    Ok(result) => result,
                    Err(error) => Err(EvalError::Join(error)),
                }
            }
            _ = tokio::time::sleep(timeout) => {
                handle.abort();
                let _ = cleanup_workspace(&run_root).await;
                let mut result = build_error_result(
                    case,
                    config,
                    started_at,
                    format!("run exceeded timeout of {} seconds", timeout.as_secs()),
                    EvalStatus::Timeout,
                );
                result.trace_id = trace_id;
                return Ok(result);
            }
        };

        let completed_at = Utc::now();
        let result = match run_outcome {
            Ok(execution) => EvalResult {
                test_case: case.name.clone(),
                agent_config: config.name.clone(),
                status: EvalStatus::Passed,
                response: execution.response,
                trajectory: execution.trajectory,
                metrics: execution.metrics,
                trace_id,
                error: None,
                started_at,
                completed_at,
                ..EvalResult::default()
            },
            Err(error) => {
                let mut result = build_error_result(
                    case,
                    config,
                    started_at,
                    error.to_string(),
                    EvalStatus::Error,
                );
                result.trace_id = trace_id;
                result
            }
        };

        let _ = cleanup_workspace(&run_root).await;
        Ok(result)
    }
}

async fn run_environment(
    input: String,
    environment: crate::AgentEnvironment,
    options: &EngineOptions,
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

    let cancel_token = CancellationToken::new();
    let hard_cancel_token = CancellationToken::new();
    let (runtime_tx, _) = broadcast::channel::<RuntimeEvent>(256);

    for turn_index in 0..MAX_AGENT_TURNS {
        let outcome = run_streamed_turn(
            environment.session_id,
            environment.session_store.clone(),
            environment.llm_provider.clone(),
            &environment.pipeline,
            Some(environment.tool_router.clone()),
            &runtime_tx,
            None,
            Some(&cancel_token),
            Some(&hard_cancel_token),
        )
        .await?;

        match outcome {
            StreamedTurnResult::Complete => break,
            StreamedTurnResult::Continue => {
                if turn_index + 1 == MAX_AGENT_TURNS {
                    return Err(EvalError::InvalidConfig(format!(
                        "agent exceeded the maximum of {MAX_AGENT_TURNS} turns"
                    )));
                }
                continue;
            }
            StreamedTurnResult::NeedsApproval(request) => {
                return Err(EvalError::ApprovalRequired {
                    tool: request.tool_name,
                });
            }
            StreamedTurnResult::Cancelled => {
                return Err(EvalError::Moa(moa_core::MoaError::Cancelled));
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

async fn cleanup_workspace(path: &Path) -> Result<()> {
    if fs_try_exists(path).await? {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|source| crate::EvalError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

async fn fs_try_exists(path: &Path) -> Result<bool> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|source| crate::EvalError::Io {
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

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionResponse, CompletionStream, LLMProvider, MoaConfig,
        ModelCapabilities, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
    };
    use tempfile::tempdir;

    use super::run_environment;
    use crate::{
        AgentConfig, EngineOptions, EvalEngine, EvalStatus, TestCase, TestSuite,
        setup::build_agent_environment_with_provider,
    };

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

    #[derive(Clone)]
    struct MockProvider;

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: moa_core::ModelId::new("mock-model"),
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
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> moa_core::Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "hello from eval".to_string(),
                content: vec![moa_core::CompletionContent::Text(
                    "hello from eval".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: moa_core::ModelId::new("mock-model"),
                input_tokens: 42,
                output_tokens: 7,
                cached_input_tokens: 0,
                usage: token_usage(42, 7),
                duration_ms: 3,
                thought_signature: None,
            }))
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
            .run_single(
                &TestCase {
                    name: "case".to_string(),
                    input: "hello".to_string(),
                    ..TestCase::default()
                },
                &AgentConfig {
                    name: "config".to_string(),
                    ..AgentConfig::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(result.status, EvalStatus::Skipped);
    }

    #[tokio::test]
    async fn run_environment_collects_response_and_metrics() {
        let temp = tempdir().unwrap();
        let environment = build_agent_environment_with_provider(
            &MoaConfig::default(),
            &AgentConfig {
                name: "config".to_string(),
                ..AgentConfig::default()
            },
            temp.path(),
            Arc::new(MockProvider),
        )
        .await
        .unwrap();

        let result = run_environment(
            "the with your".to_string(),
            environment,
            &EngineOptions {
                temp_dir: temp.path().to_path_buf(),
                ..EngineOptions::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(result.response.as_deref(), Some("hello from eval"));
        assert_eq!(result.metrics.total_tokens, 49);
        assert_eq!(result.metrics.turn_count, 1);
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
        let suite = TestSuite {
            name: "suite".to_string(),
            cases: vec![
                TestCase {
                    name: "a".to_string(),
                    input: "one".to_string(),
                    ..TestCase::default()
                },
                TestCase {
                    name: "b".to_string(),
                    input: "two".to_string(),
                    ..TestCase::default()
                },
            ],
            ..TestSuite::default()
        };
        let configs = vec![
            AgentConfig {
                name: "baseline".to_string(),
                ..AgentConfig::default()
            },
            AgentConfig {
                name: "variant".to_string(),
                ..AgentConfig::default()
            },
        ];

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
        let suite = TestSuite {
            name: "suite".to_string(),
            cases: vec![
                TestCase {
                    name: "a".to_string(),
                    input: "one".to_string(),
                    ..TestCase::default()
                },
                TestCase {
                    name: "b".to_string(),
                    input: "two".to_string(),
                    ..TestCase::default()
                },
            ],
            ..TestSuite::default()
        };
        let configs = vec![
            AgentConfig {
                name: "baseline".to_string(),
                ..AgentConfig::default()
            },
            AgentConfig {
                name: "variant".to_string(),
                ..AgentConfig::default()
            },
        ];

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
