//! Plan expansion and child-trial dispatch for behavior-lab experiment runs.

use super::*;

use std::collections::BTreeSet;
use std::future::Future;

use restate_sdk::context::macro_support::SealedDurableFuture;
use serde_json::json;

struct ActivePlanTrialWait<F> {
    trial_key: String,
    future: F,
}

pub(super) async fn run_experiment_plan(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    scope: ActionRuleScope,
    plan_revision_uid: Uuid,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    persist_run_status(
        ctx,
        scope,
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
        pool,
    )
    .await?;
    let expansion = load_plan_expansion(
        ctx,
        scope,
        request.run_uid,
        plan_revision_uid,
        request.agent_revision_variants.clone(),
        request.release_evaluation.clone(),
        pool,
    )
    .await?;
    dispatch_plan_trials(ctx, request, scope, expansion, pool, session_store).await
}

async fn dispatch_plan_trials(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    scope: ActionRuleScope,
    expansion: PlanExpansion,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let parallelism = expansion.parallelism;
    let variants = expansion.variants;
    let mut active_waits = Vec::new();
    let mut last_progress = None;

    loop {
        let aggregate = aggregate_plan_status(ctx, scope, request.run_uid, pool).await?;
        retain_active_plan_waits(&mut active_waits, &aggregate.trials);
        let progress = plan_progress_fingerprint(&aggregate);
        let state_changed = last_progress
            .as_ref()
            .is_none_or(|previous| previous != &progress);
        last_progress = Some(progress);

        if aggregate.run.status == ExperimentRunStatus::Cancelled {
            cancel_active_plan_trials(
                ctx,
                scope,
                request.run_uid,
                aggregate
                    .run
                    .error
                    .clone()
                    .unwrap_or_else(|| "parent run cancelled".to_string()),
                pool,
            )
            .await?;
            return run_status_response(
                ctx,
                ExperimentRunStatusRequest {
                    tenant_id: request.tenant_id,
                    run_uid: request.run_uid,
                },
                pool,
                session_store,
            )
            .await;
        }

        if run_status_is_terminal(aggregate.status) {
            persist_run_status(
                ctx,
                scope,
                request.run_uid,
                aggregate.status,
                aggregate.error.clone(),
                Some(durable_utc_now(ctx, "experiment_utc_now").await?),
                pool,
            )
            .await?;
            return run_status_response(
                ctx,
                ExperimentRunStatusRequest {
                    tenant_id: request.tenant_id,
                    run_uid: request.run_uid,
                },
                pool,
                session_store,
            )
            .await;
        }

        if aggregate.status != aggregate.run.status {
            persist_run_status(
                ctx,
                scope,
                request.run_uid,
                aggregate.status,
                aggregate.error.clone(),
                None,
                pool,
            )
            .await?;
        }

        let active_count = active_plan_trial_count(&aggregate.trials);
        let available_slots = parallelism.saturating_sub(active_count);
        let ready_trial_keys = aggregate
            .trials
            .iter()
            .filter(|trial| {
                trial.status == ExperimentTrialStatus::Accepted
                    && variants.contains_key(&trial.variant_key)
            })
            .take(available_slots)
            .map(|trial| trial.trial_key.clone())
            .collect::<Vec<_>>();
        let claimed_trials = claim_plan_trial_dispatches(
            ctx,
            scope,
            request.run_uid,
            ready_trial_keys,
            available_slots,
            pool,
        )
        .await?;
        let claimed_any_trials = !claimed_trials.is_empty();
        for claimed_trial in claimed_trials {
            let payload = variants.get(&claimed_trial.variant_key).ok_or_else(|| {
                bad_request(format!(
                    "experiment plan variant `{}` is missing its dispatch payload",
                    claimed_trial.variant_key
                ))
            })?;
            let trial = NewExperimentTrial::from(&claimed_trial);
            let key = trial_workflow_key(request.run_uid, &trial.trial_key);
            let (awakeable_id, completion) = ctx.awakeable::<String>();
            active_waits.push(ActivePlanTrialWait {
                trial_key: trial.trial_key.clone(),
                future: completion,
            });
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExperimentTrialRunClient>(key)
                    .run(Json::from(ExperimentTrialRunWorkflowRequest {
                        tenant_id: request.tenant_id,
                        trial,
                        target: payload.target.clone(),
                        variant: payload.variant.clone(),
                        identity: request.identity.clone(),
                        release_overlay: request
                            .release_evaluation
                            .as_ref()
                            .map(|binding| release_trial_binding(binding, &claimed_trial))
                            .transpose()?,
                        completion_awakeable_id: Some(awakeable_id),
                    })),
            )
            .send();
        }

        if !state_changed && !claimed_any_trials && active_waits.is_empty() {
            let reason =
                "experiment plan made no progress and has no active trial waiters".to_string();
            cancel_active_plan_trials(ctx, scope, request.run_uid, reason.clone(), pool).await?;
            persist_run_status(
                ctx,
                scope,
                request.run_uid,
                ExperimentRunStatus::Failed,
                Some(reason),
                Some(durable_utc_now(ctx, "experiment_utc_now").await?),
                pool,
            )
            .await?;
            return run_status_response(
                ctx,
                ExperimentRunStatusRequest {
                    tenant_id: request.tenant_id,
                    run_uid: request.run_uid,
                },
                pool,
                session_store,
            )
            .await;
        }
        if !active_waits.is_empty() {
            wait_for_plan_trial_completion(ctx, &mut active_waits).await?;
        }
    }
}

fn release_trial_binding(
    binding: &moa_wire::experiments::ArtifactReleaseExperimentBinding,
    trial: &ExperimentTrialRecord,
) -> Result<moa_wire::experiments::ArtifactReleaseExperimentTrialBinding, HandlerError> {
    let mut matches = binding
        .trials
        .iter()
        .filter(|bound| bound.trial_key == trial.trial_key);
    let bound = matches.next().cloned().ok_or_else(|| {
        bad_request(format!(
            "release trial `{}` has no provisioned release binding",
            trial.trial_key
        ))
    })?;
    if matches.next().is_some() {
        return Err(bad_request(format!(
            "release trial `{}` has duplicate provisioned release bindings",
            trial.trial_key
        )));
    }
    let (Some(scenario_id), Some(persona_id), Some(profile_id)) = (
        trial.scenario_id.as_deref(),
        trial.persona_id.as_deref(),
        trial.profile_id.as_deref(),
    ) else {
        return Err(bad_request(
            "release trial is missing its selected scenario/persona/profile identity",
        ));
    };
    if bound.arm.variant_key != trial.variant_key
        || bound.case.scenario_id != scenario_id
        || bound.case.persona_id != persona_id
        || bound.case.profile_id != profile_id
    {
        return Err(bad_request(format!(
            "release trial `{}` does not match its provisioned variant/case binding",
            trial.trial_key
        )));
    }
    if bound.arm.eval_session_id.is_nil()
        || bound.arm.overlay_uid.is_nil()
        || bound.arm.revision_uid.is_nil()
        || bound.arm.overlay_token.trim().is_empty()
    {
        return Err(bad_request(format!(
            "release trial `{}` has an incomplete provisioned runtime binding",
            trial.trial_key
        )));
    }
    Ok(bound)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanExpansion {
    parallelism: usize,
    variants: BTreeMap<String, PlanVariantDispatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanVariantDispatch {
    target: Value,
    variant: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct PlanStatusAggregate {
    pub(super) run: ExperimentRunRecord,
    pub(super) trials: Vec<ExperimentTrialRecord>,
    pub(super) status: ExperimentRunStatus,
    pub(super) error: Option<String>,
}

async fn load_plan_expansion(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    release_evaluation: Option<moa_wire::experiments::ArtifactReleaseExperimentBinding>,
    pool: &sqlx::PgPool,
) -> Result<PlanExpansion, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            expand_plan(
                pool,
                scope,
                run_uid,
                plan_revision_uid,
                agent_revision_variants,
                release_evaluation,
            )
            .await
            .map(Json::from)
        })
        .name("experiment_plan_expand")
        .await?
        .into_inner())
}

async fn expand_plan(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    release_evaluation: Option<moa_wire::experiments::ArtifactReleaseExperimentBinding>,
) -> Result<PlanExpansion, HandlerError> {
    let registry = ArtifactRegistry::new(pool.clone());
    let plan_revision = load_required_published_revision(
        &registry,
        &scope,
        plan_revision_uid,
        ArtifactKind::ExperimentPlan,
    )
    .await?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan_revision.document.definition else {
        return Err(bad_request(
            "plan revision must contain an experiment_plan definition",
        ));
    };
    let definition = definition_with_agent_revision_variants(definition, &agent_revision_variants)?;
    let definition = definition_with_release_variants(definition, release_evaluation.as_ref())?;
    let store = ExperimentStore::new(pool);
    let run = store
        .load_run(&scope, run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| bad_request("experiment run disappeared before plan expansion"))?;
    let simulator_policy = run.simulator_policy.ok_or_else(|| {
        bad_request("plan-backed experiment run has no simulator policy snapshot")
    })?;
    if simulator_policy.reference() != definition.simulator_policy {
        return Err(bad_request(
            "experiment run simulator policy does not match the pinned plan revision",
        ));
    }
    let selected_release_cases = release_evaluation.as_ref().map(release_cases).transpose()?;
    let mut remaining_release_trials = release_evaluation
        .as_ref()
        .map(release_trial_keys)
        .transpose()?;
    let mut pager = match selected_release_cases.as_ref() {
        Some(selected_cases) => {
            let cases = selected_cases
                .iter()
                .map(|case| moa_experiments::plan::PlanCaseSelection {
                    scenario_id: case.scenario_id.clone(),
                    persona_id: case.persona_id.clone(),
                    profile_id: case.profile_id.clone(),
                    repetitions: case.repetitions,
                })
                .collect::<Vec<_>>();
            PlanTrialPager::new_selected(
                run_uid,
                plan_revision_uid,
                &definition,
                &simulator_policy,
                &cases,
            )
        }
        None => PlanTrialPager::new(run_uid, plan_revision_uid, &definition, &simulator_policy),
    }
    .map_err(plan_expansion_error_to_handler_error)?;
    let mut variants = BTreeMap::new();
    loop {
        let page = pager.next_page();
        if page.is_empty() {
            break;
        }
        for trial in page {
            if let Some(remaining) = remaining_release_trials.as_mut()
                && !remaining.remove(&trial.trial.trial_key)
            {
                return Err(bad_request(format!(
                    "expanded release trial `{}` has no unique provisioned binding",
                    trial.trial.trial_key
                )));
            }
            if !variants.contains_key(&trial.trial.variant_key) {
                variants.insert(
                    trial.trial.variant_key.clone(),
                    PlanVariantDispatch {
                        target: serialized_payload("target", &trial.target)?,
                        variant: serialized_payload("variant", &trial.variant)?,
                    },
                );
            }
            store
                .insert_trial(&scope, trial.trial)
                .await
                .map_err(moa_error_to_handler_error)?;
        }
    }
    if let Some(remaining) = remaining_release_trials
        && let Some(unmatched) = remaining.into_iter().next()
    {
        return Err(bad_request(format!(
            "provisioned release trial `{unmatched}` was not emitted by the pinned plan"
        )));
    }
    Ok(PlanExpansion {
        parallelism: usize::try_from(definition.parallelism.max(1))
            .map_err(|_| bad_request("experiment plan parallelism is too large"))?,
        variants,
    })
}

fn definition_with_release_variants(
    mut definition: moa_artifacts::simulation::ExperimentPlanDefinition,
    release: Option<&moa_wire::experiments::ArtifactReleaseExperimentBinding>,
) -> Result<moa_artifacts::simulation::ExperimentPlanDefinition, HandlerError> {
    let Some(release) = release else {
        return Ok(definition);
    };
    if release.trials.is_empty() {
        return Err(bad_request(
            "artifact release experiment must declare at least one arm",
        ));
    }
    if definition.target_variants.len() != 1 {
        return Err(bad_request(
            "artifact release experiment plan must declare exactly one agent-loop target template",
        ));
    }
    let template = definition
        .target_variants
        .first()
        .cloned()
        .ok_or_else(|| bad_request("artifact release experiment requires a target variant"))?;
    if template.kind != moa_artifacts::simulation::ExperimentTargetKind::AgentLoop {
        return Err(bad_request(
            "artifact release evaluation currently supports only production agent-loop targets; execution-template targets have no release-overlay resolver",
        ));
    }
    if release.activation_target != "agent_deployment"
        && exact_agent_revision_uid(&template.config).is_none()
    {
        return Err(bad_request(
            "skill and action release evaluation plans must pin an exact host agent revision",
        ));
    }
    let mut seen = BTreeSet::new();
    let variants = release
        .trials
        .iter()
        .filter(|trial| seen.insert(trial.arm.variant_key.as_str()))
        .map(|trial| {
            let arm = &trial.arm;
            if arm.variant_key.trim().is_empty() {
                return Err(bad_request(
                    "artifact release experiment variant_key is required",
                ));
            }
            let mut config = template.config.clone();
            if release.activation_target == "agent_deployment" {
                if let Some(object) = config.as_object_mut() {
                    object.remove("agent");
                    object.remove("agent_installation_uid");
                    object.insert("agent_revision_uid".to_string(), json!(arm.revision_uid));
                } else {
                    config = json!({ "agent_revision_uid": arm.revision_uid });
                }
            }
            Ok(moa_artifacts::simulation::ExperimentTargetVariant {
                key: arm.variant_key.clone(),
                kind: template.kind,
                config,
                ui: template.ui.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    definition.target_variants = variants;
    Ok(definition)
}

fn exact_agent_revision_uid(config: &Value) -> Option<Uuid> {
    if let Some(agent) = config.get("agent") {
        return serde_json::from_value::<moa_core::types::agent::AgentSessionSelection>(
            agent.clone(),
        )
        .ok()
        .and_then(|selection| selection.revision_uid)
        .filter(|revision_uid| !revision_uid.is_nil());
    }
    config
        .get("agent_revision_uid")
        .and_then(|value| serde_json::from_value::<Uuid>(value.clone()).ok())
        .filter(|revision_uid| !revision_uid.is_nil())
}

fn release_cases(
    release: &moa_wire::experiments::ArtifactReleaseExperimentBinding,
) -> Result<Vec<moa_wire::experiments::ArtifactReleaseExperimentCase>, HandlerError> {
    let mut cases = BTreeMap::new();
    for trial in &release.trials {
        let identity = (
            trial.case.scenario_id.clone(),
            trial.case.persona_id.clone(),
            trial.case.profile_id.clone(),
        );
        if let Some(existing) = cases.get(&identity) {
            if existing != &trial.case {
                return Err(bad_request(format!(
                    "release case {}/{}/{} has conflicting provisioned definitions",
                    identity.0, identity.1, identity.2
                )));
            }
        } else {
            cases.insert(identity, trial.case.clone());
        }
    }
    Ok(cases.into_values().collect())
}

fn release_trial_keys(
    release: &moa_wire::experiments::ArtifactReleaseExperimentBinding,
) -> Result<BTreeSet<String>, HandlerError> {
    let mut keys = BTreeSet::new();
    for trial in &release.trials {
        if trial.trial_key.trim().is_empty() || !keys.insert(trial.trial_key.clone()) {
            return Err(bad_request(format!(
                "release experiment repeats or omits trial key `{}`",
                trial.trial_key
            )));
        }
    }
    Ok(keys)
}

fn definition_with_agent_revision_variants(
    definition: &moa_artifacts::simulation::ExperimentPlanDefinition,
    variants: &[AgentRevisionSimulationVariant],
) -> Result<moa_artifacts::simulation::ExperimentPlanDefinition, HandlerError> {
    if variants.is_empty() {
        return Ok(definition.clone());
    }
    let template = definition
        .target_variants
        .iter()
        .find(|variant| {
            matches!(
                variant.kind,
                moa_artifacts::simulation::ExperimentTargetKind::AgentLoop
            )
        })
        .ok_or_else(|| bad_request("agent revision simulation requires an agent-loop variant"))?;
    let mut overridden = definition.clone();
    overridden.target_variants = variants
        .iter()
        .map(|variant| {
            if variant.variant_key.trim().is_empty() {
                return Err(bad_request(
                    "agent revision simulation variant_key is required",
                ));
            }
            let mut config = template.config.clone();
            if let Some(object) = config.as_object_mut() {
                object.insert(
                    "agent_revision_uid".to_string(),
                    json!(variant.revision_uid),
                );
            } else {
                config = json!({ "agent_revision_uid": variant.revision_uid });
            }
            Ok(moa_artifacts::simulation::ExperimentTargetVariant {
                key: variant.variant_key.clone(),
                kind: moa_artifacts::simulation::ExperimentTargetKind::AgentLoop,
                config,
                ui: template.ui.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(overridden)
}

async fn load_required_published_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
    expected_kind: ArtifactKind,
) -> Result<StoredArtifactRevision, HandlerError> {
    let revision = registry
        .load_revision(scope, revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| bad_request(format!("artifact revision {revision_uid} was not found")))?;
    if revision.kind != expected_kind {
        return Err(bad_request(format!(
            "artifact revision {revision_uid} has kind {}, expected {expected_kind}",
            revision.kind
        )));
    }
    if revision.status != ArtifactStatus::Published {
        return Err(bad_request(format!(
            "artifact revision {revision_uid} must be published before experiment execution"
        )));
    }
    Ok(revision)
}

async fn aggregate_plan_status(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<PlanStatusAggregate, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            aggregate_plan_status_from_store(pool, scope, run_uid)
                .await
                .map(Json::from)
        })
        .name("experiment_plan_aggregate_status")
        .await?
        .into_inner())
}

pub(super) async fn aggregate_plan_status_from_store(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    run_uid: Uuid,
) -> Result<PlanStatusAggregate, HandlerError> {
    let store = ExperimentStore::new(pool);
    let run = store
        .load_run(&scope, run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))?;
    let mut trials = store
        .list_trials(&scope, run_uid, None, i64::MAX)
        .await
        .map_err(moa_error_to_handler_error)?;
    trials.sort_by(|left, right| left.trial_key.cmp(&right.trial_key));
    let status = if run.status == ExperimentRunStatus::Cancelled {
        ExperimentRunStatus::Cancelled
    } else {
        aggregate_status_for_trials(&trials, run.expected_trials, run.status)
    };
    let observed_trials = u64::try_from(trials.len()).unwrap_or(u64::MAX);
    let error = if observed_trials > run.expected_trials {
        Some(format!(
            "experiment run persisted {observed_trials} trials but declared {}",
            run.expected_trials
        ))
    } else if observed_trials < run.expected_trials && run.status == ExperimentRunStatus::Completed
    {
        Some(format!(
            "experiment run reached completed with {observed_trials} of {} declared trials",
            run.expected_trials
        ))
    } else {
        aggregate_error_for_trials(&trials)
    };
    Ok(PlanStatusAggregate {
        run,
        trials,
        status,
        error,
    })
}

pub(super) fn aggregate_status_for_trials(
    trials: &[ExperimentTrialRecord],
    expected_trials: u64,
    fallback: ExperimentRunStatus,
) -> ExperimentRunStatus {
    let observed_trials = u64::try_from(trials.len()).unwrap_or(u64::MAX);
    if observed_trials < expected_trials {
        return match fallback {
            ExperimentRunStatus::Accepted => ExperimentRunStatus::Accepted,
            ExperimentRunStatus::Running => ExperimentRunStatus::Running,
            ExperimentRunStatus::Failed => ExperimentRunStatus::Failed,
            ExperimentRunStatus::Completed => ExperimentRunStatus::Failed,
            ExperimentRunStatus::Cancelled => ExperimentRunStatus::Cancelled,
        };
    }
    if observed_trials > expected_trials {
        return ExperimentRunStatus::Failed;
    }
    if trials.iter().any(|trial| {
        matches!(
            trial.status,
            ExperimentTrialStatus::Accepted
                | ExperimentTrialStatus::Dispatched
                | ExperimentTrialStatus::Running
        )
    }) {
        return ExperimentRunStatus::Running;
    }
    if trials
        .iter()
        .any(|trial| trial.status == ExperimentTrialStatus::Failed)
    {
        return ExperimentRunStatus::Failed;
    }
    if trials
        .iter()
        .any(|trial| trial.status == ExperimentTrialStatus::Cancelled)
    {
        return ExperimentRunStatus::Cancelled;
    }
    ExperimentRunStatus::Completed
}

fn plan_progress_fingerprint(
    aggregate: &PlanStatusAggregate,
) -> (ExperimentRunStatus, Vec<(String, ExperimentTrialStatus)>) {
    (
        aggregate.run.status,
        aggregate
            .trials
            .iter()
            .map(|trial| (trial.trial_key.clone(), trial.status))
            .collect(),
    )
}

pub(super) fn aggregate_error_for_trials(trials: &[ExperimentTrialRecord]) -> Option<String> {
    let failed = trials
        .iter()
        .filter(|trial| trial.status == ExperimentTrialStatus::Failed)
        .count();
    if failed == 0 {
        None
    } else {
        Some(format!("{failed} experiment trial(s) failed"))
    }
}

async fn cancel_active_plan_trials(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    reason: String,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    ctx.run(|| async move {
        ExperimentStore::new(pool)
            .cancel_active_trials(&scope, run_uid, reason)
            .await
            .map_err(moa_error_to_handler_error)?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_plan_cancel_active_trials")
    .await?;
    Ok(())
}

async fn claim_plan_trial_dispatches(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    run_uid: Uuid,
    trial_keys: Vec<String>,
    available_slots: usize,
    pool: &sqlx::PgPool,
) -> Result<Vec<ExperimentTrialRecord>, HandlerError> {
    if trial_keys.is_empty() || available_slots == 0 {
        return Ok(Vec::new());
    }

    let limit = i64::try_from(available_slots)
        .map_err(|_| TerminalError::new("experiment dispatch parallelism is too large"))?;
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            let mut trials = ExperimentStore::new(pool)
                .claim_trials_for_dispatch(&scope, run_uid, &trial_keys, limit)
                .await
                .map_err(moa_error_to_handler_error)?;
            trials.sort_by(|left, right| left.trial_key.cmp(&right.trial_key));
            Ok::<_, HandlerError>(Json::from(trials))
        })
        .name("experiment_plan_claim_trial_dispatch")
        .await?
        .into_inner())
}

pub(super) fn active_plan_trial_count(trials: &[ExperimentTrialRecord]) -> usize {
    trials
        .iter()
        .filter(|trial| trial_status_occupies_dispatch_slot(trial.status))
        .count()
}

pub(super) fn trial_status_occupies_dispatch_slot(status: ExperimentTrialStatus) -> bool {
    matches!(
        status,
        ExperimentTrialStatus::Dispatched | ExperimentTrialStatus::Running
    )
}

pub(super) fn run_status_is_terminal(status: ExperimentRunStatus) -> bool {
    matches!(
        status,
        ExperimentRunStatus::Completed
            | ExperimentRunStatus::Failed
            | ExperimentRunStatus::Cancelled
    )
}

async fn wait_for_plan_trial_completion<F>(
    ctx: &WorkflowContext<'_>,
    active_waits: &mut Vec<ActivePlanTrialWait<F>>,
) -> Result<(), HandlerError>
where
    F: restate_sdk::context::DurableFuture<Output = Result<String, TerminalError>>
        + SealedDurableFuture,
{
    if active_waits.is_empty() {
        return Ok(());
    }

    let active_count = active_waits.len();
    let timeout = ctx.sleep(PLAN_CHILD_COMPLETION_WAIT_TIMEOUT);
    let inner_context = timeout.inner_context();
    let mut handles = active_waits
        .iter()
        .map(|wait| wait.future.handle())
        .collect::<Vec<_>>();
    handles.push(timeout.handle());

    let ready_index = inner_context.select(handles).await?;
    if ready_index == active_count {
        timeout.await?;
        return Err(TerminalError::new(format!(
            "experiment plan made no child-trial progress for {} seconds while waiting for {} active child trial(s)",
            PLAN_CHILD_COMPLETION_WAIT_TIMEOUT.as_secs(),
            active_count
        ))
        .into());
    }
    let trial_key = resolve_selected_plan_trial_wait(active_waits, ready_index).await?;
    remove_active_plan_wait_by_trial_key(active_waits, &trial_key);
    Ok(())
}

async fn resolve_selected_plan_trial_wait<F>(
    active_waits: &mut Vec<ActivePlanTrialWait<F>>,
    ready_index: usize,
) -> Result<String, HandlerError>
where
    F: Future<Output = Result<String, TerminalError>>,
{
    if ready_index >= active_waits.len() {
        return Err(TerminalError::new(format!(
            "experiment plan child-trial select returned out-of-range index {ready_index}"
        ))
        .into());
    }
    let wait = active_waits.remove(ready_index);
    let trial_key = wait.future.await?;
    completion_signal_matches_trial(&wait.trial_key, &trial_key)?;
    Ok(trial_key)
}

fn remove_active_plan_wait_by_trial_key<F>(
    active_waits: &mut Vec<ActivePlanTrialWait<F>>,
    trial_key: &str,
) {
    active_waits.retain(|wait| wait.trial_key != trial_key);
}

fn completion_signal_matches_trial(expected: &str, actual: &str) -> Result<(), HandlerError> {
    if expected == actual {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "experiment trial completion signal mismatch: expected {expected}, got {actual}"
    ))
    .into())
}

fn retain_active_plan_waits<F>(
    active_waits: &mut Vec<ActivePlanTrialWait<F>>,
    trials: &[ExperimentTrialRecord],
) {
    active_waits.retain(|wait| {
        trials.iter().any(|trial| {
            trial.trial_key == wait.trial_key && trial_status_occupies_dispatch_slot(trial.status)
        })
    });
}

pub(super) fn plan_revision_uid_from_run(run: &ExperimentRunRecord) -> Option<Uuid> {
    run.variant
        .metadata
        .get("plan_revision_uid")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::time::{Duration, Instant};

    use super::*;

    async fn await_plan_trial_completion_signal<F>(
        wait: ActivePlanTrialWait<F>,
    ) -> Result<(), HandlerError>
    where
        F: Future<Output = Result<String, TerminalError>>,
    {
        let actual_trial_key = wait.future.await?;
        completion_signal_matches_trial(&wait.trial_key, &actual_trial_key)
    }

    type BoxedTrialWaitFuture =
        Pin<Box<dyn Future<Output = Result<String, TerminalError>> + Send + 'static>>;

    #[test]
    fn first_release_activation_dispatches_only_the_absolute_candidate_gate_offline() {
        // Pins: without an existing serving revision, release evaluation runs
        // the candidate's absolute deterministic cohort gate. It must not
        // fabricate an unprovisioned baseline arm.
        let candidate_revision_uid = Uuid::now_v7();
        let authored_config = json!({
            "agent": {
                "installation_uid": Uuid::now_v7(),
                "revision_uid": Uuid::now_v7()
            },
            "model": "approved-control"
        });
        let definition = moa_artifacts::simulation::ExperimentPlanDefinition {
            target_variants: vec![moa_artifacts::simulation::ExperimentTargetVariant {
                key: "approved_target".to_string(),
                kind: moa_artifacts::simulation::ExperimentTargetKind::AgentLoop,
                config: authored_config.clone(),
                ui: json!({}),
            }],
            ..Default::default()
        };
        let release = moa_wire::experiments::ArtifactReleaseExperimentBinding {
            outbox_uid: Uuid::now_v7(),
            activation_target: "agent_deployment".to_string(),
            trials: vec![
                moa_wire::experiments::ArtifactReleaseExperimentTrialBinding {
                    trial_key: "candidate-trial".to_string(),
                    arm: moa_wire::experiments::ArtifactReleaseExperimentArm {
                        variant_key: moa_wire::experiments::ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY
                            .to_string(),
                        revision_uid: candidate_revision_uid,
                        overlay_uid: Uuid::now_v7(),
                        overlay_token: "candidate-token".to_string(),
                        eval_session_id: Uuid::now_v7(),
                    },
                    case: moa_wire::experiments::ArtifactReleaseExperimentCase {
                        scenario_id: "case".to_string(),
                        persona_id: "persona".to_string(),
                        profile_id: "profile".to_string(),
                        repetitions: 1,
                        assertions: Vec::new(),
                    },
                },
            ],
        };

        let expanded = definition_with_release_variants(definition, Some(&release))
            .expect("expand first activation");
        assert_eq!(expanded.target_variants.len(), 1);
        let candidate = &expanded.target_variants[0];
        assert_eq!(
            candidate.key,
            moa_wire::experiments::ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY
        );
        assert_eq!(
            candidate.config["agent_revision_uid"],
            json!(candidate_revision_uid)
        );
        assert!(
            candidate.config.get("agent").is_none(),
            "the candidate revision must replace any authored host-agent selector"
        );
    }

    #[test]
    fn release_execution_template_target_is_rejected_without_an_overlay_resolver_offline() {
        // Pins: execution-template sessions do not resolve artifact-release
        // overlays. Admitting that target would evaluate the serving revision
        // while attributing the score to the candidate.
        let definition = moa_artifacts::simulation::ExperimentPlanDefinition {
            target_variants: vec![moa_artifacts::simulation::ExperimentTargetVariant {
                key: "template".to_string(),
                kind: moa_artifacts::simulation::ExperimentTargetKind::ExecutionTemplate,
                config: json!({}),
                ui: json!({}),
            }],
            ..Default::default()
        };
        let release = moa_wire::experiments::ArtifactReleaseExperimentBinding {
            outbox_uid: Uuid::now_v7(),
            activation_target: "skill_visibility".to_string(),
            trials: vec![
                moa_wire::experiments::ArtifactReleaseExperimentTrialBinding {
                    trial_key: "trial".to_string(),
                    arm: moa_wire::experiments::ArtifactReleaseExperimentArm {
                        variant_key: moa_wire::experiments::ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY
                            .to_string(),
                        revision_uid: Uuid::now_v7(),
                        overlay_uid: Uuid::now_v7(),
                        overlay_token: "token".to_string(),
                        eval_session_id: Uuid::now_v7(),
                    },
                    case: moa_wire::experiments::ArtifactReleaseExperimentCase {
                        scenario_id: "case".to_string(),
                        persona_id: "persona".to_string(),
                        profile_id: "profile".to_string(),
                        repetitions: 1,
                        assertions: Vec::new(),
                    },
                },
            ],
        };

        assert!(definition_with_release_variants(definition, Some(&release)).is_err());
    }

    #[test]
    fn skill_release_requires_an_exact_host_agent_revision_offline() {
        // Pins: a skill/action candidate is resolved through the release overlay
        // of an eval-owned host-agent session. Without an exact host revision,
        // trial execution either cannot create the session or evaluates a
        // mutable deployment unrelated to the approved release plan.
        let definition = moa_artifacts::simulation::ExperimentPlanDefinition {
            target_variants: vec![moa_artifacts::simulation::ExperimentTargetVariant {
                key: "host".to_string(),
                kind: moa_artifacts::simulation::ExperimentTargetKind::AgentLoop,
                config: json!({ "prompt": "exercise the candidate skill" }),
                ui: json!({}),
            }],
            ..Default::default()
        };
        let release = moa_wire::experiments::ArtifactReleaseExperimentBinding {
            outbox_uid: Uuid::now_v7(),
            activation_target: "skill_visibility".to_string(),
            trials: vec![
                moa_wire::experiments::ArtifactReleaseExperimentTrialBinding {
                    trial_key: "trial".to_string(),
                    arm: moa_wire::experiments::ArtifactReleaseExperimentArm {
                        variant_key: moa_wire::experiments::ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY
                            .to_string(),
                        revision_uid: Uuid::now_v7(),
                        overlay_uid: Uuid::now_v7(),
                        overlay_token: "token".to_string(),
                        eval_session_id: Uuid::now_v7(),
                    },
                    case: moa_wire::experiments::ArtifactReleaseExperimentCase {
                        scenario_id: "case".to_string(),
                        persona_id: "persona".to_string(),
                        profile_id: "profile".to_string(),
                        repetitions: 1,
                        assertions: Vec::new(),
                    },
                },
            ],
        };

        assert!(definition_with_release_variants(definition, Some(&release)).is_err());
    }

    #[tokio::test]
    async fn plan_trial_completion_awakeable_resolves_without_polling_offline() {
        // Pins: parent dispatch waits on the child completion signal, not a 1s status poll.
        let started = Instant::now();

        tokio::time::timeout(
            Duration::from_millis(100),
            await_plan_trial_completion_signal(ActivePlanTrialWait {
                trial_key: "trial-a".to_string(),
                future: async { Ok::<_, TerminalError>("trial-a".to_string()) },
            }),
        )
        .await
        .expect("completion signal should resolve well below the old 1s polling interval")
        .expect("matching completion signal should succeed");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "completion signal path waited like the old polling loop"
        );
    }

    #[tokio::test]
    async fn plan_trial_fan_in_accepts_later_child_before_first_child_offline() {
        // Pins: plan expansion treats any active child completion as progress.
        let mut waits: Vec<ActivePlanTrialWait<BoxedTrialWaitFuture>> = vec![
            ActivePlanTrialWait {
                trial_key: "trial-slow".to_string(),
                future: Box::pin(std::future::pending()),
            },
            ActivePlanTrialWait {
                trial_key: "trial-fast".to_string(),
                future: Box::pin(async { Ok::<_, TerminalError>("trial-fast".to_string()) }),
            },
        ];
        let started = Instant::now();

        let completed_trial_key = tokio::time::timeout(
            Duration::from_millis(100),
            resolve_selected_plan_trial_wait(&mut waits, 1),
        )
        .await
        .expect("later child completion should not wait on index zero")
        .expect("selected child completion should resolve");
        remove_active_plan_wait_by_trial_key(&mut waits, &completed_trial_key);

        assert_eq!(completed_trial_key, "trial-fast");
        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0].trial_key, "trial-slow");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "fan-in path waited like the old first-child-only wait"
        );
    }

    #[test]
    fn completed_plan_trial_waits_are_removed_by_trial_key_offline() {
        // Pins: parent cleanup removes completed child awakeables by trial key.
        let mut waits: Vec<ActivePlanTrialWait<BoxedTrialWaitFuture>> = vec![
            ActivePlanTrialWait {
                trial_key: "trial-a".to_string(),
                future: Box::pin(std::future::pending()),
            },
            ActivePlanTrialWait {
                trial_key: "trial-b".to_string(),
                future: Box::pin(std::future::pending()),
            },
        ];

        remove_active_plan_wait_by_trial_key(&mut waits, "trial-b");

        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0].trial_key, "trial-a");
    }

    #[test]
    fn plan_trial_completion_signal_matches_registered_child_offline() {
        // Pins: parent dispatch waits consume the child completion signal directly.
        assert!(completion_signal_matches_trial("trial-a", "trial-a").is_ok());
    }

    #[test]
    fn plan_trial_completion_signal_rejects_wrong_child_offline() {
        // Pins: parent dispatch does not treat another trial's signal as progress.
        let error = completion_signal_matches_trial("trial-a", "trial-b")
            .expect_err("mismatched child completion signal should fail");

        assert!(
            format!("{error:?}").contains("completion signal mismatch"),
            "unexpected error: {error:?}"
        );
    }
}
