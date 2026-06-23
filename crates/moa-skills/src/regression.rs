//! Skill regression suite source generation and comparison helpers.

use std::path::PathBuf;

use moa_core::{Event, EventRecord, MoaConfig, MoaError, Result, SessionMeta, WorkspaceId};
use moa_eval_core::{ExpectedOutput, TestCase, TestSuite};
use tokio::fs;

use crate::format::{SkillDocument, slugify_skill_name};

const DEFAULT_SUITE_TIMEOUT_SECONDS: u64 = 120;

/// Generated regression suite source for a skill draft proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSkillSuite {
    /// Path relative to the configured memory root where the suite would be stored.
    pub relative_path: String,
    /// Pretty TOML source for the generated suite.
    pub source_toml: String,
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
    /// The eval suite itself errored or timed out, so no quality decision was made.
    EvalFailed,
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

/// Generates regression suite TOML for a newly proposed skill without writing files.
pub fn generate_skill_test_suite_source(
    workspace_id: &WorkspaceId,
    skill: &SkillDocument,
    events: &[EventRecord],
) -> Result<GeneratedSkillSuite> {
    let suite = build_generated_suite(skill, events);
    let source_toml = toml::to_string_pretty(&suite)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    Ok(GeneratedSkillSuite {
        relative_path: skill_suite_relative_path(workspace_id, &skill.frontmatter.name),
        source_toml,
    })
}

/// Generates and writes a minimal regression suite for explicit eval/test paths.
pub async fn generate_skill_test_suite(
    config: &MoaConfig,
    session: &SessionMeta,
    skill: &SkillDocument,
    events: &[EventRecord],
) -> Result<PathBuf> {
    let workspace_id = WorkspaceId::new(session.tenant_id.to_string());
    let generated = generate_skill_test_suite_source(&workspace_id, skill, events)?;
    let suite_path = PathBuf::from(&config.local.memory_dir).join(&generated.relative_path);
    if let Some(parent) = suite_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&suite_path, generated.source_toml).await?;
    Ok(suite_path)
}

/// Compares baseline and candidate summaries and returns whether the candidate is acceptable.
#[must_use]
pub fn compare_scores(
    previous: &SkillRegressionSummary,
    candidate: &SkillRegressionSummary,
) -> bool {
    if candidate.failed_runs != previous.failed_runs {
        return candidate.failed_runs < previous.failed_runs;
    }

    candidate.average_score + f64::EPSILON >= previous.average_score
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
            expected_output: Some(ExpectedOutput {
                contains: extract_response_keywords(events),
                ..ExpectedOutput::default()
            }),
            expected_trajectory: Some(extract_tool_trajectory(events)),
            timeout_seconds: Some(DEFAULT_SUITE_TIMEOUT_SECONDS),
            tags: vec!["skill".to_string(), "auto-generated".to_string()],
            metadata: std::collections::HashMap::new(),
            ..TestCase::default()
        }],
        default_timeout_seconds: DEFAULT_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), skill.frontmatter.name.clone()],
    }
}

fn skill_suite_relative_path(workspace_id: &WorkspaceId, skill_name: &str) -> String {
    PathBuf::from("workspaces")
        .join(workspace_id.as_str())
        .join("skills")
        .join(slugify_skill_name(skill_name))
        .join("tests")
        .join("suite.toml")
        .to_string_lossy()
        .into_owned()
}

fn extract_task_input(events: &[EventRecord]) -> String {
    events
        .iter()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(text.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| "Run the learned workflow".to_string())
}

fn extract_response_keywords(events: &[EventRecord]) -> Vec<String> {
    let mut keywords = events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(keywords_from_text(text)),
            _ => None,
        })
        .unwrap_or_default();
    if keywords.is_empty() {
        keywords.push("completed".to_string());
    }
    keywords
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

fn keywords_from_text(text: &str) -> Vec<String> {
    let mut keywords = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 5)
        .map(str::to_ascii_lowercase)
        .take(5)
        .collect::<Vec<_>>();
    keywords.sort();
    keywords.dedup();
    keywords
}

fn slugify_case_name(input: &str) -> String {
    let slug = input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(str::to_ascii_lowercase)
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    if slug.len() > 64 {
        slug.chars().take(64).collect()
    } else {
        slug
    }
}
