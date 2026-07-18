//! Durable keyed workflow that advances one persisted dynamic execution run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use moa_artifacts::execution_plan::{ExecutionOperation, ExecutionReducer};
use moa_brain::execution_planning::request::record_applied_planning_audit;
use moa_brain::execution_planning::{
    AmendmentPlanningEvidence, ExecutionAmendmentPlanningRequest,
    ExecutionAmendmentPlanningResultKind, plan_amendment,
};
use moa_core::{
    events::ExecutionInputRequired,
    traits::LLMProvider,
    types::{
        completion::{CompletionRequest, CompletionStream},
        execution_planning::{
            EXECUTION_REPORT_MAX_BYTES, ExecutionPlanningAuditEnvelope,
            ExecutionPlanningAuditPayload,
        },
        identifiers::ModelId,
        model::ModelCapabilities,
    },
};
use moa_execution::{
    completion::{
        CompletionEvaluation, CompletionEvaluationRequest, CompletionStatus, evaluate_completion,
        execution_terminal_reason, terminal_evidence_from_evaluation,
    },
    interpreter::{ScheduleRequest, ready_empty_map_nodes, schedule},
    replan::{replan_stop_gaps, replan_stop_status},
    repository::{
        CompileAuditWriteOutcome, ExecutionNodeMaterialization, ExecutionRepository,
        ExecutionRunRecord, ExecutionScope, FinalizationOutcome, MaterializationOutcome,
        PlannerCallAuditWriteOutcome, ReplanStopOutcome, ReplanStopRequest, RunFinalizationRequest,
        TransitionOutcome, WakeAckOutcome,
    },
    state::{
        ExecutionLimitStop, ExecutionRunStatus, ExecutionTaskId, ExecutionTaskStatus,
        ExecutionTerminalCause, ScheduleDecision, TerminalProjection, WaitingReason,
    },
    wire::{
        ExecutionAmendmentRequest, ExecutionMutationResponse, ExecutionPlanningContextSnapshot,
        ExecutionRunRequest, ExecutionRunWakeRequest, ExecutionRunWorkflowRequest,
        ExecutionTaskWorkflowRequest, ExecutionTerminalDelivery, execution_progress_from_run,
    },
};
use moa_hands::ToolRouter;
use moa_observability::{
    ExecutionMetricReducerKind, record_execution_map_fanout_items, record_execution_reducer_depth,
    restate_observability::annotate_restate_handler_span,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::objects::session::SessionClient;
use crate::services::{execution::ExecutionClient, llm_gateway::LLMGatewayClient};
use crate::workflows::execution_node_actions::{
    record_applied_run_transition, record_applied_task_transition,
    terminal_projection_from_evaluation,
};
use crate::workflows::execution_task::ExecutionTaskClient;

const K_PROCESSED_WAKE_EPOCH: &str = "execution_processed_wake_epoch";
const K_AWAITED_WAKE_EPOCH: &str = "execution_awaited_wake_epoch";

/// Durable workflow surface for one keyed execution run.
#[restate_sdk::workflow]
pub trait ExecutionRun {
    /// Drives the run until it is terminal, parking durably between wake epochs.
    async fn run(request: Json<ExecutionRunWorkflowRequest>) -> Result<(), HandlerError>;

    /// Records one persisted scheduling wake and resumes the parked driver when needed.
    #[shared]
    async fn wake(request: Json<ExecutionRunWakeRequest>) -> Result<(), HandlerError>;
}

/// PostgreSQL-backed execution-run workflow implementation.
#[derive(Clone)]
pub struct ExecutionRunImpl {
    repository: ExecutionRepository,
    config: moa_core::config::ExecutionConfig,
    planner_model: ModelId,
    router: Arc<ToolRouter>,
}

impl ExecutionRunImpl {
    /// Creates one durable run workflow over the shared execution repository.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        config: moa_core::config::ExecutionConfig,
        planner_model: ModelId,
        router: Arc<ToolRouter>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            config,
            planner_model,
            router,
        }
    }
}

impl ExecutionRun for ExecutionRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: started only by Execution/start after parent-session authorization; all recovery reads use the persisted scope in this keyed request.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExecutionRunWorkflowRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionRun", "run");
        let request = request.into_inner();
        annotate_execution_run_span(request.run_uid);
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "execution run id mismatch").into());
        }
        ctx.set(K_PROCESSED_WAKE_EPOCH, Json::from(0_u64));
        ctx.set(K_AWAITED_WAKE_EPOCH, Json::from(0_u64));
        let scope = execution_scope(&request);
        let mut step_index = 0_u64;
        loop {
            let repository = self.repository.clone();
            let drive_request = request.clone();
            let config = self.config.clone();
            let step = ctx
                .run(|| async move {
                    drive_once(repository, scope, drive_request, config)
                        .await
                        .map(Json::from)
                })
                .name(format!("execution_run_drive_{step_index}"))
                .await?
                .into_inner();
            step_index = step_index.saturating_add(1);
            deliver_session_projection(
                &ctx,
                self.repository.clone(),
                scope,
                &request,
                matches!(&step, RunDriveStep::Terminal { .. }),
                step_index,
            )
            .await?;
            match step {
                RunDriveStep::Continue => continue,
                RunDriveStep::PlanAmendment { plan_revision } => {
                    if pause_automatic_amendment_planner() {
                        ctx.sleep(std::time::Duration::from_millis(25)).await?;
                        continue;
                    }
                    let available_tool_names = self
                        .router
                        .capability_registrations()
                        .into_iter()
                        .map(|(definition, _)| definition.name)
                        .collect();
                    let amendment_step = plan_and_apply_waiting_replan(
                        &ctx,
                        AmendmentOperationContext {
                            repository: self.repository.clone(),
                            config: self.config.clone(),
                            planner_model: self.planner_model.clone(),
                            scope,
                            request: request.clone(),
                            available_tool_names,
                        },
                        plan_revision,
                    )
                    .await?;
                    deliver_session_projection(
                        &ctx,
                        self.repository.clone(),
                        scope,
                        &request,
                        matches!(&amendment_step, RunDriveStep::Terminal { .. }),
                        step_index,
                    )
                    .await?;
                    match amendment_step {
                        RunDriveStep::Continue => continue,
                        RunDriveStep::Terminal { task_ids, reason } => {
                            for task_id in task_ids {
                                crate::restate_identity::replay_safe_request(
                                    ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                                        .cancel(Json::from(reason.clone())),
                                )
                                .send();
                            }
                            return Ok(());
                        }
                        RunDriveStep::PlanAmendment { .. }
                        | RunDriveStep::Dispatch { .. }
                        | RunDriveStep::Park { .. } => {
                            return Err(TerminalError::new(
                                "amendment operation returned an invalid driver step",
                            )
                            .into());
                        }
                    }
                }
                RunDriveStep::Dispatch { tasks } => {
                    for task in tasks {
                        crate::restate_identity::replay_safe_request(
                            ctx.workflow_client::<ExecutionTaskClient>(task.task_id.to_string())
                                .run(Json::from(task)),
                        )
                        .send();
                    }
                }
                RunDriveStep::Park { processed_epoch } => {
                    ctx.set(K_AWAITED_WAKE_EPOCH, Json::from(processed_epoch));
                    let repository = self.repository.clone();
                    let acknowledgement = ctx
                        .run(|| async move {
                            repository
                                .ack_run_wake(scope, request.run_uid, processed_epoch)
                                .await
                                .map(Json::from)
                                .map_err(execution_error)
                        })
                        .name(format!("execution_run_ack_{processed_epoch}"))
                        .await?
                        .into_inner();
                    let processed_epoch = match acknowledgement {
                        WakeAckOutcome::Acknowledged {
                            processed_wake_epoch,
                        }
                        | WakeAckOutcome::Replayed {
                            processed_wake_epoch,
                        } => processed_wake_epoch,
                        WakeAckOutcome::Changed { .. } => continue,
                        WakeAckOutcome::NotFound => {
                            return Err(TerminalError::new_with_code(
                                404,
                                "execution run not found",
                            )
                            .into());
                        }
                    };
                    #[cfg(feature = "integration")]
                    test_wake_handoff_checkpoint(&ctx).await?;
                    ctx.set(K_PROCESSED_WAKE_EPOCH, Json::from(processed_epoch));
                    let promise_key = wake_promise_key(processed_epoch);
                    let _: u64 = ctx.promise(&promise_key).await?;
                }
                RunDriveStep::Terminal { task_ids, reason } => {
                    for task_id in task_ids {
                        crate::restate_identity::replay_safe_request(
                            ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                                .cancel(Json::from(reason.clone())),
                        )
                        .send();
                    }
                    return Ok(());
                }
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only after an authorized service or keyed task transaction persisted the exact wake epoch.
    async fn wake(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionRunWakeRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionRun", "wake");
        let request = request.into_inner();
        annotate_execution_run_span(request.run_uid);
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "execution run id mismatch").into());
        }
        let processed_epoch = ctx
            .get::<Json<u64>>(K_PROCESSED_WAKE_EPOCH)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let awaited_epoch = ctx
            .get::<Json<u64>>(K_AWAITED_WAKE_EPOCH)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        if let Some(promise_epoch) =
            wake_promise_epoch(processed_epoch, awaited_epoch, request.wake_epoch)
        {
            ctx.resolve_promise(&wake_promise_key(promise_epoch), request.wake_epoch);
        }
        Ok(())
    }
}

#[cfg(feature = "integration")]
fn pause_automatic_amendment_planner() -> bool {
    std::env::var("MOA_EXECUTION_TEST_PAUSE_AMENDMENT_PLANNER").as_deref() == Ok("true")
}

#[cfg(not(feature = "integration"))]
const fn pause_automatic_amendment_planner() -> bool {
    false
}

#[cfg(feature = "integration")]
async fn test_wake_handoff_checkpoint(ctx: &WorkflowContext<'_>) -> Result<(), HandlerError> {
    let Ok(mode) = std::env::var("MOA_EXECUTION_TEST_WAKE_HANDOFF") else {
        return Ok(());
    };
    match mode.as_str() {
        "delay" => {
            ctx.sleep(std::time::Duration::from_secs(2)).await?;
        }
        "crash_once" => {
            static CRASHED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            let should_crash = !CRASHED.swap(true, std::sync::atomic::Ordering::SeqCst);
            ctx.sleep(std::time::Duration::from_secs(2)).await?;
            if should_crash {
                return Err(anyhow::anyhow!("injected execution wake handoff crash").into());
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum RunDriveStep {
    Continue,
    PlanAmendment {
        plan_revision: u64,
    },
    Dispatch {
        tasks: Vec<ExecutionTaskWorkflowRequest>,
    },
    Park {
        processed_epoch: u64,
    },
    Terminal {
        task_ids: Vec<ExecutionTaskId>,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionProjectionDelivery {
    progress: moa_core::events::ExecutionProgress,
    inputs: Vec<ExecutionInputRequired>,
    terminal: Option<ExecutionTerminalDelivery>,
}

async fn deliver_session_projection(
    ctx: &WorkflowContext<'_>,
    repository: ExecutionRepository,
    scope: ExecutionScope,
    request: &ExecutionRunWorkflowRequest,
    include_terminal: bool,
    step_index: u64,
) -> Result<(), HandlerError> {
    #[cfg(feature = "integration")]
    if std::env::var("MOA_EXECUTION_TEST_SKIP_SESSION_DELIVERY").as_deref() == Ok("true") {
        return Ok(());
    }

    let delivery_request = request.clone();
    let delivery = ctx
        .run(|| async move {
            let snapshot = repository
                .load_scheduling_snapshot(scope, delivery_request.run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
            if snapshot.run.tenant_id != delivery_request.tenant_id
                || snapshot.run.contact_id != delivery_request.contact_id
                || snapshot.run.session_id != delivery_request.session_id
            {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution delivery scope does not match the workflow request",
                )
                .into());
            }
            let inputs = snapshot
                .projection
                .tasks
                .iter()
                .filter_map(|task| {
                    if task.status != ExecutionTaskStatus::WaitingInput {
                        return None;
                    }
                    let moa_artifacts::execution_plan::ExecutionTaskResult::NeedsInput {
                        question,
                        audience: moa_artifacts::execution_plan::InputAudience::User,
                    } = &task.outcome.as_ref()?.result
                    else {
                        return None;
                    };
                    Some(ExecutionInputRequired {
                        run_uid: snapshot.run.run_uid,
                        originating_user_sequence_num: snapshot.run.originating_user_sequence_num,
                        task_id: task.task_id.as_uuid(),
                        generation: task.generation,
                        question: question.clone(),
                    })
                })
                .collect();
            let terminal = if include_terminal {
                Some(
                    repository
                        .load_terminal_delivery(scope, delivery_request.run_uid)
                        .await
                        .map_err(execution_error)?
                        .ok_or_else(|| {
                            TerminalError::new_with_code(
                                404,
                                "terminal execution delivery disappeared after finalization",
                            )
                        })?,
                )
            } else {
                None
            };
            Ok(Json::from(SessionProjectionDelivery {
                progress: execution_progress_from_run(&snapshot.run),
                inputs,
                terminal,
            }))
        })
        .name(format!("execution_session_projection_{step_index}"))
        .await?
        .into_inner();

    let session = ctx.object_client::<SessionClient>(request.session_id.to_string());
    crate::restate_identity::replay_safe_request(
        session.execution_progress(Json::from(delivery.progress)),
    )
    .call()
    .await?;
    for input in delivery.inputs {
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(request.session_id.to_string())
                .execution_input_required(Json::from(input)),
        )
        .call()
        .await?;
    }
    if let Some(terminal) = delivery.terminal {
        match terminal.status {
            ExecutionRunStatus::Completed | ExecutionRunStatus::Cancelled => {}
            status => {
                moa_execution::wire::execution_failure_disposition(status)
                    .map_err(execution_error)?;
            }
        }
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(request.session_id.to_string())
                .execution_terminal(Json::from(terminal)),
        )
        .call()
        .await?;
    }
    Ok(())
}

struct RestateAmendmentPlannerProvider<'a> {
    ctx: &'a WorkflowContext<'a>,
}

#[async_trait]
impl LLMProvider for RestateAmendmentPlannerProvider<'_> {
    fn name(&self) -> &'static str {
        "restate-llm-gateway"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let response = crate::restate_identity::replay_safe_request(
            self.ctx
                .service_client::<LLMGatewayClient>()
                .complete(Json::from(request)),
        )
        .call()
        .await
        .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?
        .into_inner();
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PreparedAmendmentPlanning {
    context: ExecutionPlanningContextSnapshot,
    evidence: AmendmentPlanningEvidence,
    remaining_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
    now: chrono::DateTime<chrono::Utc>,
}

struct AmendmentOperationContext {
    repository: ExecutionRepository,
    config: moa_core::config::ExecutionConfig,
    planner_model: ModelId,
    scope: ExecutionScope,
    request: ExecutionRunWorkflowRequest,
    available_tool_names: BTreeSet<String>,
}

async fn plan_and_apply_waiting_replan(
    ctx: &WorkflowContext<'_>,
    operation: AmendmentOperationContext,
    plan_revision: u64,
) -> Result<RunDriveStep, HandlerError> {
    let AmendmentOperationContext {
        repository,
        config,
        planner_model,
        scope,
        request,
        available_tool_names,
    } = operation;
    let load_request = request.clone();
    let load_repository = repository.clone();
    let prepared = ctx
        .run(|| async move {
            prepare_amendment_planning(
                &load_repository,
                scope,
                &load_request,
                plan_revision,
                &available_tool_names,
            )
            .await
            .map(Json::from)
        })
        .name(format!(
            "execution_amendment_inputs_{}_{}",
            request.run_uid, plan_revision
        ))
        .await?
        .into_inner();
    let Some(prepared) = prepared else {
        return Ok(RunDriveStep::Continue);
    };

    let provider = RestateAmendmentPlannerProvider { ctx };
    let planned = plan_amendment(
        &provider,
        ExecutionAmendmentPlanningRequest {
            run_uid: request.run_uid,
            base_plan_revision: plan_revision,
            context: prepared.context,
            evidence: prepared.evidence,
            remaining_budget: prepared.remaining_budget,
            planner_model,
            config: config.clone(),
            now: prepared.now,
        },
    )
    .await
    .map_err(crate::workflows::errors::moa_error_to_handler_error)?;
    for audit in planned.audits {
        persist_amendment_audit(&repository, scope, audit).await?;
    }

    let amendment = match planned.kind {
        ExecutionAmendmentPlanningResultKind::Ready { amendment, .. } => amendment,
        ExecutionAmendmentPlanningResultKind::NeedsInput { message }
        | ExecutionAmendmentPlanningResultKind::Unsupported { message } => {
            return finalize_amendment_planner_stop(
                repository,
                scope,
                request.run_uid,
                plan_revision,
                message,
            )
            .await;
        }
    };
    let response = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionClient>()
            .apply_planned_amendment(Json::from(ExecutionAmendmentRequest {
                run: ExecutionRunRequest {
                    tenant_id: request.tenant_id,
                    contact_id: request.contact_id,
                    session_id: request.session_id,
                    run_uid: request.run_uid,
                },
                expected_plan_revision: plan_revision,
                amendment,
            })),
    )
    .call()
    .await?
    .into_inner();
    match response {
        ExecutionMutationResponse::Applied { .. }
        | ExecutionMutationResponse::Replayed { .. }
        | ExecutionMutationResponse::Conflict { .. } => Ok(RunDriveStep::Continue),
        ExecutionMutationResponse::NotFound => {
            Err(TerminalError::new_with_code(404, "execution run not found").into())
        }
    }
}

async fn prepare_amendment_planning(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    request: &ExecutionRunWorkflowRequest,
    plan_revision: u64,
    available_tool_names: &BTreeSet<String>,
) -> Result<Option<PreparedAmendmentPlanning>, HandlerError> {
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Err(TerminalError::new_with_code(404, "execution run not found").into());
    };
    if snapshot.run.tenant_id != request.tenant_id
        || snapshot.run.contact_id != request.contact_id
        || snapshot.run.session_id != request.session_id
    {
        return Err(TerminalError::new_with_code(409, "execution scope mismatch").into());
    }
    if snapshot.run.plan_revision != plan_revision
        || snapshot.run.status != ExecutionRunStatus::WaitingReplan
    {
        return Ok(None);
    }
    let waiting_tasks = snapshot
        .projection
        .tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .collect::<Vec<_>>();
    let [waiting_task] = waiting_tasks.as_slice() else {
        return Err(TerminalError::new(
            "amendment planning requires exactly one WaitingReplan task",
        )
        .into());
    };
    let Some(outcome) = waiting_task.outcome.as_ref() else {
        return Err(TerminalError::new("WaitingReplan task has no persisted outcome").into());
    };
    let moa_artifacts::execution_plan::ExecutionTaskResult::NeedsReplan { reason, evidence } =
        &outcome.result
    else {
        return Err(TerminalError::new("WaitingReplan task has no NeedsReplan evidence").into());
    };
    let failure_evidence = bounded_failure_evidence(reason, evidence)?;
    let planning_context = repository
        .load_planning_context(scope, snapshot.run.planning_context_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new("execution planning context does not exist"))?;
    if planning_context.planning_context_hash != snapshot.run.planning_context_hash
        || planning_context.snapshot.tenant_id != snapshot.run.tenant_id
        || planning_context.snapshot.contact_id != snapshot.run.contact_id
        || planning_context.snapshot.session_id != snapshot.run.session_id
        || planning_context.snapshot.originating_user_sequence_num
            != snapshot.run.originating_user_sequence_num
        || planning_context.snapshot.owner_user_id != snapshot.run.owner_user_id
        || planning_context.snapshot.catalog != snapshot.catalog
        || planning_context.snapshot.authorization != snapshot.authorization
        || planning_context.snapshot.pinned_instruction_skills != snapshot.pinned_instruction_skills
    {
        return Err(TerminalError::new_with_code(
            409,
            "persisted amendment planning authority does not match the active run",
        )
        .into());
    }
    let mut effective_context = planning_context.snapshot;
    effective_context.budget = snapshot.run.approved_budget.clone();
    let context = narrow_amendment_context(effective_context, available_tool_names)
        .map_err(execution_error)?;
    let remaining_budget = snapshot
        .budget_ledger
        .remaining_limit()
        .map_err(execution_error)?;
    let waiting_task_id = waiting_task.task_id;
    Ok(Some(PreparedAmendmentPlanning {
        context,
        evidence: AmendmentPlanningEvidence {
            goal: snapshot.run.goal,
            active_plan: snapshot.run.active_plan,
            projection: snapshot.projection,
            failure_evidence,
            waiting_task: waiting_task_id,
        },
        remaining_budget,
        now: chrono::Utc::now(),
    }))
}

fn bounded_failure_evidence(reason: &str, evidence: &Value) -> Result<Value, HandlerError> {
    let failure_evidence = json!({"reason": reason, "evidence": evidence});
    let encoded = moa_artifacts::canonical::canonical_json_bytes(&failure_evidence)
        .map_err(|error| TerminalError::new(error.to_string()))?;
    if encoded.len() > EXECUTION_REPORT_MAX_BYTES {
        return Err(TerminalError::new_with_code(
            422,
            "WaitingReplan failure evidence exceeds the bounded planner envelope",
        )
        .into());
    }
    Ok(failure_evidence)
}

fn narrow_amendment_context(
    mut context: ExecutionPlanningContextSnapshot,
    available_tool_names: &BTreeSet<String>,
) -> moa_execution::Result<ExecutionPlanningContextSnapshot> {
    use moa_execution::capability::CapabilitySource;

    let retained_refs = context
        .catalog
        .capabilities
        .iter()
        .filter(|capability| match &capability.source {
            CapabilitySource::BuiltInTool { name }
            | CapabilitySource::HandTool { name }
            | CapabilitySource::McpTool { name, .. } => available_tool_names.contains(name),
            CapabilitySource::ActionArtifact { tool_name, .. }
            | CapabilitySource::ConnectorAction { tool_name, .. }
            | CapabilitySource::SkillAction { tool_name, .. }
            | CapabilitySource::Memory { tool_name, .. } => {
                available_tool_names.contains(tool_name)
            }
            CapabilitySource::SkillCode { .. }
            | CapabilitySource::Knowledge { .. }
            | CapabilitySource::Model => true,
        })
        .map(|capability| capability.reference.clone())
        .collect::<Vec<_>>();
    narrow_authorized_capability_refs(&mut context.authorization.capability_refs, &retained_refs);
    context
        .validate()
        .map_err(|error| moa_execution::Error::InvalidRepositoryInput {
            message: error.to_string(),
        })?;
    Ok(context)
}

fn narrow_authorized_capability_refs(
    authorized: &mut Vec<moa_artifacts::execution_plan::CapabilityReference>,
    live: &[moa_artifacts::execution_plan::CapabilityReference],
) {
    authorized.retain(|reference| live.contains(reference));
}

async fn persist_amendment_audit(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    envelope: ExecutionPlanningAuditEnvelope,
) -> Result<(), HandlerError> {
    match &envelope.payload {
        ExecutionPlanningAuditPayload::PlannerCall { .. } => {
            let result = repository
                .write_planner_call_audit(scope, &envelope)
                .await
                .map_err(execution_error)?;
            record_applied_planning_audit(&result);
            if matches!(result, PlannerCallAuditWriteOutcome::Conflict { .. }) {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution amendment planner audit conflicts with first persisted evidence",
                )
                .into());
            }
        }
        ExecutionPlanningAuditPayload::Compile { .. } => {
            let result = repository
                .write_compile_audit(scope, &envelope)
                .await
                .map_err(execution_error)?;
            record_applied_planning_audit(&result);
            if matches!(result, CompileAuditWriteOutcome::Conflict { .. }) {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution amendment compile audit conflicts with first persisted evidence",
                )
                .into());
            }
        }
        ExecutionPlanningAuditPayload::Route { .. } => {
            return Err(TerminalError::new_with_code(
                422,
                "execution amendment planning produced a route audit",
            )
            .into());
        }
    }
    Ok(())
}

async fn finalize_amendment_planner_stop(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
    expected_plan_revision: u64,
    message: String,
) -> Result<RunDriveStep, HandlerError> {
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Err(TerminalError::new_with_code(404, "execution run not found").into());
    };
    if snapshot.run.plan_revision != expected_plan_revision
        || snapshot.run.status != ExecutionRunStatus::WaitingReplan
    {
        return Ok(RunDriveStep::Continue);
    }
    finalize_replan_stop(
        &repository,
        scope,
        snapshot,
        ReplanExhaustion {
            reason: moa_execution::ReplanStopReason::NoProgress,
            description: format!("amendment planner stopped: {message}"),
        },
    )
    .await
}

async fn drive_once(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    request: ExecutionRunWorkflowRequest,
    config: moa_core::config::ExecutionConfig,
) -> Result<RunDriveStep, HandlerError> {
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Err(TerminalError::new_with_code(404, "execution run not found").into());
    };
    if snapshot.run.tenant_id != request.tenant_id
        || snapshot.run.contact_id != request.contact_id
        || snapshot.run.session_id != request.session_id
    {
        return Err(TerminalError::new_with_code(409, "execution scope mismatch").into());
    }
    if snapshot.run.status.is_terminal() {
        return Ok(terminal_step(
            &snapshot.projection.tasks,
            format!("execution run ended as {}", snapshot.run.status.as_str()),
        ));
    }
    if snapshot.run.status == ExecutionRunStatus::AwaitingConfirmation {
        return Ok(park_at_epoch(&snapshot.run));
    }
    let now = chrono::Utc::now();
    let schedule_request = ScheduleRequest {
        run_uid: snapshot.run.run_uid,
        goal: snapshot.run.goal.clone(),
        plan: snapshot.run.active_plan.clone(),
        run_input: snapshot.run.input.clone(),
        catalog: snapshot.catalog.clone(),
        projection: snapshot.projection.clone(),
        config,
        budget_ledger: snapshot.budget_ledger.clone(),
        now,
    };
    let empty_map_nodes = match ready_empty_map_nodes(&schedule_request) {
        Ok(node_ids) => node_ids,
        Err(error) => {
            return finalize_internal_failure(&repository, scope, snapshot, error.to_string())
                .await;
        }
    };
    let mut applied_empty_map = false;
    for node_id in empty_map_nodes {
        let marker = ExecutionNodeMaterialization::Map {
            node_id,
            fanout_items: 0,
        };
        match repository
            .materialize_node(
                scope,
                snapshot.run.run_uid,
                snapshot.run.plan_revision,
                Some(marker),
                Vec::new(),
            )
            .await
            .map_err(execution_error)?
        {
            MaterializationOutcome::Applied(evidence) => {
                let marker = evidence.marker.as_ref().ok_or_else(|| {
                    TerminalError::new("empty map application omitted its durable marker")
                })?;
                record_materialization_marker(&snapshot.run, marker)?;
                applied_empty_map = true;
            }
            MaterializationOutcome::Replayed { tasks } => {
                if !tasks.is_empty() {
                    return Err(TerminalError::new(
                        "empty map replay unexpectedly returned logical tasks",
                    )
                    .into());
                }
            }
            MaterializationOutcome::Conflict => return Ok(RunDriveStep::Continue),
        }
    }
    if applied_empty_map {
        return Ok(RunDriveStep::Continue);
    }
    let scheduled = match schedule(schedule_request) {
        Ok(scheduled) => scheduled,
        Err(error) => {
            return finalize_internal_failure(&repository, scope, snapshot, error.to_string())
                .await;
        }
    };
    let mut snapshot = snapshot;
    snapshot.projection = scheduled.effective_projection;
    match scheduled.decision {
        ScheduleDecision::Ready(tasks) => {
            let mut tasks_by_node = BTreeMap::<String, Vec<_>>::new();
            for task in tasks {
                tasks_by_node
                    .entry(task.node_id.clone())
                    .or_default()
                    .push(task);
            }
            let mut records = Vec::new();
            for (node_id, tasks) in tasks_by_node {
                let marker = node_materialization_marker(&snapshot.run, &node_id, &tasks)?;
                match repository
                    .materialize_node(
                        scope,
                        snapshot.run.run_uid,
                        snapshot.run.plan_revision,
                        marker,
                        tasks,
                    )
                    .await
                    .map_err(execution_error)?
                {
                    MaterializationOutcome::Applied(evidence) => {
                        if let Some(marker) = evidence.marker.as_ref() {
                            record_materialization_marker(&snapshot.run, marker)?;
                        }
                        for task in &evidence.tasks {
                            if evidence
                                .inserted_task_ids
                                .binary_search(&task.task_id)
                                .is_ok()
                            {
                                record_applied_task_transition(None, task);
                            }
                        }
                        records.extend(evidence.tasks);
                    }
                    MaterializationOutcome::Replayed { tasks } => records.extend(tasks),
                    MaterializationOutcome::Conflict => return Ok(RunDriveStep::Continue),
                }
            }
            Ok(RunDriveStep::Dispatch {
                tasks: records
                    .into_iter()
                    .filter(|task| !task.status.is_terminal())
                    .map(|task| ExecutionTaskWorkflowRequest {
                        run_uid: task.run_uid,
                        task_id: task.task_id,
                        generation: task.generation,
                        tenant_id: task.tenant_id,
                        contact_id: task.contact_id,
                        session_id: snapshot.run.session_id,
                    })
                    .collect(),
            })
        }
        ScheduleDecision::Waiting(waiting) => {
            if let Some(reason) = immediately_knowable_replan_stop(&snapshot) {
                return finalize_replan_stop(&repository, scope, snapshot, reason).await;
            }
            let waiting_status = waiting_status(&snapshot.projection.tasks, &waiting);
            let transition = repository
                .transition_run_wait_with_reasons(
                    scope,
                    snapshot.run.run_uid,
                    snapshot.run.status,
                    waiting_status,
                    waiting,
                )
                .await
                .map_err(execution_error)?;
            if let TransitionOutcome::RunApplied(run) = &transition {
                record_applied_run_transition(Some(snapshot.run.status), run);
            }
            Ok(wait_transition_step(
                waiting_status,
                matches!(
                    transition,
                    TransitionOutcome::RunApplied(_) | TransitionOutcome::RunAlreadyApplied(_)
                ),
                snapshot.run.plan_revision,
                snapshot.run.wake_epoch,
            ))
        }
        ScheduleDecision::Terminal(terminal) => {
            let cause = terminal_cause(
                &snapshot.projection,
                &snapshot.budget_ledger,
                &terminal,
                now,
            );
            finalize(&repository, scope, snapshot, terminal, cause).await
        }
        ScheduleDecision::NoProgress { pending_node_ids } => {
            let evaluation = evaluate_completion(CompletionEvaluationRequest {
                goal: snapshot.run.goal.clone(),
                plan: snapshot.run.active_plan.clone(),
                run_input: snapshot.run.input.clone(),
                projection: snapshot.projection.clone(),
                terminal_output: snapshot.run.output.clone(),
                budget_ledger: snapshot.budget_ledger.clone(),
                now: chrono::Utc::now(),
            })
            .map_err(execution_error)?;
            let terminal = terminal_projection_from_evaluation(
                &evaluation,
                snapshot.run.output.clone(),
                Some(format!(
                    "scheduler made no progress with pending nodes: {}",
                    pending_node_ids.join(", ")
                )),
            );
            let terminal_evidence = terminal_evidence_from_evaluation(
                ExecutionTerminalCause::SchedulerNoProgress,
                &evaluation,
            )
            .map_err(execution_error)?;
            let terminal_reason =
                execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
                    .map_err(execution_error)?;
            match repository
                .finalize_run(
                    scope,
                    RunFinalizationRequest {
                        run_uid: snapshot.run.run_uid,
                        expected_revision: snapshot.run.plan_revision,
                        expected_wake_epoch: snapshot.run.wake_epoch,
                        terminal_projection: terminal,
                        completion_evaluation: evaluation,
                        terminal_evidence,
                        terminal_reason,
                    },
                )
                .await
                .map_err(execution_error)?
            {
                FinalizationOutcome::Finalized(run) => {
                    record_applied_run_transition(Some(snapshot.run.status), &run);
                    Ok(terminal_step(
                        &snapshot.projection.tasks,
                        "execution scheduler made no progress".to_string(),
                    ))
                }
                FinalizationOutcome::Replayed(_) => Ok(terminal_step(
                    &snapshot.projection.tasks,
                    "execution scheduler made no progress".to_string(),
                )),
                FinalizationOutcome::Conflict => Ok(RunDriveStep::Continue),
                FinalizationOutcome::NotFound => {
                    Err(TerminalError::new_with_code(404, "execution run not found").into())
                }
            }
        }
    }
}

async fn finalize_replan_stop(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    snapshot: moa_execution::repository::ExecutionSchedulingSnapshot,
    stop: ReplanExhaustion,
) -> Result<RunDriveStep, HandlerError> {
    let ReplanExhaustion {
        reason,
        description,
    } = stop;
    let waiting_task = snapshot
        .projection
        .tasks
        .iter()
        .find(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .ok_or_else(|| TerminalError::new("replan stop has no originating waiting-replan task"))?;
    let mut evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: snapshot.run.goal.clone(),
        plan: snapshot.run.active_plan.clone(),
        run_input: snapshot.run.input.clone(),
        projection: snapshot.projection.clone(),
        terminal_output: snapshot.run.output.clone(),
        budget_ledger: snapshot.budget_ledger.clone(),
        now: chrono::Utc::now(),
    })
    .map_err(execution_error)?;
    evaluation.status = replan_stop_status(
        snapshot.run.output.is_some(),
        evaluation.satisfied_requirement_ids.len(),
    );
    let stop_gaps = replan_stop_gaps(reason, Some(&description));
    evaluation.gaps.extend(stop_gaps.iter().cloned());
    evaluation.gaps.sort();
    evaluation.gaps.dedup();
    let terminal =
        terminal_projection_from_evaluation(&evaluation, snapshot.run.output.clone(), None);
    let terminal_evidence = terminal_evidence_from_evaluation(
        ExecutionTerminalCause::ReplanStop { reason },
        &evaluation,
    )
    .map_err(execution_error)?;
    let terminal_reason =
        execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
            .map_err(execution_error)?;
    let task_ids = snapshot
        .projection
        .tasks
        .iter()
        .map(|task| task.task_id)
        .collect::<Vec<_>>();
    let cancellation_reason = stop_gaps
        .get(1)
        .or_else(|| stop_gaps.first())
        .cloned()
        .ok_or_else(|| TerminalError::new("replan stop omitted typed gap evidence"))?;
    match repository
        .finalize_replan_stop(
            scope,
            ReplanStopRequest {
                run_uid: snapshot.run.run_uid,
                expected_revision: snapshot.run.plan_revision,
                expected_wake_epoch: snapshot.run.wake_epoch,
                task_id: waiting_task.task_id,
                expected_generation: waiting_task.generation,
                amendment_hash: None,
                cancellation_reason: cancellation_reason.clone(),
                terminal_projection: terminal,
                completion_evaluation: evaluation,
                terminal_evidence,
                terminal_reason,
            },
        )
        .await
        .map_err(execution_error)?
    {
        ReplanStopOutcome::Finalized(finalized) => {
            record_applied_run_transition(Some(snapshot.run.status), &finalized.run);
            Ok(RunDriveStep::Terminal {
                task_ids,
                reason: cancellation_reason,
            })
        }
        ReplanStopOutcome::Replayed(_) => Ok(RunDriveStep::Terminal {
            task_ids,
            reason: cancellation_reason,
        }),
        ReplanStopOutcome::Conflict => Ok(RunDriveStep::Continue),
        ReplanStopOutcome::NotFound => {
            Err(TerminalError::new_with_code(404, "execution run not found").into())
        }
    }
}

fn immediately_knowable_replan_stop(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
) -> Option<ReplanExhaustion> {
    if !snapshot
        .projection
        .tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::WaitingReplan)
    {
        return None;
    }
    replan_exhaustion_reason(&snapshot.budget_ledger, chrono::Utc::now())
}

fn replan_exhaustion_reason(
    ledger: &moa_execution::budget::BudgetLedger,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReplanExhaustion> {
    if ledger
        .limit
        .deadline_at
        .is_some_and(|deadline| now > deadline)
    {
        return Some(ReplanExhaustion {
            reason: moa_execution::ReplanStopReason::DeadlineExceeded,
            description: "deadline exceeded".to_string(),
        });
    }
    let mut dimensions = Vec::new();
    if ledger.overrun {
        dimensions.push("overrun");
    }
    if budget_dimension_exhausted(
        ledger.limit.max_cost_microusd,
        ledger.consumed.cost_microusd,
        ledger.reserved.cost_microusd,
    ) {
        dimensions.push("cost_microusd");
    }
    if budget_dimension_exhausted(
        ledger.limit.max_tokens,
        ledger.consumed.tokens,
        ledger.reserved.tokens,
    ) {
        dimensions.push("tokens");
    }
    if budget_dimension_exhausted(
        ledger.limit.max_tasks,
        ledger.consumed.tasks,
        ledger.reserved.tasks,
    ) {
        dimensions.push("tasks");
    }
    if budget_dimension_exhausted(
        ledger.limit.max_tool_calls,
        ledger.consumed.tool_calls,
        ledger.reserved.tool_calls,
    ) {
        dimensions.push("tool_calls");
    }
    if budget_dimension_exhausted(
        ledger.limit.max_retrieved_bytes,
        ledger.consumed.retrieved_bytes,
        ledger.reserved.retrieved_bytes,
    ) {
        dimensions.push("retrieved_bytes");
    }
    (!dimensions.is_empty()).then(|| ReplanExhaustion {
        reason: moa_execution::ReplanStopReason::BudgetExhausted,
        description: format!("budget exhausted: {}", dimensions.join(", ")),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReplanExhaustion {
    reason: moa_execution::ReplanStopReason,
    description: String,
}

fn budget_dimension_exhausted(limit: Option<u64>, consumed: u64, reserved: u64) -> bool {
    limit.is_some_and(|limit| consumed.saturating_add(reserved) >= limit)
}

fn terminal_cause(
    projection: &moa_execution::state::ExecutionProjection,
    budget_ledger: &moa_execution::budget::BudgetLedger,
    terminal: &TerminalProjection,
    now: chrono::DateTime<chrono::Utc>,
) -> ExecutionTerminalCause {
    let deadline_exceeded = budget_ledger
        .limit
        .deadline_at
        .is_some_and(|deadline| now > deadline);
    let unfinished_work = projection.node_statuses.values().any(|status| {
        !matches!(
            status,
            moa_execution::state::ExecutionNodeStatus::Completed
                | moa_execution::state::ExecutionNodeStatus::Skipped
                | moa_execution::state::ExecutionNodeStatus::Failed
                | moa_execution::state::ExecutionNodeStatus::Cancelled
        )
    });
    if deadline_exceeded && unfinished_work {
        return ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::DeadlineExceeded,
        };
    }
    if let Some(class) = projection.tasks.iter().find_map(|task| {
        task.outcome
            .as_ref()
            .and_then(|outcome| match &outcome.result {
                moa_artifacts::execution_plan::ExecutionTaskResult::Failed { class, .. } => {
                    Some(class.clone())
                }
                _ => None,
            })
    }) {
        return ExecutionTerminalCause::TaskFailure { class };
    }
    let budget_stopped_dispatch = matches!(
        terminal,
        TerminalProjection::Failed { failure }
            if failure.class == moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded
    ) || matches!(
        terminal,
        TerminalProjection::Partial { gaps, .. }
            if gaps.iter().any(|gap| gap == "execution budget cannot reserve required work")
    );
    if budget_stopped_dispatch {
        return ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::BudgetExceeded,
        };
    }
    if matches!(terminal, TerminalProjection::Cancelled { .. }) {
        return ExecutionTerminalCause::Cancellation;
    }
    if let TerminalProjection::Failed { failure } = terminal {
        return ExecutionTerminalCause::TaskFailure {
            class: failure.class.clone(),
        };
    }
    let limit_stop = if deadline_exceeded {
        Some(ExecutionLimitStop::DeadlineExceeded)
    } else if budget_ledger.overrun {
        Some(ExecutionLimitStop::BudgetExceeded)
    } else {
        None
    };
    ExecutionTerminalCause::Completion { limit_stop }
}

async fn finalize_internal_failure(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    snapshot: moa_execution::repository::ExecutionSchedulingSnapshot,
    message: String,
) -> Result<RunDriveStep, HandlerError> {
    let mut unsatisfied_requirement_ids = snapshot
        .run
        .goal
        .requirements
        .iter()
        .map(|requirement| requirement.id.clone())
        .collect::<Vec<_>>();
    unsatisfied_requirement_ids.sort();
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Failed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids,
        gaps: vec![format!("internal execution failure: {message}")],
    };
    let terminal = TerminalProjection::Failed {
        failure: moa_execution::state::ExecutionTaskFailure {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
            message: message.clone(),
            capability_ref: None,
        },
    };
    let terminal_evidence =
        terminal_evidence_from_evaluation(ExecutionTerminalCause::InternalFailure, &evaluation)
            .map_err(execution_error)?;
    let terminal_reason =
        execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
            .map_err(execution_error)?;
    match repository
        .finalize_run(
            scope,
            RunFinalizationRequest {
                run_uid: snapshot.run.run_uid,
                expected_revision: snapshot.run.plan_revision,
                expected_wake_epoch: snapshot.run.wake_epoch,
                terminal_projection: terminal,
                completion_evaluation: evaluation,
                terminal_evidence,
                terminal_reason,
            },
        )
        .await
        .map_err(execution_error)?
    {
        FinalizationOutcome::Finalized(run) => {
            record_applied_run_transition(Some(snapshot.run.status), &run);
            Ok(terminal_step(
                &snapshot.projection.tasks,
                format!("internal execution failure: {message}"),
            ))
        }
        FinalizationOutcome::Replayed(_) => Ok(terminal_step(
            &snapshot.projection.tasks,
            format!("internal execution failure: {message}"),
        )),
        FinalizationOutcome::Conflict => Ok(RunDriveStep::Continue),
        FinalizationOutcome::NotFound => {
            Err(TerminalError::new_with_code(404, "execution run not found").into())
        }
    }
}

async fn finalize(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    snapshot: moa_execution::repository::ExecutionSchedulingSnapshot,
    mut terminal: TerminalProjection,
    cause: ExecutionTerminalCause,
) -> Result<RunDriveStep, HandlerError> {
    let task_failure_gaps = snapshot
        .projection
        .tasks
        .iter()
        .filter_map(|task| {
            task.outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    moa_artifacts::execution_plan::ExecutionTaskResult::Failed {
                        message, ..
                    } => Some(message.clone()),
                    _ => None,
                })
        })
        .collect::<Vec<_>>();
    let terminal_output = match &terminal {
        TerminalProjection::Completed { output } => Some(output.clone()),
        TerminalProjection::Partial { output, .. } | TerminalProjection::Blocked { output, .. } => {
            output.clone()
        }
        TerminalProjection::Unsupported { .. }
        | TerminalProjection::Failed { .. }
        | TerminalProjection::Cancelled { .. } => None,
    };
    let task_ids = snapshot
        .projection
        .tasks
        .iter()
        .map(|task| task.task_id)
        .collect::<Vec<_>>();
    let mut evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: snapshot.run.goal.clone(),
        plan: snapshot.run.active_plan.clone(),
        run_input: snapshot.run.input.clone(),
        projection: snapshot.projection,
        terminal_output: terminal_output.clone(),
        budget_ledger: snapshot.budget_ledger,
        now: chrono::Utc::now(),
    })
    .map_err(execution_error)?;
    evaluation.gaps.extend(task_failure_gaps);
    evaluation.gaps.sort();
    evaluation.gaps.dedup();
    if !terminal_projection_matches_evaluation(&terminal, evaluation.status) {
        terminal = terminal_projection_from_evaluation(&evaluation, terminal_output, None);
    }
    let terminal_reason = format!("execution run reached terminal projection {terminal:?}");
    let terminal_evidence =
        terminal_evidence_from_evaluation(cause, &evaluation).map_err(execution_error)?;
    let selected_terminal_reason =
        execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
            .map_err(execution_error)?;
    match repository
        .finalize_run(
            scope,
            RunFinalizationRequest {
                run_uid: snapshot.run.run_uid,
                expected_revision: snapshot.run.plan_revision,
                expected_wake_epoch: snapshot.run.wake_epoch,
                terminal_projection: terminal,
                completion_evaluation: evaluation,
                terminal_evidence,
                terminal_reason: selected_terminal_reason,
            },
        )
        .await
        .map_err(execution_error)?
    {
        FinalizationOutcome::Finalized(run) => {
            record_applied_run_transition(Some(snapshot.run.status), &run);
            Ok(RunDriveStep::Terminal {
                task_ids,
                reason: terminal_reason,
            })
        }
        FinalizationOutcome::Replayed(_) => Ok(RunDriveStep::Terminal {
            task_ids,
            reason: terminal_reason,
        }),
        FinalizationOutcome::Conflict => Ok(RunDriveStep::Continue),
        FinalizationOutcome::NotFound => {
            Err(TerminalError::new_with_code(404, "execution run not found").into())
        }
    }
}

fn terminal_projection_matches_evaluation(
    terminal: &TerminalProjection,
    status: CompletionStatus,
) -> bool {
    matches!(
        (terminal, status),
        (
            TerminalProjection::Completed { .. },
            CompletionStatus::Completed
        ) | (
            TerminalProjection::Partial { .. },
            CompletionStatus::Partial
        ) | (
            TerminalProjection::Blocked { .. },
            CompletionStatus::Blocked
        ) | (
            TerminalProjection::Unsupported { .. },
            CompletionStatus::Unsupported
        ) | (TerminalProjection::Failed { .. }, CompletionStatus::Failed)
            | (TerminalProjection::Cancelled { .. }, _)
    )
}

fn terminal_step(
    tasks: &[moa_execution::state::ExecutionTaskProjection],
    reason: String,
) -> RunDriveStep {
    RunDriveStep::Terminal {
        task_ids: tasks.iter().map(|task| task.task_id).collect(),
        reason,
    }
}

fn node_materialization_marker(
    run: &ExecutionRunRecord,
    node_id: &str,
    tasks: &[moa_execution::state::LogicalTask],
) -> Result<Option<ExecutionNodeMaterialization>, HandlerError> {
    let node = run
        .active_plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| TerminalError::new("materialized execution node disappeared"))?;
    match &node.operation {
        ExecutionOperation::Map { .. } => Ok(Some(ExecutionNodeMaterialization::Map {
            node_id: node_id.to_string(),
            fanout_items: u64::try_from(tasks.len())
                .map_err(|_| TerminalError::new("map fanout exceeds u64"))?,
        })),
        ExecutionOperation::Reduce { batch_size, .. } => {
            let mut item_count = 0_u64;
            for task in tasks {
                let count = task
                    .input
                    .get("items")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .ok_or_else(|| TerminalError::new("reducer task omitted its item batch"))?;
                item_count = item_count
                    .checked_add(
                        u64::try_from(count)
                            .map_err(|_| TerminalError::new("reducer batch exceeds u64"))?,
                    )
                    .ok_or_else(|| TerminalError::new("reducer item count overflowed"))?;
            }
            Ok(Some(ExecutionNodeMaterialization::Reduce {
                node_id: node_id.to_string(),
                reducer_depth: reducer_depth(item_count, *batch_size),
            }))
        }
        ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => Ok(None),
    }
}

fn record_materialization_marker(
    run: &ExecutionRunRecord,
    marker: &ExecutionNodeMaterialization,
) -> Result<(), HandlerError> {
    match marker {
        ExecutionNodeMaterialization::Map { fanout_items, .. } => {
            record_execution_map_fanout_items(*fanout_items);
        }
        ExecutionNodeMaterialization::Reduce {
            node_id,
            reducer_depth,
        } => {
            let reducer = run
                .active_plan
                .definition
                .nodes
                .iter()
                .find(|node| node.id == *node_id)
                .and_then(|node| match &node.operation {
                    ExecutionOperation::Reduce { reducer, .. } => Some(reducer),
                    ExecutionOperation::Capability { .. }
                    | ExecutionOperation::Agent { .. }
                    | ExecutionOperation::Map { .. }
                    | ExecutionOperation::Review { .. }
                    | ExecutionOperation::WaitSignal { .. }
                    | ExecutionOperation::Output { .. } => None,
                })
                .ok_or_else(|| TerminalError::new("reducer marker lost its plan node"))?;
            let kind = match reducer {
                ExecutionReducer::Capability { .. } => ExecutionMetricReducerKind::Capability,
                ExecutionReducer::Agent { .. } => ExecutionMetricReducerKind::Agent,
            };
            record_execution_reducer_depth(kind, *reducer_depth);
        }
    }
    Ok(())
}

fn reducer_depth(mut item_count: u64, batch_size: u32) -> u64 {
    let batch_size = u64::from(batch_size);
    let mut depth = 0_u64;
    while item_count > 1 {
        item_count = item_count.div_ceil(batch_size);
        depth = depth.saturating_add(1);
    }
    depth
}

fn park_at_epoch(run: &ExecutionRunRecord) -> RunDriveStep {
    RunDriveStep::Park {
        processed_epoch: run.wake_epoch,
    }
}

fn wait_transition_step(
    waiting_status: ExecutionRunStatus,
    transition_applied: bool,
    plan_revision: u64,
    processed_epoch: u64,
) -> RunDriveStep {
    if transition_applied && waiting_status == ExecutionRunStatus::WaitingReplan {
        RunDriveStep::PlanAmendment { plan_revision }
    } else if transition_applied {
        RunDriveStep::Continue
    } else {
        RunDriveStep::Park { processed_epoch }
    }
}

fn waiting_status(
    tasks: &[moa_execution::state::ExecutionTaskProjection],
    waiting: &[WaitingReason],
) -> ExecutionRunStatus {
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::WaitingReplan)
    {
        ExecutionRunStatus::WaitingReplan
    } else if waiting
        .iter()
        .any(|reason| matches!(reason, WaitingReason::Input { .. }))
    {
        ExecutionRunStatus::WaitingInput
    } else if waiting.iter().any(|reason| {
        matches!(
            reason,
            WaitingReason::Review { .. } | WaitingReason::Signal { .. }
        )
    }) {
        ExecutionRunStatus::WaitingReview
    } else {
        ExecutionRunStatus::Running
    }
}

fn execution_scope(request: &ExecutionRunWorkflowRequest) -> ExecutionScope {
    request.contact_id.map_or(
        ExecutionScope::Tenant {
            tenant_id: request.tenant_id,
        },
        |contact_id| ExecutionScope::Contact {
            tenant_id: request.tenant_id,
            contact_id,
        },
    )
}

fn annotate_execution_run_span(run_uid: uuid::Uuid) {
    tracing::Span::current().set_attribute("moa.execution.run_uid", run_uid.to_string());
}

fn wake_promise_key(processed_epoch: u64) -> String {
    format!("execution_run_wake_after_{processed_epoch}")
}

fn wake_promise_epoch(
    processed_epoch: u64,
    awaited_epoch: u64,
    received_epoch: u64,
) -> Option<u64> {
    (received_epoch > processed_epoch && received_epoch > awaited_epoch).then_some(awaited_epoch)
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    TerminalError::new(format!("execution run workflow failed: {error}")).into()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{Duration, Utc};
    use moa_artifacts::execution_plan::{
        CapabilityReference, CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit,
        ExecutionFailureClass, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
        ExecutionPlanDefinition, ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult,
        ExecutionUsage, GeneratedAmendmentCandidate, PlanAmendment, PlanAmendmentOperation,
        RetryPolicy,
    };
    use moa_core::types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        execution_planning::{ExecutionSourceProvenance, GeneratedPlanPlannerProvenance},
        identifiers::{ModelId, SessionId, TenantId, UserId},
        model::ModelCapabilities,
        tools::IdempotencyClass,
    };
    use moa_execution::{
        ReplanStopReason,
        budget::BudgetLedger,
        capability::{
            CapabilitySource, ExecutionAuthorizationEnvelope, ExecutionCapability,
            ExecutionCapabilityCatalog, ExecutionClass, ExecutionEstimate, ExecutionHash,
        },
        compiler::{
            CompileExecutionRequest, ValidateAmendmentRequest, compile, validate_amendment,
        },
        repository::{
            ConfirmationOutcome, NewExecutionPlanningContext, NewExecutionRun,
            PlanningContextWriteOutcome, ReservationOutcome, TaskOutcomeWrite, TransitionOutcome,
        },
        state::{
            ExecutionLimitStop, ExecutionNodeStatus, ExecutionProjection, ExecutionTaskFailure,
            ExecutionTaskId, ExecutionTerminalCause, LogicalTask, LogicalTaskKind,
            TerminalProjection,
        },
        wire::{
            ExecutionAmendmentRequest, ExecutionMutationResponse, ExecutionPlanningContextSnapshot,
            ExecutionRunRequest, ExecutionRunWorkflowRequest, planning_context_hash,
        },
    };
    use moa_providers::ScriptedProvider;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        ExecutionRunStatus, ReplanExhaustion, RunDriveStep, bounded_failure_evidence,
        narrow_amendment_context, narrow_authorized_capability_refs, prepare_amendment_planning,
        replan_exhaustion_reason, terminal_cause, wait_transition_step, wake_promise_epoch,
    };

    #[tokio::test]
    async fn waiting_replan_uses_confirmed_budget_for_planning_apply_and_replay_db() {
        // Pins: confirmation may replace the initial planning budget, after which
        // WaitingReplan must use only the persisted run ledger for planning and apply.
        let test_db = ExecutionRunTestDb::new().await;
        let pool = test_db.pool().clone();
        let repository = moa_execution::repository::ExecutionRepository::new(pool.clone());
        let tenant_id = TenantId::new();
        let session_id = SessionId::new();
        let owner_user_id = UserId::new("confirmed-replan-owner");
        let scope = moa_execution::repository::ExecutionScope::Tenant { tenant_id };
        let catalog = ExecutionCapabilityCatalog::build(Vec::new())
            .expect("empty capability catalog should be valid");
        let authorization = ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: Vec::new(),
        };
        let planning_budget = replan_budget(1_000_000, 10);
        let confirmed_budget = replan_budget(2_000_000, 3);
        let goal = replan_goal();
        let compile_outcome = compile(CompileExecutionRequest {
            goal: goal.clone(),
            plan: replan_plan(),
            run_input: json!({}),
            catalog: catalog.clone(),
            authorization: authorization.clone(),
            approved_budget: planning_budget.clone(),
            config: moa_core::config::ExecutionConfig::default(),
            now: Utc::now(),
        });
        let compiled = compile_outcome.compiled.unwrap_or_else(|| {
            panic!(
                "replan fixture should compile within the initial planning budget: {:?}",
                compile_outcome.report.issues
            )
        });
        let compiled_plan = compiled.plan;
        let planning_snapshot = ExecutionPlanningContextSnapshot {
            schema_version: 1,
            tenant_id,
            contact_id: None,
            session_id,
            originating_user_sequence_num: 17,
            originating_user_event_hash: ExecutionHash::from_bytes([17; 32]).to_string(),
            owner_user_id: owner_user_id.clone(),
            catalog: catalog.clone(),
            authorization: authorization.clone(),
            pinned_instruction_skills: Vec::new(),
            execution_templates: Vec::new(),
            budget: planning_budget.clone(),
        };
        let planning_hash = planning_context_hash(&planning_snapshot)
            .expect("planning snapshot should have a canonical hash");
        let PlanningContextWriteOutcome::Created(planning_context) = repository
            .create_planning_context(
                scope,
                NewExecutionPlanningContext {
                    snapshot: planning_snapshot,
                    planning_context_hash: planning_hash,
                },
            )
            .await
            .expect("planning context should persist")
        else {
            panic!("fresh planning context should be created");
        };
        let run = repository
            .create_run(
                scope,
                NewExecutionRun {
                    tenant_id,
                    contact_id: None,
                    session_id,
                    originating_user_sequence_num: 17,
                    planning_context_uid: planning_context.planning_context_uid,
                    planning_context_hash: planning_context.planning_context_hash,
                    owner_user_id,
                    goal,
                    plan: compiled_plan.clone(),
                    catalog,
                    authorization,
                    pinned_instruction_skills: Vec::new(),
                    source_provenance: ExecutionSourceProvenance::GeneratedPlan {
                        route_rationale: "The workflow requires durable execution.".to_string(),
                        planner: GeneratedPlanPlannerProvenance {
                            model: "scripted-confirmed-replan".to_string(),
                            prompt_version: "confirmed-replan".to_string(),
                            candidate_hash: "a".repeat(64),
                            compiler_report_hash: "b".repeat(64),
                            final_plan_hash: compiled_plan.plan_hash.to_string(),
                            repair_attempts: 0,
                        },
                    },
                    input: json!({}),
                    status: ExecutionRunStatus::AwaitingConfirmation,
                    approved_budget: planning_budget.clone(),
                    idempotency_key: Some("confirmed-replan-budget".to_string()),
                },
            )
            .await
            .expect("awaiting-confirmation run should persist");
        let ConfirmationOutcome::Confirmed(confirmed) = repository
            .confirm_run(
                scope,
                run.run_uid,
                &run.active_plan_hash,
                confirmed_budget.clone(),
            )
            .await
            .expect("confirmation write should succeed")
        else {
            panic!("confirmation should replace the approved budget");
        };
        assert_eq!(confirmed.approved_budget, confirmed_budget);

        let tasks = vec![
            replan_task(run.run_uid, "prepare", json!({"value": "prepared"})),
            replan_task(run.run_uid, "output", json!({"value": "stale"})),
        ];
        repository
            .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
            .await
            .expect("confirmed run should materialize tasks");
        start_task(&repository, scope, run.run_uid, tasks[0].task_id).await;
        assert!(matches!(
            repository
                .record_task_outcome(
                    scope,
                    run.run_uid,
                    tasks[0].task_id,
                    1,
                    replan_outcome(ExecutionTaskResult::Completed {
                        output: json!({"value": "prepared"}),
                        citations: Vec::new(),
                    }),
                )
                .await
                .expect("prepare outcome should persist"),
            TaskOutcomeWrite::Applied { .. }
        ));
        start_task(&repository, scope, run.run_uid, tasks[1].task_id).await;
        assert!(matches!(
            repository
                .record_task_outcome(
                    scope,
                    run.run_uid,
                    tasks[1].task_id,
                    1,
                    replan_outcome(ExecutionTaskResult::NeedsReplan {
                        reason: "shape changed".to_string(),
                        evidence: json!({"kind": "confirmed-budget"}),
                    }),
                )
                .await
                .expect("NeedsReplan outcome should persist"),
            TaskOutcomeWrite::Applied { .. }
        ));

        let request = ExecutionRunWorkflowRequest {
            run_uid: run.run_uid,
            tenant_id,
            contact_id: None,
            session_id,
        };
        let prepared = prepare_amendment_planning(
            &repository,
            scope,
            &request,
            1,
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("confirmed WaitingReplan should prepare amendment planning")
        .expect("active WaitingReplan revision should produce planner input");
        assert_eq!(prepared.context.budget, confirmed_budget);
        assert_eq!(
            repository
                .load_planning_context(scope, planning_context.planning_context_uid)
                .await
                .expect("immutable planning context should reload")
                .expect("immutable planning context should remain present")
                .snapshot
                .budget,
            planning_budget
        );
        assert_eq!(prepared.remaining_budget.max_cost_microusd, Some(1_999_997));
        assert_eq!(prepared.remaining_budget.max_tokens, Some(199_997));
        assert_eq!(prepared.remaining_budget.max_tasks, Some(1));

        let provider = ScriptedProvider::new(ModelCapabilities::default())
            .push_text(replan_amendment_candidate());
        let planned = moa_brain::execution_planning::plan_amendment(
            &provider,
            moa_brain::execution_planning::ExecutionAmendmentPlanningRequest {
                run_uid: run.run_uid,
                base_plan_revision: 1,
                context: prepared.context,
                evidence: prepared.evidence,
                remaining_budget: prepared.remaining_budget,
                planner_model: ModelId::new("scripted-confirmed-replan"),
                config: moa_core::config::ExecutionConfig::default(),
                now: prepared.now,
            },
        )
        .await
        .expect("persisted confirmed budget should permit amendment planning");
        assert_eq!(provider.recorded_requests().len(), 1);
        let planner_prompt = serde_json::to_string(&provider.recorded_requests()[0].messages)
            .expect("recorded amendment request should serialize");
        assert!(
            planner_prompt.contains("1999997"),
            "amendment planner prompt must carry the reconciled confirmed budget"
        );
        let moa_brain::execution_planning::ExecutionAmendmentPlanningResultKind::Ready {
            amendment,
            ..
        } = planned.kind
        else {
            panic!("valid confirmed-budget amendment should be ready");
        };
        let amendment_request = ExecutionAmendmentRequest {
            run: ExecutionRunRequest {
                tenant_id,
                contact_id: None,
                session_id,
                run_uid: run.run_uid,
            },
            expected_plan_revision: 1,
            amendment,
        };
        let applied = crate::services::execution::apply_amendment_for_test(
            pool.clone(),
            moa_core::config::ExecutionConfig::default(),
            amendment_request.clone(),
        )
        .await
        .expect("planned amendment should apply through the production service boundary");
        assert!(
            matches!(
                applied,
                ExecutionMutationResponse::Applied { ref run } if run.plan_revision == 2
            ),
            "planned amendment should apply revision two: {applied:?}"
        );
        let replayed = crate::services::execution::apply_amendment_for_test(
            pool,
            moa_core::config::ExecutionConfig::default(),
            amendment_request.clone(),
        )
        .await
        .expect("exact amendment replay should remain idempotent");
        assert!(matches!(
            replayed,
            ExecutionMutationResponse::Replayed { ref run } if run.plan_revision == 2
        ));

        let mut injected =
            serde_json::to_value(amendment_request).expect("amendment request should serialize");
        injected
            .as_object_mut()
            .expect("amendment request wire shape should be an object")
            .insert(
                "approved_budget".to_string(),
                json!({"max_tasks": 1_000_000}),
            );
        serde_json::from_value::<ExecutionAmendmentRequest>(injected)
            .expect_err("caller-supplied amendment budget authority must be rejected");
        serde_json::from_value::<GeneratedAmendmentCandidate>(json!({
            "amendment": replan_amendment_value(),
            "approved_budget": {"max_tasks": 1_000_000}
        }))
        .expect_err("model-supplied amendment budget authority must be rejected");
    }

    fn replan_budget(max_resource: u64, max_tasks: u64) -> ExecutionBudgetLimit {
        ExecutionBudgetLimit {
            max_cost_microusd: Some(max_resource),
            max_tokens: Some(max_resource / 10),
            max_tasks: Some(max_tasks),
            max_tool_calls: Some(max_resource / 10_000),
            max_retrieved_bytes: Some(max_resource.saturating_mul(20)),
            deadline_at: Some(Utc::now() + Duration::hours(1)),
        }
    }

    struct ExecutionRunTestDb {
        pool: Option<sqlx::PgPool>,
        database_url: String,
        schema_name: String,
    }

    impl ExecutionRunTestDb {
        async fn new() -> Self {
            let (database_url, schema_name) = moa_session::testing::provision_cloned_database()
                .await
                .expect("execution test database should provision");
            let pool = sqlx::PgPool::connect(&database_url)
                .await
                .expect("execution test database should connect");
            Self {
                pool: Some(pool),
                database_url,
                schema_name,
            }
        }

        fn pool(&self) -> &sqlx::PgPool {
            self.pool
                .as_ref()
                .expect("execution test database pool should remain available")
        }
    }

    impl Drop for ExecutionRunTestDb {
        fn drop(&mut self) {
            let Some(pool) = self.pool.take() else {
                return;
            };
            let database_url = self.database_url.clone();
            let schema_name = self.schema_name.clone();
            let cleanup = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("execution test cleanup runtime should build");
                runtime.block_on(async move {
                    pool.close().await;
                    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
                })
            });
            cleanup
                .join()
                .expect("execution test cleanup thread should not panic")
                .expect("execution test database should clean up");
        }
    }

    fn replan_goal() -> ExecutionGoalContract {
        ExecutionGoalContract {
            objective: "repair with the confirmed budget".to_string(),
            requirements: vec![
                ExecutionRequirement {
                    id: "req_inputs".to_string(),
                    description: "prepare report inputs".to_string(),
                },
                ExecutionRequirement {
                    id: "req_report".to_string(),
                    description: "produce the repaired report".to_string(),
                },
            ],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "check_output".to_string(),
                description: "validate the repaired output".to_string(),
                requirement_ids: vec!["req_report".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        }
    }

    fn replan_plan() -> ExecutionPlanDefinition {
        ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![
                ExecutionNode {
                    id: "prepare".to_string(),
                    requirement_ids: vec!["req_inputs".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: json!({"type": "object"}),
                    operation: ExecutionOperation::Agent {
                        instructions: "prepare report inputs".to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    retry: RetryPolicy {
                        max_attempts: 1,
                        initial_backoff_ms: 0,
                        max_backoff_ms: 0,
                    },
                    budget: None,
                },
                replan_node(
                    "output",
                    vec!["prepare".to_string()],
                    json!({"value": "stale"}),
                ),
            ],
        }
    }

    fn replan_node(id: &str, depends_on: Vec<String>, value: serde_json::Value) -> ExecutionNode {
        ExecutionNode {
            id: id.to_string(),
            requirement_ids: vec!["req_report".to_string()],
            depends_on,
            when: None,
            input: json!({}),
            output_schema: json!({"type": "object"}),
            operation: ExecutionOperation::Output { value },
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            budget: None,
        }
    }

    fn replan_task(run_uid: Uuid, node_id: &str, value: serde_json::Value) -> LogicalTask {
        LogicalTask {
            task_id: ExecutionTaskId::derive(run_uid, node_id, "")
                .expect("fixture task id should derive"),
            node_id: node_id.to_string(),
            item_key: String::new(),
            requirement_ids: if node_id == "prepare" {
                vec!["req_inputs".to_string()]
            } else {
                vec!["req_report".to_string()]
            },
            plan_revision: 1,
            generation: 1,
            input: json!({}),
            kind: if node_id == "prepare" {
                LogicalTaskKind::Agent {
                    instructions: "prepare report inputs".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 1,
                }
            } else {
                LogicalTaskKind::Output { value }
            },
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            reservation: ExecutionEstimate {
                cost_microusd: 2,
                tokens: 2,
                tasks: 1,
                tool_calls: 2,
                retrieved_bytes: 2,
            },
        }
    }

    async fn start_task(
        repository: &moa_execution::repository::ExecutionRepository,
        scope: moa_execution::repository::ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
    ) {
        assert!(matches!(
            repository
                .reserve_task(scope, run_uid, task_id, 1)
                .await
                .expect("task reservation should succeed"),
            ReservationOutcome::Reserved(_)
        ));
        assert!(matches!(
            repository
                .mark_task_running(scope, run_uid, task_id, 1)
                .await
                .expect("task start should succeed"),
            TransitionOutcome::Applied(_)
        ));
    }

    fn replan_outcome(result: ExecutionTaskResult) -> ExecutionTaskOutcome {
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 1,
                tokens: 1,
                tool_calls: 1,
                retrieved_bytes: 1,
            },
            result,
        }
    }

    fn replan_amendment_candidate() -> String {
        json!({"amendment": replan_amendment_value()}).to_string()
    }

    fn replan_amendment_value() -> serde_json::Value {
        json!({
            "schema_version": 1,
            "base_plan_revision": 1,
            "reason": "replace stale output",
            "evidence": {"shape": "changed"},
            "operations": [
                {"kind": "remove_pending_node", "node_id": "output"},
                {
                    "kind": "add_node",
                    "node": {
                        "id": "replacement_output",
                        "requirement_ids": ["req_report"],
                        "depends_on": ["prepare"],
                        "when": null,
                        "input": {},
                        "output_schema": {"type": "object"},
                        "operation": {
                            "kind": "output",
                            "value": {"value": "repaired"}
                        },
                        "retry": {
                            "max_attempts": 1,
                            "initial_backoff_ms": 0,
                            "max_backoff_ms": 0
                        },
                        "budget": null
                    }
                }
            ]
        })
    }

    #[test]
    fn waiting_replan_invokes_revision_keyed_amendment_planner_instead_of_parking() {
        // Pins: the Task 6 driver hands one persisted WaitingReplan revision to
        // Task 7 instead of treating the wait as an externally awakened state.
        let step = wait_transition_step(ExecutionRunStatus::WaitingReplan, true, 7, 11);

        assert!(matches!(
            step,
            RunDriveStep::PlanAmendment { plan_revision: 7 }
        ));
    }

    #[test]
    fn amendment_context_preserves_persisted_catalog_and_only_narrows_authorization() {
        // Pins: live availability narrows transient amendment authority without
        // changing the immutable catalog hash pinned by the active plan.
        let retained = amendment_tool_capability("persisted.retained", "retained-tool");
        let unavailable = amendment_tool_capability("persisted.unavailable", "unavailable-tool");
        let retained_ref = retained.reference.clone();
        let unavailable_ref = unavailable.reference.clone();
        let catalog = ExecutionCapabilityCatalog::build(vec![retained, unavailable])
            .expect("two tool-backed capabilities should form a valid catalog");
        let authorization = ExecutionAuthorizationEnvelope {
            capability_refs: catalog
                .capabilities
                .iter()
                .map(|capability| capability.reference.clone())
                .collect(),
            skill_refs: Vec::new(),
        };
        let source = ExecutionPlanningContextSnapshot {
            schema_version: 1,
            tenant_id: TenantId::new(),
            contact_id: None,
            session_id: SessionId::new(),
            originating_user_sequence_num: 23,
            originating_user_event_hash: ExecutionHash::from_bytes([23; 32]).to_string(),
            owner_user_id: UserId::new("amendment-context-owner"),
            catalog,
            authorization,
            pinned_instruction_skills: Vec::new(),
            execution_templates: Vec::new(),
            budget: replan_budget(1_000_000, 10),
        };
        let persisted_source = source.clone();

        let narrowed = narrow_amendment_context(
            source.clone(),
            &BTreeSet::from(["retained-tool".to_string()]),
        )
        .expect("live availability should narrow a valid planning context");

        assert_eq!(narrowed.catalog, persisted_source.catalog);
        assert_eq!(
            narrowed.catalog.catalog_hash,
            persisted_source.catalog.catalog_hash
        );
        assert_eq!(
            moa_artifacts::canonical::canonical_json_bytes(&narrowed.catalog)
                .expect("narrowed catalog should serialize canonically"),
            moa_artifacts::canonical::canonical_json_bytes(&persisted_source.catalog)
                .expect("persisted catalog should serialize canonically")
        );
        assert_eq!(
            narrowed.authorization.capability_refs,
            vec![retained_ref.clone()]
        );
        let live_only_ref = CapabilityReference {
            name: "live.only".to_string(),
            version: "v1".to_string(),
        };
        assert!(
            !narrowed
                .authorization
                .capability_refs
                .contains(&live_only_ref)
        );
        assert_eq!(source, persisted_source);

        let mut active_plan = replan_plan();
        active_plan.nodes[0].operation = ExecutionOperation::Capability {
            reference: retained_ref.clone(),
        };
        let compile_outcome = compile(CompileExecutionRequest {
            goal: replan_goal(),
            plan: active_plan,
            run_input: json!({}),
            catalog: persisted_source.catalog.clone(),
            authorization: persisted_source.authorization.clone(),
            approved_budget: persisted_source.budget.clone(),
            config: moa_core::config::ExecutionConfig::default(),
            now: Utc::now(),
        });
        let compiled = compile_outcome.compiled.unwrap_or_else(|| {
            panic!(
                "active amendment fixture should compile: {:?}",
                compile_outcome.report.issues
            )
        });
        let mut replacement_prepare = compiled.plan.definition.nodes[0].clone();
        replacement_prepare.id = "replacement_prepare".to_string();
        let mut replacement_output = compiled.plan.definition.nodes[1].clone();
        replacement_output.id = "replacement_output".to_string();
        replacement_output.depends_on = vec!["replacement_prepare".to_string()];
        replacement_output.operation = ExecutionOperation::Output {
            value: json!({"$ref": "$.nodes.replacement_prepare.output"}),
        };
        let validation = ValidateAmendmentRequest {
            goal: compiled.goal,
            active_plan: compiled.plan,
            amendment: PlanAmendment {
                schema_version: 1,
                base_plan_revision: 1,
                reason: "Use the retained live capability".to_string(),
                evidence: json!({"availability": "narrowed"}),
                operations: vec![
                    PlanAmendmentOperation::ReplacePendingNode {
                        node_id: "prepare".to_string(),
                        node: replacement_prepare,
                    },
                    PlanAmendmentOperation::ReplacePendingNode {
                        node_id: "output".to_string(),
                        node: replacement_output,
                    },
                ],
            },
            projection: ExecutionProjection {
                plan_revision: 1,
                node_statuses: BTreeMap::from([
                    ("prepare".to_string(), ExecutionNodeStatus::Pending),
                    ("output".to_string(), ExecutionNodeStatus::Pending),
                ]),
                tasks: Vec::new(),
            },
            catalog: narrowed.catalog,
            authorization: narrowed.authorization,
            remaining_budget: replan_budget(1_000_000, 10),
            config: moa_core::config::ExecutionConfig::default(),
            now: Utc::now(),
        };
        let mut unavailable_validation = validation.clone();
        let PlanAmendmentOperation::ReplacePendingNode { node, .. } =
            &mut unavailable_validation.amendment.operations[0]
        else {
            panic!("first amendment operation should replace the prepare node");
        };
        node.operation = ExecutionOperation::Capability {
            reference: unavailable_ref,
        };

        let accepted = validate_amendment(validation);
        assert!(
            accepted.plan.is_some(),
            "retained capability amendment should validate: {:?}",
            accepted.report.issues
        );
        assert!(
            !accepted
                .report
                .issues
                .iter()
                .any(|issue| issue.code == "catalog_hash_changed")
        );

        let rejected = validate_amendment(unavailable_validation);
        assert!(rejected.plan.is_none());
        assert_eq!(
            rejected
                .report
                .issues
                .iter()
                .filter(|issue| issue.code == "capability_not_authorized")
                .count(),
            1
        );
    }

    #[test]
    fn amendment_live_authority_check_only_removes_persisted_capabilities() {
        // Pins: a live availability set is an intersection with persisted
        // planning authority; it cannot introduce a caller/model-selected ref.
        let persisted_a = moa_artifacts::execution_plan::CapabilityReference {
            name: "persisted-a".to_string(),
            version: "1".to_string(),
        };
        let persisted_b = moa_artifacts::execution_plan::CapabilityReference {
            name: "persisted-b".to_string(),
            version: "1".to_string(),
        };
        let live_only = moa_artifacts::execution_plan::CapabilityReference {
            name: "live-only".to_string(),
            version: "1".to_string(),
        };
        let mut authorized = vec![persisted_a, persisted_b.clone()];

        narrow_authorized_capability_refs(&mut authorized, &[persisted_b.clone(), live_only]);

        assert_eq!(authorized, vec![persisted_b]);
    }

    fn amendment_tool_capability(reference_name: &str, tool_name: &str) -> ExecutionCapability {
        ExecutionCapability {
            reference: CapabilityReference {
                name: reference_name.to_string(),
                version: "v1".to_string(),
            },
            description: format!("Capability {reference_name}"),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            action_class: ActionClass::Read,
            risk_level: RiskLevel::Low,
            default_effect: ActionPolicyEffect::Allow,
            idempotency_class: IdempotencyClass::Idempotent,
            execution_class: ExecutionClass::Data,
            source: CapabilitySource::BuiltInTool {
                name: tool_name.to_string(),
            },
            estimate: ExecutionEstimate {
                tool_calls: 1,
                tasks: 1,
                ..ExecutionEstimate::default()
            },
        }
    }

    #[test]
    fn amendment_failure_evidence_is_preserved_and_bounded_before_provider_use() {
        // Pins: runtime planning keeps exact structured NeedsReplan evidence,
        // while rejecting an over-cap value before any model call is possible.
        let evidence = json!({"shape": ["a", "b"]});
        assert_eq!(
            bounded_failure_evidence("shape changed", &evidence)
                .expect("small evidence should remain available"),
            json!({"reason": "shape changed", "evidence": evidence})
        );
        let oversized = json!({
            "body": "x".repeat(
                moa_core::types::execution_planning::EXECUTION_REPORT_MAX_BYTES
            )
        });
        let error = bounded_failure_evidence("shape changed", &oversized)
            .expect_err("oversized evidence should fail before planning");
        let message = <restate_sdk::prelude::HandlerError as AsRef<
            dyn std::error::Error + Send + Sync,
        >>::as_ref(&error)
        .to_string();
        assert!(message.contains("exceeds the bounded planner envelope"));
    }

    #[test]
    fn post_ack_wake_resolves_the_epoch_advertised_before_ack() {
        // Pins: while K_PROCESSED_WAKE_EPOCH still contains N after the DB has
        // acknowledged N+1, a wake for N+2 resolves promise N+1. A replay or
        // handler restart therefore awaits the same already-resolved promise.
        assert_eq!(wake_promise_epoch(7, 8, 9), Some(8));
        assert_eq!(wake_promise_epoch(8, 8, 8), None);
        assert_eq!(wake_promise_epoch(8, 9, 9), None);
    }

    #[test]
    fn waiting_replan_exhaustion_checks_every_reserved_resource_dimension_and_deadline() {
        // Pins: before parking WaitingReplan, exact consumed plus reserved
        // exhaustion is terminal evidence for every configured dimension.
        let now = Utc::now();
        let dimensions = [
            ("cost_microusd", 0),
            ("tokens", 1),
            ("tasks", 2),
            ("tool_calls", 3),
            ("retrieved_bytes", 4),
        ];
        for (name, index) in dimensions {
            let mut ledger = ledger();
            match index {
                0 => {
                    ledger.limit.max_cost_microusd = Some(5);
                    ledger.consumed.cost_microusd = 2;
                    ledger.reserved.cost_microusd = 3;
                }
                1 => {
                    ledger.limit.max_tokens = Some(5);
                    ledger.consumed.tokens = 2;
                    ledger.reserved.tokens = 3;
                }
                2 => {
                    ledger.limit.max_tasks = Some(5);
                    ledger.consumed.tasks = 2;
                    ledger.reserved.tasks = 3;
                }
                3 => {
                    ledger.limit.max_tool_calls = Some(5);
                    ledger.consumed.tool_calls = 2;
                    ledger.reserved.tool_calls = 3;
                }
                4 => {
                    ledger.limit.max_retrieved_bytes = Some(5);
                    ledger.consumed.retrieved_bytes = 2;
                    ledger.reserved.retrieved_bytes = 3;
                }
                _ => unreachable!("fixture dimension index is exhaustive"),
            }
            assert_eq!(
                replan_exhaustion_reason(&ledger, now),
                Some(ReplanExhaustion {
                    reason: ReplanStopReason::BudgetExhausted,
                    description: format!("budget exhausted: {name}"),
                })
            );
        }

        let mut deadline = ledger();
        deadline.limit.deadline_at = Some(now - Duration::milliseconds(1));
        assert_eq!(
            replan_exhaustion_reason(&deadline, now),
            Some(ReplanExhaustion {
                reason: ReplanStopReason::DeadlineExceeded,
                description: "deadline exceeded".to_string(),
            })
        );

        deadline.overrun = true;
        assert_eq!(
            replan_exhaustion_reason(&deadline, now),
            Some(ReplanExhaustion {
                reason: ReplanStopReason::DeadlineExceeded,
                description: "deadline exceeded".to_string(),
            }),
            "deadline must win when both typed limit conditions hold"
        );

        let mut available = ledger();
        available.limit.max_tokens = Some(6);
        available.consumed.tokens = 2;
        available.reserved.tokens = 3;
        assert_eq!(replan_exhaustion_reason(&available, now), None);
    }

    #[test]
    fn terminal_cause_selection_covers_limits_failures_completion_and_cancellation() {
        // Pins: zero-dispatch limits are distinct from ordinary completion;
        // deadline wins over simultaneous budget exhaustion.
        let now = Utc::now();
        let unfinished = ExecutionProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([("pending".to_string(), ExecutionNodeStatus::Pending)]),
            tasks: Vec::new(),
        };
        let budget_failure = TerminalProjection::Failed {
            failure: ExecutionTaskFailure {
                class: ExecutionFailureClass::BudgetExceeded,
                message: "budget".to_string(),
                capability_ref: None,
            },
        };
        let mut simultaneous = ledger();
        simultaneous.limit.deadline_at = Some(now - Duration::milliseconds(1));
        simultaneous.overrun = true;
        assert_eq!(
            terminal_cause(&unfinished, &simultaneous, &budget_failure, now),
            ExecutionTerminalCause::LimitStop {
                reason: ExecutionLimitStop::DeadlineExceeded
            }
        );

        let finished = ExecutionProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([("done".to_string(), ExecutionNodeStatus::Completed)]),
            tasks: Vec::new(),
        };
        assert_eq!(
            terminal_cause(&finished, &ledger(), &budget_failure, now),
            ExecutionTerminalCause::LimitStop {
                reason: ExecutionLimitStop::BudgetExceeded
            }
        );
        let typed_failure = TerminalProjection::Failed {
            failure: ExecutionTaskFailure {
                class: ExecutionFailureClass::InvalidOutput,
                message: "invalid".to_string(),
                capability_ref: None,
            },
        };
        assert_eq!(
            terminal_cause(&finished, &ledger(), &typed_failure, now),
            ExecutionTerminalCause::TaskFailure {
                class: ExecutionFailureClass::InvalidOutput
            }
        );
        assert_eq!(
            terminal_cause(
                &finished,
                &ledger(),
                &TerminalProjection::Cancelled {
                    reason: "cancelled".to_string(),
                },
                now,
            ),
            ExecutionTerminalCause::Cancellation
        );
        assert_eq!(
            terminal_cause(
                &finished,
                &ledger(),
                &TerminalProjection::Completed {
                    output: serde_json::json!({}),
                },
                now,
            ),
            ExecutionTerminalCause::Completion { limit_stop: None }
        );
        let mut overrun = ledger();
        overrun.overrun = true;
        assert_eq!(
            terminal_cause(
                &finished,
                &overrun,
                &TerminalProjection::Partial {
                    output: Some(serde_json::json!({})),
                    gaps: vec!["overrun".to_string()],
                },
                now,
            ),
            ExecutionTerminalCause::Completion {
                limit_stop: Some(ExecutionLimitStop::BudgetExceeded)
            }
        );
    }

    fn ledger() -> BudgetLedger {
        BudgetLedger {
            limit: ExecutionBudgetLimit {
                max_cost_microusd: None,
                max_tokens: None,
                max_tasks: None,
                max_tool_calls: None,
                max_retrieved_bytes: None,
                deadline_at: None,
            },
            reserved: ExecutionEstimate::default(),
            consumed: ExecutionEstimate::default(),
            overrun: false,
        }
    }
}
