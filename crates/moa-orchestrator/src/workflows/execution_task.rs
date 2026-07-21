//! Durable keyed workflow that executes one persisted logical task across generations.

use std::{collections::BTreeSet, sync::Arc};

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionNode, ExecutionOperation,
    ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage,
};
use moa_core::{
    config::SessionLimitsConfig,
    traits::{ChannelAdapter, SessionStore as _},
    types::{
        action_policy::{ActionRuleScope, CapabilityProvenance},
        channel::Channel,
        completion::{
            CompletionContent, CompletionRequest, DEFER_BRAIN_RESPONSE_METADATA_KEY,
            ToolCallContent,
        },
        context::ContextMessage,
        identifiers::ToolCallId,
        session::SessionMeta,
        tools::{IdempotencyClass, ToolOutput},
    },
};
use moa_execution::{
    capability::{CapabilitySource, ExecutionCapability},
    repository::{
        ActionReviewResolutionWrite, ExecutionRepository, ExecutionRunRecord, ExecutionScope,
        ExecutionTaskRecord, ReservationOutcome, TaskOutcomeWrite, TransitionOutcome,
    },
    schema::validate_instance,
    state::{ExecutionTaskStatus, LogicalTaskKind},
    wire::{
        ExecutionActionReviewAcknowledgement, ExecutionActionReviewResolution,
        ExecutionActionReviewResolutionRequest, ExecutionInputRequest,
        ExecutionReviewDecisionRequest, ExecutionRunWakeReason, ExecutionRunWakeRequest,
        ExecutionSignalRequest, ExecutionTaskWorkflowRequest,
    },
};
use moa_hands::ToolRouter;
use moa_observability::propagation::link_remote_context_from_link_headers;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::{
    ctx::RequestHeaders,
    services::{
        llm_gateway::LLMGatewayClient,
        tool_executor::{ReleaseExecutionTaskHandsRequest, ToolExecutorClient},
    },
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, invoke_governed_tool,
    },
    workflows::execution_node_actions::{
        record_applied_run_transition, record_applied_task_retry, record_applied_task_transition,
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
    router: Arc<ToolRouter>,
    session_store: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
    channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl ExecutionTaskImpl {
    /// Creates one task workflow with the exact runtime services used by governed calls.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        router: Arc<ToolRouter>,
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
        channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool.clone()),
            pool,
            router,
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
                cleanup_task_hands(&ctx, &prepared.run, &prepared.task);
                return Ok(());
            }

            match &prepared.task.kind {
                LogicalTaskKind::Review { .. } => {
                    await_review_or_cancel(&ctx, &prepared.task).await?;
                    continue;
                }
                LogicalTaskKind::WaitSignal { signal_name } => {
                    await_signal_or_cancel(&ctx, &prepared.task, signal_name).await?;
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
            let outcome = validate_task_outcome(&prepared.run, &prepared.task, outcome);
            let outcome = exhaust_retry_if_needed(&prepared.task, outcome);
            let repository = self.repository.clone();
            let persist_run = prepared.run.clone();
            let persist_task = prepared.task.clone();
            let persist_outcome = outcome.clone();
            let persisted = ctx
                .run(|| async move {
                    persist_task_outcome(
                        repository,
                        scope,
                        persist_run,
                        persist_task,
                        persist_outcome,
                    )
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
                        cleanup_task_hands(&ctx, &retried.run, &retried.task);
                        return Ok(());
                    }
                    let delay = retry_delay_ms(&retried.task);
                    ctx.sleep(std::time::Duration::from_millis(delay)).await?;
                }
                ExecutionTaskResult::NeedsInput { .. } => {
                    await_input_or_cancel(&ctx, &persisted.task).await?;
                }
                ExecutionTaskResult::NeedsReplan { .. } => {
                    let _: String = ctx.promise(K_CANCEL_PROMISE).await?;
                    cleanup_task_hands(&ctx, &persisted.run, &persisted.task);
                    return Ok(());
                }
                ExecutionTaskResult::Completed { .. }
                | ExecutionTaskResult::Cancelled { .. }
                | ExecutionTaskResult::Failed { .. } => {
                    cleanup_task_hands(&ctx, &persisted.run, &persisted.task);
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
    let prior_run_status = run.status;
    let mut run_transition_applied = false;
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
    if task.status.is_terminal() || run.status.is_terminal() {
        let wake_run = task.status.is_terminal() && !run.status.is_terminal();
        return Ok(PreparedTask {
            run,
            task,
            wake_run,
        });
    }
    if task.status == ExecutionTaskStatus::Pending {
        let prior_status = task.status;
        task = match repository
            .reserve_task(scope, task.run_uid, task.task_id, task.generation)
            .await
            .map_err(execution_error)?
        {
            ReservationOutcome::Reserved(task) => {
                record_applied_task_transition(Some(prior_status), &task);
                task
            }
            ReservationOutcome::AlreadyReserved(task) => task,
            ReservationOutcome::Terminalized(terminalized) => {
                record_applied_run_transition(Some(run.status), &terminalized.run);
                record_applied_task_transition(Some(prior_status), &terminalized.task);
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
        let prior_task_status = task.status;
        task = match repository
            .mark_task_running(scope, task.run_uid, task.task_id, task.generation)
            .await
            .map_err(execution_error)?
        {
            TransitionOutcome::Applied(task) => {
                run_transition_applied = true;
                record_applied_task_transition(Some(prior_task_status), &task);
                task
            }
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
    if run_transition_applied {
        record_applied_run_transition(Some(prior_run_status), &run);
    }
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
            Ok(completed_outcome(value.clone(), task.actual.clone()))
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
    let invocation = invoke_capability_tool(
        workflow,
        ctx,
        CapabilityInvocationContext {
            identity,
            run,
            task,
            capability,
            session: &session,
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
            .saturating_add(serialized_len(&output.structured));
    }
    let outcome = capability_invocation_outcome(capability.idempotency_class, invocation, usage)?;
    let output_usage = outcome.usage.clone();
    let ExecutionTaskResult::Completed { output: value, .. } = &outcome.result else {
        return Ok(outcome);
    };
    if let Err(error) = validate_instance(
        &capability.output_schema,
        value,
        "execution_task.capability_output",
    ) {
        return Ok(failed_outcome(
            ExecutionFailureClass::InvalidOutput,
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
        return Ok(failed_outcome(
            ExecutionFailureClass::InvalidInput,
            "agent max_turns must be positive".to_string(),
            task.actual.clone(),
        ));
    }
    let session = load_session(workflow, ctx, run.session_id, task).await?;
    let skills = load_pinned_skills(workflow, ctx, run, task, skill_refs).await?;
    let capabilities = capability_refs
        .iter()
        .map(|reference| find_capability(run, reference))
        .collect::<Result<Vec<_>, _>>()?;
    let tool_names = capabilities
        .iter()
        .map(|capability| capability_tool_name(capability))
        .collect::<Result<Vec<_>, _>>()?;
    let tools = tool_names
        .iter()
        .filter_map(|name| workflow.router.tool_definition(name))
        .map(|definition| definition.anthropic_schema())
        .collect::<Vec<_>>();
    let mut messages = vec![
        ContextMessage::system(agent_system_prompt(instructions, &skills)),
        ContextMessage::user(
            json!({
                "resolved_input": task.input,
                "resume_inputs": task.resume_input_history,
            })
            .to_string(),
        ),
    ];
    let mut usage = task.actual.clone();
    for turn in 0..max_turns {
        let mut request = CompletionRequest {
            model: None,
            messages: messages.clone(),
            tools: tools.clone(),
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
        let response = restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
                return Ok(cancelled_outcome(reason?, usage));
            },
            response = crate::restate_identity::replay_safe_request(
                ctx.service_client::<LLMGatewayClient>()
                    .complete(Json::from(request)),
            )
                .call() => {
                    response?.into_inner()
                }
        };
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
            return Ok(parse_agent_result(&response.text, usage));
        }
        messages.push(ContextMessage::assistant_with_thought_signature(
            response.text,
            response.thought_signature,
        ));
        for (call_index, tool_call) in tool_calls.into_iter().enumerate() {
            let capability = capabilities
                .iter()
                .find(|capability| {
                    capability_tool_name(capability)
                        .is_ok_and(|name| name == tool_call.invocation.name)
                })
                .ok_or_else(|| TerminalError::new("agent emitted an undeclared capability"))?;
            let invocation = invoke_capability_tool(
                workflow,
                ctx,
                CapabilityInvocationContext {
                    identity,
                    run,
                    task,
                    capability,
                    session: &session,
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
                .saturating_add(serialized_len(&output.structured));
            let tool_use_id = tool_call
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| format!("execution-{}-{turn}-{call_index}", task.task_id));
            messages.push(ContextMessage::assistant_tool_call(
                tool_call.invocation,
                "",
            ));
            messages.push(ContextMessage::tool_result(
                tool_use_id,
                output.to_text(),
                Some(output.content.clone()),
            ));
        }
    }
    Ok(failed_outcome(
        ExecutionFailureClass::Terminal,
        format!("task-local agent exhausted max_turns={max_turns}"),
        usage,
    ))
}

struct CapabilityInvocationContext<'a> {
    identity: &'a moa_core::traits::Identity,
    run: &'a ExecutionRunRecord,
    task: &'a ExecutionTaskRecord,
    capability: &'a ExecutionCapability,
    session: &'a SessionMeta,
}

enum CapabilityInvocationResult {
    Output(Box<ToolOutput>),
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
            active_canary: None,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionTask {
                run_uid: run.run_uid,
                task_uid: task.task_id.as_uuid(),
                generation: task.generation,
            },
            capability_provenance: Some(&provenance),
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let GovernedInvocationOutcome::Completed(result) = outcome else {
        return Err(TerminalError::new("execution agents cannot invoke delegation tools").into());
    };
    if result.disposition == GovernedInvocationDisposition::ReviewPending {
        let promise_key = action_review_promise_key(tool_id.0, task.generation);
        let resolution = restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_PROMISE) => {
                return Ok(CapabilityInvocationResult::Terminal(
                    ExecutionTaskResult::Cancelled { reason: reason? }
                ));
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

async fn persist_task_outcome(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    prior_run: ExecutionRunRecord,
    task: ExecutionTaskRecord,
    outcome: ExecutionTaskOutcome,
) -> Result<PreparedTask, HandlerError> {
    let prior_task_status = task.status;
    let write = repository
        .record_task_outcome(scope, task.run_uid, task.task_id, task.generation, outcome)
        .await
        .map_err(execution_error)?;
    let (task, applied, persisted_run) = match write {
        TaskOutcomeWrite::Applied { run, task, .. } => (task, true, Some(run)),
        TaskOutcomeWrite::Replayed { run, task, .. } => (task, false, Some(run)),
        TaskOutcomeWrite::Rejected { task, .. } => (task, false, None),
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
    if applied {
        record_applied_task_transition(Some(prior_task_status), &task);
        record_applied_run_transition(Some(prior_run.status), &run);
    }
    Ok(PreparedTask {
        run,
        task,
        wake_run: false,
    })
}

async fn retry_task_generation(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    task: ExecutionTaskRecord,
) -> Result<PreparedTask, HandlerError> {
    let prior_status = task.status;
    let (task, applied) = match repository
        .retry_task(scope, task.run_uid, task.task_id, task.generation)
        .await
        .map_err(execution_error)?
    {
        TransitionOutcome::Applied(task) => (task, true),
        TransitionOutcome::AlreadyApplied(task) => (task, false),
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
    if applied {
        record_applied_task_transition(Some(prior_status), &task);
        record_applied_task_retry(&task);
    }
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
                .map_err(HandlerError::from)
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
                    .map_err(HandlerError::from)
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
) -> Result<(), HandlerError> {
    let promise_key = input_promise_key(task.task_id, task.generation);
    restate_sdk::select! {
        _ = ctx.promise::<String>(K_CANCEL_PROMISE) => {},
        _ = ctx.promise::<Json<Value>>(&promise_key) => {},
    }
    Ok(())
}

async fn await_review_or_cancel(
    ctx: &WorkflowContext<'_>,
    task: &ExecutionTaskRecord,
) -> Result<(), HandlerError> {
    let promise_key = review_promise_key(task.task_id, task.generation);
    restate_sdk::select! {
        _ = ctx.promise::<String>(K_CANCEL_PROMISE) => {},
        _ = ctx.promise::<Json<moa_execution::wire::ExecutionReviewDecision>>(
            &promise_key
        ) => {},
    }
    Ok(())
}

async fn await_signal_or_cancel(
    ctx: &WorkflowContext<'_>,
    task: &ExecutionTaskRecord,
    signal_name: &str,
) -> Result<(), HandlerError> {
    let promise_key = signal_promise_key(task.task_id, task.generation, signal_name);
    restate_sdk::select! {
        _ = ctx.promise::<String>(K_CANCEL_PROMISE) => {},
        _ = ctx.promise::<Json<Value>>(
            &promise_key
        ) => {},
    }
    Ok(())
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

fn capability_tool_name(capability: &ExecutionCapability) -> Result<String, HandlerError> {
    match &capability.source {
        CapabilitySource::BuiltInTool { name }
        | CapabilitySource::HandTool { name }
        | CapabilitySource::McpTool { name, .. } => Ok(name.clone()),
        CapabilitySource::ActionArtifact { tool_name, .. }
        | CapabilitySource::ConnectorAction { tool_name, .. }
        | CapabilitySource::SkillAction { tool_name, .. }
        | CapabilitySource::Memory { tool_name, .. } => Ok(tool_name.clone()),
        CapabilitySource::SkillCode { .. }
        | CapabilitySource::Knowledge { .. }
        | CapabilitySource::Model => {
            Err(TerminalError::new("capability has no governed tool owner in Task 6").into())
        }
    }
}

const fn capability_source_kind(source: &CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::BuiltInTool { .. } => "built_in_tool",
        CapabilitySource::HandTool { .. } => "hand_tool",
        CapabilitySource::McpTool { .. } => "mcp_tool",
        CapabilitySource::ActionArtifact { .. } => "action_artifact",
        CapabilitySource::ConnectorAction { .. } => "connector_action",
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
            serde_json::from_value(tool_output)
                .map(Box::new)
                .map(CapabilityInvocationResult::Output)
                .map_err(|error| {
                    TerminalError::new(format!("invalid action-review tool output: {error}")).into()
                })
        }
        ExecutionActionReviewResolution::Failed { class, message } => Ok(
            CapabilityInvocationResult::Terminal(ExecutionTaskResult::Failed { class, message }),
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

fn capability_invocation_outcome(
    idempotency_class: IdempotencyClass,
    invocation: CapabilityInvocationResult,
    usage: ExecutionUsage,
) -> Result<ExecutionTaskOutcome, HandlerError> {
    match invocation {
        CapabilityInvocationResult::Terminal(result) => Ok(ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result,
        }),
        CapabilityInvocationResult::Output(output) if output.is_error => {
            let class = if idempotency_class == IdempotencyClass::Idempotent {
                ExecutionFailureClass::Retryable
            } else {
                ExecutionFailureClass::Terminal
            };
            Ok(failed_outcome(class, output.to_text(), usage))
        }
        CapabilityInvocationResult::Output(output) => {
            let value = output
                .structured
                .clone()
                .unwrap_or_else(|| Value::String(output.to_text()));
            Ok(completed_outcome(value, usage))
        }
    }
}

fn parse_agent_result(text: &str, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    if let Ok(result) = serde_json::from_str::<ExecutionTaskResult>(text) {
        return ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result,
        };
    }
    match serde_json::from_str::<Value>(text) {
        Ok(output) => completed_outcome(output, usage),
        Err(error) => failed_outcome(
            ExecutionFailureClass::InvalidOutput,
            format!("agent final response is not JSON: {error}"),
            usage,
        ),
    }
}

fn validate_task_outcome(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    mut outcome: ExecutionTaskOutcome,
) -> ExecutionTaskOutcome {
    let ExecutionTaskResult::Completed { output, .. } = &outcome.result else {
        return outcome;
    };
    let validation = match &task.kind {
        LogicalTaskKind::CompletionVerifier { .. } => {
            let valid = output.as_object().is_some_and(|object| {
                object.len() == 2
                    && object.get("passed").and_then(Value::as_bool).is_some()
                    && object.contains_key("evidence")
            });
            valid.then_some(()).ok_or_else(|| {
                "completion verifier output must contain exactly boolean `passed` and `evidence`"
                    .to_string()
            })
        }
        _ => run
            .active_plan
            .definition
            .nodes
            .iter()
            .find(|node| node.id == task.node_id)
            .ok_or_else(|| format!("active plan has no node `{}`", task.node_id))
            .and_then(|node| {
                validate_instance(
                    task_output_schema(node),
                    output,
                    "execution_task.node_output",
                )
                .map_err(|error| error.to_string())
            }),
    };
    if let Err(message) = validation {
        outcome.result = ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::InvalidOutput,
            message,
        };
    }
    outcome
}

fn task_output_schema(node: &ExecutionNode) -> &Value {
    match &node.operation {
        ExecutionOperation::Map {
            item_output_schema, ..
        } => item_output_schema,
        ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Reduce { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => &node.output_schema,
    }
}

fn agent_system_prompt(instructions: &str, skills: &[String]) -> String {
    format!(
        "{instructions}\n\nPinned instruction skills:\n{}\n\nReturn only JSON. To finish normally return any JSON value. To request input or replanning, return the exact ExecutionTaskResult tagged shape with status needs_input or needs_replan.",
        skills.join("\n\n---\n\n")
    )
}

fn exhaust_retry_if_needed(
    task: &ExecutionTaskRecord,
    mut outcome: ExecutionTaskOutcome,
) -> ExecutionTaskOutcome {
    if task.attempt >= task.retry.max_attempts
        && let ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            message,
        } = &outcome.result
    {
        outcome.result = ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Terminal,
            message: format!(
                "retry policy exhausted after {} attempts: {message}",
                task.attempt
            ),
        };
    }
    outcome
}

fn retry_delay_ms(task: &ExecutionTaskRecord) -> u64 {
    let exponent = task.attempt.saturating_sub(2).min(31);
    task.retry
        .initial_backoff_ms
        .saturating_mul(1_u64 << exponent)
        .min(task.retry.max_backoff_ms)
}

fn completed_outcome(output: Value, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Completed {
            output,
            citations: Vec::new(),
        },
    }
}

fn failed_outcome(
    class: ExecutionFailureClass,
    message: String,
    usage: ExecutionUsage,
) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Failed { class, message },
    }
}

fn cancelled_outcome(reason: String, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Cancelled { reason },
    }
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

fn cleanup_task_hands(
    ctx: &WorkflowContext<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
) {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .release_execution_task_hands(Json::from(ReleaseExecutionTaskHandsRequest {
                session_id: run.session_id,
                run_uid: run.run_uid,
                task_id: task.task_id,
            })),
    )
    .send();
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    TerminalError::new(format!("execution task workflow failed: {error}")).into()
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::{
        ExecutionFailureClass, ExecutionNode, ExecutionOperation, ExecutionTaskResult,
        ExecutionUsage, MapTask, RetryPolicy,
    };
    use moa_core::types::tools::IdempotencyClass;
    use moa_execution::wire::ExecutionActionReviewResolution;
    use serde_json::json;

    use super::{
        CapabilityInvocationResult, action_review_invocation_result, capability_invocation_outcome,
        task_output_schema,
    };

    #[test]
    fn map_execution_task_uses_item_output_schema() {
        // Pins: each materialized map task validates its own result before the
        // scheduler builds and validates the aggregate map-node output.
        let item_schema = json!({"type": "object", "required": ["symbol"]});
        let aggregate_schema = json!({"type": "object", "required": ["items"]});
        let node = ExecutionNode {
            id: "quotes".to_string(),
            requirement_ids: vec!["prices".to_string()],
            depends_on: Vec::new(),
            when: None,
            input: json!({}),
            output_schema: aggregate_schema,
            operation: ExecutionOperation::Map {
                items: json!([]),
                item_key: "/symbol".to_string(),
                max_items: 10,
                item_output_schema: item_schema.clone(),
                task: MapTask::Agent {
                    instructions: "quote".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 1,
                },
            },
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
            },
            budget: None,
        };

        assert_eq!(task_output_schema(&node), &item_schema);
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
}
