//! Application boundary for behavior-lab experiment service operations.

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::simulation::experiment_plan_response_schema;
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_core::traits::Identity;
use moa_core::wire::experiments::{
    AgentRevisionSimulationVariant, ExperimentCancelRequest, ExperimentCancelResponse,
    ExperimentCompareRequest, ExperimentCompareResponse, ExperimentCompareRow,
    ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse, ExperimentListRequest,
    ExperimentListResponse, ExperimentProposeImprovementsRequest,
    ExperimentProposeImprovementsResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentScenarioScoreDeltaRow, ExperimentScenarioScoreSummary, ExperimentScoreSummaryRow,
    ExperimentScoresRequest, ExperimentScoresResponse, ExperimentTrialScoreSummary,
    ExperimentTrialStatusRequest, ExperimentTrialStatusResponse, ExperimentTrialSummary,
    ExperimentTrialsRequest, ExperimentTrialsResponse, ExperimentVariantScoreDeltaRow,
};
use moa_core::{
    ActionRuleScope, CompletionRequest, ContextMessage, JsonResponseFormat, LearningCandidate,
    LearningCandidateStatus, LearningCandidateType, LearningRiskClass, MoaError, ModelId, TenantId,
};
use moa_observability::{record_experiment_run, record_experiment_score_rows};
use moa_scoring::{
    ExperimentRunCompareRef, ExperimentRunScoreRef, ScenarioScoreDeltaRow, ScenarioScoreSummary,
    ScoreCompareRef, ScoreCompareRow, ScoreRunRef, ScoreSummary, ScoreSummaryRow, ScoringError,
    TrialScoreSummary, VariantScoreDeltaRow, compare_experiment_score_breakdown_for_tenant,
    compare_score_runs_for_tenant, experiment_score_breakdown_for_tenant,
    score_summaries_for_tenant,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentTarget,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentVariant, NewExperimentRun,
};
use crate::plan::project_plan_run;
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
    /// Experiment application state could not be serialized.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Shared MOA infrastructure failed.
    #[error(transparent)]
    Moa(#[from] MoaError),
    /// Scoring storage or comparison failed.
    #[error(transparent)]
    Scoring(#[from] ScoringError),
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
fields. Include at least one scenario, persona, profile, target variant, simulator_model, \
parallelism, trials_per_combination, and budget.max_total_cents. Use stable snake_case or \
kebab-case names in metadata.name, simulation IDs, and target variant keys.";

/// Builds the structured provider request for behavior-lab plan generation.
pub fn plan_generation_request(
    request: &ExperimentGeneratePlanRequest,
) -> Result<CompletionRequest> {
    if request.description.trim().is_empty() {
        return Err(bad_request("experiment plan description must not be empty"));
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

/// Stores validated provider output as a draft experiment-plan artifact.
pub async fn store_generated_plan(
    pool: sqlx::PgPool,
    request: ExperimentGeneratePlanRequest,
    completion_text: &str,
) -> Result<ExperimentGeneratePlanResponse> {
    let document = parse_generated_plan_document(completion_text)?;
    require_valid_generated_plan(&document)?;
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
    let run_inputs = match request.plan_revision_uid {
        Some(plan_revision_uid) => {
            plan_run_inputs(
                pool.clone(),
                &scope,
                plan_revision_uid,
                &request.name,
                &request.agent_revision_variants,
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
                session_id: session_id_from_target(&run_inputs.target),
                procedure_run_uid: None,
                artifact_revision_uids: run_inputs.artifact_revision_uids.clone(),
                score_run_id,
                target: run_inputs.target,
                variant: run_inputs.variant,
                scorecard: run_inputs.scorecard,
                idempotency_key: request.idempotency_key,
                created_by_identity: identity_payload(identity.clone())?,
            },
        )
        .await?;
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
) -> Result<ExperimentCancelResponse> {
    let scope = tenant_scope(request.tenant_id);
    let reason = request
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "cancelled".to_string());
    let store = ExperimentStore::new(pool);
    let existing = load_required_run(&store, &scope, request.run_uid).await?;
    if existing.status.is_terminal() {
        return Ok(ExperimentCancelResponse {
            tenant_id: request.tenant_id,
            run_uid: existing.run_uid,
            cancelled: false,
            status: existing.status.as_str().to_string(),
            reason,
        });
    }
    let run = store
        .update_run_status(
            &scope,
            request.run_uid,
            ExperimentRunStatus::Cancelled,
            Some(reason.clone()),
            Some(Utc::now()),
        )
        .await?
        .ok_or_else(|| run_not_found(request.run_uid))?;
    store
        .cancel_active_trials(&scope, request.run_uid, reason.clone())
        .await?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());

    Ok(ExperimentCancelResponse {
        tenant_id: request.tenant_id,
        run_uid: run.run_uid,
        cancelled: true,
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
    let run_score_summary = score_summaries_for_tenant(
        &pool,
        ScoreRunRef {
            tenant_id: request.tenant_id,
            run_id: run.score_run_id,
        },
    )
    .await?;
    if run_score_summary.rows.is_empty() {
        return Err(bad_request(
            "experiment learning proposals require score rows for the run score run",
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
    require_trial_score_rows(&completed_trials, &trial_breakdown.trials)?;

    let draft_artifact_revision_uids = Vec::new();
    let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
        tenant_id: request.tenant_id,
        run: &run,
        completed_trials: &completed_trials,
        run_score_summary: &run_score_summary,
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
    let run =
        load_required_run(&ExperimentStore::new(pool.clone()), &scope, request.run_uid).await?;
    let score_response = score_summaries_for_tenant(
        &pool,
        ScoreRunRef {
            tenant_id: request.tenant_id,
            run_id: run.score_run_id,
        },
    )
    .await?;
    record_experiment_score_rows("scores", score_response.rows.len() as u64);
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

    Ok(ExperimentScoresResponse {
        tenant_id: request.tenant_id,
        run_uid: run.run_uid,
        score_run_id: run.score_run_id,
        rows: score_response
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
        trial_rollup_rows: trial_breakdown
            .trial_rollup_rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
        trials: trial_breakdown
            .trials
            .into_iter()
            .map(experiment_trial_score_summary)
            .collect(),
        scenarios: trial_breakdown
            .scenarios
            .into_iter()
            .map(experiment_scenario_score_summary)
            .collect(),
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
    /// Score summary for the experiment run score run.
    pub run_score_summary: &'a ScoreSummary,
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
        status: LearningCandidateStatus::Proposed,
        target_id: Some(format!("experiment_run:{}", evidence.run.run_uid)),
        target_label: Some(format!("Experiment proposal for {}", evidence.run.name)),
        task_fingerprint: None,
        task_facets: None,
        payload,
        evaluation_payload: None,
        source_experience_ids: Vec::new(),
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
}

fn single_target_run_inputs(
    target: Option<Value>,
    variant: Option<Value>,
    scorecard: Value,
) -> Result<ExperimentRunInputs> {
    let target = parse_payload::<ExperimentTarget>(
        "target",
        target.ok_or_else(|| bad_request("experiment target is required without a plan"))?,
    )?;
    let variant = parse_payload::<ExperimentVariant>(
        "variant",
        variant.ok_or_else(|| bad_request("experiment variant is required without a plan"))?,
    )?;
    let scorecard = parse_payload::<ExperimentScorecard>("scorecard", scorecard)?;
    Ok(ExperimentRunInputs {
        artifact_revision_uids: variant.artifact_revision_uids.clone(),
        target,
        variant,
        scorecard,
        plan_revision_uid: None,
    })
}

async fn plan_run_inputs(
    pool: sqlx::PgPool,
    scope: &ActionRuleScope,
    plan_revision_uid: Uuid,
    run_name: &str,
    agent_revision_variants: &[AgentRevisionSimulationVariant],
) -> Result<ExperimentRunInputs> {
    let plan = load_published_plan_revision(pool, scope, plan_revision_uid).await?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan.document.definition else {
        return Err(bad_request(
            "plan revision must contain an experiment_plan definition",
        ));
    };
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
    Ok(ExperimentRunInputs {
        target: projection.target,
        variant: projection.variant,
        scorecard: projection.scorecard,
        artifact_revision_uids: projection.artifact_revision_uids,
        plan_revision_uid: Some(projection.plan_revision_uid),
    })
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

fn require_trial_score_rows(
    completed_trials: &[ExperimentTrialRecord],
    score_trials: &[TrialScoreSummary],
) -> Result<()> {
    let scored_trial_ids = score_trials
        .iter()
        .filter(|trial| !trial.rows.is_empty())
        .map(|trial| trial.trial_uid)
        .collect::<std::collections::BTreeSet<_>>();
    let missing = completed_trials
        .iter()
        .find(|trial| !scored_trial_ids.contains(&trial.trial_uid));
    if let Some(trial) = missing {
        return Err(bad_request(format!(
            "experiment trial {} has no score rows",
            trial.trial_uid
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
            "procedure_run_uid": run.procedure_run_uid,
            "artifact_revision_uids": run.artifact_revision_uids,
            "variant": {
                "name": run.variant.name,
                "artifact_revision_uids": run.variant.artifact_revision_uids,
                "skill_refs": run.variant.skill_refs,
                "procedure_ref": run.variant.procedure_ref,
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
            "procedure_run_uids": evidence.completed_trials.iter().filter_map(|trial| trial.procedure_run_uid).collect::<Vec<_>>(),
            "artifact_revision_refs": artifact_revision_refs(run, evidence.completed_trials, evidence.plan_revision_uid, evidence.draft_artifact_revision_uids),
        },
        "trials": evidence.completed_trials.iter().map(trial_evidence_payload).collect::<Vec<_>>(),
        "scores": {
            "run": evidence.run_score_summary.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
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
        "procedure_run_uid": trial.procedure_run_uid,
        "score_run_id": trial.score_run_id,
        "turn_count": trial.turn_count,
        "stop_reason": trial.stop_reason.map(|reason| reason.as_str()),
        "trace_id": trial.trace_id.clone(),
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
        "Plan description:\n{}\n\n{}",
        request.description.trim(),
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

fn session_id_from_target(target: &ExperimentTarget) -> Option<moa_core::SessionId> {
    match target {
        ExperimentTarget::AgentLoop { session_id, .. }
        | ExperimentTarget::Procedure { session_id, .. } => *session_id,
    }
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
        procedure_run_uid: run.procedure_run_uid,
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
        procedure_run_uid: summary.procedure_run_uid,
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
        procedure_run_uid: trial.procedure_run_uid,
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

fn experiment_trial_score_summary(row: TrialScoreSummary) -> ExperimentTrialScoreSummary {
    ExperimentTrialScoreSummary {
        trial_uid: row.trial_uid,
        trial_key: row.trial_key,
        score_run_id: row.score_run_id,
        variant_key: row.variant_key,
        scenario_id: row.scenario_id,
        rows: row
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
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
    use moa_core::{ActionRuleScope, MessageRole, ModelId, SessionId};
    use serde_json::json;

    use super::*;
    use crate::model::{ExperimentSimulatorConfig, ExperimentTrialStopReason};

    #[test]
    fn plan_generation_request_keeps_description_out_of_system_prompt() {
        // Pins: behavior-lab generation keeps reusable artifact rules cacheable.
        let request = ExperimentGeneratePlanRequest {
            tenant_id: TenantId::new(),
            description: "Compare the refund agent against a stricter escalation policy."
                .to_string(),
            model: Some("gpt-5.4-mini".to_string()),
            artifact_refs: vec!["agent:refund-baseline@1".to_string()],
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
    fn experiment_proposal_candidate_stays_review_only() {
        // Pins: experiment-derived improvements create proposed candidates without active artifacts.
        let tenant_id = tenant_id_from_str("tenant-a");
        let run = completed_run_record(tenant_id);
        let trials = vec![completed_trial_record(run.run_uid)];
        let score_summary = ScoreSummary {
            tenant_id,
            run_id: run.score_run_id,
            rows: vec![ScoreSummaryRow {
                name: "quality".to_string(),
                value_type: "numeric".to_string(),
                n: 1,
                mean_or_rate: Some(0.92),
            }],
        };
        let trial_score_summary = TrialScoreSummary {
            trial_uid: trials[0].trial_uid,
            trial_key: trials[0].trial_key.clone(),
            score_run_id: trials[0].score_run_id,
            variant_key: trials[0].variant_key.clone(),
            scenario_id: trials[0].scenario_id.clone(),
            rows: score_summary.rows.clone(),
        };
        let scenario_score_summary = ScenarioScoreSummary {
            scenario_id: trials[0].scenario_id.clone(),
            rows: score_summary.rows.clone(),
        };

        let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
            tenant_id,
            run: &run,
            completed_trials: &trials,
            run_score_summary: &score_summary,
            trial_rollup_rows: &score_summary.rows,
            trial_score_summaries: std::slice::from_ref(&trial_score_summary),
            scenario_score_summaries: std::slice::from_ref(&scenario_score_summary),
            plan_revision_uid: fixture_uuid(20),
            draft_artifact_revision_uids: &[],
            idempotency_key: Some("proposal-key"),
            now: fixture_time(),
        });

        assert_eq!(candidate.status, LearningCandidateStatus::Proposed);
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

    fn completed_run_record(tenant_id: TenantId) -> ExperimentRunRecord {
        ExperimentRunRecord {
            scope: ActionRuleScope::Tenant { tenant_id },
            run_uid: fixture_uuid(1),
            name: "support escalation comparison".to_string(),
            target_kind: ExperimentTargetKind::AgentLoop,
            status: ExperimentRunStatus::Completed,
            target: ExperimentTarget::AgentLoop {
                prompt: "Handle the damaged order.".to_string(),
                session_id: Some(SessionId(fixture_uuid(2))),
                agent: None,
                model: ModelId::new("gpt-fixture"),
                attachments: Vec::new(),
            },
            variant: ExperimentVariant {
                name: "candidate".to_string(),
                model: Some(ModelId::new("gpt-fixture")),
                artifact_revision_uids: vec![fixture_uuid(3)],
                skill_refs: vec!["skill://support".to_string()],
                procedure_ref: Some("skill://support".to_string()),
                metadata: json!({"plan_revision_uid": fixture_uuid(20)}),
            },
            scorecard: ExperimentScorecard {
                score_names: vec!["quality".to_string()],
                evaluator_metadata: json!({}),
            },
            score_run_id: fixture_uuid(4),
            session_id: Some(SessionId(fixture_uuid(2))),
            procedure_run_uid: Some(fixture_uuid(5)),
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
                model: ModelId::new("gpt-fixture"),
                temperature: Some(0.2),
                max_turns: 6,
                token_budget: Some(1000),
                metadata: json!({}),
            },
            target_model: Some(ModelId::new("gpt-fixture")),
            seed: Some("seed-a".to_string()),
            session_id: Some(SessionId(fixture_uuid(31))),
            procedure_run_uid: Some(fixture_uuid(32)),
            score_run_id: fixture_uuid(33),
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
