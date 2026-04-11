//! Skill regression testing, suite generation, and improvement comparison.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    Event, EventRecord, LLMProvider, MemoryPath, MemoryScope, MemoryStore, MoaConfig, MoaError,
    Result, SessionMeta, SkillMetadata, WorkspaceId,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalEngine, EvalRun, EvalStatus, Evaluator, EvaluatorOptions,
    PermissionOverride, SkillOverride, TestCase, TestSuite, build_evaluators, evaluate_run,
    load_suite,
};
use moa_memory::FileMemoryStore;
use moa_memory::index::{LogChange, LogEntry};
use tokio::fs;
use uuid::Uuid;

use crate::format::{
    SkillDocument, parse_skill_markdown, render_skill_markdown, skill_from_wiki_page,
};

const DEFAULT_SUITE_TIMEOUT_SECONDS: u64 = 120;
const DEFAULT_SKILL_TEST_BUDGET_DOLLARS: f64 = 0.50;
const DEFAULT_SKILL_EVALUATORS: &[&str] = &["trajectory", "output", "tool_success"];

/// Completed execution of one skill suite.
#[derive(Debug, Clone)]
pub struct SkillEvalRun {
    /// Loaded suite definition.
    pub suite: TestSuite,
    /// Config used for this run.
    pub config: AgentConfig,
    /// Completed eval run.
    pub run: EvalRun,
}

/// Aggregate regression scoring summary for one skill version.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionSummary {
    /// Average normalized score across all evaluated results.
    pub average_score: f64,
    /// Number of results that ended failed, errored, or timed out.
    pub failed_runs: usize,
    /// Number of results evaluated.
    pub total_runs: usize,
    /// Total dollar cost across the suite.
    pub total_cost_dollars: f64,
}

/// Final decision produced by a skill regression attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRegressionDecision {
    /// Candidate skill version matched or exceeded the baseline.
    Accepted,
    /// Candidate skill version regressed and should be rolled back.
    Rejected,
    /// Regression tests were skipped because their projected cost exceeded the budget.
    SkippedBudget,
    /// No regression suite exists for the skill yet.
    MissingSuite,
}

/// Report emitted after comparing two skill versions.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionReport {
    /// Decision for the candidate skill.
    pub decision: SkillRegressionDecision,
    /// Discovered suite path, when present.
    pub suite_path: Option<PathBuf>,
    /// Baseline run summary, when a suite executed.
    pub previous: Option<SkillRegressionSummary>,
    /// Candidate run summary, when a suite executed.
    pub candidate: Option<SkillRegressionSummary>,
    /// Human-readable detail for logs and callers.
    pub detail: String,
}

impl SkillRegressionReport {
    /// Returns whether the candidate skill should be kept.
    pub fn accepted(&self) -> bool {
        matches!(
            self.decision,
            SkillRegressionDecision::Accepted
                | SkillRegressionDecision::SkippedBudget
                | SkillRegressionDecision::MissingSuite
        )
    }
}

/// Runs the persisted suite for one workspace skill using the configured provider selection.
pub async fn run_skill_suite(
    config: &MoaConfig,
    memory_store: Arc<FileMemoryStore>,
    workspace_id: &WorkspaceId,
    skill_selector: &str,
) -> Result<SkillEvalRun> {
    let resolved =
        resolve_workspace_skill(memory_store.as_ref(), workspace_id, skill_selector).await?;
    let skill_markdown = render_skill_markdown(&resolved.document)?;
    let suite_path = skill_suite_path(memory_store.as_ref(), workspace_id, &resolved.metadata.path);
    let suite = load_suite(&suite_path).map_err(map_eval_error)?;

    let temp_root = std::env::temp_dir().join(format!("moa-skill-eval-{}", Uuid::new_v4()));
    let skill_dir = materialize_skill_dir(
        &temp_root,
        &resolved.document.frontmatter.name,
        &skill_markdown,
    )
    .await?;
    let eval_run = execute_skill_suite(
        config,
        &suite,
        &skill_dir,
        &resolved.document.frontmatter.name,
        None,
    )
    .await;
    let _ = remove_dir_if_exists(&temp_root).await;
    eval_run
}

/// Runs regression tests for an updated skill candidate against the previous version.
pub async fn run_skill_regression(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    current_markdown: &str,
    candidate_markdown: &str,
    memory_store: Arc<FileMemoryStore>,
    llm: Arc<dyn LLMProvider>,
) -> Result<SkillRegressionReport> {
    let suite_path = skill_suite_path(memory_store.as_ref(), &session.workspace_id, &existing.path);
    if !fs::try_exists(&suite_path).await? {
        return Ok(SkillRegressionReport {
            decision: SkillRegressionDecision::MissingSuite,
            suite_path: None,
            previous: None,
            candidate: None,
            detail: "no skill regression suite found".to_string(),
        });
    }

    let suite = load_suite(&suite_path).map_err(map_eval_error)?;
    let estimated_cost = estimate_suite_cost(&suite, llm.as_ref()) * 2.0;
    if estimated_cost > DEFAULT_SKILL_TEST_BUDGET_DOLLARS {
        return Ok(SkillRegressionReport {
            decision: SkillRegressionDecision::SkippedBudget,
            suite_path: Some(suite_path),
            previous: None,
            candidate: None,
            detail: format!(
                "skipped regression tests because estimated cost ${estimated_cost:.4} exceeds budget ${DEFAULT_SKILL_TEST_BUDGET_DOLLARS:.2}"
            ),
        });
    }

    let temp_root = std::env::temp_dir().join(format!("moa-skill-regression-{}", Uuid::new_v4()));
    let previous_document = parse_skill_markdown(current_markdown)?;
    let candidate_document = parse_skill_markdown(candidate_markdown)?;
    let previous_dir = materialize_skill_dir(
        &temp_root.join("previous"),
        &previous_document.frontmatter.name,
        current_markdown,
    )
    .await?;
    let candidate_dir = materialize_skill_dir(
        &temp_root.join("candidate"),
        &candidate_document.frontmatter.name,
        candidate_markdown,
    )
    .await?;

    let previous_run = execute_skill_suite(
        config,
        &suite,
        &previous_dir,
        &previous_document.frontmatter.name,
        Some(llm.clone()),
    )
    .await?;
    let candidate_run = execute_skill_suite(
        config,
        &suite,
        &candidate_dir,
        &candidate_document.frontmatter.name,
        Some(llm),
    )
    .await?;

    let _ = remove_dir_if_exists(&temp_root).await;

    let previous = summarize_regression_run(&previous_run.run);
    let candidate = summarize_regression_run(&candidate_run.run);
    let accepted = compare_scores(&previous, &candidate);
    let detail = format!(
        "suite={} previous(avg={:.3}, failed={}/{}, cost=${:.4}) candidate(avg={:.3}, failed={}/{}, cost=${:.4})",
        suite.name,
        previous.average_score,
        previous.failed_runs,
        previous.total_runs,
        previous.total_cost_dollars,
        candidate.average_score,
        candidate.failed_runs,
        candidate.total_runs,
        candidate.total_cost_dollars,
    );

    Ok(SkillRegressionReport {
        decision: if accepted {
            SkillRegressionDecision::Accepted
        } else {
            SkillRegressionDecision::Rejected
        },
        suite_path: Some(suite_path),
        previous: Some(previous),
        candidate: Some(candidate),
        detail,
    })
}

/// Generates a minimal regression suite for a newly distilled skill.
pub async fn generate_skill_test_suite(
    session: &SessionMeta,
    skill: &SkillDocument,
    skill_path: &MemoryPath,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
) -> Result<PathBuf> {
    let suite = build_generated_suite(skill, events);
    let suite_path = skill_suite_path(memory_store.as_ref(), &session.workspace_id, skill_path);
    if let Some(parent) = suite_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let rendered = toml::to_string_pretty(&suite)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    fs::write(&suite_path, rendered).await?;
    Ok(suite_path)
}

/// Compares baseline and candidate summaries and returns whether the candidate is acceptable.
pub fn compare_scores(
    previous: &SkillRegressionSummary,
    candidate: &SkillRegressionSummary,
) -> bool {
    if candidate.failed_runs != previous.failed_runs {
        return candidate.failed_runs < previous.failed_runs;
    }

    candidate.average_score + f64::EPSILON >= previous.average_score
}

/// Appends a skill improvement decision to the workspace `_log.md`.
pub async fn append_skill_regression_log(
    memory_store: &FileMemoryStore,
    session: &SessionMeta,
    skill_name: &str,
    previous_version: &str,
    candidate_version: &str,
    report: &SkillRegressionReport,
) -> Result<()> {
    let mut changes = Vec::new();
    if let Some(suite_path) = &report.suite_path
        && let Some(relative) = suite_path
            .strip_prefix(workspace_memory_root(memory_store, &session.workspace_id))
            .ok()
            .map(path_to_memory_path)
    {
        changes.push(LogChange {
            action: "Compared".to_string(),
            path: relative,
            detail: Some(report.detail.clone()),
        });
    }
    changes.push(LogChange {
        action: "Updated".to_string(),
        path: crate::format::build_skill_path(skill_name),
        detail: Some(format!("Decision: {:?}", report.decision)),
    });

    memory_store
        .append_scope_log(
            &MemoryScope::Workspace(session.workspace_id.clone()),
            LogEntry {
                timestamp: Utc::now(),
                operation: "skill_improvement".to_string(),
                description: format!("{skill_name} {previous_version} -> {candidate_version}"),
                changes,
                brain_session: Some(session.id.clone()),
            },
        )
        .await
}

fn build_generated_suite(skill: &SkillDocument, events: &[EventRecord]) -> TestSuite {
    let case_name = slugify_case_name(&extract_task_input(events));
    TestSuite {
        name: format!("{}-regression", skill.frontmatter.name),
        description: Some(format!(
            "Auto-generated regression suite for {}",
            skill.frontmatter.name
        )),
        cases: vec![TestCase {
            name: if case_name.is_empty() {
                "smoke".to_string()
            } else {
                case_name
            },
            input: extract_task_input(events),
            expected_output: Some(moa_eval::ExpectedOutput {
                contains: extract_response_keywords(events),
                ..moa_eval::ExpectedOutput::default()
            }),
            expected_trajectory: Some(extract_tool_trajectory(events)),
            timeout_seconds: Some(DEFAULT_SUITE_TIMEOUT_SECONDS),
            tags: vec!["skill".to_string(), "auto-generated".to_string()],
            metadata: std::collections::HashMap::new(),
        }],
        default_timeout_seconds: DEFAULT_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), skill.frontmatter.name.clone()],
    }
}

async fn execute_skill_suite(
    config: &MoaConfig,
    suite: &TestSuite,
    skill_dir: &Path,
    skill_name: &str,
    llm_provider: Option<Arc<dyn LLMProvider>>,
) -> Result<SkillEvalRun> {
    let agent_config = skill_agent_config(skill_name, skill_dir);
    let evaluators = default_skill_evaluators()?;
    let engine = EvalEngine::new(
        config.clone(),
        EngineOptions {
            parallel: 1,
            temp_dir: std::env::temp_dir().join("moa-eval-skill"),
            ..EngineOptions::default()
        },
    )
    .map_err(map_eval_error)?;

    let mut run = if let Some(llm_provider) = llm_provider {
        engine
            .run_suite_with_provider(suite, std::slice::from_ref(&agent_config), llm_provider)
            .await
            .map_err(map_eval_error)?
    } else {
        engine
            .run_suite(suite, std::slice::from_ref(&agent_config))
            .await
            .map_err(map_eval_error)?
    };
    evaluate_run(suite, &mut run, &evaluators)
        .await
        .map_err(map_eval_error)?;

    Ok(SkillEvalRun {
        suite: suite.clone(),
        config: agent_config,
        run,
    })
}

async fn resolve_workspace_skill(
    memory_store: &FileMemoryStore,
    workspace_id: &WorkspaceId,
    selector: &str,
) -> Result<ResolvedWorkspaceSkill> {
    let scope = MemoryScope::Workspace(workspace_id.clone());
    let summaries = memory_store
        .list_pages(scope.clone(), Some(moa_core::PageType::Skill))
        .await?;

    for summary in summaries {
        let page = memory_store.read_page(scope.clone(), &summary.path).await?;
        let document = skill_from_wiki_page(&page)?;
        if skill_selector_matches(selector, &summary.path, &document.frontmatter.name) {
            return Ok(ResolvedWorkspaceSkill {
                metadata: crate::format::skill_metadata_from_document(summary.path, &document),
                document,
            });
        }
    }

    Err(MoaError::StorageError(format!(
        "skill not found in workspace: {selector}"
    )))
}

fn summarize_regression_run(run: &EvalRun) -> SkillRegressionSummary {
    let total_runs = run.results.len();
    let failed_runs = run
        .results
        .iter()
        .filter(|result| !matches!(result.status, EvalStatus::Passed | EvalStatus::Skipped))
        .count();
    let average_score = if run.results.is_empty() {
        1.0
    } else {
        run.results.iter().map(result_score).sum::<f64>() / run.results.len() as f64
    };

    SkillRegressionSummary {
        average_score,
        failed_runs,
        total_runs,
        total_cost_dollars: run.summary.total_cost_dollars,
    }
}

fn result_score(result: &moa_eval::EvalResult) -> f64 {
    if result.scores.is_empty() {
        return match result.status {
            EvalStatus::Passed | EvalStatus::Skipped => 1.0,
            EvalStatus::Failed | EvalStatus::Error | EvalStatus::Timeout => 0.0,
        };
    }

    let mut total = 0.0;
    let mut count = 0usize;
    for score in &result.scores {
        match &score.value {
            moa_eval::ScoreValue::Numeric(value) => {
                total += *value;
                count += 1;
            }
            moa_eval::ScoreValue::Boolean(value) => {
                total += if *value { 1.0 } else { 0.0 };
                count += 1;
            }
            moa_eval::ScoreValue::Categorical(_) => {}
        }
    }

    if count == 0 {
        1.0
    } else {
        total / count as f64
    }
}

fn estimate_suite_cost(suite: &TestSuite, llm: &dyn LLMProvider) -> f64 {
    let pricing = llm.capabilities().pricing;
    suite
        .cases
        .iter()
        .map(|case| {
            let prompt_tokens = estimate_tokens(&case.input).max(128);
            let output_tokens = llm.capabilities().max_output.clamp(256, 2_048);
            ((prompt_tokens as f64 * pricing.input_per_mtok)
                + (output_tokens as f64 * pricing.output_per_mtok))
                / 1_000_000.0
        })
        .sum()
}

fn default_skill_evaluators() -> Result<Vec<Box<dyn Evaluator>>> {
    let names = DEFAULT_SKILL_EVALUATORS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    build_evaluators(&names, &EvaluatorOptions::default()).map_err(map_eval_error)
}

fn skill_agent_config(skill_name: &str, skill_dir: &Path) -> AgentConfig {
    AgentConfig {
        name: format!("skill-{skill_name}"),
        skills: SkillOverride {
            include: vec![skill_dir.to_string_lossy().into_owned()],
            exclude: Vec::new(),
            exclusive: true,
        },
        permissions: PermissionOverride {
            auto_approve_all: true,
            auto_approve: Vec::new(),
            always_deny: Vec::new(),
        },
        ..AgentConfig::default()
    }
}

async fn materialize_skill_dir(root: &Path, skill_name: &str, markdown: &str) -> Result<PathBuf> {
    let slug = slugify_case_name(skill_name);
    let skill_dir = root.join(slug);
    fs::create_dir_all(&skill_dir).await?;
    fs::write(skill_dir.join("SKILL.md"), markdown).await?;
    Ok(skill_dir)
}

async fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if fs::try_exists(path).await? {
        fs::remove_dir_all(path).await?;
    }
    Ok(())
}

fn workspace_memory_root(memory_store: &FileMemoryStore, workspace_id: &WorkspaceId) -> PathBuf {
    memory_store
        .base_dir()
        .join("workspaces")
        .join(workspace_id.as_str())
        .join("memory")
}

fn skill_suite_path(
    memory_store: &FileMemoryStore,
    workspace_id: &WorkspaceId,
    skill_path: &MemoryPath,
) -> PathBuf {
    let relative = Path::new(skill_path.as_str())
        .parent()
        .unwrap_or_else(|| Path::new("skills"));
    workspace_memory_root(memory_store, workspace_id)
        .join(relative)
        .join("tests")
        .join("suite.toml")
}

fn path_to_memory_path(path: &Path) -> MemoryPath {
    MemoryPath::new(path.to_string_lossy().replace('\\', "/"))
}

fn extract_task_input(events: &[EventRecord]) -> String {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(text.trim().to_string())
            }
            _ => None,
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Use the skill to complete the task.".to_string())
}

fn extract_tool_trajectory(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

fn extract_response_keywords(events: &[EventRecord]) -> Vec<String> {
    let response = events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();

    let stopwords = [
        "about", "after", "again", "been", "from", "have", "that", "this", "with", "your", "into",
        "there", "would", "could", "should", "were", "them", "they",
    ];
    let mut keywords = Vec::new();

    for token in response.split(|character: char| !character.is_alphanumeric()) {
        let normalized = token.trim().to_ascii_lowercase();
        if normalized.len() < 4 || stopwords.contains(&normalized.as_str()) {
            continue;
        }
        if keywords.iter().any(|existing| existing == &normalized) {
            continue;
        }
        keywords.push(normalized);
        if keywords.len() == 3 {
            break;
        }
    }

    keywords
}

fn slugify_case_name(value: &str) -> String {
    let mut slug = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

fn skill_selector_matches(selector: &str, path: &MemoryPath, name: &str) -> bool {
    selector == name
        || selector == path.as_str()
        || path.as_str().contains(selector)
        || name.contains(selector)
}

fn map_eval_error(error: moa_eval::EvalError) -> MoaError {
    MoaError::StorageError(error.to_string())
}

struct ResolvedWorkspaceSkill {
    metadata: SkillMetadata,
    document: SkillDocument,
}
