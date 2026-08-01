//! Experiment and agent-revision simulation wire DTOs.

use crate::artifacts::ArtifactSummary;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::experiments::{
    ExperimentScorecard, ScorecardEligibility, ScorecardFinding, ScorecardGroupRollup,
    ScorecardValueType,
};
use moa_core::types::identifiers::{SessionId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Stable plan variant key for an artifact-release control or serving baseline.
pub const ARTIFACT_RELEASE_BASELINE_VARIANT_KEY: &str = "release_baseline";
/// Stable plan variant key for the artifact-release candidate.
pub const ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY: &str = "release_candidate";

/// Request payload for accepting a live behavior experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunRequest {
    /// Tenant used for authorization and run ownership.
    pub tenant_id: TenantId,
    /// Human-readable experiment run name.
    pub name: String,
    /// Published experiment_plan artifact revision to execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_revision_uid: Option<Uuid>,
    /// Target payload for the live behavior run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    /// Variant payload under experiment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Value>,
    /// Typed scorecard requested for a single-target experiment run.
    ///
    /// Plan-backed runs take their scorecard from the pinned plan revision and
    /// leave this unset; a single-target run must declare one, because a run with
    /// no evidence requirements can never produce deployment evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorecard: Option<ExperimentScorecard>,
    /// Optional score run identifier used to join against analytics scores.
    pub score_run_id: Option<Uuid>,
    /// Optional idempotency key for scoped run admission.
    pub idempotency_key: Option<String>,
    /// Optional exact agent revision variants used when executing an agent-loop plan.
    #[serde(default)]
    pub agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    /// Internal release-evaluation arms executed as variants of this plan-backed run.
    ///
    /// The artifact-release workflow is the only producer. Normal Behavior Lab
    /// callers leave this absent and retain the plan's authored variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_evaluation: Option<ArtifactReleaseExperimentBinding>,
}

/// One artifact-release attempt bound to a production experiment run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReleaseExperimentBinding {
    /// Durable release dispatch record that owns these arms.
    pub outbox_uid: Uuid,
    /// Release target class (`skill_visibility`, `action_visibility`, or `agent_deployment`).
    pub activation_target: String,
    /// Candidate first, followed by the serving baseline when one exists.
    pub arms: Vec<ArtifactReleaseExperimentArm>,
    /// Exact approved case tuples and repetition counts this run must expand.
    pub cases: Vec<ArtifactReleaseExperimentCase>,
}

/// One approved sparse case in an artifact-release experiment.
///
/// Unlike a normal Behavior Lab plan, a release pack is not a Cartesian
/// product. Each row selects one scenario/persona/profile tuple and declares
/// its own paired repetition count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReleaseExperimentCase {
    /// Scenario ID in the pinned experiment plan.
    pub scenario_id: String,
    /// Persona ID in the pinned experiment plan.
    pub persona_id: String,
    /// Profile ID in the pinned experiment plan.
    pub profile_id: String,
    /// Paired repetitions emitted for every release arm.
    pub repetitions: u32,
}

/// One candidate or baseline arm of an artifact-release experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReleaseExperimentArm {
    /// Stable plan variant key recorded on every trial in this arm.
    pub variant_key: String,
    /// Exact artifact revision this arm substitutes.
    pub revision_uid: Uuid,
    /// Evaluation overlay row identifier.
    pub overlay_uid: Uuid,
    /// Plaintext capability token held only in Restate journals and process memory.
    pub overlay_token: String,
    /// Eval-owned session identifier the overlay is allowed to answer for.
    pub eval_session_id: Uuid,
}

/// Request payload for generating a draft experiment plan artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentGeneratePlanRequest {
    /// Tenant used for authorization and draft ownership.
    pub tenant_id: TenantId,
    /// Natural-language behavior-lab plan description.
    pub description: String,
    /// Optional model override for plan generation.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional artifact references the generated plan should use.
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    /// Certified simulator policy the generated plan must pin.
    pub simulator_policy_uid: Uuid,
    /// Exact certified simulator policy revision.
    pub simulator_policy_revision: i32,
}

/// Response payload returned after generating a draft experiment plan artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentGeneratePlanResponse {
    /// Tenant that owns the generated draft.
    pub tenant_id: TenantId,
    /// Stored artifact row identifier.
    pub artifact_uid: Uuid,
    /// Stored draft revision identifier.
    pub revision_uid: Uuid,
    /// Stored artifact revision status.
    pub status: String,
    /// Artifact source format, currently `json`.
    pub source_format: String,
    /// Canonical generated artifact document text.
    pub source_text: String,
    /// Parsed artifact document as JSON.
    pub document: Value,
    /// Draft validation report persisted with the revision.
    pub validation_report: Value,
}

/// Response payload returned after accepting a live behavior experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunResponse {
    /// Tenant that owns the experiment run.
    pub tenant_id: TenantId,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Current run lifecycle status.
    pub status: String,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Uuid,
    /// Linked session identifier, when the target has one.
    pub session_id: Option<SessionId>,
    /// Linked execution run identifier, when the target has one.
    pub execution_run_uid: Option<Uuid>,
}

/// Request payload for reading an experiment run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunStatusRequest {
    /// Tenant used for authorization and run-result filtering.
    pub tenant_id: TenantId,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
}

/// Response payload for reading an experiment run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunStatusResponse {
    /// Tenant that owns the experiment run.
    pub tenant_id: TenantId,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Current run lifecycle status.
    pub status: String,
    /// Fast target-kind discriminator, when available.
    pub target_kind: Option<String>,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Option<Uuid>,
    /// Linked session identifier, when the target has one.
    pub session_id: Option<SessionId>,
    /// Linked execution run identifier, when the target has one.
    pub execution_run_uid: Option<Uuid>,
    /// Terminal error for failed runs.
    pub error: Option<String>,
    /// Full run record payload for service versions that can expose it.
    #[serde(default)]
    pub run: Value,
}

/// Request payload for listing experiment runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentListRequest {
    /// Tenant used for authorization and run filtering.
    pub tenant_id: TenantId,
    /// Optional lifecycle status filter.
    pub status: Option<String>,
    /// Optional maximum number of runs to return.
    pub limit: Option<u64>,
}

/// Response payload containing experiment run summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentListResponse {
    /// Tenant used for run filtering.
    pub tenant_id: TenantId,
    /// Experiment run summaries ordered for API display.
    #[serde(default)]
    pub runs: Vec<Value>,
}

/// Request payload for listing visible behavior-lab experiment plan artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentPlanListRequest {
    /// Tenant used for authorization and artifact visibility.
    pub tenant_id: TenantId,
    /// Optional scope to list from, defaulting to the tenant tier.
    #[serde(default)]
    pub scope: Option<ActionRuleScope>,
    /// Optional artifact lifecycle status filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Response payload containing visible behavior-lab experiment plan artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentPlanListResponse {
    /// Tenant used for artifact filtering.
    pub tenant_id: TenantId,
    /// Visible experiment plan artifacts ordered for API display.
    #[serde(default)]
    pub plans: Vec<ArtifactSummary>,
}

/// Request payload for listing experiment trials under a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialsRequest {
    /// Tenant used for authorization and trial filtering.
    pub tenant_id: TenantId,
    /// Experiment run whose trials should be listed.
    pub run_uid: Uuid,
    /// Optional lifecycle status filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Optional maximum number of trials to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

/// Typed summary for one experiment trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialSummary {
    /// Tenant that owns the trial.
    pub tenant_id: TenantId,
    /// Experiment run that owns the trial.
    pub run_uid: Uuid,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Current trial lifecycle status.
    pub status: String,
    /// Execution shape targeted by this trial.
    pub target_kind: String,
    /// Deterministic trial key unique inside the run.
    pub trial_key: String,
    /// Stable target variant key selected for the trial.
    pub variant_key: String,
    /// Stable scenario ID selected for the trial.
    pub scenario_id: Option<String>,
    /// Score run identifier used by trial-level score rows.
    pub score_run_id: Uuid,
    /// Linked session identifier, when the trial has one.
    pub session_id: Option<SessionId>,
    /// Linked execution run identifier, when the trial has one.
    pub execution_run_uid: Option<Uuid>,
    /// Trace identifier for observability drill-down.
    pub trace_id: Option<String>,
    /// Durable reason why the trial stopped.
    pub stop_reason: Option<String>,
    /// Terminal error for failed trials.
    pub error: Option<String>,
    /// Number of simulator-target turns persisted for this trial.
    pub turn_count: i32,
}

/// Response payload containing experiment trial summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialsResponse {
    /// Tenant used for trial filtering.
    pub tenant_id: TenantId,
    /// Experiment run whose trials were listed.
    pub run_uid: Uuid,
    /// Trial summaries ordered for API display.
    #[serde(default)]
    pub trials: Vec<ExperimentTrialSummary>,
}

/// Request payload for reading one experiment trial status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialStatusRequest {
    /// Tenant used for authorization and trial filtering.
    pub tenant_id: TenantId,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
}

/// Response payload for reading one experiment trial status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTrialStatusResponse {
    /// Tenant that owns the trial.
    pub tenant_id: TenantId,
    /// Experiment run that owns the trial.
    pub run_uid: Uuid,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Current trial lifecycle status.
    pub status: String,
    /// Execution shape targeted by this trial.
    pub target_kind: String,
    /// Deterministic trial key unique inside the run.
    pub trial_key: String,
    /// Stable target variant key selected for the trial.
    pub variant_key: String,
    /// Stable scenario ID selected for the trial.
    pub scenario_id: Option<String>,
    /// Score run identifier used by trial-level score rows.
    pub score_run_id: Uuid,
    /// Linked session identifier, when the trial has one.
    pub session_id: Option<SessionId>,
    /// Linked execution run identifier, when the trial has one.
    pub execution_run_uid: Option<Uuid>,
    /// Trace identifier for observability drill-down.
    pub trace_id: Option<String>,
    /// Durable reason why the trial stopped.
    pub stop_reason: Option<String>,
    /// Terminal error for failed trials.
    pub error: Option<String>,
    /// Number of simulator-target turns persisted for this trial.
    pub turn_count: i32,
}

/// Request payload for cancelling an experiment run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentCancelRequest {
    /// Tenant used for authorization and run filtering.
    pub tenant_id: TenantId,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Optional cancellation reason.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response payload returned after requesting experiment cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentCancelResponse {
    /// Tenant that owns the experiment run.
    pub tenant_id: TenantId,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Whether cancellation was accepted.
    pub cancelled: bool,
    /// Current run lifecycle status.
    pub status: String,
    /// Human-readable cancellation result.
    pub reason: String,
}

/// Request payload for proposing learning candidates from a completed experiment run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentProposeImprovementsRequest {
    /// Tenant used for authorization and run filtering.
    pub tenant_id: TenantId,
    /// Completed experiment run whose evidence should seed proposals.
    pub run_uid: Uuid,
    /// Optional idempotency key for stable candidate creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Response payload returned after proposing learning candidates from an experiment run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentProposeImprovementsResponse {
    /// Tenant that owns the proposal candidates.
    pub tenant_id: TenantId,
    /// Experiment run summarized by the proposal candidates.
    pub run_uid: Uuid,
    /// Learning candidate identifiers appended for review.
    #[serde(default)]
    pub candidate_ids: Vec<Uuid>,
    /// Draft artifact revisions created for suggested changes, when any are meaningful.
    #[serde(default)]
    pub draft_artifact_revision_uids: Vec<Uuid>,
}

/// Request payload for reading experiment score summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentScoresRequest {
    /// Tenant used for authorization and score filtering.
    pub tenant_id: TenantId,
    /// Experiment run identifier whose resolved score run should be summarized.
    pub run_uid: Uuid,
}

/// Tenant-scoped experiment score summary row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoreSummaryRow {
    /// Score name.
    pub name: String,
    /// Score value type.
    pub value_type: ScorecardValueType,
    /// Number of rows summarized.
    pub n: u64,
    /// Numeric mean or boolean true-rate, or `None` when every summarized value is NULL.
    pub mean_or_rate: Option<f64>,
}

/// Per-trial score summary for one experiment trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentTrialScoreSummary {
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Deterministic trial key unique inside the experiment run.
    pub trial_key: String,
    /// Score run identifier used by trial-level score rows.
    pub score_run_id: Uuid,
    /// Stable target variant key selected for the trial.
    pub variant_key: String,
    /// Stable scenario ID selected for the trial.
    pub scenario_id: Option<String>,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ExperimentScoreSummaryRow>,
    /// Scorecard eligibility computed from this trial's exact score rows.
    pub eligibility: ScorecardEligibility,
    /// Reasons this trial is not eligible, in requirement order.
    #[serde(default)]
    pub eligibility_findings: Vec<ScorecardFinding>,
}

/// Per-scenario score summary for one experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScenarioScoreSummary {
    /// Stable scenario ID summarized by this row group.
    pub scenario_id: Option<String>,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ExperimentScoreSummaryRow>,
}

/// Response payload containing experiment score summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoresResponse {
    /// Tenant used for score filtering.
    pub tenant_id: TenantId,
    /// Experiment run identifier summarized by the response.
    pub run_uid: Uuid,
    /// Resolved score run identifier summarized by the response.
    pub score_run_id: Uuid,
    /// Aggregate score rows computed across trial-level score runs.
    #[serde(default)]
    pub trial_rollup_rows: Vec<ExperimentScoreSummaryRow>,
    /// Per-trial score summaries.
    #[serde(default)]
    pub trials: Vec<ExperimentTrialScoreSummary>,
    /// Per-scenario score summaries.
    #[serde(default)]
    pub scenarios: Vec<ExperimentScenarioScoreSummary>,
    /// Run-level scorecard eligibility, computed from trial rows only.
    pub run_scorecard: ScorecardGroupRollup,
    /// Per-scenario scorecard eligibility.
    #[serde(default)]
    pub scenario_scorecards: Vec<ScorecardGroupRollup>,
    /// Per-variant scorecard eligibility.
    #[serde(default)]
    pub variant_scorecards: Vec<ScorecardGroupRollup>,
}

/// Request payload for comparing two experiment score runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentCompareRequest {
    /// Tenant used for authorization and score filtering.
    pub tenant_id: TenantId,
    /// Baseline experiment run identifier.
    pub base_run_uid: Uuid,
    /// New experiment run identifier.
    pub new_run_uid: Uuid,
}

/// Tenant-scoped experiment run comparison row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentCompareRow {
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Numeric experiment score delta for one scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScenarioScoreDeltaRow {
    /// Stable scenario ID compared by this row.
    pub scenario_id: Option<String>,
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Numeric experiment score delta for one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentVariantScoreDeltaRow {
    /// Stable target variant key compared by this row.
    pub variant_key: String,
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Response payload containing experiment score comparison rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentCompareResponse {
    /// Tenant used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline experiment run identifier.
    pub base_run_uid: Uuid,
    /// New experiment run identifier.
    pub new_run_uid: Uuid,
    /// Resolved baseline score run identifier.
    pub base_score_run_id: Uuid,
    /// Resolved new score run identifier.
    pub new_score_run_id: Uuid,
    /// Comparison rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ExperimentCompareRow>,
    /// Numeric scenario deltas ordered for API display.
    #[serde(default)]
    pub scenario_deltas: Vec<ExperimentScenarioScoreDeltaRow>,
    /// Numeric variant deltas ordered for API display.
    #[serde(default)]
    pub variant_deltas: Vec<ExperimentVariantScoreDeltaRow>,
}

/// One exact agent revision variant used by simulation runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationVariant {
    /// Stable variant key used in trial rows and score comparisons.
    pub variant_key: String,
    /// Exact published agent revision to select for the variant.
    pub revision_uid: Uuid,
}

/// Request payload for running one plan-backed simulation across agent revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationRunRequest {
    /// Tenant used for authorization and artifact visibility.
    pub tenant_id: TenantId,
    /// Human-readable simulation run name.
    pub name: String,
    /// Published experiment_plan revision that defines scenarios/personas/profiles.
    pub plan_revision_uid: Uuid,
    /// Baseline agent revision variant.
    pub base: AgentRevisionSimulationVariant,
    /// Candidate agent revision variants.
    #[serde(default)]
    pub candidates: Vec<AgentRevisionSimulationVariant>,
    /// Optional idempotency key for scoped run admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Response payload returned after admitting an agent-revision simulation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationRunResponse {
    /// Tenant that owns the simulation run.
    pub tenant_id: TenantId,
    /// Created experiment run identifier.
    pub run_uid: Uuid,
    /// Initial experiment run status.
    pub status: String,
    /// Score run identifier used for run-level analytics.
    pub score_run_id: Uuid,
    /// Published experiment_plan revision used by this simulation.
    pub plan_revision_uid: Uuid,
    /// Exact agent revision variants accepted for this simulation.
    #[serde(default)]
    pub variants: Vec<AgentRevisionSimulationVariant>,
}

/// Request payload for comparing variants inside one agent-revision simulation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationCompareRequest {
    /// Tenant that owns the simulation run.
    pub tenant_id: TenantId,
    /// Experiment run to compare.
    pub run_uid: Uuid,
    /// Baseline variant key.
    pub base_variant_key: String,
    /// Candidate variant keys. When empty, every non-baseline variant is compared.
    #[serde(default)]
    pub candidate_variant_keys: Vec<String>,
}

/// Per-variant execution summary for an agent-revision simulation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationVariantResult {
    /// Stable target variant key.
    pub variant_key: String,
    /// Exact agent revision selected by this variant.
    pub revision_uid: Uuid,
    /// Number of trial rows for this variant.
    pub trial_count: u64,
    /// Number of completed trials.
    pub completed_count: u64,
    /// Number of failed trials.
    pub failed_count: u64,
    /// Number of cancelled trials.
    pub cancelled_count: u64,
    /// Trial score run identifiers.
    #[serde(default)]
    pub score_run_ids: Vec<Uuid>,
    /// Sessions created for this variant.
    #[serde(default)]
    pub session_ids: Vec<SessionId>,
    /// Stop reason counts keyed by persisted stop-reason label.
    #[serde(default)]
    pub stop_reason_counts: BTreeMap<String, u64>,
    /// Terminal errors observed for this variant.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Response payload comparing variants inside one agent-revision simulation run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRevisionSimulationCompareResponse {
    /// Tenant that owns the simulation run.
    pub tenant_id: TenantId,
    /// Experiment run that was compared.
    pub run_uid: Uuid,
    /// Baseline variant key.
    pub base_variant_key: String,
    /// Variant execution summaries.
    #[serde(default)]
    pub variants: Vec<AgentRevisionSimulationVariantResult>,
    /// Numeric score deltas from baseline to candidates.
    #[serde(default)]
    pub variant_deltas: Vec<ExperimentVariantScoreDeltaRow>,
}

/// Request payload for comparing two resolved agent revision policies before simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionCompareRequest {
    /// Tenant used for authorization and artifact visibility.
    pub tenant_id: TenantId,
    /// Baseline published agent revision.
    pub base_revision_uid: Uuid,
    /// Candidate published agent revision.
    pub new_revision_uid: Uuid,
}

/// Change category for dependency differences between two resolved agent revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDependencyChange {
    /// Dependency exists only on the candidate revision.
    Added,
    /// Dependency exists only on the baseline revision.
    Removed,
    /// Dependency exists on both revisions with different pinned content.
    Changed,
}

/// Artifact dependency delta between two resolved agent revision policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentArtifactDependencyDelta {
    /// Stable dependency reference being compared.
    pub reference: String,
    /// Baseline dependency revision, when present.
    pub base_revision_uid: Option<Uuid>,
    /// Candidate dependency revision, when present.
    pub new_revision_uid: Option<Uuid>,
    /// Change category.
    pub change: AgentDependencyChange,
}

/// Tool dependency delta between two resolved agent revision policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolDependencyDelta {
    /// Stable tool name being compared.
    pub name: String,
    /// Baseline tool identity hash, when present.
    ///
    /// Identity, not contract: this changes when the tool's name or provider
    /// namespace changes, and does NOT change when a tool's input schema does.
    /// A comparison showing no delta here is not evidence that two revisions
    /// agree on what the tool accepts.
    pub base_identity_hash: Option<String>,
    /// Candidate tool identity hash, when present.
    pub new_identity_hash: Option<String>,
    /// Change category.
    pub change: AgentDependencyChange,
}

/// Response payload for comparing two resolved agent revision policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRevisionCompareResponse {
    /// Tenant used for artifact visibility.
    pub tenant_id: TenantId,
    /// Baseline published agent revision.
    pub base_revision_uid: Uuid,
    /// Candidate published agent revision.
    pub new_revision_uid: Uuid,
    /// Stable baseline policy hash.
    pub base_policy_hash: String,
    /// Stable candidate policy hash.
    pub new_policy_hash: String,
    /// Whether the resolved runtime policies differ.
    pub changed: bool,
    /// Whether resolved instruction text changed.
    pub instructions_changed: bool,
    /// Whether resolved tool selection policy changed.
    pub tool_policy_changed: bool,
    /// Exact artifact dependency deltas.
    #[serde(default)]
    pub artifact_dependency_deltas: Vec<AgentArtifactDependencyDelta>,
    /// Exact tool dependency deltas.
    #[serde(default)]
    pub tool_dependency_deltas: Vec<AgentToolDependencyDelta>,
}
