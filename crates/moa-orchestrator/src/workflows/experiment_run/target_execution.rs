//! Target execution paths for behavior-lab experiment runs.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartedWorkflowRun {
    run_uid: Uuid,
    /// Terminal experiment-run status (with any error) when the procedure run was
    /// already terminal at start time, for example an idempotent replay that
    /// matched a completed run by idempotency key. `None` means the caller must
    /// durably await the outcome.
    terminal: Option<(ExperimentRunStatus, Option<String>)>,
}

#[derive(Debug, Clone)]
struct WorkflowTargetStart {
    scope: ActionRuleScope,
    experiment_run_uid: Uuid,
    procedure_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
pub(super) async fn run_agent_loop_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    prompt: String,
    session_id: Option<SessionId>,
    agent: Option<AgentSessionSelection>,
    model: ModelId,
    attachments: Vec<moa_core::types::channel::Attachment>,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let scope = tenant_scope(request.tenant_id);
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

    let model = variant.model.unwrap_or(model);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            let agent = agent.ok_or_else(|| {
                bad_request("agent-loop experiment target requires an agent selector")
            })?;
            let (session_id, meta) = create_new_session(
                ctx,
                request.tenant_id,
                model.clone(),
                &request,
                agent,
                pool,
                session_store,
            )
            .await?;
            with_identity_headers(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .set_meta(Json::from(meta)),
                &request.identity,
            )
            .call()
            .await?;
            // The session authz tuples are applied through the normal outbox poller.
            ctx.sleep(Duration::from_millis(750)).await?;
            session_id
        }
    };

    ctx.set(K_SESSION_ID, Json(session_id));
    tracing::Span::current().set_attribute("moa.experiment.session_id", session_id.to_string());
    persist_attached_session(ctx, scope, request.run_uid, session_id, pool).await?;

    with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .queue_message(Json::from(QueueMessageRequest {
                user_message: prompt,
                attachments,
                model: Some(model.to_string()),
                contact: None,
                max_turns: None,
            })),
        &request.identity,
    )
    .call()
    .await?;

    procedure_status_response(
        ctx,
        ExperimentRunStatusRequest {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
        pool,
        session_store,
    )
    .await
}

/// Executes a procedure-backed experiment run and durably waits for the
/// procedure to reach a terminal state before finalizing the run.
///
/// The run starts the durable procedure run row, then awaits the procedure via
/// [`procedure_target_wait::wait_for_procedure_outcome`], which races a durable
/// request-response `.call()` to the
/// [`ProcedureExecution`](crate::workflows::procedure_execution::ProcedureExecution)
/// `run` handler against [`TARGET_WAIT_TIMEOUT`](crate::workflows::procedure_target_wait::TARGET_WAIT_TIMEOUT).
/// The prior fire-and-forget `.send()` let the experiment run workflow return —
/// and its scoring/analytics row settle — while the procedure was still
/// executing; awaiting the terminal outcome keeps the run reported as `Running`
/// until the procedure actually finishes.
///
/// The procedure `run` handler blocks internally while a run is paused on a
/// `Review` or `WaitSignal` node, so a review-gated run never resolves the call
/// and the run times out instead. That is intentional: an experiment procedure
/// awaiting human review times the run out (recorded as `Failed`) rather than
/// reporting a premature terminal status.
#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
pub(super) async fn run_procedure_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    procedure_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
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

    let workflow_run = start_and_attach_workflow_run(
        ctx,
        WorkflowTargetStart {
            scope,
            experiment_run_uid: request.run_uid,
            procedure_ref,
            input,
            session_id,
            idempotency_key,
        },
        pool,
    )
    .await?;
    ctx.set(K_PROCEDURE_RUN_UID, Json(workflow_run.run_uid));
    tracing::Span::current().set_attribute(
        "moa.experiment.procedure_run_uid",
        workflow_run.run_uid.to_string(),
    );

    // Idempotent replay where the procedure was already terminal at start time
    // (for example a completed run matched by idempotency key): finalize
    // immediately without re-invoking the executor.
    let (status, error) = match workflow_run.terminal {
        Some(terminal) => terminal,
        None => {
            wait_for_procedure_target_status(
                ctx,
                request.tenant_id,
                request.identity.clone(),
                workflow_run.run_uid,
                session_id,
            )
            .await?
        }
    };
    finalize_run_status(ctx, request.tenant_id, request.run_uid, status, error, pool).await?;

    procedure_status_response(
        ctx,
        ExperimentRunStatusRequest {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
        pool,
        session_store,
    )
    .await
}

/// Durably awaits a procedure run and maps its terminal outcome into the
/// experiment run status vocabulary.
///
/// Delegates the replay-safe race to
/// [`procedure_target_wait::wait_for_procedure_outcome`] and mirrors the
/// agent-loop turn-timeout disposition: a timeout, or an unexpected non-terminal
/// outcome, records the run as `Failed`.
async fn wait_for_procedure_target_status(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    identity: Identity,
    run_uid: Uuid,
    session_id: Option<SessionId>,
) -> Result<(ExperimentRunStatus, Option<String>), HandlerError> {
    match procedure_target_wait::wait_for_procedure_outcome(
        ctx, tenant_id, identity, run_uid, session_id,
    )
    .await?
    {
        ProcedureWaitOutcome::Terminal(status, outcome) => {
            match run_status_for_workflow_status(&status) {
                Some(run_status) => Ok((run_status, outcome.error)),
                None => Ok(procedure_failure_status(format!(
                    "procedure run {run_uid} returned non-terminal status {}",
                    outcome.status
                ))),
            }
        }
        ProcedureWaitOutcome::NonTerminal(outcome) => Ok(procedure_failure_status(format!(
            "procedure run {run_uid} returned non-terminal status {}",
            outcome.status
        ))),
        ProcedureWaitOutcome::TimedOut => Ok(procedure_failure_status(format!(
            "timed out waiting for procedure run {run_uid} to reach a terminal state"
        ))),
    }
}

/// Persists the terminal experiment-run status with a durable completion
/// timestamp once the procedure has finished.
async fn finalize_run_status(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let completed_at = durable_utc_now(ctx, "experiment_utc_now").await?;
    persist_run_status(
        ctx,
        tenant_id,
        run_uid,
        status,
        error,
        Some(completed_at),
        pool,
    )
    .await
}

/// Maps a terminal artifact-run status into the experiment run status
/// vocabulary. Returns `None` for non-terminal statuses (queued/running/
/// pending_review), which must not resolve the run.
fn run_status_for_workflow_status(status: &ArtifactRunStatus) -> Option<ExperimentRunStatus> {
    match status {
        ArtifactRunStatus::Queued
        | ArtifactRunStatus::Running
        | ArtifactRunStatus::PendingReview => None,
        ArtifactRunStatus::Completed => Some(ExperimentRunStatus::Completed),
        ArtifactRunStatus::Failed => Some(ExperimentRunStatus::Failed),
        ArtifactRunStatus::Cancelled => Some(ExperimentRunStatus::Cancelled),
    }
}

/// Run status recorded when a procedure target fails to reach a terminal state
/// in time (timeout) or reports an unexpected non-terminal status. Mirrors the
/// agent-loop turn-timeout disposition (`Failed`).
fn procedure_failure_status(message: String) -> (ExperimentRunStatus, Option<String>) {
    (ExperimentRunStatus::Failed, Some(message))
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    request: &ExperimentRunWorkflowRequest,
    agent: AgentSessionSelection,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = session_store.clone();
    let pool = pool.clone();
    let identity = request.identity.clone();
    Ok(ctx
        .run(|| async move {
            let mut meta = new_session_meta(tenant_id, model, &identity)?;
            let agent_context =
                resolve_agent_context_for_session(pool.clone(), &meta, &agent).await?;
            apply_agent_model_policy(&mut meta, &agent_context)?;
            meta.agent_context = Some(agent_context);
            let session_id =
                create_session_for_identity(store.as_ref(), &pool, meta.clone(), identity)
                    .await
                    .map_err(non_retryable_handler_error)?;
            Ok::<_, HandlerError>(Json::from((session_id, meta)))
        })
        .name("experiment_create_session")
        .await?
        .into_inner())
}

/// Creates the durable procedure run row and links it to the experiment run.
///
/// This only writes durable state; the executor is invoked later by
/// [`wait_for_procedure_target_status`] so the run can durably await the terminal
/// outcome instead of returning while the procedure runs. The returned
/// [`StartedWorkflowRun::terminal`] is populated only when the run was already
/// terminal at start time (an idempotent replay).
async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    start: WorkflowTargetStart,
    pool: &sqlx::PgPool,
) -> Result<StartedWorkflowRun, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            let run = workflow_runtime(pool.clone())
                .start(
                    &start.scope,
                    StartProcedureRun {
                        procedure_ref: start.procedure_ref,
                        input: start.input,
                        session_id: start.session_id,
                        idempotency_key: start.idempotency_key,
                    },
                )
                .await
                .map_err(procedure_handler_error)?;
            attach_procedure_run(pool, start.scope, start.experiment_run_uid, run.run_uid).await?;
            let terminal = run_status_for_workflow_status(&run.status)
                .map(|status| (status, run.error.clone()));
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
                terminal,
            }))
        })
        .name("experiment_start_workflow_run")
        .await?
        .into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_status_finalizes_run_only_after_terminal_states_offline() {
        // Pins: a procedure-backed run stays in flight (no terminal status) while the
        // procedure is queued, running, or paused on review, and each terminal artifact
        // status maps to the matching experiment run status.
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::Queued),
            None
        );
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::Running),
            None
        );
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::PendingReview),
            None
        );
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::Completed),
            Some(ExperimentRunStatus::Completed)
        );
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::Failed),
            Some(ExperimentRunStatus::Failed)
        );
        assert_eq!(
            run_status_for_workflow_status(&ArtifactRunStatus::Cancelled),
            Some(ExperimentRunStatus::Cancelled)
        );
    }

    #[test]
    fn procedure_timeout_records_failed_status_offline() {
        // Pins: a procedure run that times out (for example blocked on human review) or
        // returns an unexpected non-terminal status finalizes the run as Failed with the
        // diagnostic message, mirroring the agent-loop turn-timeout disposition.
        let (status, error) = procedure_failure_status("timed out waiting".to_string());
        assert_eq!(status, ExperimentRunStatus::Failed);
        assert_eq!(error.as_deref(), Some("timed out waiting"));
    }
}
