//! Durable keyed workflow that executes one persisted logical task across generations.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage,
};
use moa_config::SessionLimitsConfig;
use moa_core::{
    traits::{ChannelAdapter, SessionStore as _},
    types::{
        action_policy::{
            ActionReviewOwner, ActionRuleScope, CapabilityProvenance, ExecutionTaskOrigin,
        },
        channel::Channel,
        completion::{
            CompletionContent, CompletionRequest, DEFER_BRAIN_RESPONSE_METADATA_KEY, StopReason,
            ToolCallContent,
        },
        context::ContextMessage,
        identifiers::ToolCallId,
        session::SessionMeta,
        tools::IdempotencyClass,
    },
};
use moa_execution::{
    capability::{CapabilitySource, ExecutionCapability},
    interpreter::validate_task_outcome,
    repository::{
        ActionReviewResolutionWrite, ExecutionRepository, ExecutionRunRecord, ExecutionScope,
        ExecutionTaskRecord, ReservationOutcome, TaskOutcomeWrite, TransitionOutcome,
    },
    schema::validate_instance,
    state::{
        ExecutionTaskStatus, LogicalTaskKind, cancelled_task_outcome, completed_task_outcome,
        exhaust_retry_outcome, failed_task_outcome, parse_agent_task_outcome, retry_delay_ms,
    },
    wire::{
        ExecutionActionReviewAcknowledgement, ExecutionActionReviewResolution,
        ExecutionActionReviewResolutionRequest, ExecutionInputRequest,
        ExecutionReviewDecisionRequest, ExecutionRunWakeReason, ExecutionRunWakeRequest,
        ExecutionSignalRequest, ExecutionTaskWorkflowRequest, ExecutionToolDispatchRejection,
    },
};
use moa_observability::propagation::link_remote_context_from_link_headers;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::ctx::RequestHeaders;

use crate::workflows::child_invocation::{ChildInvocationOutcome, cancel_and_join_child_call};
use crate::{
    services::{
        action_reviews::{
            ActionReviewsClient, ExecutionActionReviewSettlement,
            SettleExecutionActionReviewRequest,
        },
        llm_gateway::{
            LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient, attach_completion_owner,
            completion_idempotency_key,
        },
        tool_executor::{ReleaseExecutionTaskHandsRequest, ToolExecutorClient},
    },
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, GovernedInvocationResult, invoke_governed_tool,
    },
    workflows::execution_run::ExecutionRunClient,
};

const K_CANCEL_PROMISE: &str = "execution_task_cancel";

/// Durable workflow surface for one stable logical execution task.
#[restate_sdk::workflow]
pub trait ExecutionTask {
    /// Executes and persists one task across bounded retries and input resumes.
    async fn run(request: Json<ExecutionTaskWorkflowRequest>) -> Result<(), HandlerError>;

    /// Resumes a task after the authorized service persisted one input payload.
    #[shared]
    async fn input_delivered(request: Json<ExecutionInputRequest>) -> Result<(), HandlerError>;

    /// Releases a parked explicit review promise after persistence.
    #[shared]
    async fn review_decided(
        request: Json<ExecutionReviewDecisionRequest>,
    ) -> Result<(), HandlerError>;

    /// Releases a parked named-signal promise after persistence.
    #[shared]
    async fn signal_delivered(request: Json<ExecutionSignalRequest>) -> Result<(), HandlerError>;

    /// Persists and resolves one action-policy review outbox delivery.
    #[shared]
    async fn resolve_action_review(
        request: Json<ExecutionActionReviewResolutionRequest>,
    ) -> Result<Json<ExecutionActionReviewAcknowledgement>, HandlerError>;

    /// Cancels any currently parked promise for terminal cleanup.
    #[shared]
    async fn cancel(reason: Json<String>) -> Result<(), HandlerError>;
}

/// Runtime dependencies for one execution-task workflow.
#[derive(Clone)]
pub struct ExecutionTaskImpl {
    repository: ExecutionRepository,
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
    channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl ExecutionTaskImpl {
    /// Creates one task workflow with the exact runtime services used by governed calls.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
        channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool.clone()),
            pool,
            session_store,
            session_limits,
            channel_adapters,
        }
    }
}

impl ExecutionTask for ExecutionTaskImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: dispatched only by the keyed ExecutionRun workflow from persisted scoped task rows.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExecutionTaskWorkflowRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "run");
        let request = request.into_inner();
        annotate_execution_task_identity_span(request.run_uid, request.task_id);
        if request.task_id.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "execution task id mismatch").into());
        }
        if request.identity.tenant_id != request.tenant_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution task identity tenant mismatch",
            )
            .into());
        }
        let scope = execution_scope(&request);
        let mut operation_index = 0_u64;
        loop {
            let repository = self.repository.clone();
            let prepare_request = request.clone();
            let enforce_dispatch_generation = operation_index == 0;
            let prepared = ctx
                .run(|| async move {
                    prepare_task(
                        repository,
                        scope,
                        prepare_request,
                        enforce_dispatch_generation,
                    )
                    .await
                    .map(Json::from)
                })
                .name(format!("execution_task_prepare_{operation_index}"))
                .await?
                .into_inner();
            operation_index = operation_index.saturating_add(1);
            annotate_execution_task_record_span(&prepared.run, &prepared.task);
            if prepared.wake_run {
                send_run_wake(
                    &ctx,
                    prepared.run.run_uid,
                    prepared.run.wake_epoch,
                    ExecutionRunWakeReason::TaskOutcome,
                );
            }
            if prepared.task.status.is_terminal() || prepared.run.status.is_terminal() {
                cleanup_task_hands(&ctx, &prepared.run, &prepared.task).await?;
                return Ok(());
            }
            if prepared.run.pending_terminal.is_some() {
                persist_cancelled_task_and_finish(
                    &ctx,
                    self.repository.clone(),
                    scope,
                    prepared,
                    "execution run fenced forward work before compensation".to_string(),
                    operation_index,
                )
                .await?;
                return Ok(());
            }

            match &prepared.task.kind {
                LogicalTaskKind::Review { .. } => {
                    if let ParkedTaskWake::Cancelled(reason) =
                        await_review_or_cancel(&ctx, &prepared.task).await?
                    {
                        persist_cancelled_task_and_finish(
                            &ctx,
                            self.repository.clone(),
                            scope,
                            prepared,
                            reason,
                            operation_index,
                        )
                        .await?;
                        return Ok(());
                    }
                    continue;
                }
                LogicalTaskKind::WaitSignal { signal_name } => {
                    if let ParkedTaskWake::Cancelled(reason) =
                        await_signal_or_cancel(&ctx, &prepared.task, signal_name).await?
                    {
                        persist_cancelled_task_and_finish(
                            &ctx,
                            self.repository.clone(),
                            scope,
                            prepared,
                            reason,
                            operation_index,
                        )
                        .await?;
                        return Ok(());
                    }
                    continue;
                }
                _ => {}
            }

            let outcome =
                execute_task(self, &ctx, &request.identity, &prepared.run, &prepared.task).await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => ExecutionTaskOutcome {
                    schema_version: 1,
                    usage: prepared.task.actual.clone(),
                    result: ExecutionTaskResult::Failed {
                        class: ExecutionFailureClass::Terminal,
                        message: format!("{error:?}"),
                    },
                },
            };
            let outcome = validate_task_outcome(
                &prepared.run.active_plan,
                &prepared.task.node_id,
                &prepared.task.kind,
                outcome,
            );
            let outcome =
                exhaust_retry_outcome(prepared.task.attempt, &prepared.task.retry, outcome);
            let repository = self.repository.clone();
            let persist_task = prepared.task.clone();
            let persist_outcome = outcome.clone();
            let persisted = ctx
                .run(|| async move {
                    persist_task_outcome(repository, scope, persist_task, persist_outcome)
                        .await
                        .map(Json::from)
                })
                .name(format!("execution_task_outcome_{operation_index}"))
                .await?
                .into_inner();
            operation_index = operation_index.saturating_add(1);
            send_run_wake(
                &ctx,
                persisted.run.run_uid,
                persisted.run.wake_epoch,
                ExecutionRunWakeReason::TaskOutcome,
            );
            match &outcome.result {
                ExecutionTaskResult::Failed {
                    class: ExecutionFailureClass::Retryable,
                    ..
                } if persisted.task.status == ExecutionTaskStatus::Running => {
                    let repository = self.repository.clone();
                    let retry_task = persisted.task.clone();
                    let retried = ctx
                        .run(|| async move {
                            retry_task_generation(repository, scope, retry_task)
                                .await
                                .map(Json::from)
                        })
                        .name(format!("execution_task_retry_{operation_index}"))
                        .await?
                        .into_inner();
                    operation_index = operation_index.saturating_add(1);
                    send_run_wake(
                        &ctx,
                        retried.run.run_uid,
                        retried.run.wake_epoch,
                        ExecutionRunWakeReason::TaskOutcome,
                    );
                    if retried.task.status.is_terminal() || retried.run.status.is_terminal() {
                        cleanup_task_hands(&ctx, &retried.run, &retried.task).await?;
                        return Ok(());
                    }
                    let delay = retry_delay_ms(retried.task.attempt, &retried.task.retry);
                    ctx.sleep(std::time::Duration::from_millis(delay)).await?;
                }
                ExecutionTaskResult::NeedsInput { .. } => {
                    if let ParkedTaskWake::Cancelled(reason) =
                        await_input_or_cancel(&ctx, &persisted.task).await?
                    {
                        persist_cancelled_task_and_finish(
                            &ctx,
                            self.repository.clone(),
                            scope,
                            persisted,
                            reason,
                            operation_index,
                        )
                        .await?;
                        return Ok(());
                    }
                }
                ExecutionTaskResult::NeedsReplan { .. } => {
                    let reason: String = ctx.promise(K_CANCEL_PROMISE).await?;
                    persist_cancelled_task_and_finish(
                        &ctx,
                        self.repository.clone(),
                        scope,
                        persisted,
                        reason,
                        operation_index,
                    )
                    .await?;
                    return Ok(());
                }
                ExecutionTaskResult::Completed { .. }
                | ExecutionTaskResult::Cancelled { .. }
                | ExecutionTaskResult::UnknownOutcome { .. }
                | ExecutionTaskResult::Failed { .. } => {
                    cleanup_task_hands(&ctx, &persisted.run, &persisted.task).await?;
                    return Ok(());
                }
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only by Execution/deliver_input after the generation-fenced input transaction commits.
    async fn input_delivered(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionInputRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "input_delivered");
        let request = request.into_inner();
        annotate_execution_task_identity_span(request.run_uid, request.task_id);
        require_task_key(ctx.key(), request.task_id)?;
        ctx.resolve_promise(
            &input_promise_key(request.task_id, request.expected_generation),
            Json::from(request.input),
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only by Execution/decide_review after tenant-operator authorization and generation-fenced persistence.
    async fn review_decided(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionReviewDecisionRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "review_decided");
        let request = request.into_inner();
        annotate_execution_task_identity_span(request.run_uid, request.task_id);
        require_task_key(ctx.key(), request.task_id)?;
        ctx.resolve_promise(
            &review_promise_key(request.task_id, request.expected_generation),
            Json::from(request.decision),
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only by Execution/deliver_signal after tenant-operator authorization and exact signal persistence.
    async fn signal_delivered(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionSignalRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "signal_delivered");
        let request = request.into_inner();
        annotate_execution_task_identity_span(request.run_uid, request.task_id);
        require_task_key(ctx.key(), request.task_id)?;
        ctx.resolve_promise(
            &signal_promise_key(
                request.task_id,
                request.expected_generation,
                &request.signal_name,
            ),
            Json::from(request.payload),
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only by the bounded execution-action-review outbox dispatcher from a terminal persisted review row.
    async fn resolve_action_review(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionActionReviewResolutionRequest>,
    ) -> Result<Json<ExecutionActionReviewAcknowledgement>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "resolve_action_review");
        let headers = ctx.request_headers();
        let _ = link_remote_context_from_link_headers(&tracing::Span::current(), |name| {
            headers.get(name).cloned()
        });
        let request = request.into_inner();
        annotate_execution_task_identity_span(request.run_uid, request.task_id);
        require_task_key(ctx.key(), request.task_id)?;
        let repository = self.repository.clone();
        let record_request = request.clone();
        let write = ctx
            .run(|| async move {
                repository
                    .record_action_review_resolution(
                        ExecutionScope::ControlPlane,
                        record_request.run_uid,
                        record_request.task_id,
                        record_request.generation,
                        record_request.review_uid,
                        &record_request.resolution,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error)
            })
            .name(format!(
                "execution_action_review_resolution_{}",
                request.review_uid
            ))
            .await?
            .into_inner();
        let acknowledgement = match write {
            ActionReviewResolutionWrite::Applied => {
                ctx.resolve_promise(
                    &action_review_promise_key(request.review_uid, request.generation),
                    Json::from(request.resolution),
                );
                ExecutionActionReviewAcknowledgement::Applied
            }
            ActionReviewResolutionWrite::Replayed => {
                ctx.resolve_promise(
                    &action_review_promise_key(request.review_uid, request.generation),
                    Json::from(request.resolution),
                );
                ExecutionActionReviewAcknowledgement::Replayed
            }
            ActionReviewResolutionWrite::AuditedStale | ActionReviewResolutionWrite::NotFound => {
                ExecutionActionReviewAcknowledgement::AuditedStale
            }
        };
        Ok(Json::from(acknowledgement))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    // SAFETY: invoked by the owning run workflow after persisted cancellation.
    async fn cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionTask", "cancel");
        tracing::Span::current().set_attribute("moa.execution.task_id", ctx.key().to_string());
        ctx.resolve_promise(K_CANCEL_PROMISE, reason.into_inner());
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PreparedTask {
    run: ExecutionRunRecord,
    task: ExecutionTaskRecord,
    wake_run: bool,
}

async fn prepare_task(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    request: ExecutionTaskWorkflowRequest,
    enforce_dispatch_generation: bool,
) -> Result<PreparedTask, HandlerError> {
    let run = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    if run.tenant_id != request.tenant_id
        || run.contact_id != request.contact_id
        || run.session_id != request.session_id
    {
        return Err(TerminalError::new_with_code(409, "execution task scope mismatch").into());
    }
    let mut task = repository
        .load_task(scope, request.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution task not found"))?;
    if enforce_dispatch_generation && task.generation != request.generation {
        return Err(TerminalError::new_with_code(
            409,
            "execution task dispatch generation is stale",
        )
        .into());
    }
    if run.pending_terminal.is_some() && !task.status.is_terminal() {
        return Ok(PreparedTask {
            run,
            task,
            wake_run: false,
        });
    }
    if task.status.is_terminal() || run.status.is_terminal() {
        let wake_run = task.status.is_terminal() && !run.status.is_terminal();
        return Ok(PreparedTask {
            run,
            task,
            wake_run,
        });
    }
    if task.status == ExecutionTaskStatus::Pending {
        task = match repository
            .reserve_task(scope, task.run_uid, task.task_id, task.generation)
            .await
            .map_err(execution_error)?
        {
            ReservationOutcome::Reserved(task) => task,
            ReservationOutcome::AlreadyReserved(task) => task,
            ReservationOutcome::Terminalized(terminalized) => {
                return Ok(PreparedTask {
                    run: terminalized.run,
                    task: terminalized.task,
                    wake_run: true,
                });
            }
            ReservationOutcome::AlreadyTerminalized(terminalized) => {
                return Ok(PreparedTask {
                    run: terminalized.run,
                    task: terminalized.task,
                    wake_run: true,
                });
            }
            ReservationOutcome::NotFound => {
                return Err(TerminalError::new_with_code(404, "execution task not found").into());
            }
            ReservationOutcome::Rejected(reason) => {
                return Err(TerminalError::new(format!(
                    "execution task reservation rejected: {reason:?}"
                ))
                .into());
            }
        };
    }
    let mut wake_run = task.status.is_terminal();
    if task.status == ExecutionTaskStatus::Reserved {
        task = match repository
            .mark_task_running(scope, task.run_uid, task.task_id, task.generation)
            .await
            .map_err(execution_error)?
        {
            TransitionOutcome::Applied(task) => task,
            TransitionOutcome::AlreadyApplied(task) => task,
            other => {
                return Err(TerminalError::new(format!(
                    "execution task start rejected: {other:?}"
                ))
                .into());
            }
        };
        wake_run = true;
    }
    let run = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    Ok(PreparedTask {
        run,
        task,
        wake_run,
    })
}

async fn execute_task(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    identity: &moa_core::traits::Identity,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
) -> Result<ExecutionTaskOutcome, HandlerError> {
    match &task.kind {
        LogicalTaskKind::Capability { reference } => {
            execute_capability(workflow, ctx, identity, run, task, reference).await
        }
        LogicalTaskKind::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => {
            execute_agent(
                workflow,
                ctx,
                AgentExecutionRequest {
                    identity,
                    run,
                    task,
                    instructions,
                    skill_refs,
                    capability_refs,
                    max_turns: *max_turns,
                },
            )
            .await
        }
        LogicalTaskKind::Output { value } => {
            validate_instance(
                &run.active_plan.definition.output_schema,
                value,
                "execution_task.output",
            )
            .map_err(execution_error)?;
            Ok(completed_task_outcome(value.clone(), task.actual.clone()))
        }
        LogicalTaskKind::CompletionVerifier {
            instructions,
            max_turns,
            ..
        } => {
            execute_agent(
                workflow,
                ctx,
                AgentExecutionRequest {
                    identity,
                    run,
                    task,
                    instructions,
                    skill_refs: &[],
                    capability_refs: &[],
                    max_turns: *max_turns,
                },
            )
            .await
        }
        LogicalTaskKind::Review { .. } | LogicalTaskKind::WaitSignal { .. } => {
            Err(TerminalError::new("parked execution task reached executable adapter").into())
        }
    }
}

async fn execute_capability(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    identity: &moa_core::traits::Identity,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    reference: &CapabilityReference,
) -> Result<ExecutionTaskOutcome, HandlerError> {
    let capability = find_capability(run, reference)?;
    validate_instance(
        &capability.input_schema,
        &task.input,
        "execution_task.capability_input",
    )
    .map_err(execution_error)?;
    let session = load_session(workflow, ctx, run.session_id, task).await?;
    if let Some(outcome) = cancellation_outcome_before_next_agent_tool(
        ctx.peek_promise::<String>(K_CANCEL_PROMISE).await?,
        &task.actual,
    ) {
        return Ok(outcome);
    }
    let invocation = invoke_capability_tool(
        workflow,
        ctx,
        CapabilityInvocationContext {
            identity,
            run,
            task,
            capability,
            session: &session,
            // A direct capability task runs no model turn, so there is no system
            // context holding a canary for its output to leak. The canary belongs
            // to agent turns, which do have one.
            active_canary: None,
        },
        task.input.clone(),
        0,
    )
    .await?;
    let mut usage = task.actual.clone();
    usage.tool_calls = usage.tool_calls.saturating_add(1);
    if let CapabilityInvocationResult::Output(output) = &invocation {
        usage.retrieved_bytes = usage
            .retrieved_bytes
            .saturating_add(serialized_len(&output.safe_output.structured));
    }
    let outcome = capability_invocation_outcome(
        capability.idempotency_class,
        capability.action_class,
        invocation,
        usage,
    )?;
    let output_usage = outcome.usage.clone();
    let ExecutionTaskResult::Completed { output: value, .. } = &outcome.result else {
        return Ok(outcome);
    };
    if let Err(error) = validate_instance(
        &capability.output_schema,
        value,
        "execution_task.capability_output",
    ) {
        return Ok(invalid_capability_output_outcome(
            capability.action_class,
            error.to_string(),
            output_usage,
        ));
    }
    Ok(outcome)
}

struct AgentExecutionRequest<'a> {
    identity: &'a moa_core::traits::Identity,
    run: &'a ExecutionRunRecord,
    task: &'a ExecutionTaskRecord,
    instructions: &'a str,
    skill_refs: &'a [moa_artifacts::reference::ArtifactRef],
    capability_refs: &'a [CapabilityReference],
    max_turns: u32,
}

#[derive(Debug)]
struct AgentCapabilityBinding<'a> {
    capability: &'a ExecutionCapability,
    tool_name: &'a str,
}

fn validate_agent_capability_bindings(
    capabilities: Vec<&ExecutionCapability>,
) -> Result<Vec<AgentCapabilityBinding<'_>>, HandlerError> {
    let mut bindings = Vec::with_capacity(capabilities.len());
    let mut references_by_tool = BTreeMap::<&str, Vec<&CapabilityReference>>::new();
    for capability in capabilities {
        let Some(tool_name) = capability.source.model_visible_tool_name() else {
            return Err(
                TerminalError::new("capability has no governed tool owner in Task 6").into(),
            );
        };
        let references = references_by_tool.entry(tool_name).or_default();
        if !references
            .iter()
            .any(|reference| **reference == capability.reference)
        {
            references.push(&capability.reference);
        }
        bindings.push(AgentCapabilityBinding {
            capability,
            tool_name,
        });
    }
    for references in references_by_tool.values_mut() {
        references.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.version.cmp(&right.version))
        });
    }
    if let Some((tool_name, references)) = references_by_tool
        .iter()
        .find(|(_, references)| references.len() > 1)
    {
        let references = references
            .iter()
            .map(|reference| format!("{}@{}", reference.name, reference.version))
            .collect::<Vec<_>>()
            .join(" and ");
        return Err(TerminalError::new(format!(
            "task-local agent capability references {references} resolve to ambiguous model-visible tool `{tool_name}`"
        ))
        .into());
    }
    Ok(bindings)
}

/// Fixed terminal message for a task the security circuit halted.
const EXECUTION_TASK_SECURITY_HALT_MESSAGE: &str = "task stopped: a capability returned output classified as a prompt-injection or \
     restricted-material result";

/// Fixed user-facing question for a task the security circuit suspended.
///
/// Fixed rather than derived from the output: the output is exactly what MOA has
/// decided it cannot trust, so quoting it into a user prompt would forward the
/// attack to the human.
const EXECUTION_TASK_SECURITY_INPUT_QUESTION: &str = "A tool this task used returned output that MOA classified as a possible \
     prompt-injection attempt. Should this task continue without that capability?";

/// Derives the replay-stable tool-call identity for one task-local agent call.
///
/// The circuit deduplicates by tool call, so this must be a pure function of the
/// task and the position in its loop — a fresh UUID would let a replayed turn
/// score the same output twice.
fn execution_task_tool_call_id(
    task_uid: uuid::Uuid,
    turn: u32,
    call_index: usize,
) -> moa_core::types::identifiers::ToolCallId {
    const NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6d6f_615f_6574_6300_9e3a_41d5_b7c8_0002);
    let name = format!("{task_uid}:{turn}:{call_index}");
    moa_core::types::identifiers::ToolCallId(uuid::Uuid::new_v5(&NAMESPACE, name.as_bytes()))
}

/// Journals one execution-task circuit transition and its signed finding.
async fn record_execution_task_transition(
    ctx: &WorkflowContext<'_>,
    tenant_id: moa_core::types::identifiers::TenantId,
    session_id: moa_core::types::identifiers::SessionId,
    transition: moa_core::types::security::SecurityCircuitTransition,
    assessment: &moa_core::types::security::ToolOutputAssessment,
) -> Result<(), HandlerError> {
    let occurred_at = ctx
        .run(|| async move { Ok(Json::from(chrono::Utc::now())) })
        .name("execution_task_prompt_injection_transition_timestamp")
        .await?
        .into_inner();

    let dedupe_key = transition.key.clone();
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<crate::services::session_store::RestateSessionStoreClient>()
            .append_event(Json::from(moa_wire::session_store::AppendEventRequest {
                session_id,
                event: moa_core::events::Event::PromptInjectionCircuitTransition {
                    transition: transition.clone(),
                    signals: assessment.signals.clone(),
                    redacted_spans: assessment.redacted_spans,
                    deduplicated_carriers: assessment.deduplicated_carriers,
                },
                dedupe_key: Some(dedupe_key),
            })),
    )
    .call()
    .await?;

    crate::restate_identity::replay_safe_request(
        ctx.service_client::<crate::services::security_events::SecurityEventsClient>()
            .record_circuit_transition(Json::from(
                crate::services::security_events::RecordCircuitTransitionRequest {
                    tenant_id,
                    session_id,
                    transition,
                    signals: assessment.signals.clone(),
                    occurred_at,
                },
            )),
    )
    .call()
    .await?;
    Ok(())
}

async fn execute_agent(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    request: AgentExecutionRequest<'_>,
) -> Result<ExecutionTaskOutcome, HandlerError> {
    let AgentExecutionRequest {
        identity,
        run,
        task,
        instructions,
        skill_refs,
        capability_refs,
        max_turns,
    } = request;
    if max_turns == 0 {
        return Ok(failed_task_outcome(
            ExecutionFailureClass::InvalidInput,
            "agent max_turns must be positive".to_string(),
            task.actual.clone(),
        ));
    }
    // Ordinary, map, and reduce agents all materialize as `LogicalTaskKind::Agent`,
    // so this one pre-I/O guard covers every task-local model/tool loop.
    let capability_bindings = validate_agent_capability_bindings(
        capability_refs
            .iter()
            .map(|reference| find_capability(run, reference))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let session = load_session(workflow, ctx, run.session_id, task).await?;
    let skills = load_pinned_skills(workflow, ctx, run, task, skill_refs).await?;
    // The model sees the exact schemas persisted in the run's scoped capability
    // catalog. Re-reading the deployment router here would make a replay depend
    // on another tenant's catalog state and would discard installed-connector
    // provenance before governed dispatch rechecks its durable pins.
    let tools = capability_bindings
        .iter()
        .map(task_agent_tool_schema)
        .collect::<Vec<_>>();
    // One canary per task turn, journaled so a replay reproduces the same token
    // rather than minting a fresh one that the already-persisted output could
    // never match. It goes into the system context AND to every capability
    // invocation: the system copy is what an attacker can exfiltrate, and the
    // invocation copy is what lets the classifier recognize the exfiltration.
    let active_canary = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(moa_security::new_canary_token())) })
        .name("execution_task_agent_canary")
        .await?
        .into_inner();
    let mut messages = vec![
        ContextMessage::system(agent_system_prompt(instructions, &skills)),
        ContextMessage::system(moa_security::canary_system_message(&active_canary)),
        ContextMessage::user(
            json!({
                "resolved_input": task.input,
                "resume_inputs": task.resume_input_history,
            })
            .to_string(),
        ),
    ];
    let mut usage = task.actual.clone();
    // The task-local agent is its own circuit owner. The state lives in this
    // durable workflow rather than in a virtual object because the workflow *is*
    // the single writer for this owner, and its journal already makes the state
    // replay-stable. Routing it through the Session VO instead would make one
    // shared circuit alternate between the coordinator owner and each detached
    // task owner, and each switch would clear the other's accumulated score.
    let mut circuit = moa_core::types::security::SecurityCircuitState::default();
    let mut disabled_capabilities =
        std::collections::BTreeMap::<String, moa_core::types::security::ToolCapabilityId>::new();
    let circuit_owner = moa_core::types::security::SecurityCircuitOwner::ExecutionTask {
        run_uid: run.run_uid,
        task_uid: task.task_id.as_uuid(),
        generation: task.generation,
    };
    circuit.adopt_owner(&circuit_owner);
    for turn in 0..max_turns {
        let mut request = CompletionRequest {
            model: None,
            messages: messages.clone(),
            tools: tools
                .iter()
                .filter(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .is_none_or(|name| !disabled_capabilities.contains_key(name))
                })
                .cloned()
                .collect(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata: std::collections::HashMap::new(),
        };
        request.metadata.insert(
            DEFER_BRAIN_RESPONSE_METADATA_KEY.to_string(),
            Value::Bool(true),
        );
        let completion_owner = LLMCompletionOwner::execution_run(run.run_uid.to_string());
        attach_completion_owner(&mut request, &completion_owner);
        let call = crate::restate_identity::replay_safe_request(
            ctx.service_client::<LLMGatewayClient>()
                .complete(Json::from(request))
                .idempotency_key(completion_idempotency_key(
                    ctx.invocation_id(),
                    LLMCompletionAction::ExecutionTaskModel { turn },
                )),
        )
        .call();
        let response = match cancel_and_join_child_call(
            ctx.promise::<String>(K_CANCEL_PROMISE),
            call,
        )
        .await?
        {
            ChildInvocationOutcome::Cancelled(reason) => {
                return Ok(cancelled_task_outcome(reason, usage));
            }
            ChildInvocationOutcome::Completed(response) => response.into_inner(),
        };
        if response.stop_reason == StopReason::Cancelled {
            return Ok(cancelled_task_outcome(
                "execution run fenced its provider completion".to_string(),
                usage,
            ));
        }
        usage.tokens = usage
            .tokens
            .saturating_add(response.usage.total_input_tokens() as u64)
            .saturating_add(response.usage.output_tokens as u64);
        usage.cost_microusd = usage.cost_microusd.saturating_add(
            moa_providers::pricing_for_model(response.model.as_str())
                .map(|pricing| pricing.cost_micros(&response.usage))
                .unwrap_or_default(),
        );
        let tool_calls = response
            .content
            .iter()
            .filter_map(|content| match content {
                CompletionContent::ToolCall(call) => Some(call.clone()),
                CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
            })
            .collect::<Vec<_>>();
        if tool_calls.is_empty() {
            return Ok(parse_agent_task_outcome(&response.text, usage));
        }
        messages.push(ContextMessage::assistant_with_thought_signature(
            response.text,
            response.thought_signature,
        ));
        for (call_index, tool_call) in tool_calls.into_iter().enumerate() {
            let cancellation_reason = ctx.peek_promise::<String>(K_CANCEL_PROMISE).await?;
            if let Some(outcome) =
                cancellation_outcome_before_next_agent_tool(cancellation_reason, &usage)
            {
                // A prior governed effect is always joined and accounted before
                // reaching this boundary. Once cancellation is durable, no later
                // tool from the same model response may cross the admission fence.
                return Ok(outcome);
            }
            let binding = capability_bindings
                .iter()
                .find(|binding| binding.tool_name == tool_call.invocation.name)
                .ok_or_else(|| TerminalError::new("agent emitted an undeclared capability"))?;
            if disabled_capabilities.contains_key(&tool_call.invocation.name) {
                let tool_use_id =
                    tool_call.invocation.id.clone().unwrap_or_else(|| {
                        format!("execution-{}-{turn}-{call_index}", task.task_id)
                    });
                messages.push(ContextMessage::assistant_tool_call(
                    tool_call.invocation,
                    "",
                ));
                messages.push(ContextMessage::tool_result(
                    tool_use_id,
                    EXECUTION_TASK_DISABLED_CAPABILITY_MESSAGE,
                    None,
                ));
                continue;
            }
            let invocation = invoke_capability_tool(
                workflow,
                ctx,
                CapabilityInvocationContext {
                    identity,
                    run,
                    task,
                    capability: binding.capability,
                    session: &session,
                    active_canary: Some(active_canary.as_str()),
                },
                tool_call.invocation.input.clone(),
                u64::from(turn)
                    .saturating_mul(1_000)
                    .saturating_add(call_index as u64),
            )
            .await?;
            usage.tool_calls = usage.tool_calls.saturating_add(1);
            let output = match invocation {
                CapabilityInvocationResult::Output(output) => output,
                CapabilityInvocationResult::Terminal(result) => {
                    return Ok(ExecutionTaskOutcome {
                        schema_version: 1,
                        usage,
                        result,
                    });
                }
            };
            usage.retrieved_bytes = usage
                .retrieved_bytes
                .saturating_add(serialized_len(&output.safe_output.structured));

            // Score the classified output against this task's own circuit. A halt
            // is a terminal task failure and a suspend is a user-audience input
            // request, which are the execution domain's equivalents of the
            // coordinator's halt and NeedsInput outcomes.
            if !output.assessment.is_safe() {
                let applied = moa_security::apply_owner_assessment(
                    &mut circuit,
                    moa_security::CircuitTarget {
                        session_id: session.id,
                        owner: &circuit_owner,
                        capability: &output.capability,
                        tool_call_id: execution_task_tool_call_id(
                            task.task_id.as_uuid(),
                            turn,
                            call_index,
                        ),
                    },
                    &output.assessment,
                )
                .map_err(|error| {
                    tracing::error!(
                        active_owner_kind = error.active.as_ref().map(|owner| owner.kind()),
                        active_owner_generation =
                            error.active.as_ref().map(|owner| owner.generation()),
                        received_owner_kind = error.received.kind(),
                        received_owner_generation = error.received.generation(),
                        "execution task security assessment owner mismatch"
                    );
                    TerminalError::new("security assessment owner mismatch")
                })?;
                if let Some(transition) = applied {
                    record_execution_task_transition(
                        ctx,
                        session.tenant_id,
                        session.id,
                        transition,
                        &output.assessment,
                    )
                    .await?;
                }
                let stage = circuit.stage(&circuit_owner, &output.capability);
                if !stage.permits_dispatch() {
                    disabled_capabilities
                        .insert(tool_call.invocation.name.clone(), output.capability.clone());
                }
                match stage {
                    moa_core::types::security::SecurityCircuitStage::Halted => {
                        return Ok(failed_task_outcome(
                            ExecutionFailureClass::Terminal,
                            EXECUTION_TASK_SECURITY_HALT_MESSAGE.to_string(),
                            usage,
                        ));
                    }
                    moa_core::types::security::SecurityCircuitStage::SuspendedForInput => {
                        return Ok(ExecutionTaskOutcome {
                            schema_version: 1,
                            usage,
                            result: ExecutionTaskResult::NeedsInput {
                                question: EXECUTION_TASK_SECURITY_INPUT_QUESTION.to_string(),
                                audience: moa_artifacts::execution_plan::InputAudience::User,
                            },
                        });
                    }
                    _ => {}
                }
            }
            let tool_use_id = tool_call
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| format!("execution-{}-{turn}-{call_index}", task.task_id));
            messages.push(ContextMessage::assistant_tool_call(
                tool_call.invocation,
                "",
            ));
            // Only the classified output reaches the task-local agent's context.
            messages.push(ContextMessage::tool_result(
                tool_use_id,
                output.safe_output.to_text(),
                Some(output.safe_output.content.clone()),
            ));
        }
    }
    Ok(failed_task_outcome(
        ExecutionFailureClass::Terminal,
        format!("task-local agent exhausted max_turns={max_turns}"),
        usage,
    ))
}

fn cancellation_outcome_before_next_agent_tool(
    cancellation_reason: Option<String>,
    usage: &ExecutionUsage,
) -> Option<ExecutionTaskOutcome> {
    cancellation_reason.map(|reason| cancelled_task_outcome(reason, usage.clone()))
}

fn task_agent_tool_schema(binding: &AgentCapabilityBinding<'_>) -> Value {
    json!({
        "name": binding.tool_name,
        "description": binding.capability.description,
        "input_schema": binding.capability.input_schema,
    })
}

/// Fixed model-facing refusal for a capability disabled by the task circuit.
const EXECUTION_TASK_DISABLED_CAPABILITY_MESSAGE: &str = "This tool capability is disabled for the current task because its prior output triggered the security circuit.";

struct CapabilityInvocationContext<'a> {
    identity: &'a moa_core::traits::Identity,
    run: &'a ExecutionRunRecord,
    task: &'a ExecutionTaskRecord,
    capability: &'a ExecutionCapability,
    session: &'a SessionMeta,
    /// Per-task-turn canary this invocation must be screened against.
    ///
    /// Without it `CanaryLeak` is unreachable for execution-agent turns, which
    /// makes the whole clear-to-halt jump unreachable for this owner — the
    /// circuit would silently top out at the lower classes.
    active_canary: Option<&'a str>,
}

enum CapabilityInvocationResult {
    Output(Box<moa_core::types::tools::SecuredToolOutput>),
    Terminal(ExecutionTaskResult),
}

async fn invoke_capability_tool(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    invocation: CapabilityInvocationContext<'_>,
    input: Value,
    call_index: u64,
) -> Result<CapabilityInvocationResult, HandlerError> {
    let CapabilityInvocationContext {
        identity,
        run,
        task,
        capability,
        session,
        active_canary,
    } = invocation;
    let tool_name = capability_tool_name(capability)?;
    let tool_id = ToolCallId(uuid::Uuid::new_v5(
        &task.task_id.as_uuid(),
        format!("{}:{call_index}", task.generation).as_bytes(),
    ));
    let tool_call = ToolCallContent {
        invocation: moa_core::types::completion::ToolInvocation {
            id: Some(tool_id.to_string()),
            name: tool_name.clone(),
            input,
        },
        provider_metadata: None,
    };
    let allowed_tools = BTreeSet::from([tool_name]);
    let provenance = CapabilityProvenance {
        kind: Some(capability_source_kind(&capability.source).to_string()),
        id: Some(format!(
            "{}@{}",
            capability.reference.name, capability.reference.version
        )),
        step_id: Some(task.node_id.clone()),
    };
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session,
            identity,
            session_id: run.session_id,
            tool_id,
            tool_call: &tool_call,
            allowed_tools: &allowed_tools,
            expected_tool_contract_revision: Some(&capability.contract_revision),
            active_canary,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionTask {
                run_uid: run.run_uid,
                task_uid: task.task_id.as_uuid(),
                generation: task.generation,
            },
            capability_provenance: Some(&provenance),
            capability_policy_context: Some(&capability.policy_context),
            resource_budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let result = match classify_governed_capability_outcome(outcome)? {
        GovernedCapabilityOutcome::Completed(result) => result,
        GovernedCapabilityOutcome::Terminal(result) => {
            return Ok(CapabilityInvocationResult::Terminal(result));
        }
    };
    if result.disposition == GovernedInvocationDisposition::ReviewPending {
        let promise_key = action_review_promise_key(tool_id.0, task.generation);
        let resolution = restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
                let reason = reason?;
                let settlement = crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ActionReviewsClient>()
                        .settle_execution_owner_review(Json::from(
                            SettleExecutionActionReviewRequest {
                                tenant_id: run.tenant_id,
                                review_id: tool_id.0,
                                owner: ActionReviewOwner::ExecutionTask {
                                    session_id: run.session_id,
                                    origin: ExecutionTaskOrigin {
                                        run_uid: run.run_uid,
                                        task_uid: task.task_id.as_uuid(),
                                        generation: task.generation,
                                    },
                                },
                            },
                        )),
                )
                .call()
                .await?
                .into_inner();
                match action_review_cancellation_step(settlement, &reason) {
                    ActionReviewCancellationStep::Cancelled(result) => {
                        return Ok(CapabilityInvocationResult::Terminal(result));
                    }
                    ActionReviewCancellationStep::JoinResolution => {
                        ctx.promise::<Json<ExecutionActionReviewResolution>>(&promise_key)
                            .await?
                            .into_inner()
                    }
                }
            },
            resolution = ctx.promise::<Json<ExecutionActionReviewResolution>>(
                &promise_key
            ) => {
                resolution?.into_inner()
            }
        };
        return action_review_invocation_result(resolution);
    }
    Ok(CapabilityInvocationResult::Output(Box::new(result.output)))
}

enum ActionReviewCancellationStep {
    Cancelled(ExecutionTaskResult),
    JoinResolution,
}

fn action_review_cancellation_step(
    settlement: ExecutionActionReviewSettlement,
    reason: &str,
) -> ActionReviewCancellationStep {
    match settlement {
        ExecutionActionReviewSettlement::Revoked => {
            ActionReviewCancellationStep::Cancelled(ExecutionTaskResult::Cancelled {
                reason: reason.to_string(),
            })
        }
        ExecutionActionReviewSettlement::JoinRequired => {
            ActionReviewCancellationStep::JoinResolution
        }
    }
}

enum GovernedCapabilityOutcome {
    Completed(Box<GovernedInvocationResult>),
    Terminal(ExecutionTaskResult),
}

fn classify_governed_capability_outcome(
    outcome: GovernedInvocationOutcome,
) -> Result<GovernedCapabilityOutcome, HandlerError> {
    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            Ok(GovernedCapabilityOutcome::Completed(result))
        }
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => Ok(
            GovernedCapabilityOutcome::Terminal(ExecutionTaskResult::UnknownOutcome { message }),
        ),
        GovernedInvocationOutcome::NotDispatched { reason, .. } => Ok(
            GovernedCapabilityOutcome::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message: execution_dispatch_rejection_message(reason),
            }),
        ),
        GovernedInvocationOutcome::Delegation { .. } => {
            Err(TerminalError::new("execution agents cannot invoke delegation tools").into())
        }
    }
}

async fn persist_task_outcome(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    task: ExecutionTaskRecord,
    outcome: ExecutionTaskOutcome,
) -> Result<PreparedTask, HandlerError> {
    let write = repository
        .record_task_outcome(scope, task.run_uid, task.task_id, task.generation, outcome)
        .await
        .map_err(execution_error)?;
    let (task, persisted_run) = match write {
        TaskOutcomeWrite::Applied { run, task, .. }
        | TaskOutcomeWrite::Replayed { run, task, .. } => (task, Some(run)),
        TaskOutcomeWrite::Rejected { task, .. } => (task, None),
        TaskOutcomeWrite::NotFound => {
            return Err(TerminalError::new_with_code(404, "execution task not found").into());
        }
    };
    let run = match persisted_run {
        Some(run) => run,
        None => repository
            .load_run(scope, task.run_uid)
            .await
            .map_err(execution_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?,
    };
    Ok(PreparedTask {
        run,
        task,
        wake_run: false,
    })
}

async fn persist_cancelled_task_and_finish(
    ctx: &WorkflowContext<'_>,
    repository: ExecutionRepository,
    scope: ExecutionScope,
    prepared: PreparedTask,
    reason: String,
    operation_index: u64,
) -> Result<(), HandlerError> {
    let task = prepared.task.clone();
    let outcome = cancelled_task_outcome(reason, task.actual.clone());
    let persisted = ctx
        .run(|| async move {
            persist_task_outcome(repository, scope, task, outcome)
                .await
                .map(Json::from)
        })
        .name(format!(
            "execution_task_cancelled_outcome_{operation_index}"
        ))
        .await?
        .into_inner();
    send_run_wake(
        ctx,
        persisted.run.run_uid,
        persisted.run.wake_epoch,
        ExecutionRunWakeReason::TaskOutcome,
    );
    cleanup_task_hands(ctx, &persisted.run, &persisted.task).await
}

async fn retry_task_generation(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    task: ExecutionTaskRecord,
) -> Result<PreparedTask, HandlerError> {
    let task = match repository
        .retry_task(scope, task.run_uid, task.task_id, task.generation)
        .await
        .map_err(execution_error)?
    {
        TransitionOutcome::Applied(task) | TransitionOutcome::AlreadyApplied(task) => task,
        other => {
            return Err(TerminalError::new(format!(
                "execution retry transition rejected: {other:?}"
            ))
            .into());
        }
    };
    let run = repository
        .load_run(scope, task.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    Ok(PreparedTask {
        run,
        task,
        wake_run: false,
    })
}

async fn load_session(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    session_id: moa_core::types::identifiers::SessionId,
    task: &ExecutionTaskRecord,
) -> Result<SessionMeta, HandlerError> {
    let store = workflow.session_store.clone();
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(crate::workflows::errors::moa_error_to_handler_error)
        })
        .name(format!(
            "execution_task_load_session_{}_{}",
            task.generation, task.attempt
        ))
        .await?
        .into_inner())
}

async fn load_pinned_skills(
    workflow: &ExecutionTaskImpl,
    ctx: &WorkflowContext<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    skill_refs: &[moa_artifacts::reference::ArtifactRef],
) -> Result<Vec<String>, HandlerError> {
    let mut markdown = Vec::with_capacity(skill_refs.len());
    let scope = action_scope(run.tenant_id, run.contact_id);
    for (index, skill_ref) in skill_refs.iter().enumerate() {
        if !run.authorization.skill_refs.contains(skill_ref) {
            return Err(TerminalError::new(
                "task requested an instruction skill outside the authorization envelope",
            )
            .into());
        }
        let pinned = run
            .pinned_instruction_skills
            .iter()
            .find(|pinned| pinned.skill_ref == *skill_ref)
            .ok_or_else(|| TerminalError::new("task requested an unpinned instruction skill"))?;
        let pool = workflow.pool.clone();
        let revision_uid = pinned.revision_uid;
        let loaded = ctx
            .run(|| async move {
                moa_skills::registry::SkillRegistry::new(pool)
                    .load_skill_markdown(&scope, revision_uid)
                    .await
                    .map(Json::from)
                    .map_err(crate::workflows::errors::moa_error_to_handler_error)
            })
            .name(format!(
                "execution_task_skill_{}_{}_{}",
                task.generation, index, revision_uid
            ))
            .await?
            .into_inner();
        markdown.push(loaded);
    }
    Ok(markdown)
}

async fn await_input_or_cancel(
    ctx: &WorkflowContext<'_>,
    task: &ExecutionTaskRecord,
) -> Result<ParkedTaskWake, HandlerError> {
    let promise_key = input_promise_key(task.task_id, task.generation);
    Ok(restate_sdk::select! {
        reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
            ParkedTaskWake::Cancelled(reason?)
        },
        _ = ctx.promise::<Json<Value>>(&promise_key) => ParkedTaskWake::Resumed,
    })
}

async fn await_review_or_cancel(
    ctx: &WorkflowContext<'_>,
    task: &ExecutionTaskRecord,
) -> Result<ParkedTaskWake, HandlerError> {
    let promise_key = review_promise_key(task.task_id, task.generation);
    Ok(restate_sdk::select! {
        reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
            ParkedTaskWake::Cancelled(reason?)
        },
        _ = ctx.promise::<Json<moa_execution::wire::ExecutionReviewDecision>>(
            &promise_key
        ) => ParkedTaskWake::Resumed,
    })
}

async fn await_signal_or_cancel(
    ctx: &WorkflowContext<'_>,
    task: &ExecutionTaskRecord,
    signal_name: &str,
) -> Result<ParkedTaskWake, HandlerError> {
    let promise_key = signal_promise_key(task.task_id, task.generation, signal_name);
    Ok(restate_sdk::select! {
        reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
            ParkedTaskWake::Cancelled(reason?)
        },
        _ = ctx.promise::<Json<Value>>(
            &promise_key
        ) => ParkedTaskWake::Resumed,
    })
}

enum ParkedTaskWake {
    Resumed,
    Cancelled(String),
}

fn find_capability<'a>(
    run: &'a ExecutionRunRecord,
    reference: &CapabilityReference,
) -> Result<&'a ExecutionCapability, HandlerError> {
    if !run.authorization.capability_refs.contains(reference) {
        return Err(TerminalError::new(
            "capability is outside the persisted authorization envelope",
        )
        .into());
    }
    run.catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| TerminalError::new("capability is absent from the persisted catalog").into())
}

/// Returns the registered tool name a capability dispatches through.
///
/// Every arm must yield a name the router actually knows. A connector tool's
/// published name is not one — it resolves only under its server-qualified
/// reference — which is why `McpTool` contributes `tool_name` here and its
/// `remote_name` appears nowhere in this function.
pub(crate) fn capability_tool_name(
    capability: &ExecutionCapability,
) -> Result<String, HandlerError> {
    capability
        .source
        .model_visible_tool_name()
        .map(str::to_string)
        .ok_or_else(|| TerminalError::new("capability has no governed tool owner in Task 6").into())
}

const fn capability_source_kind(source: &CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::BuiltInTool { .. } => "built_in_tool",
        CapabilitySource::HandTool { .. } => "hand_tool",
        CapabilitySource::McpTool { .. } => "mcp_tool",
        CapabilitySource::ActionArtifact { .. } => "action_artifact",
        CapabilitySource::ConnectorAction { .. } => "connector_action",
        CapabilitySource::InstalledConnectorAction { .. } => "installed_connector_action",
        CapabilitySource::SkillAction { .. } => "skill_action",
        CapabilitySource::SkillCode { .. } => "skill_code",
        CapabilitySource::Memory { .. } => "memory",
        CapabilitySource::Knowledge { .. } => "knowledge",
        CapabilitySource::Model => "model",
    }
}

fn action_review_invocation_result(
    resolution: ExecutionActionReviewResolution,
) -> Result<CapabilityInvocationResult, HandlerError> {
    match resolution {
        ExecutionActionReviewResolution::Completed { tool_output } => {
            match serde_json::from_value(tool_output) {
                Ok(output) => Ok(CapabilityInvocationResult::Output(Box::new(output))),
                Err(error) => Ok(CapabilityInvocationResult::Terminal(
                    ExecutionTaskResult::UnknownOutcome {
                        message: format!(
                            "reviewed capability returned an invalid output after possible commit: {error}"
                        ),
                    },
                )),
            }
        }
        ExecutionActionReviewResolution::Failed { class, message } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed { class, message }),
        ),
        ExecutionActionReviewResolution::UnknownOutcome { message } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::UnknownOutcome { message }),
        ),
        ExecutionActionReviewResolution::NotDispatched { reason } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message: execution_dispatch_rejection_message(reason),
            }),
        ),
        ExecutionActionReviewResolution::Denied { reason } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::AuthorizationDenied,
                message: reason,
            }),
        ),
        ExecutionActionReviewResolution::TimedOut { reason } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::DeadlineExceeded,
                message: reason,
            }),
        ),
    }
}

fn execution_dispatch_rejection_message(reason: ExecutionToolDispatchRejection) -> String {
    let reason = match reason {
        ExecutionToolDispatchRejection::OriginNotFound => "origin_not_found",
        ExecutionToolDispatchRejection::StaleGeneration => "stale_generation",
        ExecutionToolDispatchRejection::OperationNotRunning => "operation_not_running",
        ExecutionToolDispatchRejection::RunNotDispatchable => "run_not_dispatchable",
    };
    format!("execution effect was not dispatched: {reason}")
}

fn capability_invocation_outcome(
    idempotency_class: IdempotencyClass,
    action_class: moa_core::types::action_policy::ActionClass,
    invocation: CapabilityInvocationResult,
    usage: ExecutionUsage,
) -> Result<ExecutionTaskOutcome, HandlerError> {
    match invocation {
        CapabilityInvocationResult::Terminal(result) => Ok(ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result,
        }),
        CapabilityInvocationResult::Output(output) if output.is_error() => {
            if idempotency_class == IdempotencyClass::Idempotent {
                Ok(failed_task_outcome(
                    ExecutionFailureClass::Retryable,
                    output.safe_output.to_text(),
                    usage,
                ))
            } else if action_class != moa_core::types::action_policy::ActionClass::Read {
                Ok(ExecutionTaskOutcome {
                    schema_version: 1,
                    usage,
                    result: ExecutionTaskResult::UnknownOutcome {
                        message: format!(
                            "non-idempotent side-effecting capability returned an error after possible commit: {}",
                            output.safe_output.to_text()
                        ),
                    },
                })
            } else {
                Ok(failed_task_outcome(
                    ExecutionFailureClass::Terminal,
                    output.safe_output.to_text(),
                    usage,
                ))
            }
        }
        CapabilityInvocationResult::Output(output) => {
            // A non-safe class already cleared `structured`, so a task whose
            // capability returned attacker-shaped output completes with the safe
            // replacement text rather than the raw structured payload.
            let value = output
                .safe_output
                .structured
                .clone()
                .unwrap_or_else(|| Value::String(output.safe_output.to_text()));
            Ok(completed_task_outcome(value, usage))
        }
    }
}

fn invalid_capability_output_outcome(
    action_class: moa_core::types::action_policy::ActionClass,
    message: String,
    usage: ExecutionUsage,
) -> ExecutionTaskOutcome {
    if action_class == moa_core::types::action_policy::ActionClass::Read {
        failed_task_outcome(ExecutionFailureClass::InvalidOutput, message, usage)
    } else {
        ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result: ExecutionTaskResult::UnknownOutcome {
                message: format!(
                    "side-effecting capability returned invalid output after possible commit: {message}"
                ),
            },
        }
    }
}

fn agent_system_prompt(instructions: &str, skills: &[String]) -> String {
    format!(
        "{instructions}\n\nPinned instruction skills:\n{}\n\nReturn only JSON. To finish normally return any JSON value. To request input or replanning, return the exact ExecutionTaskResult tagged shape with status needs_input or needs_replan.",
        skills.join("\n\n---\n\n")
    )
}

fn serialized_len<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_default()
}

fn input_promise_key(task_id: moa_execution::state::ExecutionTaskId, generation: u64) -> String {
    format!("execution_input:{task_id}:{generation}")
}

fn review_promise_key(task_id: moa_execution::state::ExecutionTaskId, generation: u64) -> String {
    format!("execution_review:{task_id}:{generation}")
}

fn signal_promise_key(
    task_id: moa_execution::state::ExecutionTaskId,
    generation: u64,
    signal_name: &str,
) -> String {
    format!("execution_signal:{task_id}:{generation}:{signal_name}")
}

fn action_review_promise_key(review_uid: uuid::Uuid, generation: u64) -> String {
    format!("execution_action_review:{review_uid}:{generation}")
}

fn require_task_key(
    key: &str,
    task_id: moa_execution::state::ExecutionTaskId,
) -> Result<(), HandlerError> {
    if key == task_id.to_string() {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(404, "execution task id mismatch").into())
    }
}

fn execution_scope(request: &ExecutionTaskWorkflowRequest) -> ExecutionScope {
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

fn annotate_execution_task_identity_span(
    run_uid: uuid::Uuid,
    task_id: moa_execution::state::ExecutionTaskId,
) {
    let span = tracing::Span::current();
    span.set_attribute("moa.execution.run_uid", run_uid.to_string());
    span.set_attribute("moa.execution.task_id", task_id.to_string());
}

fn annotate_execution_task_record_span(run: &ExecutionRunRecord, task: &ExecutionTaskRecord) {
    annotate_execution_task_identity_span(task.run_uid, task.task_id);
    let span = tracing::Span::current();
    span.set_attribute("moa.execution.plan_hash", run.active_plan_hash.to_string());
    span.set_attribute(
        "moa.execution.plan_revision",
        task.plan_revision.to_string(),
    );
    span.set_attribute("moa.execution.node_id", task.node_id.clone());
}

fn action_scope(
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
) -> ActionRuleScope {
    contact_id.map_or(ActionRuleScope::Tenant { tenant_id }, |contact_id| {
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        }
    })
}

fn send_run_wake(
    ctx: &WorkflowContext<'_>,
    run_uid: uuid::Uuid,
    wake_epoch: u64,
    reason: ExecutionRunWakeReason,
) {
    // Detached by design: wake_epoch is a persisted generation fence and the run
    // workflow ignores duplicate or superseded notifications.
    crate::restate_identity::replay_safe_request(
        ctx.workflow_client::<ExecutionRunClient>(run_uid.to_string())
            .wake(Json::from(ExecutionRunWakeRequest {
                run_uid,
                wake_epoch,
                reason,
            })),
    )
    .send();
}

async fn cleanup_task_hands(
    ctx: &WorkflowContext<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .release_execution_task_hands(Json::from(ReleaseExecutionTaskHandsRequest {
                session_id: run.session_id,
                run_uid: run.run_uid,
                task_id: task.task_id,
            })),
    )
    .call()
    .await?;
    Ok(())
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    TerminalError::new(format!("execution task workflow failed: {error}")).into()
}

#[cfg(test)]
mod tests {
    use moa_artifacts::{
        execution_plan::{
            CapabilityReference, ExecutionFailureClass, ExecutionTaskResult, ExecutionUsage,
        },
        reference::ArtifactRef,
    };
    use moa_core::types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        completion::ToolInvocation,
        identifiers::{ConnectorConnectionId, ToolCallId},
        tools::IdempotencyClass,
    };
    use moa_execution::capability::{
        CapabilityPolicyContext, CapabilitySource, ExecutionCapability, ExecutionClass,
        ExecutionEstimate,
    };
    use moa_execution::wire::ExecutionActionReviewResolution;
    use moa_execution::wire::ExecutionToolDispatchRejection;
    use serde_json::json;

    use super::{
        ActionReviewCancellationStep, CapabilityInvocationResult, ExecutionActionReviewSettlement,
        GovernedCapabilityOutcome, GovernedInvocationOutcome, action_review_cancellation_step,
        action_review_invocation_result, cancellation_outcome_before_next_agent_tool,
        capability_invocation_outcome, classify_governed_capability_outcome,
        task_agent_tool_schema, validate_agent_capability_bindings,
    };

    fn task_agent_capability(
        reference_name: &str,
        source: CapabilitySource,
        policy_context: CapabilityPolicyContext,
    ) -> ExecutionCapability {
        ExecutionCapability {
            reference: CapabilityReference {
                name: reference_name.to_string(),
                version: "v1".to_string(),
            },
            contract_revision: "contract-v1".to_string(),
            description: format!("Task agent capability {reference_name}"),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            action_class: ActionClass::ExternalWrite,
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            idempotency_class: IdempotencyClass::Idempotent,
            execution_class: ExecutionClass::External,
            source,
            policy_context,
            estimate: ExecutionEstimate {
                tool_calls: 1,
                tasks: 1,
                ..ExecutionEstimate::default()
            },
            rollback: None,
        }
    }

    fn registered_task_agent_capability(tool_name: &str) -> ExecutionCapability {
        let source = CapabilitySource::McpTool {
            server: "fixture".to_string(),
            tool_name: tool_name.to_string(),
            remote_name: "probe".to_string(),
        };
        task_agent_capability(
            tool_name,
            source.clone(),
            CapabilityPolicyContext::registered(source),
        )
    }

    fn action_task_agent_capability(tool_name: &str) -> ExecutionCapability {
        let action_ref = ArtifactRef::action_artifact("reviewed-operation");
        let revision_uid = uuid::Uuid::from_u128(11);
        let source = CapabilitySource::ActionArtifact {
            action_ref: action_ref.clone(),
            revision_uid,
            tool_name: tool_name.to_string(),
        };
        task_agent_capability(
            &action_ref.to_string(),
            source.clone(),
            CapabilityPolicyContext::artifact(
                source,
                Some(action_ref),
                uuid::Uuid::from_u128(10),
                revision_uid,
                ActionPolicyEffect::AdminReview,
            ),
        )
    }

    fn skill_action_task_agent_capability(tool_name: &str) -> ExecutionCapability {
        let skill_ref = ArtifactRef::artifact(
            moa_artifacts::document::ArtifactKind::Skill,
            "reviewed-operations",
        );
        let action_ref = ArtifactRef::action_artifact("reviewed-operation");
        let revision_uid = uuid::Uuid::from_u128(21);
        let source = CapabilitySource::SkillAction {
            skill_ref: skill_ref.clone(),
            revision_uid,
            action_id: "reviewed-operation".to_string(),
            tool_name: tool_name.to_string(),
        };
        task_agent_capability(
            &format!("{skill_ref}#reviewed-operation"),
            source.clone(),
            CapabilityPolicyContext::artifact(
                source,
                Some(action_ref),
                uuid::Uuid::from_u128(20),
                revision_uid,
                ActionPolicyEffect::AdminReview,
            ),
        )
    }

    #[test]
    fn task_agent_schema_uses_persisted_installed_connector_capability() {
        // Pins: replayed task-agent prompts use the exact model name and schema
        // already persisted with typed connector provenance; no live global
        // router lookup or connector-name parsing can substitute authority.
        let connector_ref = ArtifactRef::connector("support");
        let action_ref = ArtifactRef::action("support", "create_ticket");
        let source = CapabilitySource::InstalledConnectorAction {
            connector_ref,
            connection_id: ConnectorConnectionId(uuid::Uuid::from_u128(71)),
            binding_id: uuid::Uuid::from_u128(72),
            connection_generation: 9,
            definition_artifact_uid: uuid::Uuid::from_u128(73),
            definition_revision_uid: uuid::Uuid::from_u128(74),
            action_id: "create_ticket".to_string(),
            contract_hash: "ab".repeat(32),
            governed_contract_revision: "governed-v9".to_string(),
            minimum_effect: ActionPolicyEffect::AdminReview,
            tool_name: "conn__00000000000000000000000000000047__create_ticket".to_string(),
        };
        let capability = task_agent_capability(
            &action_ref.to_string(),
            source.clone(),
            CapabilityPolicyContext::artifact(
                source,
                Some(action_ref),
                uuid::Uuid::from_u128(73),
                uuid::Uuid::from_u128(74),
                ActionPolicyEffect::AdminReview,
            ),
        );
        let bindings = validate_agent_capability_bindings(vec![&capability])
            .expect("one installed connector capability should be unambiguous");

        assert_eq!(
            task_agent_tool_schema(&bindings[0]),
            json!({
                "name": "conn__00000000000000000000000000000047__create_ticket",
                "description": "Task agent capability action://support.create_ticket",
                "input_schema": {"type": "object"},
            })
        );
    }

    #[test]
    fn artifact_policy_floor_rejects_ambiguous_task_agent_bindings_in_both_orders() {
        // Pins: model-visible backing-tool names cannot collapse raw Allow authority
        // with either an Action or inherited SkillAction review floor. This pure
        // production guard runs before the task performs any model or tool I/O.
        let tool_name = "mcp__fixture__probe";
        let raw = registered_task_agent_capability(tool_name);
        for alias in [
            action_task_agent_capability(tool_name),
            skill_action_task_agent_capability(tool_name),
        ] {
            let mut reference_labels = [
                format!("{}@{}", alias.reference.name, alias.reference.version),
                format!("{}@{}", raw.reference.name, raw.reference.version),
            ];
            reference_labels.sort();
            let expected = format!(
                "Terminal error [500]: task-local agent capability references {} and {} resolve to ambiguous model-visible tool `{tool_name}`",
                reference_labels[0], reference_labels[1]
            );
            for capabilities in [vec![&raw, &alias], vec![&alias, &raw]] {
                let error = validate_agent_capability_bindings(capabilities)
                    .expect_err("ambiguous model-visible authority must fail before model use");
                let actual = <restate_sdk::prelude::HandlerError as AsRef<
                    dyn std::error::Error + Send + Sync,
                >>::as_ref(&error)
                .to_string();
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn idempotent_capability_action_review_terminal_results_never_become_retryable() {
        // Pins: durable action-review delivery remains typed through capability
        // execution; capability idempotency may classify ordinary tool errors,
        // but it cannot rewrite review denial, timeout, or terminal failure.
        let cases = [
            (
                ExecutionActionReviewResolution::Failed {
                    class: ExecutionFailureClass::Unsupported,
                    message: "reviewed action failed".to_string(),
                },
                ExecutionFailureClass::Unsupported,
                "reviewed action failed",
            ),
            (
                ExecutionActionReviewResolution::Denied {
                    reason: "tenant admin denied".to_string(),
                },
                ExecutionFailureClass::AuthorizationDenied,
                "tenant admin denied",
            ),
            (
                ExecutionActionReviewResolution::TimedOut {
                    reason: "review deadline elapsed".to_string(),
                },
                ExecutionFailureClass::DeadlineExceeded,
                "review deadline elapsed",
            ),
        ];

        for (resolution, expected_class, expected_message) in cases {
            let invocation = action_review_invocation_result(resolution)
                .expect("terminal review resolution should be valid");
            assert!(matches!(
                invocation,
                CapabilityInvocationResult::Terminal(_)
            ));
            let outcome = capability_invocation_outcome(
                IdempotencyClass::Idempotent,
                ActionClass::Read,
                invocation,
                ExecutionUsage {
                    cost_microusd: 0,
                    tokens: 0,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                },
            )
            .expect("typed review result should map to a task outcome");
            assert!(matches!(
                outcome.result,
                ExecutionTaskResult::Failed { class, message }
                    if class == expected_class && message == expected_message
            ));
        }
    }

    #[test]
    fn governed_execution_ambiguity_is_a_typed_unknown_task_outcome() {
        // Pins: once ToolExecutor reports that a side effect may have committed,
        // the execution workflow persists UnknownOutcome and cannot resend it as
        // an ordinary failed invocation.
        let classified =
            classify_governed_capability_outcome(GovernedInvocationOutcome::UnknownOutcome {
                tool_id: ToolCallId(uuid::Uuid::from_u128(81)),
                invocation: ToolInvocation {
                    id: Some("tool-81".to_string()),
                    name: "fixture_effect".to_string(),
                    input: json!({"value": 1}),
                },
                message: "external result is ambiguous".to_string(),
            })
            .expect("typed ambiguity is a valid terminal task result");
        assert!(matches!(
            classified,
            GovernedCapabilityOutcome::Terminal(ExecutionTaskResult::UnknownOutcome { message })
                if message == "external result is ambiguous"
        ));
    }

    #[test]
    fn governed_execution_admission_rejection_is_definitive_failure() {
        // Pins: the row-locked owner admission proved no external effect began, so
        // a fenced or stale origin is terminal Failed and never UnknownOutcome.
        let classified =
            classify_governed_capability_outcome(GovernedInvocationOutcome::NotDispatched {
                tool_id: ToolCallId(uuid::Uuid::from_u128(82)),
                invocation: ToolInvocation {
                    id: Some("tool-82".to_string()),
                    name: "fixture_effect".to_string(),
                    input: json!({"value": 2}),
                },
                reason: ExecutionToolDispatchRejection::RunNotDispatchable,
            })
            .expect("definitive admission rejection is a valid task result");
        assert!(matches!(
            classified,
            GovernedCapabilityOutcome::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message,
            }) if message.ends_with("run_not_dispatchable")
        ));
    }

    #[test]
    fn reviewed_execution_ambiguity_and_malformed_output_are_unknown() {
        // Pins: an approved external effect has crossed the commit boundary, so
        // both an explicit ambiguous resolution and undecodable completed output
        // require reconciliation instead of retry or generic workflow failure.
        for resolution in [
            ExecutionActionReviewResolution::UnknownOutcome {
                message: "reviewed result is ambiguous".to_string(),
            },
            ExecutionActionReviewResolution::Completed {
                tool_output: json!("not a secured tool output"),
            },
        ] {
            let result = action_review_invocation_result(resolution)
                .expect("review ambiguity remains a typed task result");
            assert!(matches!(
                result,
                CapabilityInvocationResult::Terminal(ExecutionTaskResult::UnknownOutcome { .. })
            ));
        }
    }

    #[test]
    fn cancellation_revokes_unclaimed_review_but_joins_claimed_effect() {
        // Pins: cancellation before tenant clear terminalizes the review before any
        // tool dispatch; once the decision transaction claimed the effect, the task
        // must join its definitive resolution and cannot overwrite it as Cancelled.
        assert!(matches!(
            action_review_cancellation_step(
                ExecutionActionReviewSettlement::Revoked,
                "run cancelled",
            ),
            ActionReviewCancellationStep::Cancelled(ExecutionTaskResult::Cancelled { reason })
                if reason == "run cancelled"
        ));
        assert!(matches!(
            action_review_cancellation_step(
                ExecutionActionReviewSettlement::JoinRequired,
                "run cancelled",
            ),
            ActionReviewCancellationStep::JoinResolution
        ));

        let definitive =
            action_review_invocation_result(ExecutionActionReviewResolution::UnknownOutcome {
                message: "claimed effect is ambiguous".to_string(),
            })
            .expect("claimed review ambiguity must remain typed");
        assert!(matches!(
            definitive,
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::UnknownOutcome { message })
                if message == "claimed effect is ambiguous"
        ));

        let no_effect =
            action_review_invocation_result(ExecutionActionReviewResolution::NotDispatched {
                reason: ExecutionToolDispatchRejection::RunNotDispatchable,
            })
            .expect("claimed no-effect admission rejection stays definitive");
        assert!(matches!(
            no_effect,
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message,
            }) if message.ends_with("run_not_dispatchable")
        ));

        let unfenced =
            action_review_invocation_result(ExecutionActionReviewResolution::NotDispatched {
                reason: ExecutionToolDispatchRejection::StaleGeneration,
            })
            .expect("definitive no-effect admission rejection is typed");
        assert!(matches!(
            unfenced,
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message,
            }) if message.ends_with("stale_generation")
        ));
    }

    #[test]
    fn cancellation_fence_stops_the_next_agent_tool() {
        // Pins: after one joined governed effect, a cancellation observed at the
        // per-tool admission boundary terminates the task before a second tool
        // from the same model response can be dispatched.
        let outcome = cancellation_outcome_before_next_agent_tool(
            Some("run fenced forward work".to_string()),
            &ExecutionUsage {
                cost_microusd: 0,
                tokens: 0,
                tool_calls: 1,
                retrieved_bytes: 0,
            },
        )
        .expect("durable cancellation must stop the next tool");
        assert!(matches!(
            outcome.result,
            ExecutionTaskResult::Cancelled { reason }
                if reason == "run fenced forward work"
        ));
    }

    #[test]
    fn a_leaked_task_canary_halts_the_task_owner_in_exactly_one_transition() {
        // Pins the execution-task end of the canary contract, which nothing else
        // reaches: an agent turn mints a canary, and a capability that echoes it
        // back must halt THAT task owner in a single transition.
        //
        // Three properties, each load bearing:
        //  1. The token this crate mints is the token the classifier recognizes.
        //     A format change on either side would silently make every task
        //     canary undetectable, and no other test composes the two.
        //  2. `CanaryLeak` scores 4, so a clear circuit jumps straight to
        //     `Halted`. Exactly one transition must be produced — a walk through
        //     warned and disabled would mean the single-highest-stage rule broke
        //     for this owner.
        //  3. The token never survives into `safe_output`. A halt that still
        //     forwarded the leaked marker to the model would defeat its purpose.
        use moa_core::types::identifiers::{SessionId, ToolCallId};
        use moa_core::types::security::{
            OutputAssessmentClass, SecurityCircuitOwner, SecurityCircuitStage,
            SecurityCircuitState, ToolCapabilityId,
        };
        use moa_core::types::tools::ToolOutput;

        let canary = moa_security::new_canary_token();
        assert!(
            moa_security::canary_system_message(&canary).contains(&canary),
            "the system copy must carry the exact minted token; it is what an \
             attacker exfiltrates"
        );

        let leaked = ToolOutput::text(
            format!("Here is the marker you asked for: {canary}"),
            std::time::Duration::from_millis(1),
        );
        let capability = ToolCapabilityId::builtin("lookup");
        let secured = moa_security::classify_tool_output(
            &leaked,
            moa_security::OutputClassification {
                capability: &capability,
                active_canary: Some(canary.as_str()),
            },
        );

        assert_eq!(
            secured.assessment.class,
            OutputAssessmentClass::CanaryLeak,
            "a capability echoing the turn's canary is a leak, not merely suspicious"
        );
        assert!(secured.assessment.class.clears_raw_carriers());
        assert!(
            !serde_json::to_string(&secured)
                .expect("serialize secured output")
                .contains(&canary),
            "the leaked marker must not survive anywhere in the envelope"
        );

        let owner = SecurityCircuitOwner::ExecutionTask {
            run_uid: uuid::Uuid::from_u128(0x9001),
            task_uid: uuid::Uuid::from_u128(0x9002),
            generation: 3,
        };
        let mut circuit = SecurityCircuitState::default();
        circuit.adopt_owner(&owner);
        let transition = moa_security::apply_owner_assessment(
            &mut circuit,
            moa_security::CircuitTarget {
                session_id: SessionId(uuid::Uuid::from_u128(0x9003)),
                owner: &owner,
                capability: &capability,
                tool_call_id: ToolCallId(uuid::Uuid::from_u128(0x9004)),
            },
            &secured.assessment,
        )
        .expect("the admitted owner matches")
        .expect("a first-strike canary leak must produce one transition");

        assert_eq!(transition.prior_stage, SecurityCircuitStage::Clear);
        assert_eq!(
            transition.reached_stage,
            SecurityCircuitStage::Halted,
            "score 4 from clear halts directly; no warned or disabled step"
        );
        assert_eq!(transition.prior_score, 0);
        assert_eq!(transition.reached_score, 4);
        assert_eq!(
            circuit.stage(&owner, &capability),
            SecurityCircuitStage::Halted,
            "the halted stage is what drives ExecutionTaskResult::Failed{{Terminal}}"
        );
        assert!(
            !circuit.permits_dispatch(&owner, &capability),
            "a halted capability must not dispatch again under this owner"
        );
    }
}
