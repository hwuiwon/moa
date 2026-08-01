//! Application boundary for behavior-lab experiment service operations.

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::simulation::{
    ExperimentTargetKind, MAX_PLAN_TRIAL_COST_CENTS, MAX_PLAN_TRIAL_TOKENS,
    experiment_plan_response_schema,
};
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_core::traits::Identity;
use moa_core::{
    error::MoaError,
    types::action_policy::ActionRuleScope,
    types::completion::CompletionRequest,
    types::completion::JsonResponseFormat,
    types::context::ContextMessage,
    types::experience::LearningCandidate,
    types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateType,
    types::experience::LearningProposalKind,
    types::experience::LearningRiskClass,
    types::experiments::{ExperimentCancelSignal, ExperimentScorecard},
    types::identifiers::ModelId,
    types::identifiers::TenantId,
    types::resource::ResourceAmounts,
};
use moa_observability::{record_experiment_run, record_experiment_score_rows};
use moa_scoring::{
    Error, ExperimentRunScoreRowsRef, ExperimentScoreRow, ScoreCompareRef, ScoreCompareRow,
    ScoreSummaryRow, compare_score_runs_for_tenant, exact_experiment_run_score_rows_for_tenant,
};
use moa_wire::experiments::{
    AgentRevisionSimulationVariant, ArtifactReleaseExperimentBinding, ExperimentCancelRequest,
    ExperimentCancelResponse, ExperimentCompareRequest, ExperimentCompareResponse,
    ExperimentCompareRow, ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse,
    ExperimentListRequest, ExperimentListResponse, ExperimentProposeImprovementsRequest,
    ExperimentProposeImprovementsResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentScenarioScoreDeltaRow, ExperimentScenarioScoreSummary, ExperimentScoreSummaryRow,
    ExperimentScoresRequest, ExperimentScoresResponse, ExperimentTrialScoreSummary,
    ExperimentTrialStatusRequest, ExperimentTrialStatusResponse, ExperimentTrialSummary,
    ExperimentTrialsRequest, ExperimentTrialsResponse, ExperimentVariantScoreDeltaRow,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

use crate::eligibility::{
    ScorecardAssessment, ScorecardEligibility, ScorecardExpectation, ScorecardFinding,
    ScorecardGroupRollup, ScorecardSupportSummary, assess_trial_scorecard,
    require_runnable_scorecard, roll_up_group,
};
use crate::evidence::TrialScoreTarget;
use crate::model::{
    ExperimentResourceEnvelope, ExperimentRunRecord, ExperimentRunStatus, ExperimentTarget,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentVariant, MICRO_USD_PER_CENT,
    NewExperimentRun,
};
use crate::plan::{
    PlanCaseSelection, PlanMatrixShape, plan_matrix_shape, project_plan_run,
    selected_plan_matrix_shape,
};
use crate::scores::{
    ExperimentRunCompareRef, ExperimentRunScoreRef, ScenarioScoreDeltaRow, ScenarioScoreSummary,
    TrialScoreSummary, VariantScoreDeltaRow, compare_experiment_score_breakdown_for_tenant,
    experiment_score_breakdown_for_tenant,
};
use crate::simulator_policy::store::SimulatorPolicyStore;
use crate::store::ExperimentStore;

const DEFAULT_LIST_LIMIT: i64 = 50;
const GENERATED_PLAN_SOURCE_FORMAT: &str = "json";

/// Error returned by behavior-lab application operations.
#[derive(Debug, Error)]
pub enum ExperimentAppError {
    /// Caller supplied an invalid experiment request.
    #[error("{0}")]
    BadRequest(String),
    /// Requested experiment data was not visible in the requested scope.
    #[error("{0}")]
    NotFound(String),
    /// An admission quota refused the run before any row or dispatch existed.
    #[error("{0}")]
    QuotaExceeded(String),
    /// Experiment application state could not be serialized.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Shared MOA infrastructure failed.
    #[error(transparent)]
    Moa(#[from] MoaError),
    /// Scoring storage or comparison failed.
    #[error(transparent)]
    Scoring(#[from] Error),
}

/// Convenience result type for behavior-lab application operations.
pub type Result<T> = std::result::Result<T, ExperimentAppError>;

/// Stored experiment run admission plus workflow-ready payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmittedExperimentRun {
    /// Public response returned by `Experiments/run`.
    pub response: ExperimentRunResponse,
    /// Stable run identifier used as the parent workflow key.
    pub run_uid: Uuid,
    /// Serialized target payload for the run workflow.
    pub target: Value,
    /// Serialized variant payload for the run workflow.
    pub variant: Value,
    /// Published plan revision used by plan-backed runs.
    pub plan_revision_uid: Option<Uuid>,
    /// Identity that admitted the run, propagated into workflow execution.
    pub identity: Identity,
    /// Score run identifier used for analytics joins.
    pub score_run_id: Uuid,
    /// Exact agent revision variants selected for plan-backed simulation, when present.
    #[serde(default)]
    pub agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    /// Internal artifact-release arms to execute through the production plan path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_evaluation: Option<ArtifactReleaseExperimentBinding>,
}

/// Proposed experiment-derived learning candidate plus API response metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentImprovementProposal {
    /// Candidate that must be appended before any promotion can happen.
    pub candidate: LearningCandidate,
    /// Public response returned after the candidate append succeeds.
    pub response: ExperimentProposeImprovementsResponse,
}

const PLAN_GENERATION_SYSTEM_PROMPT: &str = "\
Generate one canonical behavior-lab experiment_plan artifact document as JSON. Return only the \
JSON object. Do not start, run, publish, or execute the plan.

Create a draft artifact document with api_version `moa.artifact/v1`, kind `experiment_plan`, \
status `draft`, and definition.type `experiment_plan`.

The document must validate as a draft experiment_plan artifact. Put scenarios, personas, profiles, \
and optional data_bundles inside definition.spec.simulation as embedded objects with stable `id` \
fields. Include at least one scenario, persona, profile, target variant, simulator_policy, \
parallelism, trials_per_combination, and budget.max_total_cents. Use stable snake_case or \
kebab-case names in metadata.name, simulation IDs, and target variant keys. Every scenario must \
have a non-empty initial_situation, at least one goal, at least one success criterion, and max_turns \
between 1 and 100. Every persona must have a non-empty voice, at least one goal, and non-empty \
stop_behavior. Every profile must have a non-empty facts object.";

/// Builds the structured provider request for behavior-lab plan generation.
pub fn plan_generation_request(
    request: &ExperimentGeneratePlanRequest,
) -> Result<CompletionRequest> {
    if request.description.trim().is_empty() {
        return Err(bad_request("experiment plan description must not be empty"));
    }
    if request.simulator_policy_uid.is_nil() || request.simulator_policy_revision < 1 {
        return Err(bad_request(
            "experiment plan generation requires an exact non-nil simulator policy revision",
        ));
    }

    let mut completion = CompletionRequest::new(plan_generation_user_prompt(request));
    completion
        .messages
        .insert(0, ContextMessage::system(PLAN_GENERATION_SYSTEM_PROMPT));
    completion.model = request.model.as_ref().map(ModelId::new);
    completion.max_output_tokens = Some(4096);
    completion.temperature = Some(0.2);
    completion.response_format = Some(experiment_plan_response_format());
    Ok(completion)
}

/// Builds one bounded repair request after generated artifact parsing or
/// validation fails.
pub fn plan_generation_repair_request(
    request: &ExperimentGeneratePlanRequest,
    invalid_output: &str,
    validation_error: &str,
) -> Result<CompletionRequest> {
    let mut completion = plan_generation_request(request)?;
    let repair_prompt = format!(
        "The previous JSON did not validate. Return a corrected complete JSON object only.\n\n\
         Validation failure:\n{}\n\nPrevious JSON:\n{}",
        validation_error.trim(),
        invalid_output.trim()
    );
    completion
        .messages
        .push(ContextMessage::user(repair_prompt));
    Ok(completion)
}

/// Stores validated provider output as a draft experiment-plan artifact.
pub async fn store_generated_plan(
    pool: sqlx::PgPool,
    request: ExperimentGeneratePlanRequest,
    completion_text: &str,
) -> Result<ExperimentGeneratePlanResponse> {
    let document = parse_generated_plan_document(completion_text)?;
    require_valid_generated_plan(&document)?;
    let ArtifactDefinition::ExperimentPlan(definition) = &document.definition else {
        return Err(bad_request(
            "generated artifact must contain an experiment_plan definition",
        ));
    };
    if definition.simulator_policy.policy_uid != request.simulator_policy_uid
        || definition.simulator_policy.revision != request.simulator_policy_revision
    {
        return Err(bad_request(
            "generated plan simulator policy does not match the requested certified revision",
        ));
    }
    let source_text = document.to_json().map_err(bad_request_from)?;
    let document_value =
        serde_json::to_value(&document).map_err(|error| serialization_error(error.to_string()))?;
    let scope = tenant_scope(request.tenant_id);
    let stored = ArtifactRegistry::new(pool)
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: GENERATED_PLAN_SOURCE_FORMAT,
                source_text: source_text.as_bytes(),
                files: &[],
            },
        )
        .await?;

    Ok(ExperimentGeneratePlanResponse {
        tenant_id: request.tenant_id,
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        status: stored.status.to_string(),
        source_format: stored.source_format,
        source_text,
        document: document_value,
        validation_report: stored.validation_report,
    })
}

/// Admits a behavior-lab experiment run and returns workflow dispatch payloads.
pub async fn admit_run(
    pool: sqlx::PgPool,
    request: ExperimentRunRequest,
    identity: Identity,
) -> Result<AdmittedExperimentRun> {
    let scope = tenant_scope(request.tenant_id);
    let release_evaluation = request.release_evaluation.clone();
    let run_inputs = match request.plan_revision_uid {
        Some(plan_revision_uid) => {
            plan_run_inputs(
                pool.clone(),
                request.tenant_id,
                &scope,
                plan_revision_uid,
                &request.name,
                &request.agent_revision_variants,
                release_evaluation.as_ref(),
            )
            .await?
        }
        None => single_target_run_inputs(request.target, request.variant, request.scorecard)?,
    };
    let score_run_id = request.score_run_id.unwrap_or_else(Uuid::now_v7);
    let run = ExperimentStore::new(pool)
        .insert_run(
            &scope,
            NewExperimentRun {
                name: request.name,
                session_id: run_inputs.target.attached_session_id(),
                execution_run_uid: None,
                artifact_revision_uids: run_inputs.artifact_revision_uids.clone(),
                score_run_id,
                target: run_inputs.target,
                variant: run_inputs.variant,
                scorecard: run_inputs.scorecard,
                idempotency_key: request.idempotency_key,
                created_by_identity: identity_payload(identity.clone())?,
                plan_artifact_uid: run_inputs.plan_artifact_uid,
                expected_trials: run_inputs.expected_trials,
                resource_envelope: run_inputs.resource_envelope,
                simulator_policy: run_inputs.simulator_policy.clone(),
            },
        )
        .await
        .map_err(admission_app_error)?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());

    Ok(AdmittedExperimentRun {
        response: run_response_from_record(request.tenant_id, &run),
        run_uid: run.run_uid,
        target: serialized_payload("target", &run.target)?,
        variant: serialized_payload("variant", &run.variant)?,
        plan_revision_uid: run_inputs.plan_revision_uid,
        identity,
        score_run_id: run.score_run_id,
        agent_revision_variants: request.agent_revision_variants,
        release_evaluation,
    })
}

/// Lists behavior-lab experiment runs in tenant scope.
pub async fn list_runs(
    pool: sqlx::PgPool,
    request: ExperimentListRequest,
) -> Result<ExperimentListResponse> {
    let scope = tenant_scope(request.tenant_id);
    let status = request.status.as_deref().map(parse_status).transpose()?;
    let limit = request
        .limit
        .map(|limit| i64::try_from(limit).map_err(|_| bad_request("limit is too large")))
        .transpose()?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let runs = ExperimentStore::new(pool)
        .list_runs(&scope, status, limit)
        .await?
        .into_iter()
        .map(|run| serialized_payload("run", &run))
        .collect::<Result<Vec<_>>>()?;

    Ok(ExperimentListResponse {
        tenant_id: request.tenant_id,
        runs,
    })
}

/// Lists behavior-lab experiment trials for one run.
pub async fn list_trials(
    pool: sqlx::PgPool,
    request: ExperimentTrialsRequest,
) -> Result<ExperimentTrialsResponse> {
    let scope = tenant_scope(request.tenant_id);
    let status = request
        .status
        .as_deref()
        .map(parse_trial_status)
        .transpose()?;
    let limit = request
        .limit
        .map(|limit| i64::try_from(limit).map_err(|_| bad_request("limit is too large")))
        .transpose()?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let trials = ExperimentStore::new(pool)
        .list_trials(&scope, request.run_uid, status, limit)
        .await?
        .into_iter()
        .map(|trial| trial_summary_from_record(request.tenant_id, &trial))
        .collect();

    Ok(ExperimentTrialsResponse {
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        trials,
    })
}

/// Loads one behavior-lab trial status summary.
pub async fn trial_status(
    pool: sqlx::PgPool,
    request: ExperimentTrialStatusRequest,
) -> Result<ExperimentTrialStatusResponse> {
    let scope = tenant_scope(request.tenant_id);
    let trial = ExperimentStore::new(pool)
        .load_trial(&scope, request.trial_uid)
        .await?
        .ok_or_else(|| trial_not_found(request.trial_uid))?;
    Ok(trial_status_response_from_summary(
        trial_summary_from_record(request.tenant_id, &trial),
    ))
}

/// Cancels a behavior-lab run and its active trials.
pub async fn cancel_run(
    pool: sqlx::PgPool,
    request: ExperimentCancelRequest,
    identity: Identity,
) -> Result<ExperimentCancelResponse> {
    let scope = tenant_scope(request.tenant_id);
    let reason = request
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "cancelled".to_string());
    let store = ExperimentStore::new(pool);
    let existing = load_required_run(&store, &scope, request.run_uid).await?;
    // A genuinely finished run has nothing to cancel or reconcile. An
    // already-`cancelled` run is deliberately NOT short-circuited here: a retry
    // must still reconcile any active trial rows that were stranded when a prior
    // attempt updated the run projection but crashed before cancelling trials.
    if matches!(
        existing.status,
        ExperimentRunStatus::Completed | ExperimentRunStatus::Failed
    ) {
        return Ok(ExperimentCancelResponse {
            tenant_id: request.tenant_id,
            run_uid: existing.run_uid,
            cancelled: false,
            status: existing.status.as_str().to_string(),
            reason,
        });
    }
    // Cancel the run and reconcile its active trials atomically in one
    // transaction so the parent projection and trial rows can never diverge.
    let (run, _cancelled_trials) = store
        .cancel_run_and_active_trials(
            &scope,
            request.run_uid,
            ExperimentCancelSignal {
                reason: reason.clone(),
                identity,
            },
        )
        .await?;
    let run = run.ok_or_else(|| run_not_found(request.run_uid))?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());

    Ok(ExperimentCancelResponse {
        tenant_id: request.tenant_id,
        run_uid: run.run_uid,
        // `true` on the first cancel; `false` for an idempotent retry behind an
        // already-cancelled parent (which still reconciles active trials above).
        cancelled: existing.status != ExperimentRunStatus::Cancelled,
        status: run.status.as_str().to_string(),
        reason,
    })
}

/// Builds an experiment-derived learning proposal without activating any learned state.
pub async fn propose_improvement_candidate(
    pool: sqlx::PgPool,
    request: ExperimentProposeImprovementsRequest,
) -> Result<ExperimentImprovementProposal> {
    let scope = tenant_scope(request.tenant_id);
    let store = ExperimentStore::new(pool.clone());
    let run = load_required_run(&store, &scope, request.run_uid).await?;
    require_completed_run(&run)?;
    let plan_revision_uid = require_proposal_enabled_plan(pool.clone(), &scope, &run).await?;
    let completed_trials = store
        .list_trials(
            &scope,
            run.run_uid,
            Some(ExperimentTrialStatus::Completed),
            10_000,
        )
        .await?;
    if completed_trials.is_empty() {
        return Err(bad_request(
            "experiment learning proposals require at least one completed trial",
        ));
    }
    let trial_breakdown = experiment_score_breakdown_for_tenant(
        &pool,
        ExperimentRunScoreRef {
            tenant_id: request.tenant_id,
            run_uid: run.run_uid,
        },
    )
    .await?;
    // Trial score runs are authoritative for plan-backed Behavior Lab. The
    // previous gate also demanded rows on the RUN score run, which no trial path
    // ever writes, so the only way to satisfy it was to seed rows out of band —
    // exactly the "seeded rows prove query mechanics, not evidence" problem.
    let scorecards =
        experiment_run_scorecards(&pool, request.tenant_id, &run, &completed_trials).await?;
    require_eligible_trial_scorecards(&scorecards)?;

    let draft_artifact_revision_uids = Vec::new();
    let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
        tenant_id: request.tenant_id,
        run: &run,
        completed_trials: &completed_trials,
        scorecards: &scorecards,
        trial_rollup_rows: &trial_breakdown.trial_rollup_rows,
        trial_score_summaries: &trial_breakdown.trials,
        scenario_score_summaries: &trial_breakdown.scenarios,
        plan_revision_uid,
        draft_artifact_revision_uids: &draft_artifact_revision_uids,
        idempotency_key: request.idempotency_key.as_deref(),
        now: Utc::now(),
    });
    let response = ExperimentProposeImprovementsResponse {
        tenant_id: request.tenant_id,
        run_uid: run.run_uid,
        candidate_ids: vec![candidate.id],
        draft_artifact_revision_uids,
    };

    Ok(ExperimentImprovementProposal {
        candidate,
        response,
    })
}

/// Reads experiment score summaries and drilldown breakdowns.
pub async fn scores(
    pool: sqlx::PgPool,
    request: ExperimentScoresRequest,
) -> Result<ExperimentScoresResponse> {
    let scope = tenant_scope(request.tenant_id);
    let store = ExperimentStore::new(pool.clone());
    let run = load_required_run(&store, &scope, request.run_uid).await?;
    let trials = store
        .list_trials(&scope, run.run_uid, None, 100_000)
        .await?;
    let trial_breakdown = experiment_score_breakdown_for_tenant(
        &pool,
        ExperimentRunScoreRef {
            tenant_id: request.tenant_id,
            run_uid: run.run_uid,
        },
    )
    .await?;
    record_experiment_score_rows(
        "trial_rollup",
        trial_breakdown.trial_rollup_rows.len() as u64,
    );
    record_experiment_score_rows(
        "trial_breakdown",
        trial_breakdown
            .trials
            .iter()
            .map(|trial| trial.rows.len() as u64)
            .sum(),
    );
    record_experiment_score_rows(
        "scenario_breakdown",
        trial_breakdown
            .scenarios
            .iter()
            .map(|scenario| scenario.rows.len() as u64)
            .sum(),
    );

    let scorecards = experiment_run_scorecards(&pool, request.tenant_id, &run, &trials).await?;
    // Keyed by trial so EVERY trial appears below, scored or not. Building the
    // list from the score join alone silently omitted trials with zero evidence,
    // which is exactly the trial a reader most needs to see.
    let rows_by_trial = trial_breakdown
        .trials
        .into_iter()
        .map(|summary| (summary.trial_uid, summary))
        .collect::<BTreeMap<_, _>>();

    Ok(ExperimentScoresResponse {
        tenant_id: request.tenant_id,
        run_uid: run.run_uid,
        score_run_id: run.score_run_id,
        trial_rollup_rows: trial_breakdown
            .trial_rollup_rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
        trials: scorecards
            .trials
            .iter()
            .map(|entry| experiment_trial_score_summary(entry, rows_by_trial.get(&entry.trial_uid)))
            .collect(),
        scenarios: trial_breakdown
            .scenarios
            .into_iter()
            .map(experiment_scenario_score_summary)
            .collect(),
        run_scorecard: scorecards.run,
        scenario_scorecards: scorecards.scenarios,
        variant_scorecards: scorecards.variants,
    })
}

/// Compares two experiment score runs and returns score deltas.
pub async fn compare_runs(
    pool: sqlx::PgPool,
    request: ExperimentCompareRequest,
) -> Result<ExperimentCompareResponse> {
    let scope = tenant_scope(request.tenant_id);
    let store = ExperimentStore::new(pool.clone());
    let base_run = load_required_run(&store, &scope, request.base_run_uid).await?;
    let new_run = load_required_run(&store, &scope, request.new_run_uid).await?;
    let compare_response = compare_score_runs_for_tenant(
        &pool,
        ScoreCompareRef {
            tenant_id: request.tenant_id,
            base_run: base_run.score_run_id,
            new_run: new_run.score_run_id,
        },
    )
    .await?;
    record_experiment_score_rows("compare", compare_response.rows.len() as u64);
    let breakdown_compare = compare_experiment_score_breakdown_for_tenant(
        &pool,
        ExperimentRunCompareRef {
            tenant_id: request.tenant_id,
            base_run_uid: base_run.run_uid,
            new_run_uid: new_run.run_uid,
        },
    )
    .await?;
    record_experiment_score_rows(
        "scenario_compare",
        breakdown_compare.scenario_deltas.len() as u64,
    );
    record_experiment_score_rows(
        "variant_compare",
        breakdown_compare.variant_deltas.len() as u64,
    );

    Ok(ExperimentCompareResponse {
        tenant_id: request.tenant_id,
        base_run_uid: base_run.run_uid,
        new_run_uid: new_run.run_uid,
        base_score_run_id: base_run.score_run_id,
        new_score_run_id: new_run.score_run_id,
        rows: compare_response
            .rows
            .into_iter()
            .map(experiment_compare_row)
            .collect(),
        scenario_deltas: breakdown_compare
            .scenario_deltas
            .into_iter()
            .map(experiment_scenario_score_delta_row)
            .collect(),
        variant_deltas: breakdown_compare
            .variant_deltas
            .into_iter()
            .map(experiment_variant_score_delta_row)
            .collect(),
    })
}

/// Evidence used to build one experiment-derived learning candidate.
pub struct ExperimentLearningProposalEvidence<'a> {
    /// Tenant that owns the candidate review lifecycle.
    pub tenant_id: TenantId,
    /// Completed experiment run that produced proposal evidence.
    pub run: &'a ExperimentRunRecord,
    /// Completed trials attached to the experiment run.
    pub completed_trials: &'a [ExperimentTrialRecord],
    /// Run, scenario, variant, and per-trial scorecards computed from trial rows.
    pub scorecards: &'a ExperimentRunScorecards,
    /// Aggregate score rows across trial score runs.
    pub trial_rollup_rows: &'a [ScoreSummaryRow],
    /// Per-trial score summaries.
    pub trial_score_summaries: &'a [TrialScoreSummary],
    /// Per-scenario score summaries.
    pub scenario_score_summaries: &'a [ScenarioScoreSummary],
    /// Plan revision that explicitly enabled learning proposals.
    pub plan_revision_uid: Uuid,
    /// Draft artifact revisions created for suggested changes.
    pub draft_artifact_revision_uids: &'a [Uuid],
    /// Optional caller-provided idempotency key.
    pub idempotency_key: Option<&'a str>,
    /// Candidate creation timestamp.
    pub now: chrono::DateTime<Utc>,
}

/// Builds the typed provenance for one experiment-derived proposal.
///
/// Every level the plan requires is emitted as a row: the run, each completed
/// trial, the run's score run, the run's session when it had one, and each draft
/// artifact revision the proposal points at. All of these already appeared in
/// the candidate payload as uuid strings; the difference is that these can be
/// joined, tenant-checked, and reached in reverse by an erasure.
fn experiment_candidate_sources(
    evidence: &ExperimentLearningProposalEvidence<'_>,
) -> Vec<LearningCandidateSourceRef> {
    let mut sources = vec![
        LearningCandidateSourceRef::ExperimentRun {
            run_uid: evidence.run.run_uid,
        },
        LearningCandidateSourceRef::ScoreRun {
            run_id: evidence.run.score_run_id,
        },
    ];
    if let Some(session_id) = evidence.run.session_id {
        sources.push(LearningCandidateSourceRef::Session { session_id });
    }
    for trial in evidence.completed_trials {
        sources.push(LearningCandidateSourceRef::ExperimentTrial {
            trial_uid: trial.trial_uid,
        });
    }
    for revision_uid in evidence.draft_artifact_revision_uids {
        sources.push(LearningCandidateSourceRef::ArtifactRevision {
            revision_uid: *revision_uid,
        });
    }
    sources
}

/// Builds a proposed learning candidate from completed experiment evidence.
#[must_use]
pub fn build_experiment_learning_candidate(
    evidence: ExperimentLearningProposalEvidence<'_>,
) -> LearningCandidate {
    let candidate_id = deterministic_candidate_id(
        evidence.tenant_id,
        evidence.run.run_uid,
        evidence.idempotency_key.unwrap_or("default"),
    );
    let payload = experiment_learning_candidate_payload(&evidence);

    LearningCandidate {
        id: candidate_id,
        tenant_id: evidence.tenant_id,
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        // An experiment proposal names draft revisions but has no accept path
        // that publishes one: `no_automatic_artifact_publish` is its own
        // promotion requirement. It is therefore authoring work a human picks
        // up, not a reviewable draft, and writing it as `Proposed` is what put
        // it on the same queue as candidates that can actually be accepted.
        proposal_kind: LearningProposalKind::SkillAuthoring,
        status: LearningProposalKind::SkillAuthoring.initial_status(),
        target_id: Some(format!("experiment_run:{}", evidence.run.run_uid)),
        target_label: Some(format!("Experiment proposal for {}", evidence.run.name)),
        task_fingerprint: None,
        task_facets: None,
        payload,
        evaluation_payload: None,
        // Run, trials, score run, session, and every draft revision this
        // proposal points at, as rows. This candidate previously stood on
        // nothing at all: an empty provenance array beside a payload full of
        // uuids that no join could follow.
        sources: experiment_candidate_sources(&evidence),
        confidence: None,
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec![
            "human_review".to_string(),
            "explicit_candidate_evaluation".to_string(),
            "no_automatic_artifact_publish".to_string(),
        ],
        status_reason: Some(
            "proposed from completed experiment evidence; no promotion performed".to_string(),
        ),
        batch_id: None,
        created_at: evidence.now,
        updated_at: evidence.now,
    }
}

/// Returns the tenant override scope used by behavior-lab runs.
#[must_use]
pub fn tenant_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

#[cfg(test)]
fn tenant_id_from_str(value: &str) -> TenantId {
    uuid::Uuid::parse_str(value)
        .map(TenantId::from)
        .unwrap_or_else(|_| {
            let hash = blake3::hash(value.as_bytes());
            let mut bytes = [0_u8; 16];
            bytes.copy_from_slice(&hash.as_bytes()[..16]);
            bytes[6] = (bytes[6] & 0x0f) | 0x80;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            TenantId::from(uuid::Uuid::from_bytes(bytes))
        })
}

#[derive(Debug, Clone)]
struct ExperimentRunInputs {
    target: ExperimentTarget,
    variant: ExperimentVariant,
    scorecard: ExperimentScorecard,
    artifact_revision_uids: Vec<Uuid>,
    plan_revision_uid: Option<Uuid>,
    plan_artifact_uid: Option<Uuid>,
    expected_trials: u64,
    resource_envelope: ExperimentResourceEnvelope,
    simulator_policy: Option<crate::simulator_policy::registry::ResolvedSimulatorPolicy>,
}

/// Longest a behavior-lab run may stay live before its envelope expires.
///
/// The parent plan workflow already refuses to wait longer than this for child
/// progress, so a run that outlives it is stuck rather than working, and every
/// further reservation it makes is spend with nothing to show for it.
const EXPERIMENT_RUN_DEADLINE_HOURS: i64 = 24;

/// Worst-case model calls one simulated turn may issue.
///
/// One call generates the simulator message. The target's worst case includes
/// input guardrail, routing, response loop, output guardrail, and assessment.
const EXPERIMENT_MODEL_CALLS_PER_TURN: u64 = 6;

/// Worst-case governed tool calls one simulated turn may issue.
///
/// A target turn can fan out into tool and sandbox work; this is the ceiling one
/// turn is allowed to reserve against, not an expectation.
const EXPERIMENT_TOOL_CALLS_PER_TURN: u64 = 16;

/// Turns a run without a plan is allowed to reserve for.
const EXPERIMENT_DIRECT_RUN_TURNS: u64 = 16;

/// Derives the durable envelope a plan-backed run executes inside.
///
/// Cost comes from the plan's own declared budget, converted from authored cents
/// into the integer micro-USD the runtime ledger meters. Token, turn, model-call
/// and tool-call ceilings are projected from the bounded matrix shape with
/// checked arithmetic, because a wrapped projection would understate the budget
/// and admit unbounded paid work.
fn plan_resource_envelope(
    definition: &moa_artifacts::simulation::ExperimentPlanDefinition,
    shape: &PlanMatrixShape,
    now: chrono::DateTime<Utc>,
) -> Result<ExperimentResourceEnvelope> {
    let trials = u64::from(shape.total_trials).max(1);
    let trial_turns = u64::from(
        definition
            .simulation
            .scenarios
            .iter()
            .map(|scenario| scenario.max_turns)
            .max()
            .unwrap_or(0),
    )
    .max(1);

    let trial_cost_micro_usd = u64::from(
        definition
            .budget
            .max_trial_cents
            .unwrap_or(definition.budget.max_total_cents),
    )
    .saturating_mul(MICRO_USD_PER_CENT);
    let trial_tokens = u64::from(
        definition
            .budget
            .max_trial_tokens
            .unwrap_or(definition.budget.max_total_tokens.unwrap_or(u32::MAX)),
    );
    let trial_limits = ResourceAmounts {
        cost_micro_usd: trial_cost_micro_usd,
        tokens: trial_tokens,
        turns: trial_turns,
        model_calls: trial_turns.saturating_mul(EXPERIMENT_MODEL_CALLS_PER_TURN),
        tool_calls: trial_turns.saturating_mul(EXPERIMENT_TOOL_CALLS_PER_TURN),
    };

    // The run total is the plan's declared budget, never the per-trial ceiling
    // multiplied out: a plan that declares a small total must not gain a larger
    // one by declaring many trials.
    let run_cost_micro_usd =
        u64::from(definition.budget.max_total_cents).saturating_mul(MICRO_USD_PER_CENT);
    let run_tokens = u64::from(definition.budget.max_total_tokens.unwrap_or(u32::MAX));
    let run_limits = trial_limits
        .checked_mul(trials)
        .map(|projected| ResourceAmounts {
            cost_micro_usd: run_cost_micro_usd,
            tokens: run_tokens,
            ..projected
        })
        .ok_or_else(|| {
            bad_request("experiment plan resource projection overflowed before admission")
        })?;

    Ok(ExperimentResourceEnvelope::new(
        run_limits,
        trial_limits,
        now + chrono::Duration::hours(EXPERIMENT_RUN_DEADLINE_HOURS),
    ))
}

/// Derives the envelope for a run admitted without a plan.
///
/// A direct run is one target with no declared budget of its own, so it inherits
/// the platform's per-trial ceilings. That is deliberately the smaller envelope:
/// an unbudgeted run should be the cheapest thing the platform will run, not the
/// most expensive.
fn direct_resource_envelope(now: chrono::DateTime<Utc>) -> ExperimentResourceEnvelope {
    let limits = ResourceAmounts {
        cost_micro_usd: u64::from(MAX_PLAN_TRIAL_COST_CENTS).saturating_mul(MICRO_USD_PER_CENT),
        tokens: u64::from(MAX_PLAN_TRIAL_TOKENS),
        turns: EXPERIMENT_DIRECT_RUN_TURNS,
        model_calls: EXPERIMENT_DIRECT_RUN_TURNS.saturating_mul(EXPERIMENT_MODEL_CALLS_PER_TURN),
        tool_calls: EXPERIMENT_DIRECT_RUN_TURNS.saturating_mul(EXPERIMENT_TOOL_CALLS_PER_TURN),
    };
    ExperimentResourceEnvelope::new(
        limits,
        limits,
        now + chrono::Duration::hours(EXPERIMENT_RUN_DEADLINE_HOURS),
    )
}

fn single_target_run_inputs(
    target: Option<Value>,
    variant: Option<Value>,
    scorecard: Option<ExperimentScorecard>,
) -> Result<ExperimentRunInputs> {
    let target = parse_payload::<ExperimentTarget>(
        "target",
        target.ok_or_else(|| bad_request("experiment target is required without a plan"))?,
    )?;
    let variant = parse_payload::<ExperimentVariant>(
        "variant",
        variant.ok_or_else(|| bad_request("experiment variant is required without a plan"))?,
    )?;
    let scorecard =
        scorecard.ok_or_else(|| bad_request("experiment scorecard is required without a plan"))?;
    require_runnable_scorecard(&scorecard).map_err(|error| {
        bad_request(format!(
            "experiment scorecard is not runnable in this build: {error}"
        ))
    })?;
    let mut artifact_revision_uids = variant.artifact_revision_uids.clone();
    validate_target_variant(&target, &variant, &mut artifact_revision_uids)?;
    Ok(ExperimentRunInputs {
        artifact_revision_uids,
        target,
        variant,
        scorecard,
        plan_revision_uid: None,
        plan_artifact_uid: None,
        // A direct run drives exactly one target and mints no trial rows, so it
        // adds a run slot and no trial load.
        expected_trials: 0,
        resource_envelope: direct_resource_envelope(Utc::now()),
        simulator_policy: None,
    })
}

fn validate_target_variant(
    target: &ExperimentTarget,
    variant: &ExperimentVariant,
    artifact_revision_uids: &mut Vec<Uuid>,
) -> Result<()> {
    match target {
        ExperimentTarget::AgentLoop { .. } => {
            if variant.execution_template.is_some() {
                return Err(bad_request(
                    "agent-loop experiment variants cannot pin an execution template",
                ));
            }
        }
        ExperimentTarget::ExecutionTemplate {
            template,
            objective,
            ..
        } => {
            if objective.trim().is_empty() {
                return Err(bad_request(
                    "execution-template experiment objective must not be empty",
                ));
            }
            let parsed = ArtifactRef::from_str(&template.skill_ref).map_err(bad_request_from)?;
            let canonical = parsed.canonical_string().map_err(bad_request_from)?;
            if canonical != template.skill_ref {
                return Err(bad_request(
                    "execution-template experiment skill_ref must be canonical",
                ));
            }
            if variant.execution_template.as_ref() != Some(template) {
                return Err(bad_request(
                    "execution-template target and variant must pin the same exact revision",
                ));
            }
            artifact_revision_uids.push(template.revision_uid);
            artifact_revision_uids.sort_unstable();
            artifact_revision_uids.dedup();
        }
    }
    Ok(())
}

async fn plan_run_inputs(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    scope: &ActionRuleScope,
    plan_revision_uid: Uuid,
    run_name: &str,
    agent_revision_variants: &[AgentRevisionSimulationVariant],
    release_evaluation: Option<&ArtifactReleaseExperimentBinding>,
) -> Result<ExperimentRunInputs> {
    let plan = load_published_plan_revision(pool.clone(), scope, plan_revision_uid).await?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan.document.definition else {
        return Err(bad_request(
            "plan revision must contain an experiment_plan definition",
        ));
    };
    let now = Utc::now();
    let simulator_policy = SimulatorPolicyStore::new(pool.clone())
        .resolve_policy(tenant_id, definition.simulator_policy, now)
        .await
        .map_err(bad_request_from)?;
    let shape = plan_matrix_shape_for_release(definition, release_evaluation)?;
    let resource_envelope = plan_resource_envelope(definition, &shape, now)?;
    let mut projection = project_plan_run(definition, plan_revision_uid, &plan.name, run_name)
        .map_err(bad_request_from)?;
    if !agent_revision_variants.is_empty() {
        let variants = serde_json::to_value(agent_revision_variants)
            .map_err(|error| serialization_error(error.to_string()))?;
        if let Some(metadata) = projection.variant.metadata.as_object_mut() {
            metadata.insert("agent_revision_variants".to_string(), variants);
        }
        projection.artifact_revision_uids.extend(
            agent_revision_variants
                .iter()
                .map(|variant| variant.revision_uid),
        );
        projection.artifact_revision_uids.sort_unstable();
        projection.artifact_revision_uids.dedup();
    }
    if let Some(binding) = release_evaluation {
        projection
            .artifact_revision_uids
            .extend(binding.arms.iter().map(|arm| arm.revision_uid));
        projection.artifact_revision_uids.sort_unstable();
        projection.artifact_revision_uids.dedup();
    }
    Ok(ExperimentRunInputs {
        target: projection.target,
        variant: projection.variant,
        scorecard: projection.scorecard,
        artifact_revision_uids: projection.artifact_revision_uids,
        plan_revision_uid: Some(projection.plan_revision_uid),
        plan_artifact_uid: Some(plan.artifact_uid),
        expected_trials: u64::from(shape.total_trials),
        resource_envelope,
        simulator_policy: Some(simulator_policy),
    })
}

fn plan_matrix_shape_for_release(
    definition: &moa_artifacts::simulation::ExperimentPlanDefinition,
    release_evaluation: Option<&ArtifactReleaseExperimentBinding>,
) -> Result<PlanMatrixShape> {
    let Some(binding) = release_evaluation else {
        return plan_matrix_shape(definition).map_err(bad_request_from);
    };
    if binding.arms.is_empty() {
        return Err(bad_request(
            "artifact release experiment must declare at least one arm",
        ));
    }
    let template = definition
        .target_variants
        .first()
        .ok_or_else(|| bad_request("artifact release experiment plan has no target variant"))?;
    let mut effective = definition.clone();
    let mut variants = Vec::with_capacity(binding.arms.len() + 1);
    if !binding
        .arms
        .iter()
        .any(|arm| arm.variant_key == moa_wire::experiments::ARTIFACT_RELEASE_BASELINE_VARIANT_KEY)
    {
        let mut control = template.clone();
        control.key = moa_wire::experiments::ARTIFACT_RELEASE_BASELINE_VARIANT_KEY.to_string();
        variants.push(control);
    }
    variants.extend(binding.arms.iter().map(|arm| {
        moa_artifacts::simulation::ExperimentTargetVariant {
            key: arm.variant_key.clone(),
            kind: template.kind,
            config: template.config.clone(),
            ui: template.ui.clone(),
        }
    }));
    effective.target_variants = variants;
    let cases = binding
        .cases
        .iter()
        .map(|case| PlanCaseSelection {
            scenario_id: case.scenario_id.clone(),
            persona_id: case.persona_id.clone(),
            profile_id: case.profile_id.clone(),
            repetitions: case.repetitions,
        })
        .collect::<Vec<_>>();
    selected_plan_matrix_shape(&effective, &cases).map_err(bad_request_from)
}

async fn load_published_plan_revision(
    pool: sqlx::PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let revision = ArtifactRegistry::new(pool)
        .load_revision(scope, revision_uid)
        .await?
        .ok_or_else(|| bad_request(format!("experiment plan revision {revision_uid} not found")))?;
    if revision.kind != ArtifactKind::ExperimentPlan {
        return Err(bad_request(format!(
            "artifact revision {revision_uid} has kind {}, expected experiment_plan",
            revision.kind
        )));
    }
    if revision.status != ArtifactStatus::Published {
        return Err(bad_request(format!(
            "experiment plan revision {revision_uid} must be published before execution"
        )));
    }
    Ok(revision)
}

async fn load_required_run(
    store: &ExperimentStore,
    scope: &ActionRuleScope,
    run_uid: Uuid,
) -> Result<ExperimentRunRecord> {
    store
        .load_run(scope, run_uid)
        .await?
        .ok_or_else(|| run_not_found(run_uid))
}

fn require_completed_run(run: &ExperimentRunRecord) -> Result<()> {
    if run.status != ExperimentRunStatus::Completed {
        return Err(bad_request(format!(
            "experiment run {} must be completed before proposing improvements",
            run.run_uid
        )));
    }
    Ok(())
}

async fn require_proposal_enabled_plan(
    pool: sqlx::PgPool,
    scope: &ActionRuleScope,
    run: &ExperimentRunRecord,
) -> Result<Uuid> {
    let registry = ArtifactRegistry::new(pool);
    let plan_revision_uid = experiment_plan_revision_uid(&registry, scope, run).await?;
    let plan = registry
        .load_revision(scope, plan_revision_uid)
        .await?
        .ok_or_else(|| {
            bad_request(format!(
                "experiment plan revision {plan_revision_uid} is not visible"
            ))
        })?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan.document.definition else {
        return Err(bad_request(format!(
            "artifact revision {plan_revision_uid} is not an experiment_plan"
        )));
    };
    if !definition.learning_proposals.enabled {
        return Err(bad_request(format!(
            "experiment plan revision {plan_revision_uid} has learning_proposals.enabled=false"
        )));
    }
    Ok(plan_revision_uid)
}

async fn experiment_plan_revision_uid(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    run: &ExperimentRunRecord,
) -> Result<Uuid> {
    if let Some(plan_revision_uid) = run
        .variant
        .metadata
        .get("plan_revision_uid")
        .and_then(Value::as_str)
        .map(|value| {
            Uuid::parse_str(value).map_err(|error| {
                bad_request(format!(
                    "experiment run {} has invalid plan_revision_uid metadata: {error}",
                    run.run_uid
                ))
            })
        })
        .transpose()?
    {
        return Ok(plan_revision_uid);
    }

    for revision_uid in &run.artifact_revision_uids {
        let Some(revision) = registry.load_revision(scope, *revision_uid).await? else {
            continue;
        };
        if revision.kind == ArtifactKind::ExperimentPlan {
            return Ok(*revision_uid);
        }
    }

    Err(bad_request(
        "experiment learning proposals require a proposal-enabled experiment plan revision",
    ))
}

/// One trial's scorecard assessment with the plan coordinates that group it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialScorecardAssessment {
    /// Trial the assessment belongs to.
    pub trial_uid: Uuid,
    /// Score run the trial writes into.
    pub score_run_id: Uuid,
    /// Deterministic trial key.
    pub trial_key: String,
    /// Variant key the trial ran.
    pub variant_key: String,
    /// Scenario the trial ran, when the plan named one.
    pub scenario_id: Option<String>,
    /// Simulated persona used by the modeled case, when available.
    pub persona_id: Option<String>,
    /// Simulation profile used by the modeled case, when available.
    pub profile_id: Option<String>,
    /// Assessment computed from this trial's exact score rows.
    pub assessment: ScorecardAssessment,
}

/// Run, scenario, variant, and per-trial scorecards for one experiment run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentRunScorecards {
    /// Run-level rollup across every trial.
    pub run: ScorecardGroupRollup,
    /// Per-scenario rollups, ordered by scenario ID.
    pub scenarios: Vec<ScorecardGroupRollup>,
    /// Per-variant rollups, ordered by variant key.
    pub variants: Vec<ScorecardGroupRollup>,
    /// Per-trial assessments in trial order.
    pub trials: Vec<TrialScorecardAssessment>,
}

/// Computes run, scenario, and variant scorecards from exact trial score rows.
///
/// Every level is derived from the same per-trial assessments, so a scenario can
/// never look healthier than the trials inside it, and the run can never look
/// healthier than its worst scenario.
async fn experiment_run_scorecards(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    run: &ExperimentRunRecord,
    trials: &[ExperimentTrialRecord],
) -> Result<ExperimentRunScorecards> {
    let rows = exact_experiment_run_score_rows_for_tenant(
        pool,
        ExperimentRunScoreRowsRef {
            tenant_id,
            experiment_run_uid: run.run_uid,
        },
    )
    .await?;
    let mut by_trial: BTreeMap<Uuid, Vec<ExperimentScoreRow>> = BTreeMap::new();
    for row in rows {
        by_trial.entry(row.trial_uid).or_default().push(row);
    }

    let mut assessments = Vec::with_capacity(trials.len());
    for trial in trials {
        let Some(target) = trial_score_target(trial) else {
            // A trial with neither a session nor an execution run never reached a
            // target, so nothing could have observed it. That is Incomplete, not
            // a reason to skip the trial and let the run look eligible.
            assessments.push(TrialScorecardAssessment {
                trial_uid: trial.trial_uid,
                score_run_id: trial.score_run_id,
                trial_key: trial.trial_key.clone(),
                variant_key: trial.variant_key.clone(),
                scenario_id: trial.scenario_id.clone(),
                persona_id: trial.persona_id.clone(),
                profile_id: trial.profile_id.clone(),
                assessment: ScorecardAssessment {
                    eligibility: ScorecardEligibility::Incomplete,
                    findings: vec![ScorecardFinding {
                        score_name: "*".to_string(),
                        detail: "trial has no target session or execution run".to_string(),
                    }],
                },
            });
            continue;
        };
        let trial_rows = by_trial
            .get(&trial.trial_uid)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let expectation = ScorecardExpectation {
            score_run_id: trial.score_run_id,
            experiment_run_uid: run.run_uid,
            // The trial row is the authority for which pinned plan revision it
            // ran; V000361's composite foreign key already refuses provenance
            // that disagrees, and this re-checks it on the read path.
            plan_revision_uid: trial.plan_revision_uid,
            trial_uid: trial.trial_uid,
            target,
            // This independent trial-ledger value prevents a score row from
            // supplying both the evidence claim and the value used to verify it.
            evidence_hash: trial.final_evidence_hash.clone().unwrap_or_default(),
        };
        assessments.push(TrialScorecardAssessment {
            trial_uid: trial.trial_uid,
            score_run_id: trial.score_run_id,
            trial_key: trial.trial_key.clone(),
            variant_key: trial.variant_key.clone(),
            scenario_id: trial.scenario_id.clone(),
            persona_id: trial.persona_id.clone(),
            profile_id: trial.profile_id.clone(),
            assessment: assess_trial_scorecard(&run.scorecard, &expectation, trial_rows),
        });
    }

    let all = assessments.iter().collect::<Vec<_>>();
    Ok(ExperimentRunScorecards {
        run: roll_up_assessments(run.run_uid.to_string(), &all),
        scenarios: grouped_rollups(&assessments, |entry| {
            entry
                .scenario_id
                .clone()
                .unwrap_or_else(|| "<none>".to_string())
        }),
        variants: grouped_rollups(&assessments, |entry| entry.variant_key.clone()),
        trials: assessments,
    })
}

fn grouped_rollups(
    assessments: &[TrialScorecardAssessment],
    key: impl Fn(&TrialScorecardAssessment) -> String,
) -> Vec<ScorecardGroupRollup> {
    let mut grouped: BTreeMap<String, Vec<&TrialScorecardAssessment>> = BTreeMap::new();
    for assessment in assessments {
        grouped.entry(key(assessment)).or_default().push(assessment);
    }
    grouped
        .into_iter()
        .map(|(key, assessments)| roll_up_assessments(key, &assessments))
        .collect()
}

fn roll_up_assessments(
    key: impl Into<String>,
    assessments: &[&TrialScorecardAssessment],
) -> ScorecardGroupRollup {
    let eligibilities = assessments
        .iter()
        .map(|entry| entry.assessment.eligibility)
        .collect::<Vec<_>>();
    roll_up_group(key, &eligibilities, support_for_assessments(assessments))
}

fn support_for_assessments(assessments: &[&TrialScorecardAssessment]) -> ScorecardSupportSummary {
    let mut cases = BTreeSet::new();
    let mut identity_unavailable = false;
    for assessment in assessments {
        let identity = (
            nonblank_id(assessment.scenario_id.as_deref()),
            nonblank_id(assessment.persona_id.as_deref()),
            nonblank_id(assessment.profile_id.as_deref()),
        );
        if let (Some(scenario_id), Some(persona_id), Some(profile_id)) = identity {
            cases.insert((scenario_id, persona_id, profile_id));
        } else {
            identity_unavailable = true;
        }
    }
    let required = crate::eligibility::group_support_floor();
    if identity_unavailable {
        return ScorecardSupportSummary::case_identity_unavailable(cases.len(), required);
    }
    ScorecardSupportSummary::from_counts(cases.len(), required)
}

fn nonblank_id(value: Option<&str>) -> Option<&str> {
    value.filter(|id| !id.trim().is_empty())
}

fn trial_score_target(trial: &ExperimentTrialRecord) -> Option<TrialScoreTarget> {
    match trial.target_kind {
        ExperimentTargetKind::ExecutionTemplate => trial
            .execution_run_uid
            .map(|execution_run_uid| TrialScoreTarget::ExecutionRun { execution_run_uid }),
        ExperimentTargetKind::AgentLoop => trial
            .session_id
            .map(|session_id| TrialScoreTarget::Session { session_id }),
    }
}

/// Refuses a proposal whose trials did not produce complete, passing evidence.
///
/// This replaces an any-row-per-trial check. One arbitrary score row attached to
/// a trial used to satisfy that check; now every blocking requirement in the
/// pinned scorecard needs exactly one correct, passing, provenance-backed row.
fn require_eligible_trial_scorecards(scorecards: &ExperimentRunScorecards) -> Result<()> {
    if scorecards.run.eligibility != ScorecardEligibility::Eligible {
        return Err(bad_request(format!(
            "experiment learning proposals require an eligible scorecard; run scorecard is {}",
            scorecards.run.eligibility.as_str()
        )));
    }
    Ok(())
}

fn experiment_learning_candidate_payload(
    evidence: &ExperimentLearningProposalEvidence<'_>,
) -> Value {
    let run = evidence.run;
    serde_json::json!({
        "kind": "experiment_learning_proposal",
        "source": "Experiments/propose_improvements",
        "tenant_id": evidence.tenant_id,
        "experiment": {
            "run_uid": run.run_uid,
            "name": run.name,
            "status": run.status.as_str(),
            "target_kind": run.target_kind.as_str(),
            "run_score_run_id": run.score_run_id,
            "session_id": run.session_id,
            "execution_run_uid": run.execution_run_uid,
            "artifact_revision_uids": run.artifact_revision_uids,
            "variant": {
                "name": run.variant.name,
                "artifact_revision_uids": run.variant.artifact_revision_uids,
                "skill_refs": run.variant.skill_refs,
                "execution_template": run.variant.execution_template,
                "metadata": run.variant.metadata,
            },
        },
        "evidence_refs": {
            "experiment_run_uid": run.run_uid,
            "run_score_run_id": run.score_run_id,
            "plan_revision_uid": evidence.plan_revision_uid,
            "trial_uids": evidence.completed_trials.iter().map(|trial| trial.trial_uid).collect::<Vec<_>>(),
            "trial_score_run_ids": evidence.completed_trials.iter().map(|trial| trial.score_run_id).collect::<Vec<_>>(),
            "session_ids": evidence.completed_trials.iter().filter_map(|trial| trial.session_id).collect::<Vec<_>>(),
            "execution_run_uids": evidence.completed_trials.iter().filter_map(|trial| trial.execution_run_uid).collect::<Vec<_>>(),
            "artifact_revision_refs": artifact_revision_refs(run, evidence.completed_trials, evidence.plan_revision_uid, evidence.draft_artifact_revision_uids),
        },
        "trials": evidence.completed_trials.iter().map(trial_evidence_payload).collect::<Vec<_>>(),
        "scores": {
            "scorecard": {
                "run": scorecard_rollup_payload(&evidence.scorecards.run),
                "scenarios": evidence.scorecards.scenarios.iter().map(scorecard_rollup_payload).collect::<Vec<_>>(),
                "variants": evidence.scorecards.variants.iter().map(scorecard_rollup_payload).collect::<Vec<_>>(),
                "trials": evidence.scorecards.trials.iter().map(trial_scorecard_payload).collect::<Vec<_>>(),
            },
            "trial_rollup": evidence.trial_rollup_rows.iter().map(score_row_payload).collect::<Vec<_>>(),
            "trials": evidence.trial_score_summaries.iter().map(trial_score_payload).collect::<Vec<_>>(),
            "scenarios": evidence.scenario_score_summaries.iter().map(scenario_score_payload).collect::<Vec<_>>(),
        },
        "suggested_changes": {
            "draft_artifact_revision_uids": evidence.draft_artifact_revision_uids,
            "note": "No draft artifacts were created because this proposal path has experiment evidence but no existing suggested artifact patch payload to store as a meaningful draft.",
        },
    })
}

fn artifact_revision_refs(
    run: &ExperimentRunRecord,
    completed_trials: &[ExperimentTrialRecord],
    plan_revision_uid: Uuid,
    draft_artifact_revision_uids: &[Uuid],
) -> Value {
    serde_json::json!({
        "plan_revision_uid": plan_revision_uid,
        "run_artifact_revision_uids": run.artifact_revision_uids,
        "variant_artifact_revision_uids": run.variant.artifact_revision_uids,
        "scenario_ids": completed_trials.iter().filter_map(|trial| trial.scenario_id.clone()).collect::<Vec<_>>(),
        "persona_ids": completed_trials.iter().filter_map(|trial| trial.persona_id.clone()).collect::<Vec<_>>(),
        "profile_ids": completed_trials.iter().filter_map(|trial| trial.profile_id.clone()).collect::<Vec<_>>(),
        "data_bundle_ids": completed_trials.iter().flat_map(|trial| trial.data_bundle_ids.iter().cloned()).collect::<Vec<_>>(),
        "trial_artifact_revision_uids": completed_trials.iter().flat_map(|trial| trial.artifact_revision_uids.iter().copied()).collect::<Vec<_>>(),
        "draft_artifact_revision_uids": draft_artifact_revision_uids,
    })
}

fn trial_evidence_payload(trial: &ExperimentTrialRecord) -> Value {
    serde_json::json!({
        "trial_uid": trial.trial_uid,
        "trial_key": trial.trial_key.clone(),
        "status": trial.status.as_str(),
        "target_kind": trial.target_kind.as_str(),
        "variant_key": trial.variant_key.clone(),
        "scenario_id": trial.scenario_id.clone(),
        "persona_id": trial.persona_id.clone(),
        "profile_id": trial.profile_id.clone(),
        "data_bundle_ids": trial.data_bundle_ids.clone(),
        "artifact_revision_uids": trial.artifact_revision_uids.clone(),
        "session_id": trial.session_id,
        "execution_run_uid": trial.execution_run_uid,
        "score_run_id": trial.score_run_id,
        "turn_count": trial.turn_count,
        "stop_reason": trial.stop_reason.map(|reason| reason.as_str()),
        "trace_id": trial.trace_id.clone(),
    })
}

fn scorecard_rollup_payload(rollup: &ScorecardGroupRollup) -> Value {
    serde_json::json!({
        "key": rollup.key,
        "eligibility": rollup.eligibility.as_str(),
        "trials": rollup.trials,
        "support": rollup.support,
    })
}

fn trial_scorecard_payload(assessment: &TrialScorecardAssessment) -> Value {
    serde_json::json!({
        "trial_uid": assessment.trial_uid,
        "trial_key": assessment.trial_key,
        "variant_key": assessment.variant_key,
        "scenario_id": assessment.scenario_id,
        "eligibility": assessment.assessment.eligibility.as_str(),
        "findings": assessment.assessment.findings.iter().map(|finding| {
            serde_json::json!({ "score_name": finding.score_name, "detail": finding.detail })
        }).collect::<Vec<_>>(),
    })
}

fn score_row_payload(row: &ScoreSummaryRow) -> Value {
    serde_json::json!({
        "name": row.name,
        "value_type": row.value_type,
        "n": row.n,
        "mean_or_rate": row.mean_or_rate,
    })
}

fn trial_score_payload(trial: &TrialScoreSummary) -> Value {
    serde_json::json!({
        "trial_uid": trial.trial_uid,
        "trial_key": trial.trial_key.clone(),
        "score_run_id": trial.score_run_id,
        "variant_key": trial.variant_key.clone(),
        "scenario_id": trial.scenario_id.clone(),
        "rows": trial.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
    })
}

fn scenario_score_payload(scenario: &ScenarioScoreSummary) -> Value {
    serde_json::json!({
        "scenario_id": scenario.scenario_id.clone(),
        "rows": scenario.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
    })
}

fn deterministic_candidate_id(tenant_id: TenantId, run_uid: Uuid, idempotency_key: &str) -> Uuid {
    let digest = blake3::hash(
        format!("experiment_learning_proposal:{tenant_id}:{run_uid}:{idempotency_key}").as_bytes(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn plan_generation_user_prompt(request: &ExperimentGeneratePlanRequest) -> String {
    let refs = if request.artifact_refs.is_empty() {
        "No existing artifact references were supplied.".to_string()
    } else {
        format!(
            "Use these existing artifact references when they fit the requested matrix:\n{}",
            request
                .artifact_refs
                .iter()
                .map(|artifact_ref| format!("- {artifact_ref}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    format!(
        "Plan description:\n{}\n\nPin this exact simulator policy in definition.spec.simulator_policy:\n\
         - policy_uid: {}\n- revision: {}\n\n{}",
        request.description.trim(),
        request.simulator_policy_uid,
        request.simulator_policy_revision,
        refs
    )
}

fn experiment_plan_response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "moa_experiment_plan_artifact",
        "An artifact document whose kind and definition type are both experiment_plan.",
        experiment_plan_response_schema(),
    )
}

fn parse_generated_plan_document(source_text: &str) -> Result<ArtifactDocument> {
    ArtifactDocument::from_json(source_text.trim()).map_err(|error| {
        bad_request(format!(
            "generated experiment plan is not valid artifact JSON: {error}"
        ))
    })
}

fn require_valid_generated_plan(document: &ArtifactDocument) -> Result<()> {
    if document.kind != ArtifactKind::ExperimentPlan {
        return Err(generated_plan_validation_error(
            "generated artifact kind must be experiment_plan",
            &validate_for_status(document, ArtifactStatus::Draft),
        ));
    }

    let report = validate_for_status(document, ArtifactStatus::Draft);
    if !report.is_ok() {
        return Err(generated_plan_validation_error(
            "generated experiment plan failed draft validation",
            &report,
        ));
    }
    Ok(())
}

fn generated_plan_validation_error(message: &str, report: &ValidationReport) -> ExperimentAppError {
    let report = serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string());
    bad_request(format!("{message}: {report}"))
}

fn parse_payload<T>(field: &'static str, value: Value) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
        .map_err(|error| bad_request(format!("invalid experiment {field}: {error}")))
}

fn serialized_payload<T>(field: &'static str, value: &T) -> Result<Value>
where
    T: Serialize,
{
    serde_json::to_value(value).map_err(|error| {
        serialization_error(format!("serialize experiment {field} failed: {error}"))
    })
}

fn parse_status(status: &str) -> Result<ExperimentRunStatus> {
    ExperimentRunStatus::from_db(status)
        .ok_or_else(|| bad_request(format!("invalid experiment status `{status}`")))
}

fn parse_trial_status(status: &str) -> Result<ExperimentTrialStatus> {
    ExperimentTrialStatus::from_db(status)
        .ok_or_else(|| bad_request(format!("invalid experiment trial status `{status}`")))
}

fn identity_payload(identity: Identity) -> Result<Value> {
    serde_json::to_value(identity)
        .map_err(|error| serialization_error(format!("serialize identity failed: {error}")))
}

fn run_response_from_record(
    tenant_id: TenantId,
    run: &ExperimentRunRecord,
) -> ExperimentRunResponse {
    ExperimentRunResponse {
        tenant_id,
        run_uid: run.run_uid,
        status: run.status.as_str().to_string(),
        score_run_id: run.score_run_id,
        session_id: run.session_id,
        execution_run_uid: run.execution_run_uid,
    }
}

fn trial_status_response_from_summary(
    summary: ExperimentTrialSummary,
) -> ExperimentTrialStatusResponse {
    ExperimentTrialStatusResponse {
        tenant_id: summary.tenant_id,
        run_uid: summary.run_uid,
        trial_uid: summary.trial_uid,
        status: summary.status,
        target_kind: summary.target_kind,
        trial_key: summary.trial_key,
        variant_key: summary.variant_key,
        scenario_id: summary.scenario_id,
        score_run_id: summary.score_run_id,
        session_id: summary.session_id,
        execution_run_uid: summary.execution_run_uid,
        trace_id: summary.trace_id,
        stop_reason: summary.stop_reason,
        error: summary.error,
        turn_count: summary.turn_count,
    }
}

fn trial_summary_from_record(
    tenant_id: TenantId,
    trial: &ExperimentTrialRecord,
) -> ExperimentTrialSummary {
    ExperimentTrialSummary {
        tenant_id,
        run_uid: trial.run_uid,
        trial_uid: trial.trial_uid,
        status: trial.status.as_str().to_string(),
        target_kind: trial.target_kind.as_str().to_string(),
        trial_key: trial.trial_key.clone(),
        variant_key: trial.variant_key.clone(),
        scenario_id: trial.scenario_id.clone(),
        score_run_id: trial.score_run_id,
        session_id: trial.session_id,
        execution_run_uid: trial.execution_run_uid,
        trace_id: trial.trace_id.clone(),
        stop_reason: trial.stop_reason.map(|reason| reason.as_str().to_string()),
        error: trial.error.clone(),
        turn_count: trial.turn_count,
    }
}

fn experiment_score_summary_row(row: ScoreSummaryRow) -> ExperimentScoreSummaryRow {
    ExperimentScoreSummaryRow {
        name: row.name,
        value_type: row.value_type,
        n: row.n,
        mean_or_rate: row.mean_or_rate,
    }
}

fn experiment_trial_score_summary(
    assessment: &TrialScorecardAssessment,
    summary: Option<&TrialScoreSummary>,
) -> ExperimentTrialScoreSummary {
    ExperimentTrialScoreSummary {
        trial_uid: assessment.trial_uid,
        trial_key: assessment.trial_key.clone(),
        score_run_id: assessment.score_run_id,
        variant_key: assessment.variant_key.clone(),
        scenario_id: assessment.scenario_id.clone(),
        rows: summary
            .map(|summary| {
                summary
                    .rows
                    .iter()
                    .cloned()
                    .map(experiment_score_summary_row)
                    .collect()
            })
            .unwrap_or_default(),
        eligibility: assessment.assessment.eligibility,
        eligibility_findings: assessment.assessment.findings.clone(),
    }
}

fn experiment_scenario_score_summary(row: ScenarioScoreSummary) -> ExperimentScenarioScoreSummary {
    ExperimentScenarioScoreSummary {
        scenario_id: row.scenario_id,
        rows: row
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
    }
}

fn experiment_compare_row(row: ScoreCompareRow) -> ExperimentCompareRow {
    ExperimentCompareRow {
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn experiment_scenario_score_delta_row(
    row: ScenarioScoreDeltaRow,
) -> ExperimentScenarioScoreDeltaRow {
    ExperimentScenarioScoreDeltaRow {
        scenario_id: row.scenario_id,
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn experiment_variant_score_delta_row(row: VariantScoreDeltaRow) -> ExperimentVariantScoreDeltaRow {
    ExperimentVariantScoreDeltaRow {
        variant_key: row.variant_key,
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn run_not_found(run_uid: Uuid) -> ExperimentAppError {
    ExperimentAppError::NotFound(format!("experiment run {run_uid} not found"))
}

fn trial_not_found(trial_uid: Uuid) -> ExperimentAppError {
    ExperimentAppError::NotFound(format!("experiment trial {trial_uid} not found"))
}

fn bad_request(message: impl Into<String>) -> ExperimentAppError {
    ExperimentAppError::BadRequest(message.into())
}

/// Separates an admission refusal from an infrastructure fault on the run-insert
/// path.
///
/// The store decides quotas inside the transaction that inserts the run, so the
/// refusal can only reach the caller as a store error. Every validation error
/// that path can produce is a full quota, and it must not be reported as a
/// server fault a caller would retry into the same full quota.
fn admission_app_error(error: MoaError) -> ExperimentAppError {
    match error {
        MoaError::ValidationError(message) => ExperimentAppError::QuotaExceeded(message),
        other => ExperimentAppError::Moa(other),
    }
}

fn serialization_error(message: impl Into<String>) -> ExperimentAppError {
    ExperimentAppError::Serialization(message.into())
}

/// Maps any displayable upstream error into a `BadRequest` app error.
fn bad_request_from(error: impl std::fmt::Display) -> ExperimentAppError {
    bad_request(error.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_artifacts::simulation::ExperimentTargetKind;
    use moa_core::{
        types::action_policy::ActionRuleScope,
        types::context::MessageRole,
        types::experiments::{ScorecardEffect, ScorecardRequirement, ScorecardValueType},
        types::identifiers::ModelId,
        types::identifiers::SessionId,
    };
    use serde_json::json;

    use super::*;
    use crate::eligibility::group_support_floor;
    use crate::model::{ExperimentSimulatorConfig, ExperimentTrialStopReason};

    #[test]
    fn direct_envelope_prices_every_bounded_model_stage_offline() {
        // Pins: a direct target reserves enough model-call capacity for the same
        // bounded pipeline as a simulated turn, rather than only the visible answer.
        let envelope = direct_resource_envelope(
            Utc.timestamp_opt(1_700_000_000, 0)
                .single()
                .expect("fixed timestamp"),
        );
        assert_eq!(
            envelope.run_limits.model_calls,
            EXPERIMENT_DIRECT_RUN_TURNS.saturating_mul(EXPERIMENT_MODEL_CALLS_PER_TURN)
        );
        assert_eq!(EXPERIMENT_MODEL_CALLS_PER_TURN, 6);
    }

    #[test]
    fn plan_generation_request_keeps_description_out_of_system_prompt() {
        // Pins: behavior-lab generation keeps reusable artifact rules cacheable.
        let request = ExperimentGeneratePlanRequest {
            tenant_id: TenantId::new(),
            description: "Compare the refund agent against a stricter escalation policy."
                .to_string(),
            model: Some("gpt-5.4-mini".to_string()),
            artifact_refs: vec!["agent:refund-baseline@1".to_string()],
            simulator_policy_uid: fixture_uuid(41),
            simulator_policy_revision: 3,
        };

        let completion =
            plan_generation_request(&request).expect("valid plan-generation request should build");

        assert_eq!(completion.messages.len(), 2);
        assert_eq!(completion.messages[0].role, MessageRole::System);
        assert_eq!(completion.messages[1].role, MessageRole::User);
        assert!(
            completion.messages[0]
                .content
                .contains("experiment_plan artifact document")
        );
        assert!(!completion.messages[0].content.contains("refund agent"));
        assert!(
            completion.messages[1]
                .content
                .contains("Compare the refund agent")
        );
        assert!(
            completion.messages[1]
                .content
                .contains("agent:refund-baseline@1")
        );
        assert_eq!(completion.model, Some(ModelId::new("gpt-5.4-mini")));
        assert!(completion.response_format.is_some());
    }

    #[test]
    fn plan_generation_repair_request_keeps_feedback_in_dynamic_user_message() {
        // Pins: invalid structured output receives one actionable repair turn
        // without contaminating the reusable system-prompt cache prefix.
        let request = ExperimentGeneratePlanRequest {
            tenant_id: TenantId::new(),
            description: "Create a greeting experiment.".to_string(),
            model: Some("gpt-5.4-mini".to_string()),
            artifact_refs: Vec::new(),
            simulator_policy_uid: fixture_uuid(41),
            simulator_policy_revision: 3,
        };

        let completion = plan_generation_repair_request(
            &request,
            r#"{"definition":{"spec":{"simulation":{"scenarios":[]}}}}"#,
            "simulation scenario must include at least one goal",
        )
        .expect("repair request should build");

        assert_eq!(completion.messages.len(), 3);
        assert!(!completion.messages[0].content.contains("previous JSON"));
        assert!(completion.messages[2].content.contains("Previous JSON"));
        assert!(
            completion.messages[2]
                .content
                .contains("must include at least one goal")
        );
        assert!(completion.response_format.is_some());
    }

    #[test]
    fn direct_execution_template_target_rejects_blank_objective() {
        // Pins: direct behavior-lab admission cannot bypass the explicit-objective contract.
        let template = moa_core::types::execution_planning::PinnedExecutionTemplateRef {
            skill_ref: "skill://damaged-food-order".to_string(),
            revision_uid: fixture_uuid(77),
        };
        let target = ExperimentTarget::ExecutionTemplate {
            template: template.clone(),
            objective: " \t\n".to_string(),
            input: json!({"order_id": "order-123"}),
            session_id: None,
            idempotency_key: None,
        };
        let variant = ExperimentVariant {
            name: "template".to_string(),
            model: None,
            artifact_revision_uids: vec![template.revision_uid],
            skill_refs: Vec::new(),
            execution_template: Some(template),
            metadata: json!({}),
        };
        let mut artifact_revision_uids = Vec::new();

        let error = validate_target_variant(&target, &variant, &mut artifact_revision_uids)
            .expect_err("blank execution-template objective should reject direct admission");

        assert!(matches!(
            error,
            ExperimentAppError::BadRequest(message)
                if message == "execution-template experiment objective must not be empty"
        ));
    }

    #[test]
    fn scorecard_support_counts_distinct_cases_not_repetitions_offline() {
        // Pins: variant and repetition coordinates create observations, not new
        // independent cases. Only scenario/persona/profile identity may satisfy
        // the shared support floor, and a missing identity fails closed.
        let assessment =
            |index: usize,
             variant_key: String,
             scenario_id: Option<String>,
             persona_id: Option<String>,
             profile_id: Option<String>| TrialScorecardAssessment {
                trial_uid: Uuid::from_u128(index as u128 + 1),
                score_run_id: Uuid::from_u128(100),
                trial_key: format!("trial-{index}"),
                variant_key,
                scenario_id,
                persona_id,
                profile_id,
                assessment: ScorecardAssessment {
                    eligibility: ScorecardEligibility::Eligible,
                    findings: Vec::new(),
                },
            };

        let repetitions = (0..group_support_floor() + 2)
            .map(|index| {
                assessment(
                    index,
                    format!("variant-{index}"),
                    Some("scenario-a".to_string()),
                    Some("persona-a".to_string()),
                    Some("profile-a".to_string()),
                )
            })
            .collect::<Vec<_>>();
        let repetition_refs = repetitions.iter().collect::<Vec<_>>();
        let repetition_support = support_for_assessments(&repetition_refs);
        assert_eq!(repetition_support.independent_units, 1);
        assert_eq!(
            repetition_support.status,
            crate::eligibility::ScorecardSupportStatus::InsufficientIndependentUnits
        );
        let repetition_rollup = roll_up_assessments("repetitions", &repetition_refs);
        assert_eq!(repetition_rollup.trials, repetitions.len());
        assert_eq!(
            repetition_rollup.eligibility,
            ScorecardEligibility::Incomplete
        );

        for (dimension, identities) in [
            (
                "scenario",
                [
                    ("scenario-a", "persona-a", "profile-a"),
                    ("scenario-b", "persona-a", "profile-a"),
                ],
            ),
            (
                "persona",
                [
                    ("scenario-a", "persona-a", "profile-a"),
                    ("scenario-a", "persona-b", "profile-a"),
                ],
            ),
            (
                "profile",
                [
                    ("scenario-a", "persona-a", "profile-a"),
                    ("scenario-a", "persona-a", "profile-b"),
                ],
            ),
        ] {
            let pair = identities
                .into_iter()
                .enumerate()
                .map(|(index, (scenario_id, persona_id, profile_id))| {
                    assessment(
                        index,
                        "candidate".to_string(),
                        Some(scenario_id.to_string()),
                        Some(persona_id.to_string()),
                        Some(profile_id.to_string()),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                support_for_assessments(&pair.iter().collect::<Vec<_>>()).independent_units,
                2,
                "{dimension} is not part of modeled-case identity"
            );
        }

        let distinct_cases = (0..group_support_floor())
            .map(|index| {
                assessment(
                    index,
                    "candidate".to_string(),
                    Some(format!("scenario-{index}")),
                    Some("persona-a".to_string()),
                    Some("profile-a".to_string()),
                )
            })
            .collect::<Vec<_>>();
        let distinct_refs = distinct_cases.iter().collect::<Vec<_>>();
        let distinct_support = support_for_assessments(&distinct_refs);
        assert_eq!(distinct_support.independent_units, group_support_floor());
        assert_eq!(
            distinct_support.status,
            crate::eligibility::ScorecardSupportStatus::Sufficient
        );
        assert_eq!(
            roll_up_assessments("distinct", &distinct_refs).eligibility,
            ScorecardEligibility::Eligible
        );

        let missing_identity = assessment(
            999,
            "candidate".to_string(),
            Some("scenario-a".to_string()),
            Some("persona-a".to_string()),
            Some("  ".to_string()),
        );
        let known_identity = assessment(
            998,
            "candidate".to_string(),
            Some("scenario-a".to_string()),
            Some("persona-a".to_string()),
            Some("profile-a".to_string()),
        );
        let missing_refs = vec![&known_identity, &missing_identity];
        let missing_support = support_for_assessments(&missing_refs);
        assert_eq!(missing_support.independent_units, 1);
        assert_eq!(
            missing_support.status,
            crate::eligibility::ScorecardSupportStatus::CaseIdentityUnavailable
        );
        assert_eq!(
            roll_up_assessments("missing", &missing_refs).eligibility,
            ScorecardEligibility::Incomplete
        );
    }

    #[test]
    fn experiment_proposal_candidate_stays_review_only() {
        // Pins: experiment-derived improvements create proposed candidates without active artifacts.
        let tenant_id = tenant_id_from_str("tenant-a");
        let run = completed_run_record(tenant_id);
        let trials = vec![completed_trial_record(run.run_uid)];
        let summary_rows = vec![ScoreSummaryRow {
            name: "target_completed".to_string(),
            value_type: ScorecardValueType::Boolean,
            n: 1,
            mean_or_rate: Some(1.0),
        }];
        let scorecards = ExperimentRunScorecards {
            run: ScorecardGroupRollup {
                key: run.run_uid.to_string(),
                eligibility: ScorecardEligibility::Eligible,
                trials: 1,
                support: ScorecardSupportSummary::from_counts(
                    group_support_floor(),
                    group_support_floor(),
                ),
            },
            scenarios: Vec::new(),
            variants: Vec::new(),
            trials: vec![TrialScorecardAssessment {
                trial_uid: trials[0].trial_uid,
                score_run_id: trials[0].score_run_id,
                trial_key: trials[0].trial_key.clone(),
                variant_key: trials[0].variant_key.clone(),
                scenario_id: trials[0].scenario_id.clone(),
                persona_id: trials[0].persona_id.clone(),
                profile_id: trials[0].profile_id.clone(),
                assessment: ScorecardAssessment {
                    eligibility: ScorecardEligibility::Eligible,
                    findings: Vec::new(),
                },
            }],
        };
        let trial_score_summary = TrialScoreSummary {
            trial_uid: trials[0].trial_uid,
            trial_key: trials[0].trial_key.clone(),
            score_run_id: trials[0].score_run_id,
            variant_key: trials[0].variant_key.clone(),
            scenario_id: trials[0].scenario_id.clone(),
            rows: summary_rows.clone(),
        };
        let scenario_score_summary = ScenarioScoreSummary {
            scenario_id: trials[0].scenario_id.clone(),
            rows: summary_rows.clone(),
        };

        let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
            tenant_id,
            run: &run,
            completed_trials: &trials,
            scorecards: &scorecards,
            trial_rollup_rows: &summary_rows,
            trial_score_summaries: std::slice::from_ref(&trial_score_summary),
            scenario_score_summaries: std::slice::from_ref(&scenario_score_summary),
            plan_revision_uid: fixture_uuid(20),
            draft_artifact_revision_uids: &[],
            idempotency_key: Some("proposal-key"),
            now: fixture_time(),
        });

        // Behavior Lab proposals are authoring items: they describe work a human
        // would have to do, and there is no materializer that could accept one.
        assert_eq!(
            candidate.status,
            moa_core::types::experience::LearningCandidateStatus::NeedsAuthoring
        );
        assert!(
            !candidate.proposal_kind.is_reviewable(),
            "an experiment proposal must not offer an accept path it cannot honour"
        );
        assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
        assert_eq!(candidate.payload["kind"], "experiment_learning_proposal");
        assert_eq!(
            candidate.promotion_requirements,
            vec![
                "human_review".to_string(),
                "explicit_candidate_evaluation".to_string(),
                "no_automatic_artifact_publish".to_string(),
            ]
        );
        assert_eq!(
            candidate.payload["evidence_refs"]["experiment_run_uid"],
            run.run_uid.to_string()
        );
        assert_eq!(
            candidate.payload["suggested_changes"]["draft_artifact_revision_uids"]
                .as_array()
                .expect("draft list should be an array")
                .len(),
            0,
            "proposal evidence currently has no meaningful artifact patch payload to draft"
        );
    }

    /// Envelope for record fixtures, derived through the production helper so a
    /// fixture cannot drift into a ceiling the platform would never admit.
    fn fixture_resource_envelope() -> ExperimentResourceEnvelope {
        direct_resource_envelope(moa_test_support::fixtures::pg_now())
    }

    fn completed_run_record(tenant_id: TenantId) -> ExperimentRunRecord {
        ExperimentRunRecord {
            scope: ActionRuleScope::Tenant { tenant_id },
            plan_artifact_uid: None,
            resource_envelope: fixture_resource_envelope(),
            simulator_policy: None,
            run_uid: fixture_uuid(1),
            name: "support escalation comparison".to_string(),
            target_kind: ExperimentTargetKind::AgentLoop,
            status: ExperimentRunStatus::Completed,
            target: ExperimentTarget::AgentLoop {
                prompt: "Handle the damaged order.".to_string(),
                agent: None,
                model: ModelId::new("gpt-fixture"),
                attachments: Vec::new(),
            },
            variant: ExperimentVariant {
                name: "candidate".to_string(),
                model: Some(ModelId::new("gpt-fixture")),
                artifact_revision_uids: vec![fixture_uuid(3)],
                skill_refs: vec!["skill://support".to_string()],
                execution_template: None,
                metadata: json!({"plan_revision_uid": fixture_uuid(20)}),
            },
            scorecard: ExperimentScorecard::new(vec![ScorecardRequirement {
                evaluator_id: "target_completed".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Blocking,
            }])
            .expect("fixture scorecard is valid"),
            score_run_id: fixture_uuid(4),
            session_id: Some(SessionId(fixture_uuid(2))),
            execution_run_uid: Some(fixture_uuid(5)),
            artifact_revision_uids: vec![fixture_uuid(20)],
            idempotency_key: Some("run-key".to_string()),
            created_by_identity: json!({"subject": "user:creator"}),
            error: None,
            created_at: fixture_time(),
            started_at: Some(fixture_time()),
            completed_at: Some(fixture_time()),
            updated_at: fixture_time(),
        }
    }

    fn completed_trial_record(run_uid: Uuid) -> ExperimentTrialRecord {
        ExperimentTrialRecord {
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id_from_str("workspace-a"),
            },
            resource_envelope: fixture_resource_envelope().trial_envelope(),
            trial_uid: fixture_uuid(30),
            run_uid,
            trial_key: "scenario/persona/profile/candidate/0".to_string(),
            status: ExperimentTrialStatus::Completed,
            target_kind: ExperimentTargetKind::AgentLoop,
            variant_key: "candidate".to_string(),
            plan_revision_uid: fixture_uuid(20),
            persona_id: Some("persona-a".to_string()),
            profile_id: Some("profile-a".to_string()),
            scenario_id: Some("scenario-a".to_string()),
            data_bundle_ids: vec!["bundle-a".to_string()],
            artifact_revision_uids: vec![fixture_uuid(3)],
            simulator: ExperimentSimulatorConfig {
                policy: crate::simulator_policy::test_support::resolved_policy(),
                max_turns: 6,
                token_budget: Some(1000),
            },
            target_model: Some(ModelId::new("gpt-fixture")),
            seed: Some("seed-a".to_string()),
            session_id: Some(SessionId(fixture_uuid(31))),
            execution_run_uid: Some(fixture_uuid(32)),
            score_run_id: fixture_uuid(33),
            final_evidence_hash: Some(vec![7; 32]),
            turn_count: 3,
            stop_reason: Some(ExperimentTrialStopReason::Success),
            error: None,
            trace_id: Some("trace-fixture".to_string()),
            started_at: Some(fixture_time()),
            completed_at: Some(fixture_time()),
            created_at: fixture_time(),
            updated_at: fixture_time(),
        }
    }

    fn fixture_uuid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    fn fixture_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("valid fixture time")
    }
}
