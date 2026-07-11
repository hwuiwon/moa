//! Plan expansion and child-trial dispatch for behavior-lab experiment runs.

use super::*;

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
    plan_revision_uid: Uuid,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    persist_run_status(
        ctx,
        request.tenant_id,
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
        pool,
    )
    .await?;
    let expansion = load_plan_expansion(
        ctx,
        request.tenant_id,
        request.run_uid,
        plan_revision_uid,
        request.agent_revision_variants.clone(),
        pool,
    )
    .await?;
    let trials = create_plan_trial_rows(ctx, request.tenant_id, expansion.trials, pool).await?;
    dispatch_plan_trials(
        ctx,
        request,
        expansion.parallelism,
        trials,
        pool,
        session_store,
    )
    .await
}

async fn dispatch_plan_trials(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    parallelism: usize,
    trials: Vec<PlanTrialDispatch>,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let dispatch_index = trials
        .into_iter()
        .map(|trial| (trial.trial.trial_key.clone(), trial))
        .collect::<BTreeMap<_, _>>();
    let mut active_waits = Vec::new();
    let mut last_progress = None;

    loop {
        let aggregate =
            aggregate_plan_status(ctx, request.tenant_id, request.run_uid, pool).await?;
        retain_active_plan_waits(&mut active_waits, &aggregate.trials);
        let progress = plan_progress_fingerprint(&aggregate);
        let state_changed = last_progress
            .as_ref()
            .is_none_or(|previous| previous != &progress);
        last_progress = Some(progress);

        if aggregate.run.status == ExperimentRunStatus::Cancelled {
            cancel_active_plan_trials(
                ctx,
                request.tenant_id,
                request.run_uid,
                aggregate
                    .run
                    .error
                    .clone()
                    .unwrap_or_else(|| "parent run cancelled".to_string()),
                pool,
            )
            .await?;
            return procedure_status_response(
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
                request.tenant_id,
                request.run_uid,
                aggregate.status,
                aggregate.error.clone(),
                Some(durable_utc_now(ctx, "experiment_utc_now").await?),
                pool,
            )
            .await?;
            return procedure_status_response(
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
                request.tenant_id,
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
                    && dispatch_index.contains_key(&trial.trial_key)
            })
            .take(available_slots)
            .map(|trial| trial.trial_key.clone())
            .collect::<Vec<_>>();
        let claimed_trials = claim_plan_trial_dispatches(
            ctx,
            request.tenant_id,
            request.run_uid,
            ready_trial_keys,
            available_slots,
            pool,
        )
        .await?;
        let claimed_any_trials = !claimed_trials.is_empty();
        for claimed_trial in claimed_trials {
            let Some(trial) = dispatch_index.get(&claimed_trial.trial_key) else {
                continue;
            };
            let key = trial_workflow_key(request.run_uid, &trial.trial.trial_key);
            let (awakeable_id, completion) = ctx.awakeable::<String>();
            active_waits.push(ActivePlanTrialWait {
                trial_key: trial.trial.trial_key.clone(),
                future: completion,
            });
            ctx.workflow_client::<ExperimentTrialRunClient>(key)
                .run(Json::from(ExperimentTrialRunWorkflowRequest {
                    tenant_id: request.tenant_id,
                    trial: trial.trial.clone(),
                    target: trial.target.clone(),
                    variant: trial.variant.clone(),
                    identity: request.identity.clone(),
                    completion_awakeable_id: Some(awakeable_id),
                }))
                .send();
        }

        if !state_changed && !claimed_any_trials && active_waits.is_empty() {
            let reason =
                "experiment plan made no progress and has no active trial waiters".to_string();
            cancel_active_plan_trials(
                ctx,
                request.tenant_id,
                request.run_uid,
                reason.clone(),
                pool,
            )
            .await?;
            persist_run_status(
                ctx,
                request.tenant_id,
                request.run_uid,
                ExperimentRunStatus::Failed,
                Some(reason),
                Some(durable_utc_now(ctx, "experiment_utc_now").await?),
                pool,
            )
            .await?;
            return procedure_status_response(
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
    tenant_id: TenantId,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    agent_revision_variants: Vec<AgentRevisionSimulationVariant>,
    pool: &sqlx::PgPool,
) -> Result<PlanExpansion, HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
    Ok(ctx
        .run(|| async move {
            expand_plan(
                pool,
                scope,
                run_uid,
                plan_revision_uid,
                agent_revision_variants,
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
    let definition = definition_with_agent_revision_variants(definition, &agent_revision_variants)?;
    let trials = expand_plan_trials(run_uid, plan_revision_uid, &definition)
        .map_err(plan_expansion_error_to_handler_error)?;
    Ok(PlanExpansion {
        parallelism: usize::try_from(definition.parallelism.max(1))
            .map_err(|_| bad_request("experiment plan parallelism is too large"))?,
        trials,
    })
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

async fn create_plan_trial_rows(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    trials: Vec<ExpandedPlanTrial>,
    pool: &sqlx::PgPool,
) -> Result<Vec<PlanTrialDispatch>, HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
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
    tenant_id: TenantId,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<PlanStatusAggregate, HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
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
    tenant_id: TenantId,
    run_uid: Uuid,
    reason: String,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    let scope = tenant_scope(tenant_id);
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
    tenant_id: TenantId,
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
    let scope = tenant_scope(tenant_id);
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

    #[tokio::test]
    async fn plan_trial_completion_awakeable_resolves_before_legacy_poll_interval_offline() {
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
