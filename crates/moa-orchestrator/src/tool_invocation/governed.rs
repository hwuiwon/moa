//! Governed tool invocation coordination for turn workflows.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_config::SessionLimitsConfig;
use moa_core::traits::ChannelAdapter;
use moa_core::{
    events::Event, types::action_policy::ActionPolicyEffect,
    types::action_policy::ActionReviewOwner, types::action_policy::CapabilityProvenance,
    types::action_policy::ExecutionCompensationOrigin, types::action_policy::ExecutionTaskOrigin,
    types::channel::Channel, types::completion::CompletionRequest,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::identifiers::SessionId, types::identifiers::ToolCallId, types::resource::ResourceBudget,
    types::security::ToolCapabilityId, types::session::SessionMeta,
    types::tools::SecuredToolOutput, types::tools::ToolCallRequest, types::tools::ToolOutput,
    types::tools::TrustedSandboxFileManifestRef,
    types::worker::tool_schema::is_delegation_tool_name,
};
use moa_execution::CapabilityPolicyContext;
use moa_hands::{ToolCatalogDrift, ToolCatalogPin};
use moa_observability::restate_observability::{event_persist_span, tool_dispatch_span};
use moa_observability::{record_turn_event_persist_duration, record_turn_tool_dispatch_duration};
use moa_security::{OutputClassification, classify_tool_output};
use moa_session::PostgresSessionStore;
use moa_wire::session_store::{AppendEventRequest, RecordSegmentToolUseRequest};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::services::{
    action_policy::{
        ActionPolicyClient, PrepareActionReviewRequest, PreparedActionReview,
        PreparedActionReviewResponse,
    },
    action_reviews::{ActionReviewsClient, RequestActionReview},
    session_store::RestateSessionStoreClient,
    tool_executor::{
        ExecutionToolCallOrigin, ExecutionToolCallOutcome, ExecutionToolCallRequest,
        ScopedToolCatalogRequest, ToolExecutorClient,
    },
};
use crate::turn::util::{
    blocked_canary_tool_output, denied_tool_output, disallowed_tool_output, tool_input_leaks_canary,
};
use crate::workflows::turn_progress;

/// Completion metadata key carrying the exact catalog pin used for tool schemas.
pub(crate) const TOOL_CATALOG_PIN_METADATA_KEY: &str = "_moa.tools.catalog_pin";

/// Decodes the catalog pin paired atomically with a completion request's tools.
pub(crate) fn completion_tool_catalog_pin(
    request: &CompletionRequest,
) -> Result<ToolCatalogPin, HandlerError> {
    let value = request
        .metadata
        .get(TOOL_CATALOG_PIN_METADATA_KEY)
        .cloned()
        .ok_or_else(|| TerminalError::new("completion request is missing its tool catalog pin"))?;
    serde_json::from_value(value).map_err(|error| {
        TerminalError::new(format!(
            "completion request tool catalog pin is invalid: {error}"
        ))
        .into()
    })
}

/// Workflow origin metadata for a governed tool invocation.
///
/// Every variant carries the exact fence the owning runtime admitted the call
/// under, so an action review queued from this call records a typed
/// [`ActionReviewOwner`] instead of leaving ownership to later inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GovernedInvocationOrigin<'a> {
    /// Tool call came from the root session turn.
    RootTurn {
        /// Coordinator turn id that produced the tool call.
        turn_id: &'a str,
        /// Session turn generation that admitted the coordinator turn.
        generation: u64,
    },
    /// Tool call came from a worker turn.
    Worker {
        /// Worker object id.
        worker_id: &'a str,
        /// Worker turn id that produced the tool call.
        turn_id: &'a str,
        /// Worker generation that admitted the worker turn.
        generation: u64,
    },
    /// Tool call belongs to one persisted dynamic execution task.
    ExecutionTask {
        /// Owning execution run identifier.
        run_uid: uuid::Uuid,
        /// Owning persisted task identifier.
        task_uid: uuid::Uuid,
        /// Task generation fenced by the execution workflow.
        generation: u64,
    },
    /// Tool call belongs to one persisted execution compensation.
    ExecutionCompensation {
        /// Owning execution run identifier.
        run_uid: uuid::Uuid,
        /// Stable compensation identifier within the run.
        compensation_id: uuid::Uuid,
        /// Compensation generation fenced by the execution workflow.
        generation: u64,
    },
}

impl GovernedInvocationOrigin<'_> {
    /// Returns the typed action-review owner for this origin.
    fn action_review_owner(self, session_id: SessionId) -> ActionReviewOwner {
        match self {
            Self::RootTurn {
                turn_id,
                generation,
            } => ActionReviewOwner::Coordinator {
                session_id,
                turn_id: turn_id.to_string(),
                generation,
            },
            Self::Worker {
                worker_id,
                turn_id,
                generation,
            } => ActionReviewOwner::Worker {
                session_id,
                worker_id: worker_id.to_string(),
                turn_id: turn_id.to_string(),
                generation,
            },
            Self::ExecutionTask {
                run_uid,
                task_uid,
                generation,
            } => ActionReviewOwner::ExecutionTask {
                session_id,
                origin: ExecutionTaskOrigin {
                    run_uid,
                    task_uid,
                    generation,
                },
            },
            Self::ExecutionCompensation {
                run_uid,
                compensation_id,
                generation,
            } => ActionReviewOwner::ExecutionCompensation {
                session_id,
                origin: ExecutionCompensationOrigin {
                    run_uid,
                    compensation_id,
                    generation,
                },
            },
        }
    }
}

/// Request for coordinating one governed tool invocation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GovernedInvocationRequest<'a> {
    /// Session metadata used for policy and execution.
    pub(crate) session: &'a SessionMeta,
    /// Exact authenticated caller and delegation provenance admitted for this call.
    pub(crate) identity: &'a moa_core::traits::Identity,
    /// Session event stream that receives tool events.
    pub(crate) session_id: SessionId,
    /// Stable tool call id for event correlation.
    pub(crate) tool_id: ToolCallId,
    /// Provider tool-call block.
    pub(crate) tool_call: &'a ToolCallContent,
    /// Allowed tool names selected for this turn.
    pub(crate) allowed_tools: &'a BTreeSet<String>,
    /// Exact governed contract revision that admitted this tool call.
    pub(crate) expected_tool_contract_revision: Option<&'a str>,
    /// Active prompt-injection canary marker, when present.
    pub(crate) active_canary: Option<&'a str>,
    /// Trusted sandbox file manifest selected by the runtime that built this tool call.
    pub(crate) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    /// Root or worker origin metadata.
    pub(crate) origin: GovernedInvocationOrigin<'a>,
    /// Capability-level provenance, independent of execution-task ownership.
    pub(crate) capability_provenance: Option<&'a CapabilityProvenance>,
    /// Immutable capability policy floor pinned by a durable execution catalog.
    pub(crate) capability_policy_context: Option<&'a CapabilityPolicyContext>,
    /// Downward-only resource slice that bounds the eventual tool dispatch.
    pub(crate) resource_budget: ResourceBudget,
}

/// Completed governed tool invocation result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GovernedInvocationResult {
    /// Stable tool-call id used for session and worker records.
    pub(crate) tool_id: ToolCallId,
    /// Tool invocation copied from the provider tool call.
    pub(crate) invocation: ToolInvocation,
    /// Model-visible classified tool output.
    pub(crate) output: SecuredToolOutput,
    /// Outcome classification for workflow-local recording.
    pub(crate) disposition: GovernedInvocationDisposition,
    /// Event ownership plan used for the output event.
    pub(crate) event_plan: GovernedInvocationEventPlan,
}

impl GovernedInvocationResult {
    /// Returns whether the caller should record a successful segment tool use.
    pub(crate) fn should_record_segment_tool_use(&self) -> bool {
        self.disposition == GovernedInvocationDisposition::Executed && !self.output.is_error()
    }

    /// Returns whether a worker should record the result as denied.
    pub(crate) fn should_record_denied_worker_tool(&self) -> bool {
        matches!(
            self.disposition,
            GovernedInvocationDisposition::Disallowed
                | GovernedInvocationDisposition::Denied
                | GovernedInvocationDisposition::CanaryBlocked
                | GovernedInvocationDisposition::ReviewPending
                | GovernedInvocationDisposition::CatalogDrift
        )
    }
}

/// Non-delegation outcome classification for one tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GovernedInvocationDisposition {
    /// Tool was not in the model's allowed tool set for the turn.
    Disallowed,
    /// Model-authored input failed the tool's schema validation.
    InvalidInput,
    /// Action policy denied the tool call.
    Denied,
    /// Admin-review payload was blocked before persistence by canary screening.
    CanaryBlocked,
    /// Tool call was queued for tenant-admin review.
    ReviewPending,
    /// The governed tool contract that admitted the call is no longer active.
    CatalogDrift,
    /// Tool call was executed through `ToolExecutor`.
    Executed,
}

/// Event ownership for the output of one governed invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GovernedInvocationEventPlan {
    /// This coordinator appended the workflow-synthetic `ToolResult`.
    WorkflowSyntheticResult { success: bool },
    /// `ToolExecutor` owns appending the final `ToolResult`.
    ToolExecutorResult,
}

/// Result of classifying and coordinating one provider tool call.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GovernedInvocationOutcome {
    /// Tool call was fully handled by the governed coordinator.
    Completed(Box<GovernedInvocationResult>),
    /// Tool call is a delegation tool and must stay on the workflow-owned path.
    Delegation {
        /// Stable tool-call id for the delegation path.
        tool_id: ToolCallId,
        /// Delegation invocation copied from the provider tool call.
        invocation: ToolInvocation,
    },
    /// A non-idempotent external effect may have committed and cannot be resent.
    UnknownOutcome {
        /// Stable tool-call id for durable reconciliation.
        tool_id: ToolCallId,
        /// Invocation whose external effect is ambiguous.
        invocation: ToolInvocation,
        /// Stable diagnostic safe for durable failure state.
        message: String,
    },
    /// The execution owner was fenced or stale before the external effect began.
    NotDispatched {
        /// Stable tool-call id for durable owner settlement.
        tool_id: ToolCallId,
        /// Invocation that was definitively not sent to its backend.
        invocation: ToolInvocation,
        /// Closed atomic-admission rejection reason.
        reason: moa_execution::wire::ExecutionToolDispatchRejection,
    },
}

enum GovernedDispatchOutcome {
    Completed(Box<SecuredToolOutput>),
    UnknownOutcome {
        message: String,
    },
    NotDispatched {
        reason: moa_execution::wire::ExecutionToolDispatchRejection,
    },
}

/// Coordinates policy, review, idempotency metadata, dispatch, and synthetic output events.
pub(crate) async fn invoke_governed_tool(
    ctx: &WorkflowContext<'_>,
    request: GovernedInvocationRequest<'_>,
    session_limits: &SessionLimitsConfig,
    session_store: Arc<PostgresSessionStore>,
    channel_adapters: &HashMap<Channel, Arc<dyn ChannelAdapter>>,
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let invocation = request.tool_call.invocation.clone();

    if !request.allowed_tools.contains(&invocation.name) {
        append_tool_call_event(ctx, &request).await?;
        let output = disallowed_tool_output(&invocation.name);
        append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
        return Ok(GovernedInvocationOutcome::Completed(Box::new(
            completed_result(
                request.tool_id,
                invocation,
                output,
                GovernedInvocationDisposition::Disallowed,
            ),
        )));
    }

    if is_delegation_tool_name(&invocation.name) {
        return Ok(GovernedInvocationOutcome::Delegation {
            tool_id: request.tool_id,
            invocation,
        });
    }
    let Some(expected_tool_contract_revision) = request.expected_tool_contract_revision else {
        return Err(TerminalError::new_with_code(
            409,
            format!(
                "tool {} is missing its admitted governed contract revision",
                invocation.name
            ),
        )
        .into());
    };

    append_tool_call_event(ctx, &request).await?;

    if let Some(drift) =
        current_tool_contract_drift(ctx, &request, expected_tool_contract_revision, &invocation)
            .await?
    {
        let output = catalog_drift_output(&invocation, &drift);
        append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
        return Ok(GovernedInvocationOutcome::Completed(Box::new(
            completed_result(
                request.tool_id,
                invocation,
                output,
                GovernedInvocationDisposition::CatalogDrift,
            ),
        )));
    }

    let prepared_action = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ActionPolicyClient>()
            .prepare_action_review(Json(prepare_action_review_request(
                &request,
                &invocation,
                expected_tool_contract_revision,
            ))),
    )
    .call()
    .await?
    .into_inner();

    let prepared_action = match prepared_action {
        PreparedActionReviewResponse::Prepared(prepared) => *prepared,
        PreparedActionReviewResponse::InvalidInput { reason } => {
            let output = invalid_input_output(&invocation, &reason);
            append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
            return Ok(GovernedInvocationOutcome::Completed(Box::new(
                completed_result(
                    request.tool_id,
                    invocation,
                    output,
                    GovernedInvocationDisposition::InvalidInput,
                ),
            )));
        }
    };

    if matches!(prepared_action.effect, ActionPolicyEffect::Deny) {
        let output = denied_action_output(&prepared_action, &invocation);
        append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
        return Ok(GovernedInvocationOutcome::Completed(Box::new(
            completed_result(
                request.tool_id,
                invocation,
                output,
                GovernedInvocationDisposition::Denied,
            ),
        )));
    }

    if matches!(prepared_action.effect, ActionPolicyEffect::AdminReview) {
        if let Some(output) = bounded_review_refusal(&request, &invocation) {
            append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
            return Ok(GovernedInvocationOutcome::Completed(Box::new(
                completed_result(
                    request.tool_id,
                    invocation,
                    output,
                    GovernedInvocationDisposition::Denied,
                ),
            )));
        }
        return request_action_review(
            ctx,
            request,
            invocation,
            prepared_action,
            expected_tool_contract_revision,
        )
        .await;
    }

    execute_allowed_tool(
        ctx,
        request,
        invocation,
        session_limits,
        session_store,
        channel_adapters,
        expected_tool_contract_revision,
    )
    .await
}

/// Returns how the invoked tool's governed contract moved since it was offered.
///
/// The expected revision comes from the same immutable snapshot that supplied
/// the prompt schema, or from a durable execution capability. Checking before
/// action policy prevents a rolling deployment from evaluating one policy while
/// a different contract is eventually dispatched. `ToolExecutor` repeats the
/// check against its own dispatch snapshot because it may run on another replica.
async fn current_tool_contract_drift(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
    expected_revision: &str,
    invocation: &ToolInvocation,
) -> Result<Option<ToolCatalogDrift>, HandlerError> {
    let activated = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ToolExecutorClient>()
            .activated_tool_catalog(Json(ScopedToolCatalogRequest {
                session_id: request.session_id,
                caller_identity: request.identity.clone(),
            })),
    )
    .call()
    .await?
    .into_inner();
    Ok(tool_contract_drift(
        expected_revision,
        &activated,
        &invocation.name,
    ))
}

/// Compares one admitted tool contract with an activated catalog snapshot.
///
/// Scoped to the single invoked tool on purpose: an unrelated connector
/// refreshing mid-turn moves the whole-catalog digest, and refusing this call for
/// that would be a false positive that made every busy deployment unusable.
fn tool_contract_drift(
    expected_revision: &str,
    activated: &ToolCatalogPin,
    tool: &str,
) -> Option<ToolCatalogDrift> {
    match activated.contract_revision(tool) {
        Some(activated_revision) if expected_revision != activated_revision => {
            Some(ToolCatalogDrift::ContractMoved {
                tool: tool.to_string(),
                pinned_revision: expected_revision.to_string(),
                activated_revision: activated_revision.to_string(),
            })
        }
        None => Some(ToolCatalogDrift::Withdrawn {
            tool: tool.to_string(),
        }),
        Some(_) => None,
    }
}

/// Builds the model-visible refusal for a governed tool contract that moved.
///
/// Phrased so the model stops re-sending a call admitted under stale validation,
/// policy, retry, output, ownership, or routing semantics.
fn catalog_drift_output(invocation: &ToolInvocation, drift: &ToolCatalogDrift) -> ToolOutput {
    let detail = match drift {
        ToolCatalogDrift::Withdrawn { .. } => "it is no longer registered".to_string(),
        ToolCatalogDrift::ContractMoved {
            pinned_revision,
            activated_revision,
            ..
        } => format!(
            "its governed contract changed from revision {pinned_revision} to {activated_revision}"
        ),
    };
    denied_tool_output(format!(
        "Tool {} was not called because {detail}. Do not retry with the same arguments; \
         the tool contract you were shown is stale.",
        invocation.name
    ))
}

fn bounded_review_refusal(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
) -> Option<ToolOutput> {
    (!request.resource_budget.is_unbounded()).then(|| {
        denied_tool_output(format!(
            "Tool {} requires admin review, which a resource-bounded turn cannot detach.",
            invocation.name
        ))
    })
}

async fn request_action_review(
    ctx: &WorkflowContext<'_>,
    request: GovernedInvocationRequest<'_>,
    invocation: ToolInvocation,
    prepared_action: PreparedActionReview,
    expected_tool_contract_revision: &str,
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let tool_request = tool_call_request(&request, &invocation, expected_tool_contract_revision);
    if tool_input_leaks_canary(request.active_canary, &tool_request.input)
        .map_err(|error| TerminalError::new(format!("serialize tool input: {error}")))?
    {
        let output = blocked_canary_tool_output(&invocation.name);
        append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
        return Ok(GovernedInvocationOutcome::Completed(Box::new(
            completed_result(
                request.tool_id,
                invocation,
                output,
                GovernedInvocationDisposition::CanaryBlocked,
            ),
        )));
    }

    crate::restate_identity::replay_safe_request(
        ctx.service_client::<ActionReviewsClient>()
            .request(Json::from(RequestActionReview {
                envelope: prepared_action.envelope,
                preview: prepared_action.preview,
                tool_request,
            })),
    )
    .call()
    .await?;
    let output = pending_review_output(&invocation, &prepared_action.input_summary);
    append_synthetic_tool_result(ctx, &request, &invocation, &output).await?;
    Ok(GovernedInvocationOutcome::Completed(Box::new(
        completed_result(
            request.tool_id,
            invocation,
            output,
            GovernedInvocationDisposition::ReviewPending,
        ),
    )))
}

async fn execute_allowed_tool(
    ctx: &WorkflowContext<'_>,
    request: GovernedInvocationRequest<'_>,
    invocation: ToolInvocation,
    session_limits: &SessionLimitsConfig,
    session_store: Arc<PostgresSessionStore>,
    channel_adapters: &HashMap<Channel, Arc<dyn ChannelAdapter>>,
    expected_tool_contract_revision: &str,
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let span = tool_dispatch_span(&invocation.name);
    if !matches!(
        request.origin,
        GovernedInvocationOrigin::ExecutionTask { .. }
            | GovernedInvocationOrigin::ExecutionCompensation { .. }
    ) {
        turn_progress::maybe_emit(
            ctx,
            request.session_id,
            turn_progress::running_tool_summary(&invocation.name),
            session_limits,
            session_store.clone(),
            channel_adapters,
        )
        .await?;
    }
    let dispatch_started = Instant::now();
    let tool_request = tool_call_request(&request, &invocation, expected_tool_contract_revision);
    let dispatch = match request.origin {
        GovernedInvocationOrigin::ExecutionTask {
            run_uid,
            task_uid,
            generation,
        } => span
            .in_scope(|| {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution(Json::from(ExecutionToolCallRequest {
                            call: tool_request,
                            origin: ExecutionToolCallOrigin::Task(ExecutionTaskOrigin {
                                run_uid,
                                task_uid,
                                generation,
                            }),
                        })),
                )
            })
            .call()
            .instrument(span)
            .await?
            .into_inner()
            .into(),
        GovernedInvocationOrigin::ExecutionCompensation {
            run_uid,
            compensation_id,
            generation,
        } => span
            .in_scope(|| {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution(Json::from(ExecutionToolCallRequest {
                            call: tool_request,
                            origin: ExecutionToolCallOrigin::Compensation(
                                ExecutionCompensationOrigin {
                                    run_uid,
                                    compensation_id,
                                    generation,
                                },
                            ),
                        })),
                )
            })
            .call()
            .instrument(span)
            .await?
            .into_inner()
            .into(),
        GovernedInvocationOrigin::RootTurn { .. } | GovernedInvocationOrigin::Worker { .. } => span
            .in_scope(|| {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute(Json::from(tool_request)),
                )
            })
            .call()
            .instrument(span)
            .await?
            .into_inner()
            .into(),
    };
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);
    let output = match dispatch {
        GovernedDispatchOutcome::Completed(output) => *output,
        GovernedDispatchOutcome::UnknownOutcome { message } => {
            return Ok(GovernedInvocationOutcome::UnknownOutcome {
                tool_id: request.tool_id,
                invocation,
                message,
            });
        }
        GovernedDispatchOutcome::NotDispatched { reason } => {
            return Ok(GovernedInvocationOutcome::NotDispatched {
                tool_id: request.tool_id,
                invocation,
                reason,
            });
        }
    };

    Ok(GovernedInvocationOutcome::Completed(Box::new(
        GovernedInvocationResult {
            tool_id: request.tool_id,
            invocation,
            output,
            disposition: GovernedInvocationDisposition::Executed,
            event_plan: GovernedInvocationEventPlan::ToolExecutorResult,
        },
    )))
}

impl From<ExecutionToolCallOutcome> for GovernedDispatchOutcome {
    fn from(outcome: ExecutionToolCallOutcome) -> Self {
        match outcome {
            ExecutionToolCallOutcome::Completed { output } => Self::Completed(output),
            ExecutionToolCallOutcome::UnknownOutcome { message } => {
                Self::UnknownOutcome { message }
            }
            ExecutionToolCallOutcome::NotDispatched { reason } => Self::NotDispatched { reason },
        }
    }
}

impl From<SecuredToolOutput> for GovernedDispatchOutcome {
    fn from(output: SecuredToolOutput) -> Self {
        Self::Completed(Box::new(output))
    }
}

/// Records a successful segment tool use through the session-store service.
pub(crate) async fn record_segment_tool_use(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id,
                tool_name: tool_name.to_string(),
            })),
    )
    .send();
    Ok(())
}

fn completed_result(
    tool_id: ToolCallId,
    invocation: ToolInvocation,
    output: ToolOutput,
    disposition: GovernedInvocationDisposition,
) -> GovernedInvocationResult {
    let output = secured_synthetic_output(&invocation, output);
    GovernedInvocationResult {
        tool_id,
        invocation,
        output,
        disposition,
        event_plan: GovernedInvocationEventPlan::WorkflowSyntheticResult { success: false },
    }
}

/// Classifies one workflow-authored refusal or notice.
///
/// These bytes are MOA's, not a capability's — the tool never ran — so they are
/// keyed under the built-in namespace rather than the tool's real routing
/// identity. Classifying them is not paranoia about MOA's own strings: several
/// embed a model-authored input summary, which an earlier injection can shape.
fn secured_synthetic_output(invocation: &ToolInvocation, raw: ToolOutput) -> SecuredToolOutput {
    classify_tool_output(
        &raw,
        OutputClassification {
            capability: &ToolCapabilityId::builtin(&invocation.name),
            active_canary: None,
        },
    )
}

fn prepare_action_review_request(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
    expected_tool_contract_revision: &str,
) -> PrepareActionReviewRequest {
    let default_provenance = match request.origin {
        GovernedInvocationOrigin::RootTurn { .. }
        | GovernedInvocationOrigin::ExecutionTask { .. }
        | GovernedInvocationOrigin::ExecutionCompensation { .. } => CapabilityProvenance::default(),
        GovernedInvocationOrigin::Worker {
            worker_id, turn_id, ..
        } => CapabilityProvenance {
            kind: Some("worker".to_string()),
            id: Some(worker_id.to_string()),
            step_id: Some(turn_id.to_string()),
        },
    };

    PrepareActionReviewRequest {
        session: request.session.clone(),
        caller_identity: request.identity.clone(),
        invocation: invocation.clone(),
        review_id: request.tool_id.0,
        tool_call_id: request.tool_id,
        owner: request.origin.action_review_owner(request.session_id),
        capability_provenance: request
            .capability_provenance
            .cloned()
            .unwrap_or(default_provenance),
        capability_policy_context: request.capability_policy_context.cloned(),
        idempotency_key: invocation.id.clone(),
        expected_tool_contract_revision: expected_tool_contract_revision.to_owned(),
    }
}

fn tool_call_request(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
    expected_tool_contract_revision: &str,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id: request.tool_id,
        caller_identity: request.identity.clone(),
        provider_tool_use_id: invocation.id.clone(),
        tool_name: invocation.name.clone(),
        expected_tool_contract_revision: expected_tool_contract_revision.to_owned(),
        input: invocation.input.clone(),
        active_canary: request.active_canary.map(ToOwned::to_owned),
        session_id: request.session_id,
        trusted_sandbox_manifest: request.trusted_sandbox_manifest.cloned(),
        worker_id: match request.origin {
            GovernedInvocationOrigin::RootTurn { .. }
            | GovernedInvocationOrigin::ExecutionTask { .. }
            | GovernedInvocationOrigin::ExecutionCompensation { .. } => None,
            GovernedInvocationOrigin::Worker { worker_id, .. } => Some(worker_id.to_string()),
        },
        resource_budget: request.resource_budget,
    }
}

/// Builds the synthetic error result for model-authored input that failed the
/// tool's schema validation, phrased so the model corrects and retries.
fn invalid_input_output(invocation: &ToolInvocation, reason: &str) -> ToolOutput {
    denied_tool_output(format!(
        "Tool {} rejected invalid input: {reason}. Correct the arguments and call the tool again.",
        invocation.name
    ))
}

fn denied_action_output(
    prepared_action: &PreparedActionReview,
    invocation: &ToolInvocation,
) -> ToolOutput {
    let reason = prepared_action
        .reason
        .as_deref()
        .unwrap_or("denied by action policy");
    denied_tool_output(format!(
        "Tool {} denied by action policy: {reason}",
        invocation.name
    ))
}

fn pending_review_output(invocation: &ToolInvocation, input_summary: &str) -> ToolOutput {
    ToolOutput::error(
        format!(
            "Action is pending tenant admin review: {}: {}",
            invocation.name, input_summary
        ),
        Duration::ZERO,
    )
}

async fn append_tool_call_event(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
) -> Result<(), HandlerError> {
    if !owns_root_session_tool_events(request.origin) {
        return Ok(());
    }
    let invocation = request.tool_call.invocation.clone();
    append_session_event(
        ctx,
        request.session_id,
        Event::ToolCall {
            tool_id: request.tool_id,
            provider_tool_use_id: invocation.id,
            provider_thought_signature: request
                .tool_call
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.thought_signature())
                .map(str::to_string),
            tool_name: invocation.name,
            input: invocation.input,
            hand_id: None,
        },
    )
    .await
    .map(|_| ())
}

async fn append_synthetic_tool_result(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    if !owns_root_session_tool_events(request.origin) {
        return Ok(());
    }
    let mut secured = secured_synthetic_output(invocation, output.clone());
    // A synthetic result is a refusal: it is durably a failure regardless of
    // whether the notice text happens to render as an error output.
    secured.safe_output.is_error = true;
    secured.safe_output.duration = std::time::Duration::ZERO;
    append_session_event(
        ctx,
        request.session_id,
        Event::tool_result(request.tool_id, invocation.id.clone(), secured),
    )
    .await
    .map(|_| ())
}

/// Appends the tool-call and a successful tool-result event for a tool served from
/// the per-turn cache without re-dispatching it.
///
/// Mirrors the synthetic-result path used for disallowed/denied calls but records
/// `success: true`, because the output is a corrective notice pointing at a valid
/// earlier result rather than a policy rejection. The cached output carries only the
/// notice (the file body is not repeated, since it is already in context from the
/// first read), and the dispatch itself is skipped, so no `ToolExecutor` round-trip or
/// sandbox work occurs.
pub(crate) async fn append_cached_tool_result(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_tool_call_event(ctx, request).await?;
    append_session_event(
        ctx,
        request.session_id,
        Event::ToolResult {
            tool_id: request.tool_id,
            provider_tool_use_id: request.tool_call.invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: true,
            duration_ms: 0,
            assessment: moa_core::types::security::ToolOutputAssessment::safe(),
            capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
        },
    )
    .await
    .map(|_| ())
}

fn owns_root_session_tool_events(origin: GovernedInvocationOrigin<'_>) -> bool {
    !matches!(
        origin,
        GovernedInvocationOrigin::ExecutionTask { .. }
            | GovernedInvocationOrigin::ExecutionCompensation { .. }
    )
}

async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    let sequence_num = persist_span
        .in_scope(|| {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<RestateSessionStoreClient>()
                    .append_event(Json(AppendEventRequest {
                        session_id,
                        event,
                        dedupe_key: None,
                    })),
            )
        })
        .call()
        .instrument(persist_span)
        .await?
        .into_inner()
        .sequence_num;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(sequence_num)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::LazyLock;

    use moa_artifacts::reference::ArtifactRef;
    use moa_core::{
        traits::{Identity, IdentityType},
        types::action_policy::ActionPolicyEffect,
        types::action_policy::CapabilityProvenance,
        types::completion::CompletionRequest,
        types::completion::ToolCallContent,
        types::completion::ToolInvocation,
        types::contact::ContactId,
        types::contact::ContactRef,
        types::contact::ContactVerificationState,
        types::contact::SessionActorRef,
        types::identifiers::ConnectorConnectionId,
        types::identifiers::SessionId,
        types::identifiers::TenantId,
        types::identifiers::ToolCallId,
        types::identifiers::UserId,
        types::session::SessionMeta,
        types::tools::TrustedSandboxFileEntry,
        types::tools::TrustedSandboxFileManifestRef,
    };
    use moa_execution::{CapabilityPolicyContext, CapabilitySource};
    use moa_hands::{PinnedToolContract, PinnedToolOwner};
    use moa_test_support::fixtures::contact_ref_fixture;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        ActionReviewOwner, GovernedDispatchOutcome, GovernedInvocationDisposition,
        GovernedInvocationEventPlan, GovernedInvocationOrigin, GovernedInvocationRequest,
        TOOL_CATALOG_PIN_METADATA_KEY, ToolCatalogDrift, ToolCatalogPin, bounded_review_refusal,
        catalog_drift_output, completed_result, completion_tool_catalog_pin,
        owns_root_session_tool_events, pending_review_output, prepare_action_review_request,
        tool_call_request, tool_contract_drift,
    };
    use crate::delegation::storage_user_id;
    use crate::services::tool_executor::ExecutionToolCallOutcome;

    static TEST_IDENTITY: LazyLock<Identity> = LazyLock::new(|| Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(20),
        tenant_id: TenantId::from(Uuid::from_u128(10)),
        api_key_id: Some(Uuid::from_u128(21)),
        acting_on_behalf_of: Some(Uuid::from_u128(22)),
    });

    fn test_session_meta() -> SessionMeta {
        SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(10)),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(20),
            }),
            ..SessionMeta::default()
        }
    }

    fn tool_call() -> ToolCallContent {
        ToolCallContent {
            invocation: ToolInvocation {
                id: Some("provider-tool-1".to_string()),
                name: "file_read".to_string(),
                input: json!({"path": "README.md"}),
            },
            provider_metadata: None,
        }
    }

    fn request<'a>(
        session: &'a SessionMeta,
        tool_call: &'a ToolCallContent,
        allowed_tools: &'a BTreeSet<String>,
        origin: GovernedInvocationOrigin<'a>,
    ) -> GovernedInvocationRequest<'a> {
        GovernedInvocationRequest {
            session,
            identity: &TEST_IDENTITY,
            session_id: session.id,
            tool_id: ToolCallId(Uuid::from_u128(30)),
            tool_call,
            allowed_tools,
            expected_tool_contract_revision: Some("contract-v1"),
            active_canary: Some("canary"),
            trusted_sandbox_manifest: None,
            origin,
            capability_provenance: None,
            capability_policy_context: None,
            resource_budget: Default::default(),
        }
    }

    #[test]
    fn action_policy_root_request_has_no_origin_and_uses_provider_idempotency_key() {
        // Pins: root turns omit execution provenance and use provider idempotency.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-1",
                generation: 3,
            },
        );

        let policy_request =
            prepare_action_review_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(policy_request.session, session);
        assert_eq!(policy_request.caller_identity, *TEST_IDENTITY);
        assert_eq!(policy_request.invocation, tool_call.invocation);
        assert_eq!(policy_request.review_id, request.tool_id.0);
        assert_eq!(policy_request.tool_call_id, request.tool_id);
        assert_eq!(
            policy_request.owner,
            ActionReviewOwner::Coordinator {
                session_id: session.id,
                turn_id: "turn-governed-1".to_string(),
                generation: 3,
            }
        );
        assert_eq!(policy_request.capability_provenance, Default::default());
        assert_eq!(
            policy_request.expected_tool_contract_revision,
            "contract-v1"
        );
        assert_eq!(
            policy_request.idempotency_key.as_deref(),
            Some("provider-tool-1")
        );
    }

    #[test]
    fn governed_tool_request_preserves_the_turn_resource_budget() {
        // Pins: the Session-owned resource slice reaches ToolExecutor on the wire;
        // dropping it here would make the budget-aware Hands path unbounded again.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let mut request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-budget",
                generation: 1,
            },
        );
        request.resource_budget = moa_core::types::resource::ResourceBudget::new(
            None,
            Some(moa_core::types::resource::ResourceAmounts {
                tool_calls: 1,
                ..moa_core::types::resource::ResourceAmounts::ZERO
            }),
        );

        let durable = tool_call_request(&request, &tool_call.invocation, "contract-v1");
        assert_eq!(durable.resource_budget, request.resource_budget);
    }

    #[test]
    fn capability_policy_context_survives_governed_review_request_serialization() {
        // Pins: durable dispatch copies the exact installed-connector source,
        // connection/binding/generation pins, canonical action reference, and
        // policy floor into the serialized policy request without name parsing.
        let session = test_session_meta();
        let tool_name = "conn__00000000000000000000000000000055__create_ticket";
        let tool_call = ToolCallContent {
            invocation: ToolInvocation {
                id: Some("provider-connector-1".to_string()),
                name: tool_name.to_string(),
                input: json!({"subject": "incident"}),
            },
            provider_metadata: None,
        };
        let allowed_tools = BTreeSet::from([tool_name.to_string()]);
        let action_ref = ArtifactRef::action("support", "create_ticket");
        let definition_artifact_uid = Uuid::from_u128(81);
        let definition_revision_uid = Uuid::from_u128(82);
        let source = CapabilitySource::InstalledConnectorAction {
            connector_ref: ArtifactRef::connector("support"),
            connection_id: ConnectorConnectionId(Uuid::from_u128(85)),
            binding_id: Uuid::from_u128(86),
            connection_generation: 7,
            definition_artifact_uid,
            definition_revision_uid,
            action_id: "create_ticket".to_string(),
            contract_hash: "cd".repeat(32),
            governed_contract_revision: "support-ticket-v7".to_string(),
            minimum_effect: ActionPolicyEffect::AdminReview,
            tool_name: tool_name.to_string(),
        };
        let context = CapabilityPolicyContext::artifact(
            source,
            Some(action_ref),
            definition_artifact_uid,
            definition_revision_uid,
            ActionPolicyEffect::AdminReview,
        );
        let mut request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::ExecutionTask {
                run_uid: Uuid::from_u128(83),
                task_uid: Uuid::from_u128(84),
                generation: 2,
            },
        );
        request.capability_policy_context = Some(&context);

        let prepared =
            prepare_action_review_request(&request, &tool_call.invocation, "contract-v1");
        assert_eq!(prepared.capability_policy_context.as_ref(), Some(&context));
        let encoded = serde_json::to_vec(&prepared).expect("serialize policy preparation request");
        let decoded: crate::services::action_policy::PrepareActionReviewRequest =
            serde_json::from_slice(&encoded).expect("deserialize policy preparation request");
        assert_eq!(decoded, prepared);
    }

    #[test]
    fn bounded_turn_refuses_to_detach_an_admin_review() {
        // Pins: a reviewed action cannot outlive the target reservation that
        // admitted it and execute later as unaccounted experiment work.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let mut request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-review-budget",
                generation: 1,
            },
        );
        request.resource_budget = moa_core::types::resource::ResourceBudget::new(
            None,
            Some(moa_core::types::resource::ResourceAmounts {
                tool_calls: 1,
                ..moa_core::types::resource::ResourceAmounts::ZERO
            }),
        );

        let output = bounded_review_refusal(&request, &tool_call.invocation)
            .expect("bounded review must be refused before enqueue");
        assert!(output.is_error);
        assert!(output.to_text().contains("cannot detach"));
    }

    #[test]
    fn governed_origin_maps_to_exactly_one_typed_action_review_owner() {
        // Pins: who is resumed after a review is decided at the moment the tool call is
        // issued, by the runtime that issued it, with the fence it was admitted under.
        // Nothing downstream may infer ownership from optional envelope fields.
        let session_id = SessionId::new();

        let root = GovernedInvocationOrigin::RootTurn {
            turn_id: "turn-owner-1",
            generation: 4,
        }
        .action_review_owner(session_id);
        assert_eq!(
            root,
            ActionReviewOwner::Coordinator {
                session_id,
                turn_id: "turn-owner-1".to_string(),
                generation: 4,
            }
        );

        let worker = GovernedInvocationOrigin::Worker {
            worker_id: "worker-owner-1",
            turn_id: "worker-owner-1-turn-2",
            generation: 6,
        }
        .action_review_owner(session_id);
        assert_eq!(
            worker,
            ActionReviewOwner::Worker {
                session_id,
                worker_id: "worker-owner-1".to_string(),
                turn_id: "worker-owner-1-turn-2".to_string(),
                generation: 6,
            }
        );

        let execution = GovernedInvocationOrigin::ExecutionTask {
            run_uid: Uuid::from_u128(50),
            task_uid: Uuid::from_u128(51),
            generation: 2,
        }
        .action_review_owner(session_id);
        assert_eq!(
            execution,
            ActionReviewOwner::ExecutionTask {
                session_id,
                origin: moa_core::types::action_policy::ExecutionTaskOrigin {
                    run_uid: Uuid::from_u128(50),
                    task_uid: Uuid::from_u128(51),
                    generation: 2,
                },
            }
        );
        let compensation = GovernedInvocationOrigin::ExecutionCompensation {
            run_uid: Uuid::from_u128(50),
            compensation_id: Uuid::from_u128(52),
            generation: 7,
        }
        .action_review_owner(session_id);
        assert_eq!(
            compensation,
            ActionReviewOwner::ExecutionCompensation {
                session_id,
                origin: moa_core::types::action_policy::ExecutionCompensationOrigin {
                    run_uid: Uuid::from_u128(50),
                    compensation_id: Uuid::from_u128(52),
                    generation: 7,
                },
            }
        );
        assert_eq!(compensation.execution_origin(), None);
        assert!(root.is_conversational());
        assert!(worker.is_conversational());
        assert!(
            !execution.is_conversational(),
            "an execution task must never route a conversational callback"
        );
        assert!(
            !compensation.is_conversational(),
            "an execution compensation must never route a conversational callback"
        );
    }

    #[test]
    fn action_policy_worker_request_sets_origin_fields() {
        // Pins: worker review records remain traceable to the child turn, on both the
        // capability-provenance axis and the typed owner axis.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "child-turn-1",
                generation: 5,
            },
        );

        let policy_request =
            prepare_action_review_request(&request, &tool_call.invocation, "contract-v1");

        assert!(
            matches!(
                &policy_request.owner,
                moa_core::types::action_policy::ActionReviewOwner::Worker { worker_id, .. }
                    if worker_id.as_str() == "worker-1"
            ),
            "worker origin must map to a Worker review owner: {:?}",
            policy_request.owner
        );
        assert_eq!(
            policy_request.capability_provenance.kind.as_deref(),
            Some("worker")
        );
        assert_eq!(
            policy_request.capability_provenance.id.as_deref(),
            Some("worker-1")
        );
        assert_eq!(
            policy_request.capability_provenance.step_id.as_deref(),
            Some("child-turn-1")
        );
        assert_eq!(policy_request.owner.execution_origin(), None);
        assert_eq!(
            policy_request.owner.worker_id().map(String::as_str),
            Some("worker-1")
        );
    }

    #[test]
    fn action_policy_execution_task_keeps_capability_and_execution_provenance_separate() {
        // Pins: review envelopes preserve both provenance axes without overloading worker fields.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let capability = CapabilityProvenance {
            kind: Some("skill_action".to_string()),
            id: Some("skill://research#fetch".to_string()),
            step_id: Some("fetch".to_string()),
        };
        let mut request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::ExecutionTask {
                run_uid: Uuid::from_u128(40),
                task_uid: Uuid::from_u128(41),
                generation: 2,
            },
        );
        request.capability_provenance = Some(&capability);

        let policy_request =
            prepare_action_review_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(policy_request.session, session);
        assert_eq!(policy_request.invocation, tool_call.invocation);
        assert_eq!(policy_request.owner.worker_id(), None);
        assert_eq!(policy_request.capability_provenance, capability);
        assert_eq!(
            policy_request.owner.execution_origin(),
            Some(moa_core::types::action_policy::ExecutionTaskOrigin {
                run_uid: Uuid::from_u128(40),
                task_uid: Uuid::from_u128(41),
                generation: 2,
            })
        );
    }

    #[test]
    fn execution_unknown_outcome_stays_distinct_from_completed_output() {
        // Pins: the governed execution boundary routes explicit ambiguity as
        // control data rather than synthesizing a normal classified tool output.
        let dispatch = GovernedDispatchOutcome::from(ExecutionToolCallOutcome::UnknownOutcome {
            message: "manual reconciliation required".to_string(),
        });

        assert!(matches!(
            dispatch,
            GovernedDispatchOutcome::UnknownOutcome { .. }
        ));
    }

    #[test]
    fn execution_not_dispatched_stays_distinct_from_unknown_and_completed() {
        // Pins: a stale/fenced origin is definitive non-execution and remains
        // owner-visible control data across the governed invocation boundary.
        let reason = moa_execution::wire::ExecutionToolDispatchRejection::StaleGeneration;
        let dispatch =
            GovernedDispatchOutcome::from(ExecutionToolCallOutcome::NotDispatched { reason });

        assert!(matches!(
            dispatch,
            GovernedDispatchOutcome::NotDispatched {
                reason: moa_execution::wire::ExecutionToolDispatchRejection::StaleGeneration
            }
        ));
    }

    #[test]
    fn action_policy_origin_variants_share_one_preparation_shape() {
        // Pins: root, worker, and execution-task calls evaluate the same session/invocation contract.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let root = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-1",
                generation: 3,
            },
        );
        let worker = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "turn-1",
                generation: 5,
            },
        );
        let execution = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::ExecutionTask {
                run_uid: Uuid::from_u128(40),
                task_uid: Uuid::from_u128(41),
                generation: 2,
            },
        );

        let prepared = [root, worker, execution]
            .iter()
            .map(|request| {
                prepare_action_review_request(request, &tool_call.invocation, "contract-v1")
            })
            .collect::<Vec<_>>();

        assert!(prepared.iter().all(|request| request.session == session));
        assert!(
            prepared
                .iter()
                .all(|request| request.invocation == tool_call.invocation)
        );
        assert!(
            prepared
                .iter()
                .all(|request| request.idempotency_key.as_deref() == Some("provider-tool-1"))
        );
        assert_eq!(prepared[0].owner.execution_origin(), None);
        assert_eq!(prepared[1].owner.execution_origin(), None);
        assert!(prepared[2].owner.execution_origin().is_some());
    }

    #[test]
    fn execution_task_origin_never_owns_root_session_tool_events() {
        // Pins: all governed root event appenders share one execution-task exclusion guard.
        assert!(owns_root_session_tool_events(
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-1",
                generation: 1,
            }
        ));
        assert!(owns_root_session_tool_events(
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "turn-1",
                generation: 1,
            }
        ));
        assert!(!owns_root_session_tool_events(
            GovernedInvocationOrigin::ExecutionTask {
                run_uid: Uuid::from_u128(40),
                task_uid: Uuid::from_u128(41),
                generation: 2,
            }
        ));
    }

    #[test]
    fn tool_request_preserves_session_identity_and_idempotency() {
        // Pins: deferred review and direct execution share one durable request shape.
        let mut session = test_session_meta();
        let contact_id = ContactId::new();
        session.contact = Some(contact_ref(session.tenant_id, contact_id));
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-1",
                generation: 3,
            },
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(tool_request.tool_call_id, request.tool_id);
        assert_eq!(
            tool_request.provider_tool_use_id.as_deref(),
            Some("provider-tool-1")
        );
        assert_eq!(tool_request.tool_name, "file_read");
        assert_eq!(tool_request.input, json!({"path": "README.md"}));
        assert_eq!(tool_request.active_canary.as_deref(), Some("canary"));
        assert_eq!(tool_request.session_id, session.id);
        assert_eq!(tool_request.caller_identity, *TEST_IDENTITY);
        assert_eq!(tool_request.trusted_sandbox_manifest, None);
        assert_eq!(tool_request.worker_id, None);
        assert_eq!(tool_request.expected_tool_contract_revision, "contract-v1");
    }

    #[test]
    fn worker_origin_request_carries_worker_hand_scope() {
        // Pins: a worker tool call provisions a hand scoped to its worker_id.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "child-turn-1",
                generation: 5,
            },
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(tool_request.worker_id.as_deref(), Some("worker-1"));
        assert_eq!(tool_request.expected_tool_contract_revision, "contract-v1");
    }

    #[test]
    fn root_origin_request_keeps_session_level_hand_scope() {
        // Pins: the root coordinator stays on the session-level hand scope (no isolation).
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-1",
                generation: 3,
            },
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(tool_request.worker_id, None);
    }

    #[test]
    fn tool_request_carries_selected_trusted_sandbox_manifest() {
        // Pins: file install intent survives Restate handoff without journaling file bytes per tool.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let manifest = TrustedSandboxFileManifestRef {
            blob_id: "blob-1".to_string(),
            size: 128,
            manifest_sha256: "manifest-hash".to_string(),
            files: vec![TrustedSandboxFileEntry {
                path: ".moa/skills/test/SKILL.md".to_string(),
                content_sha256: "content-hash".to_string(),
                size: 14,
                executable: false,
            }],
        };
        let mut request = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::RootTurn {
                turn_id: "turn-governed-1",
                generation: 3,
            },
        );
        request.trusted_sandbox_manifest = Some(&manifest);

        let tool_request = tool_call_request(&request, &tool_call.invocation, "contract-v1");

        assert_eq!(tool_request.trusted_sandbox_manifest, Some(manifest));
    }

    #[test]
    fn workflow_synthetic_results_do_not_record_segment_success() {
        // Pins: denied/review/disallowed synthetic outputs do not count as tool use.
        let result = completed_result(
            ToolCallId(Uuid::from_u128(1)),
            tool_call().invocation,
            pending_review_output(
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({}),
                },
                "file read",
            ),
            GovernedInvocationDisposition::ReviewPending,
        );

        assert_eq!(
            result.event_plan,
            GovernedInvocationEventPlan::WorkflowSyntheticResult { success: false }
        );
        assert!(result.should_record_denied_worker_tool());
        assert!(!result.should_record_segment_tool_use());
    }

    #[test]
    fn executed_results_delegate_final_event_to_tool_executor() {
        // Pins: normal execution keeps ToolExecutor as the canonical result writer.
        let result = super::GovernedInvocationResult {
            tool_id: ToolCallId(Uuid::from_u128(1)),
            invocation: tool_call().invocation,
            output: moa_core::types::tools::SecuredToolOutput::assessed_safe(
                moa_core::types::tools::ToolOutput::text("ok", std::time::Duration::from_millis(5)),
                moa_core::types::security::ToolCapabilityId::builtin("noop"),
            ),
            disposition: GovernedInvocationDisposition::Executed,
            event_plan: GovernedInvocationEventPlan::ToolExecutorResult,
        };

        assert!(!result.should_record_denied_worker_tool());
        assert!(result.should_record_segment_tool_use());
    }

    #[test]
    fn storage_user_id_prefers_contact_then_actor_then_tenant() {
        // Pins: tool requests keep the same storage actor fallback as both workflows used.
        let tenant_id = TenantId::from(Uuid::from_u128(99));
        let mut session = SessionMeta {
            tenant_id,
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(42),
            }),
            ..SessionMeta::default()
        };
        assert_eq!(
            storage_user_id(&session),
            UserId::new(format!("identity:{}", Uuid::from_u128(42)))
        );

        let contact_id = ContactId::new();
        session.contact = Some(contact_ref(tenant_id, contact_id));
        assert_eq!(
            storage_user_id(&session),
            UserId::new(contact_id.to_string())
        );

        session.contact = None;
        session.created_by = None;
        assert_eq!(
            storage_user_id(&session),
            UserId::new(format!("tenant:{tenant_id}"))
        );
    }

    fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
        let mut contact =
            contact_ref_fixture(contact_id, tenant_id, ContactVerificationState::Verified);
        contact.permissions = json!({});
        contact
    }

    #[test]
    fn completion_request_carries_the_exact_catalog_pin() {
        // Pins: the model-visible tools and their governed revisions cross the
        // provider boundary as one request instead of separate mutable reads.
        let expected = pin(&[("file_read", "contract-v1")]);
        let mut request = CompletionRequest::new("read it");
        request.metadata.insert(
            TOOL_CATALOG_PIN_METADATA_KEY.to_string(),
            serde_json::to_value(&expected).expect("catalog pin should serialize"),
        );

        assert_eq!(
            completion_tool_catalog_pin(&request).expect("catalog pin should decode"),
            expected
        );

        request.metadata.remove(TOOL_CATALOG_PIN_METADATA_KEY);
        let error = completion_tool_catalog_pin(&request)
            .expect_err("a request without its catalog pin must fail closed");
        let error = <restate_sdk::prelude::HandlerError as AsRef<
            dyn std::error::Error + Send + Sync,
        >>::as_ref(&error)
        .to_string();
        assert!(error.contains("missing"), "error: {error}");
    }

    fn pin(tools: &[(&str, &str)]) -> ToolCatalogPin {
        ToolCatalogPin {
            contract_hash: format!("hash-of-{}", tools.len()),
            mcp_catalog_revision: "mcp-revision".to_string(),
            tools: tools
                .iter()
                .map(|(tool, revision)| PinnedToolContract {
                    tool: (*tool).to_string(),
                    owner: PinnedToolOwner::Connector {
                        server: "crm".to_string(),
                    },
                    contract_revision: (*revision).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_turn_only_refuses_a_tool_call_whose_own_contract_moved() {
        // Pins: conversational turns now carry a contract pin, and it is scoped to
        // the invoked tool. A tool whose revision is unchanged must dispatch even
        // though the surrounding catalog moved — otherwise one unrelated connector
        // refreshing would refuse every tool call on a busy deployment — while the
        // invoked tool's own contract moving or being withdrawn must be refused.
        let reference = moa_hands::mcp_tool_reference("crm", "lookup");
        let other = moa_hands::mcp_tool_reference("crm", "search");

        let pinned = pin(&[
            (reference.as_str(), "schema-a"),
            (other.as_str(), "schema-x"),
        ]);
        let unrelated_change = pin(&[
            (reference.as_str(), "schema-a"),
            (other.as_str(), "schema-y"),
        ]);
        assert_eq!(
            tool_contract_drift(
                pinned
                    .contract_revision(&reference)
                    .expect("reference is pinned"),
                &unrelated_change,
                &reference,
            ),
            None,
            "an unrelated connector contract change must not refuse this tool"
        );

        let moved = pin(&[
            (reference.as_str(), "schema-b"),
            (other.as_str(), "schema-x"),
        ]);
        assert_eq!(
            tool_contract_drift(
                pinned
                    .contract_revision(&reference)
                    .expect("reference is pinned"),
                &moved,
                &reference,
            ),
            Some(ToolCatalogDrift::ContractMoved {
                tool: reference.clone(),
                pinned_revision: "schema-a".to_string(),
                activated_revision: "schema-b".to_string(),
            })
        );

        let withdrawn = pin(&[(other.as_str(), "schema-x")]);
        assert_eq!(
            tool_contract_drift(
                pinned
                    .contract_revision(&reference)
                    .expect("reference is pinned"),
                &withdrawn,
                &reference,
            ),
            Some(ToolCatalogDrift::Withdrawn {
                tool: reference.clone(),
            })
        );
        assert_eq!(
            tool_contract_drift("contract-not-offered", &pinned, &reference),
            Some(ToolCatalogDrift::ContractMoved {
                tool: reference.clone(),
                pinned_revision: "contract-not-offered".to_string(),
                activated_revision: "schema-a".to_string(),
            })
        );
    }

    #[test]
    fn a_catalog_drift_refusal_tells_the_model_its_contract_is_stale() {
        // Pins: the refusal is actionable rather than a bare error. A model that
        // retried the same arguments against a moved schema would loop, so the
        // message names the revisions and forbids the retry, and the disposition
        // is recorded as a denied tool for worker accounting.
        let invocation = ToolInvocation {
            id: Some("provider-tool-9".to_string()),
            name: moa_hands::mcp_tool_reference("crm", "lookup"),
            input: json!({}),
        };
        let output = catalog_drift_output(
            &invocation,
            &ToolCatalogDrift::ContractMoved {
                tool: invocation.name.clone(),
                pinned_revision: "schema-a".to_string(),
                activated_revision: "schema-b".to_string(),
            },
        );

        assert!(output.is_error);
        let text = output.to_text();
        assert!(
            text.contains("schema-a") && text.contains("schema-b"),
            "{text}"
        );
        assert!(
            text.contains("Do not retry with the same arguments"),
            "{text}"
        );

        let result = completed_result(
            ToolCallId::new(),
            invocation,
            output,
            GovernedInvocationDisposition::CatalogDrift,
        );
        assert!(result.should_record_denied_worker_tool());
        assert!(!result.should_record_segment_tool_use());
    }
}
