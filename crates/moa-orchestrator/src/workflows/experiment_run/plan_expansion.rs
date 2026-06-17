//! Plan expansion and child-trial dispatch for behavior-lab experiment runs.

use super::*;

pub(super) async fn run_experiment_plan(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    plan_revision_uid: Uuid,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    persist_run_status(
        ctx,
        request.workspace_id.clone(),
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
    )
    .await?;
    let expansion = load_plan_expansion(
        ctx,
        request.workspace_id.clone(),
        request.run_uid,
        plan_revision_uid,
    )
    .await?;
    let trials =
        create_plan_trial_rows(ctx, request.workspace_id.clone(), expansion.trials).await?;
    dispatch_plan_trials(ctx, request, expansion.parallelism, trials).await
}

async fn dispatch_plan_trials(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    parallelism: usize,
    trials: Vec<PlanTrialDispatch>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let dispatch_index = trials
        .into_iter()
        .map(|trial| (trial.trial.trial_key.clone(), trial))
        .collect::<BTreeMap<_, _>>();
    let mut idle_polls = 0_u32;

    loop {
        let aggregate =
            aggregate_plan_status(ctx, request.workspace_id.clone(), request.run_uid).await?;
        if aggregate.run.status == ExperimentRunStatus::Cancelled {
            cancel_active_plan_trials(
                ctx,
                request.workspace_id.clone(),
                request.run_uid,
                aggregate
                    .run
                    .error
                    .clone()
                    .unwrap_or_else(|| "parent run cancelled".to_string()),
            )
            .await?;
            return workflow_status_response(
                ctx,
                ExperimentRunStatusRequest {
                    workspace_id: request.workspace_id,
                    run_uid: request.run_uid,
                },
            )
            .await;
        }

        if run_status_is_terminal(aggregate.status) {
            persist_run_status(
                ctx,
                request.workspace_id.clone(),
                request.run_uid,
                aggregate.status,
                aggregate.error.clone(),
                Some(durable_utc_now(ctx).await?),
            )
            .await?;
            return workflow_status_response(
                ctx,
                ExperimentRunStatusRequest {
                    workspace_id: request.workspace_id,
                    run_uid: request.run_uid,
                },
            )
            .await;
        }

        if aggregate.status != aggregate.run.status {
            persist_run_status(
                ctx,
                request.workspace_id.clone(),
                request.run_uid,
                aggregate.status,
                aggregate.error.clone(),
                None,
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
                    && dispatch_index.contains_key(&trial.trial_key)
            })
            .take(available_slots)
            .map(|trial| trial.trial_key.clone())
            .collect::<Vec<_>>();
        let had_ready_trials = !ready_trial_keys.is_empty();
        let claimed_trials = claim_plan_trial_dispatches(
            ctx,
            request.workspace_id.clone(),
            request.run_uid,
            ready_trial_keys,
            available_slots,
        )
        .await?;
        for claimed_trial in claimed_trials {
            let Some(trial) = dispatch_index.get(&claimed_trial.trial_key) else {
                continue;
            };
            let key = trial_workflow_key(request.run_uid, &trial.trial.trial_key);
            ctx.workflow_client::<ExperimentTrialRunClient>(key)
                .run(Json::from(ExperimentTrialRunWorkflowRequest {
                    workspace_id: request.workspace_id.clone(),
                    trial: trial.trial.clone(),
                    target: trial.target.clone(),
                    variant: trial.variant.clone(),
                    identity: request.identity.clone(),
                }))
                .send();
        }

        if available_slots == 0 || had_ready_trials {
            idle_polls = 0;
        } else {
            idle_polls = idle_polls.saturating_add(1);
        }
        ctx.sleep(plan_status_poll_interval(idle_polls)).await?;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanExpansion {
    parallelism: usize,
    trials: Vec<ExpandedPlanTrial>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanTrialDispatch {
    trial: NewExperimentTrial,
    record: ExperimentTrialRecord,
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
    workspace_id: WorkspaceId,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
) -> Result<PlanExpansion, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    Ok(ctx
        .run(|| async move {
            expand_plan(pool, scope, run_uid, plan_revision_uid)
                .await
                .map(Json::from)
        })
        .name("experiment_plan_expand")
        .await?
        .into_inner())
}

async fn expand_plan(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
) -> Result<PlanExpansion, HandlerError> {
    let registry = ArtifactRegistry::new(pool);
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
    let trials = expand_plan_trials(run_uid, plan_revision_uid, definition)
        .map_err(plan_expansion_error_to_handler_error)?;
    Ok(PlanExpansion {
        parallelism: usize::try_from(definition.parallelism.max(1))
            .map_err(|_| bad_request("experiment plan parallelism is too large"))?,
        trials,
    })
}

async fn load_required_published_revision(
    registry: &ArtifactRegistry,
    scope: &MemoryScope,
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

async fn create_plan_trial_rows(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trials: Vec<ExpandedPlanTrial>,
) -> Result<Vec<PlanTrialDispatch>, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    Ok(ctx
        .run(|| async move {
            let store = ExperimentStore::new(pool);
            let mut dispatch = Vec::with_capacity(trials.len());
            for trial in trials {
                let record = store
                    .insert_trial(&scope, trial.trial.clone())
                    .await
                    .map_err(moa_error_to_handler_error)?;
                dispatch.push(PlanTrialDispatch {
                    trial: trial.trial,
                    record,
                    target: serialized_payload("target", &trial.target)?,
                    variant: serialized_payload("variant", &trial.variant)?,
                });
            }
            Ok::<_, HandlerError>(Json::from(dispatch))
        })
        .name("experiment_plan_create_trials")
        .await?
        .into_inner())
}

async fn aggregate_plan_status(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    run_uid: Uuid,
) -> Result<PlanStatusAggregate, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
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
    scope: MemoryScope,
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
        aggregate_status_for_trials(&trials, run.status)
    };
    let error = aggregate_error_for_trials(&trials);
    Ok(PlanStatusAggregate {
        run,
        trials,
        status,
        error,
    })
}

pub(super) fn aggregate_status_for_trials(
    trials: &[ExperimentTrialRecord],
    fallback: ExperimentRunStatus,
) -> ExperimentRunStatus {
    if trials.is_empty() {
        return if fallback.is_terminal() {
            fallback
        } else {
            ExperimentRunStatus::Completed
        };
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
        .any(|trial| trial.status == ExperimentTrialStatus::WaitingApproval)
    {
        return ExperimentRunStatus::WaitingApproval;
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

pub(super) fn plan_status_poll_interval(idle_polls: u32) -> Duration {
    let seconds = match idle_polls {
        0 => PLAN_STATUS_POLL_INTERVAL.as_secs(),
        1 => PLAN_STATUS_POLL_INTERVAL.as_secs().saturating_mul(2),
        2 => PLAN_STATUS_POLL_INTERVAL.as_secs().saturating_mul(4),
        _ => PLAN_STATUS_POLL_MAX_INTERVAL.as_secs(),
    };
    Duration::from_secs(seconds.min(PLAN_STATUS_POLL_MAX_INTERVAL.as_secs()))
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
    workspace_id: WorkspaceId,
    run_uid: Uuid,
    reason: String,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
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
    workspace_id: WorkspaceId,
    run_uid: Uuid,
    trial_keys: Vec<String>,
    available_slots: usize,
) -> Result<Vec<ExperimentTrialRecord>, HandlerError> {
    if trial_keys.is_empty() || available_slots == 0 {
        return Ok(Vec::new());
    }

    let limit = i64::try_from(available_slots)
        .map_err(|_| TerminalError::new("experiment dispatch parallelism is too large"))?;
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
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
        ExperimentTrialStatus::Dispatched
            | ExperimentTrialStatus::Running
            | ExperimentTrialStatus::WaitingApproval
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

pub(super) fn plan_revision_uid_from_run(run: &ExperimentRunRecord) -> Option<Uuid> {
    run.variant
        .metadata
        .get("plan_revision_uid")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}
