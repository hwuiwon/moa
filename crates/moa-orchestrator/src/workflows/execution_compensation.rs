//! Durable keyed workflow for one exact execution compensation.

use std::{collections::BTreeSet, sync::Arc};

use moa_artifacts::execution_plan::{ExecutionFailureClass, ExecutionUsage};
use moa_config::SessionLimitsConfig;
use moa_core::{
    traits::{ChannelAdapter, SessionStore as _},
    types::{
        action_policy::{ActionClass, CapabilityProvenance},
        channel::Channel,
        completion::{ToolCallContent, ToolInvocation},
        identifiers::ToolCallId,
        session::SessionMeta,
        tools::IdempotencyClass,
    },
};
use moa_execution::{
    capability::{CapabilitySource, ExecutionCapability},
    repository::{
        ActionReviewResolutionWrite, CompensationClaimOutcome, CompensationOutcomeWrite,
        ExecutionRepository, ExecutionRunRecord, ExecutionScope, ExecutionTaskRecord,
    },
    schema::validate_instance,
    state::{
        CompensationRegistrationProjection, CompensationStatus, ExecutionCompensationOutcome,
        ExecutionRunStatus, LogicalTaskKind,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionCompensationReviewAcknowledgement,
        ExecutionCompensationReviewResolutionRequest, ExecutionCompensationWorkflowRequest,
        ExecutionToolDispatchRejection,
    },
};
use moa_observability::{
    propagation::link_remote_context_from_link_headers,
    restate_observability::annotate_restate_handler_span,
};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::{
    ctx::RequestHeaders,
    services::tool_executor::{ReleaseExecutionCompensationHandsRequest, ToolExecutorClient},
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, GovernedInvocationResult, invoke_governed_tool,
    },
};

/// Durable workflow surface for one stable compensation registration.
#[restate_sdk::workflow]
pub trait ExecutionCompensation {
    /// Executes the exact pinned compensator through bounded generation-fenced retries.
    async fn run(request: Json<ExecutionCompensationWorkflowRequest>) -> Result<(), HandlerError>;

    /// Persists and resolves one compensation action-review delivery.
    #[shared]
    async fn resolve_action_review(
        request: Json<ExecutionCompensationReviewResolutionRequest>,
    ) -> Result<Json<ExecutionCompensationReviewAcknowledgement>, HandlerError>;
}

/// Runtime dependencies for one governed compensation workflow.
#[derive(Clone)]
pub struct ExecutionCompensationImpl {
    repository: ExecutionRepository,
    session_store: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
    channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl ExecutionCompensationImpl {
    /// Creates one compensation workflow over the exact execution and tool-runtime stores.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
        channel_adapters: Arc<std::collections::HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            session_store,
            session_limits,
            channel_adapters,
        }
    }
}

impl ExecutionCompensation for ExecutionCompensationImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: dispatched only by the owning ExecutionRun workflow after reverse-order repository claim.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExecutionCompensationWorkflowRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionCompensation", "run");
        let request = request.into_inner();
        require_compensation_key(ctx.key(), request.compensation_id)?;
        if request.identity.tenant_id != request.tenant_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution compensation identity tenant mismatch",
            )
            .into());
        }
        annotate_compensation_span(request.run_uid, request.compensation_id);
        let scope = execution_scope(&request);
        let mut generation = request.generation;
        let mut operation_index = 0_u64;
        loop {
            let repository = self.repository.clone();
            let load_request = request.clone();
            let prepared = ctx
                .run(|| async move {
                    prepare_compensation(repository, scope, &load_request, generation)
                        .await
                        .map(Json::from)
                })
                .name(format!("execution_compensation_prepare_{operation_index}"))
                .await?
                .into_inner();
            operation_index = operation_index.saturating_add(1);
            let prepared = match prepared {
                PreparedCompensation::Ready(prepared) => prepared,
                PreparedCompensation::Settled => {
                    cleanup_compensation_hands(&ctx, &request).await?;
                    return Ok(());
                }
            };

            let outcome = execute_compensation_attempt(self, &ctx, &request, &prepared).await?;
            let repository = self.repository.clone();
            let run_uid = request.run_uid;
            let compensation_id = request.compensation_id;
            let recorded = ctx
                .run(|| async move {
                    let write = repository
                        .record_compensation_outcome(
                            scope,
                            run_uid,
                            compensation_id,
                            generation,
                            outcome,
                        )
                        .await
                        .map_err(execution_error)?;
                    compensation_record_step(write).map(Json::from)
                })
                .name(format!("execution_compensation_outcome_{operation_index}"))
                .await?
                .into_inner();
            operation_index = operation_index.saturating_add(1);
            match recorded {
                CompensationRecordStep::Settled => {
                    cleanup_compensation_hands(&ctx, &request).await?;
                    return Ok(());
                }
                CompensationRecordStep::Retry { next_generation } => {
                    let repository = self.repository.clone();
                    let claim = ctx
                        .run(|| async move {
                            let outcome = repository
                                .claim_next_compensation(
                                    scope,
                                    run_uid,
                                    compensation_id,
                                    next_generation,
                                )
                                .await
                                .map_err(execution_error)?;
                            claim_retry_step(outcome, next_generation).map(Json::from)
                        })
                        .name(format!("execution_compensation_reclaim_{operation_index}"))
                        .await?
                        .into_inner();
                    operation_index = operation_index.saturating_add(1);
                    let CompensationRetryClaim::Claimed {
                        generation: claimed,
                    } = claim
                    else {
                        cleanup_compensation_hands(&ctx, &request).await?;
                        return Ok(());
                    };
                    generation = claimed;
                }
                CompensationRecordStep::Conflict => {
                    cleanup_compensation_hands(&ctx, &request).await?;
                    return Ok(());
                }
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only by the bounded compensation-review outbox dispatcher from a terminal persisted review row.
    async fn resolve_action_review(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExecutionCompensationReviewResolutionRequest>,
    ) -> Result<Json<ExecutionCompensationReviewAcknowledgement>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionCompensation", "resolve_action_review");
        let headers = ctx.request_headers();
        let _ = link_remote_context_from_link_headers(&tracing::Span::current(), |name| {
            headers.get(name).cloned()
        });
        let request = request.into_inner();
        require_compensation_key(ctx.key(), request.compensation_id)?;
        annotate_compensation_span(request.run_uid, request.compensation_id);
        let repository = self.repository.clone();
        let record_request = request.clone();
        let write = ctx
            .run(|| async move {
                repository
                    .record_compensation_action_review_resolution(
                        ExecutionScope::ControlPlane,
                        record_request.run_uid,
                        record_request.compensation_id,
                        record_request.generation,
                        record_request.review_uid,
                        &record_request.resolution,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error)
            })
            .name(format!(
                "execution_compensation_review_resolution_{}",
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
                ExecutionCompensationReviewAcknowledgement::Applied
            }
            ActionReviewResolutionWrite::Replayed => {
                ctx.resolve_promise(
                    &action_review_promise_key(request.review_uid, request.generation),
                    Json::from(request.resolution),
                );
                ExecutionCompensationReviewAcknowledgement::Replayed
            }
            ActionReviewResolutionWrite::AuditedStale | ActionReviewResolutionWrite::NotFound => {
                ExecutionCompensationReviewAcknowledgement::AuditedStale
            }
        };
        Ok(Json::from(acknowledgement))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PreparedCompensation {
    Ready(Box<PreparedCompensationAttempt>),
    Settled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PreparedCompensationAttempt {
    run: ExecutionRunRecord,
    registration: CompensationRegistrationProjection,
    forward_task: ExecutionTaskRecord,
}

async fn prepare_compensation(
    repository: ExecutionRepository,
    scope: ExecutionScope,
    request: &ExecutionCompensationWorkflowRequest,
    generation: u64,
) -> Result<PreparedCompensation, HandlerError> {
    let snapshot = repository
        .load_compensation_snapshot(scope, request.run_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    if snapshot.run.tenant_id != request.tenant_id
        || snapshot.run.contact_id != request.contact_id
        || snapshot.run.session_id != request.session_id
    {
        return Err(
            TerminalError::new_with_code(409, "execution compensation scope mismatch").into(),
        );
    }
    if snapshot.run.status != ExecutionRunStatus::Compensating {
        return Ok(PreparedCompensation::Settled);
    }
    let Some(registration) = snapshot
        .registrations
        .into_iter()
        .find(|candidate| candidate.compensation_id == request.compensation_id)
    else {
        return Err(TerminalError::new_with_code(404, "execution compensation not found").into());
    };
    if registration.status.is_settled() {
        return Ok(PreparedCompensation::Settled);
    }
    if registration.status != CompensationStatus::Running || registration.generation != generation {
        return Ok(PreparedCompensation::Settled);
    }
    let forward_task = repository
        .load_task(scope, request.run_uid, registration.forward_task_id)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "compensation forward task not found"))?;
    Ok(PreparedCompensation::Ready(Box::new(
        PreparedCompensationAttempt {
            run: snapshot.run,
            registration,
            forward_task,
        },
    )))
}

async fn execute_compensation_attempt(
    workflow: &ExecutionCompensationImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationWorkflowRequest,
    prepared: &PreparedCompensationAttempt,
) -> Result<ExecutionCompensationOutcome, HandlerError> {
    let mut usage = prepared
        .registration
        .outcome
        .as_ref()
        .map(ExecutionCompensationOutcome::usage)
        .cloned()
        .unwrap_or(ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        });
    let result = invoke_exact_compensator(workflow, ctx, request, prepared).await?;
    if result.is_ok() {
        usage.tool_calls = usage.tool_calls.saturating_add(1);
    }
    Ok(match result {
        Ok(CompensatorResult::Output(output)) => {
            usage.retrieved_bytes = usage
                .retrieved_bytes
                .saturating_add(serialized_len(&output));
            let capability = match find_catalog_capability(
                &prepared.run,
                &prepared.registration.compensator.compensator,
            ) {
                Ok(capability) => capability,
                Err(message) => return Ok(failed_compensation(message, false, usage)),
            };
            if let Err(error) = validate_instance(
                &capability.output_schema,
                &output,
                "execution_compensation.output",
            ) {
                return Ok(ExecutionCompensationOutcome::UnknownOutcome {
                    message: format!(
                        "compensator returned an invalid output after possible commit: {error}"
                    ),
                    usage,
                });
            }
            ExecutionCompensationOutcome::Completed { output, usage }
        }
        Ok(CompensatorResult::Failed { message, retryable }) => {
            failed_compensation(message, retryable, usage)
        }
        Ok(CompensatorResult::UnknownOutcome { message }) => {
            ExecutionCompensationOutcome::UnknownOutcome { message, usage }
        }
        Err(message) => failed_compensation(message, false, usage),
    })
}

enum CompensatorResult {
    Output(Value),
    Failed { message: String, retryable: bool },
    UnknownOutcome { message: String },
}

async fn invoke_exact_compensator(
    workflow: &ExecutionCompensationImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationWorkflowRequest,
    prepared: &PreparedCompensationAttempt,
) -> Result<Result<CompensatorResult, String>, HandlerError> {
    let capability = match validate_runtime_contract(prepared) {
        Ok(capability) => capability,
        Err(message) => return Ok(Err(message)),
    };
    if let Err(error) = validate_instance(
        &capability.input_schema,
        &prepared.registration.mapped_input,
        "execution_compensation.input",
    ) {
        return Ok(Err(format!(
            "compensator mapped input failed pinned schema: {error}"
        )));
    }
    let session = load_session(
        workflow,
        ctx,
        request.session_id,
        request.compensation_id,
        prepared.registration.generation,
        prepared.registration.attempt,
    )
    .await?;
    let tool_name = match capability_tool_name(capability) {
        Ok(tool_name) => tool_name,
        Err(message) => return Ok(Err(message)),
    };
    let tool_id = ToolCallId(uuid::Uuid::new_v5(
        &request.compensation_id.as_uuid(),
        format!("generation:{}", prepared.registration.generation).as_bytes(),
    ));
    let tool_call = ToolCallContent {
        invocation: ToolInvocation {
            id: Some(tool_id.to_string()),
            name: tool_name.clone(),
            input: prepared.registration.mapped_input.clone(),
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
        step_id: Some(format!(
            "compensation:{}",
            prepared.registration.forward_task_id
        )),
    };
    let governed = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: &session,
            identity: &request.identity,
            session_id: request.session_id,
            tool_id,
            tool_call: &tool_call,
            allowed_tools: &allowed_tools,
            expected_tool_contract_revision: Some(&capability.contract_revision),
            active_canary: None,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionCompensation {
                run_uid: request.run_uid,
                compensation_id: request.compensation_id.as_uuid(),
                generation: prepared.registration.generation,
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
    let result = match classify_governed_compensation_outcome(governed) {
        GovernedCompensationOutcome::Completed(result) => result,
        GovernedCompensationOutcome::Settled(result) => return Ok(Ok(result)),
    };
    if result.disposition == GovernedInvocationDisposition::ReviewPending {
        let resolution = ctx
            .promise::<Json<ExecutionActionReviewResolution>>(&action_review_promise_key(
                result.tool_id.0,
                prepared.registration.generation,
            ))
            .await?
            .into_inner();
        return Ok(Ok(compensation_review_result(resolution)));
    }
    if result.output.is_error() {
        return Ok(Ok(CompensatorResult::Failed {
            message: result.output.safe_output.to_text(),
            retryable: result.disposition == GovernedInvocationDisposition::Executed,
        }));
    }
    Ok(Ok(CompensatorResult::Output(
        result
            .output
            .safe_output
            .structured_payload()
            .cloned()
            .unwrap_or_else(|| Value::String(result.output.safe_output.to_text())),
    )))
}

enum GovernedCompensationOutcome {
    Completed(Box<GovernedInvocationResult>),
    Settled(CompensatorResult),
}

fn classify_governed_compensation_outcome(
    outcome: GovernedInvocationOutcome,
) -> GovernedCompensationOutcome {
    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            GovernedCompensationOutcome::Completed(result)
        }
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => {
            GovernedCompensationOutcome::Settled(CompensatorResult::UnknownOutcome { message })
        }
        GovernedInvocationOutcome::NotDispatched { reason, .. } => {
            GovernedCompensationOutcome::Settled(CompensatorResult::Failed {
                message: execution_dispatch_rejection_message(reason),
                retryable: false,
            })
        }
        GovernedInvocationOutcome::Delegation { .. } => {
            GovernedCompensationOutcome::Settled(CompensatorResult::Failed {
                message: "compensator attempted an unsupported delegation path".to_string(),
                retryable: false,
            })
        }
    }
}

fn validate_runtime_contract(
    prepared: &PreparedCompensationAttempt,
) -> Result<&ExecutionCapability, String> {
    let LogicalTaskKind::Capability {
        reference: forward_reference,
    } = &prepared.forward_task.kind
    else {
        return Err("registered compensation forward task is not a direct capability".to_string());
    };
    if prepared.forward_task.compensation_contract.as_ref()
        != Some(&prepared.registration.compensator)
    {
        return Err("registered compensation drifted from the forward task contract".to_string());
    }
    let forward = find_catalog_capability(&prepared.run, forward_reference)?;
    if !forward
        .rollback
        .as_ref()
        .is_some_and(|rollback| rollback.matches(&prepared.registration.compensator))
    {
        return Err("pinned forward capability no longer promises the exact rollback".to_string());
    }
    let compensator = find_catalog_capability(
        &prepared.run,
        &prepared.registration.compensator.compensator,
    )?;
    if compensator.action_class == ActionClass::Read {
        return Err("compensator catalog entry is read-only".to_string());
    }
    if compensator.idempotency_class != IdempotencyClass::Idempotent {
        return Err("compensator catalog entry is not idempotent".to_string());
    }
    Ok(compensator)
}

fn find_catalog_capability<'a>(
    run: &'a ExecutionRunRecord,
    reference: &moa_artifacts::execution_plan::CapabilityReference,
) -> Result<&'a ExecutionCapability, String> {
    if !run.authorization.capability_refs.contains(reference) {
        return Err("compensator is outside the persisted authorization envelope".to_string());
    }
    run.catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| "compensator is absent from the persisted catalog".to_string())
}

fn capability_tool_name(capability: &ExecutionCapability) -> Result<String, String> {
    capability
        .source
        .model_visible_tool_name()
        .map(str::to_string)
        .ok_or_else(|| "compensator has no governed tool owner".to_string())
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

fn compensation_review_result(resolution: ExecutionActionReviewResolution) -> CompensatorResult {
    match resolution {
        ExecutionActionReviewResolution::Completed { tool_output } => {
            let output = match serde_json::from_value::<moa_core::types::tools::SecuredToolOutput>(
                tool_output,
            ) {
                Ok(output) => output,
                Err(error) => {
                    return CompensatorResult::UnknownOutcome {
                        message: format!("invalid compensation review tool output: {error}"),
                    };
                }
            };
            if output.is_error() {
                CompensatorResult::Failed {
                    message: output.safe_output.to_text(),
                    retryable: true,
                }
            } else {
                CompensatorResult::Output(
                    output
                        .safe_output
                        .structured_payload()
                        .cloned()
                        .unwrap_or_else(|| Value::String(output.safe_output.to_text())),
                )
            }
        }
        ExecutionActionReviewResolution::Failed { class, message } => CompensatorResult::Failed {
            message,
            retryable: class == ExecutionFailureClass::Retryable,
        },
        ExecutionActionReviewResolution::UnknownOutcome { message } => {
            CompensatorResult::UnknownOutcome { message }
        }
        ExecutionActionReviewResolution::NotDispatched { reason } => CompensatorResult::Failed {
            message: execution_dispatch_rejection_message(reason),
            retryable: false,
        },
        ExecutionActionReviewResolution::Denied { reason }
        | ExecutionActionReviewResolution::TimedOut { reason } => CompensatorResult::Failed {
            message: reason,
            retryable: false,
        },
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

fn failed_compensation(
    message: String,
    retryable: bool,
    usage: ExecutionUsage,
) -> ExecutionCompensationOutcome {
    ExecutionCompensationOutcome::Failed {
        message,
        retryable,
        usage,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CompensationRecordStep {
    Settled,
    Retry { next_generation: u64 },
    Conflict,
}

fn compensation_record_step(
    write: CompensationOutcomeWrite,
) -> Result<CompensationRecordStep, HandlerError> {
    match write {
        CompensationOutcomeWrite::Completed(_)
        | CompensationOutcomeWrite::Failed(_)
        | CompensationOutcomeWrite::UnknownOutcome(_) => Ok(CompensationRecordStep::Settled),
        CompensationOutcomeWrite::Requeued(registration) => Ok(CompensationRecordStep::Retry {
            next_generation: registration.generation,
        }),
        CompensationOutcomeWrite::Replayed(registration) => {
            if registration.status == CompensationStatus::Pending {
                Ok(CompensationRecordStep::Retry {
                    next_generation: registration.generation,
                })
            } else {
                Ok(CompensationRecordStep::Settled)
            }
        }
        CompensationOutcomeWrite::NotFound => {
            Err(TerminalError::new_with_code(404, "execution compensation not found").into())
        }
        CompensationOutcomeWrite::Conflict => Ok(CompensationRecordStep::Conflict),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CompensationRetryClaim {
    Claimed { generation: u64 },
    Settled,
}

fn claim_retry_step(
    outcome: CompensationClaimOutcome,
    expected_generation: u64,
) -> Result<CompensationRetryClaim, HandlerError> {
    match outcome {
        CompensationClaimOutcome::Claimed(registration)
        | CompensationClaimOutcome::Replayed(registration)
            if registration.generation == expected_generation =>
        {
            Ok(CompensationRetryClaim::Claimed {
                generation: registration.generation,
            })
        }
        CompensationClaimOutcome::Claimed(_) | CompensationClaimOutcome::Replayed(_) => {
            Ok(CompensationRetryClaim::Settled)
        }
        CompensationClaimOutcome::BudgetRejected(_) => Ok(CompensationRetryClaim::Settled),
        CompensationClaimOutcome::Conflict => Ok(CompensationRetryClaim::Settled),
        CompensationClaimOutcome::NotFound => {
            Err(TerminalError::new_with_code(404, "execution compensation not found").into())
        }
    }
}

async fn load_session(
    workflow: &ExecutionCompensationImpl,
    ctx: &WorkflowContext<'_>,
    session_id: moa_core::types::identifiers::SessionId,
    compensation_id: moa_execution::state::CompensationId,
    generation: u64,
    attempt: u64,
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
            "execution_compensation_load_session_{compensation_id}_{generation}_{attempt}"
        ))
        .await?
        .into_inner())
}

fn action_review_promise_key(review_uid: uuid::Uuid, generation: u64) -> String {
    format!("execution_compensation_action_review:{review_uid}:{generation}")
}

fn require_compensation_key(
    key: &str,
    compensation_id: moa_execution::state::CompensationId,
) -> Result<(), HandlerError> {
    if key == compensation_id.to_string() {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(404, "execution compensation id mismatch").into())
    }
}

fn execution_scope(request: &ExecutionCompensationWorkflowRequest) -> ExecutionScope {
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

fn annotate_compensation_span(
    run_uid: uuid::Uuid,
    compensation_id: moa_execution::state::CompensationId,
) {
    let span = tracing::Span::current();
    span.set_attribute("moa.execution.run_uid", run_uid.to_string());
    span.set_attribute("moa.execution.compensation_id", compensation_id.to_string());
}

fn serialized_len(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_default()
}

async fn cleanup_compensation_hands(
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationWorkflowRequest,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .release_execution_compensation_hands(Json::from(
                ReleaseExecutionCompensationHandsRequest {
                    session_id: request.session_id,
                    run_uid: request.run_uid,
                    compensation_id: request.compensation_id,
                },
            )),
    )
    .call()
    .await?;
    Ok(())
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    match error {
        storage @ moa_execution::Error::Storage { .. } => HandlerError::from(storage),
        deterministic => TerminalError::new(format!(
            "execution compensation workflow failed: {deterministic}"
        ))
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::completion::ToolInvocation;
    use serde_json::json;

    #[test]
    fn stable_compensation_tool_id_changes_only_with_generation() {
        // Pins: replay of one compensation generation addresses the same governed
        // invocation, while a persisted retry generation receives a fresh identity.
        let compensation_id =
            moa_execution::state::CompensationId::from_uuid(uuid::Uuid::from_u128(0xc011));
        let first = ToolCallId(uuid::Uuid::new_v5(
            &compensation_id.as_uuid(),
            b"generation:1",
        ));
        let replay = ToolCallId(uuid::Uuid::new_v5(
            &compensation_id.as_uuid(),
            b"generation:1",
        ));
        let retry = ToolCallId(uuid::Uuid::new_v5(
            &compensation_id.as_uuid(),
            b"generation:2",
        ));
        assert_eq!(first, replay);
        assert_ne!(first, retry);
    }

    #[test]
    fn review_timeout_is_terminal_not_retryable() {
        // Pins: an expired reviewed undo requires manual repair and never resends
        // the external effect under a new compensation generation.
        let result = compensation_review_result(ExecutionActionReviewResolution::TimedOut {
            reason: "review expired".to_string(),
        });
        assert!(matches!(
            result,
            CompensatorResult::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn governed_compensation_ambiguity_is_terminal_unknown() {
        // Pins: an ambiguous compensator effect is never retried because the undo
        // may already have committed and a second dispatch could double-apply it.
        let classified =
            classify_governed_compensation_outcome(GovernedInvocationOutcome::UnknownOutcome {
                tool_id: ToolCallId(uuid::Uuid::from_u128(91)),
                invocation: ToolInvocation {
                    id: Some("tool-91".to_string()),
                    name: "fixture_undo".to_string(),
                    input: json!({"id": 91}),
                },
                message: "undo result is ambiguous".to_string(),
            });
        assert!(matches!(
            classified,
            GovernedCompensationOutcome::Settled(CompensatorResult::UnknownOutcome { message })
                if message == "undo result is ambiguous"
        ));
    }

    #[test]
    fn governed_compensation_admission_rejection_is_terminal_not_unknown() {
        // Pins: compensation admission rejection is definitive zero-effect; the
        // reverse driver stops for manual repair without retrying or claiming ambiguity.
        let classified =
            classify_governed_compensation_outcome(GovernedInvocationOutcome::NotDispatched {
                tool_id: ToolCallId(uuid::Uuid::from_u128(92)),
                invocation: ToolInvocation {
                    id: Some("tool-92".to_string()),
                    name: "fixture_undo".to_string(),
                    input: json!({"id": 92}),
                },
                reason: ExecutionToolDispatchRejection::StaleGeneration,
            });
        assert!(matches!(
            classified,
            GovernedCompensationOutcome::Settled(CompensatorResult::Failed {
                retryable: false,
                message,
            }) if message.ends_with("stale_generation")
        ));
    }

    #[test]
    fn reviewed_compensation_ambiguity_is_terminal_unknown() {
        // Pins: the durable review dispatcher preserves ToolExecutor ambiguity all
        // the way into the compensation state machine without retry classification.
        let result = compensation_review_result(ExecutionActionReviewResolution::UnknownOutcome {
            message: "reviewed undo is ambiguous".to_string(),
        });
        assert!(matches!(
            result,
            CompensatorResult::UnknownOutcome { message }
                if message == "reviewed undo is ambiguous"
        ));
    }

    #[test]
    fn reviewed_compensator_error_preserves_idempotent_retry() {
        // Pins: compiler and runtime admission require an idempotent compensator,
        // so a definitive reviewed error has the same bounded retry eligibility as
        // a direct compensator error; denial, timeout, and ambiguity remain terminal.
        let output = moa_core::types::tools::SecuredToolOutput::assessed_safe(
            moa_core::types::tools::ToolOutput::from(moa_core::error::ToolFailureClass::Fatal {
                reason: "temporary undo failure".to_string(),
            }),
            moa_core::types::security::ToolCapabilityId::builtin("fixture_undo"),
        );
        let result = compensation_review_result(ExecutionActionReviewResolution::Completed {
            tool_output: serde_json::to_value(output).expect("secured output should serialize"),
        });
        assert!(matches!(
            result,
            CompensatorResult::Failed {
                retryable: true,
                message,
            } if message.contains("temporary undo failure")
        ));
    }

    #[test]
    fn reviewed_compensation_admission_rejection_is_terminal_not_unknown() {
        // Pins: the atomic owner fence proved that no undo began, so admission
        // rejection requires manual repair without retry and never claims ambiguity.
        let result = compensation_review_result(ExecutionActionReviewResolution::NotDispatched {
            reason: ExecutionToolDispatchRejection::OperationNotRunning,
        });
        assert!(matches!(
            result,
            CompensatorResult::Failed {
                retryable: false,
                message,
            } if message.ends_with("operation_not_running")
        ));
    }
}
