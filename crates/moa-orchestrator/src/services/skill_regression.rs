//! Review-boundary regression reporting for proposed skill updates.

use std::sync::Arc;

use moa_artifacts::registry::{ArtifactFile, StoredArtifactRevision};
use moa_config::MoaConfig;
use moa_core::{
    error::Result,
    types::{action_policy::ActionRuleScope, experience::LearningCandidate},
};
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use moa_skills::registry::StoredSkillPackage;
use serde_json::Value;

mod compilation;
mod gate;
mod report;
mod runner;
mod suite;
#[cfg(test)]
mod tests;

const DEFAULT_SKILL_TEST_BUDGET_DOLLARS: f64 = 0.50;
const DEFAULT_SKILL_EVALUATORS: &[&str] = &["trajectory", "output", "tool_success"];
/// Floor applied when a generated suite carries no (or a zero) case timeout.
const DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS: u64 = 90;

/// What the review-time regression gate actually executed.
///
/// Acceptance checks recorded on a promoted candidate are derived from this
/// value, so it must describe reality: the gate either compared the candidate
/// against the previous active skill, smoke-ran the candidate alone because no
/// previous revision exists, or blocked promotion without a passing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRegressionExecution {
    /// Previous and candidate suites both executed and scores were compared.
    ComparedWithPrevious,
    /// No previous active skill existed; the candidate suite executed alone.
    CandidateOnly,
    /// Nothing passed; promotion is blocked with a rejection reason.
    Blocked,
}

impl SkillRegressionExecution {
    /// Returns the stable snake_case label recorded in gate reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ComparedWithPrevious => "compared_with_previous",
            Self::CandidateOnly => "candidate_only",
            Self::Blocked => "blocked",
        }
    }
}

/// Outcome of review-time regression evaluation for a skill proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRegressionGate {
    /// Structured report to attach to the candidate evaluation payload.
    pub report: Value,
    /// Whether review acceptance may continue to activate and materialize the skill.
    pub allow_promotion: bool,
    /// Human-readable rejection reason when regression blocks promotion.
    pub rejection_reason: Option<String>,
    /// What the gate actually executed, for honest acceptance-check derivation.
    pub execution: SkillRegressionExecution,
    /// Number of held-out suite sources (prior revisions + sibling sessions)
    /// that actually executed, for honest acceptance-check derivation.
    pub held_out_sources: usize,
    /// Exact compile-audit operation key required by the terminal candidate CAS.
    pub compile_operation_key: Option<String>,
}

impl SkillRegressionGate {
    /// Creates an accepted gate result before attaching an optional compile operation key.
    pub(super) fn accepted(
        report: Value,
        execution: SkillRegressionExecution,
        held_out_sources: usize,
    ) -> Self {
        Self {
            report,
            allow_promotion: true,
            rejection_reason: None,
            execution,
            held_out_sources,
            compile_operation_key: None,
        }
    }

    /// Creates a blocked gate result with no executed held-out sources.
    pub(super) fn blocked(report: Value, rejection_reason: String) -> Self {
        Self {
            report,
            allow_promotion: false,
            rejection_reason: Some(rejection_reason),
            execution: SkillRegressionExecution::Blocked,
            held_out_sources: 0,
            compile_operation_key: None,
        }
    }

    /// Attaches the compile-audit operation key produced for a template-bearing draft.
    pub(super) fn with_compile_operation_key(mut self, operation_key: Option<String>) -> Self {
        self.compile_operation_key = operation_key;
        self
    }
}

/// Draft execution inputs needed only when a regression suite compiles a template.
pub struct SkillRegressionCompileContext {
    /// Production tool router used only for its immutable deployment catalog.
    ///
    /// Skill review has no authenticated session or exact agent policy binding,
    /// so it must not enumerate tenant connector connections or claim connector
    /// invocation authority. Agent-scoped connectors enter execution planning
    /// only when the reviewed skill is later compiled under a bound agent.
    pub router: Arc<ToolRouter>,
    /// Exact draft revision whose template is being reviewed.
    pub draft: StoredArtifactRevision,
    /// Files belonging to the exact draft revision.
    pub draft_files: Vec<ArtifactFile>,
    /// Exact serving package captured before regression execution, if one served.
    pub previous_package: Option<StoredSkillPackage>,
}

/// Builds the review-time regression report for accepting a skill candidate.
pub async fn skill_acceptance_regression_report(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    store: Arc<PostgresSessionStore>,
    scope: ActionRuleScope,
    candidate: LearningCandidate,
    compile_context: SkillRegressionCompileContext,
) -> Result<SkillRegressionGate> {
    gate::evaluate(config, providers, store, scope, candidate, compile_context).await
}
