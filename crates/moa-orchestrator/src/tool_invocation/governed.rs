//! Governed tool invocation coordination for turn workflows.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::wire::session_store::{AppendEventRequest, RecordSegmentToolUseRequest};
use moa_core::{config::SessionLimitsConfig, traits::ChannelAdapter};
use moa_core::{
    events::Event, types::action_policy::ActionPolicyEffect,
    types::action_policy::CapabilityProvenance, types::action_policy::ExecutionTaskOrigin,
    types::channel::Channel, types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::identifiers::SessionId, types::identifiers::ToolCallId, types::session::SessionMeta,
    types::tools::ToolCallRequest, types::tools::ToolOutput,
    types::tools::TrustedSandboxFileManifestRef, types::worker::state::WorkerId,
    types::worker::tool_schema::is_delegation_tool_name,
};
use moa_observability::restate_observability::{event_persist_span, tool_dispatch_span};
use moa_observability::{record_turn_event_persist_duration, record_turn_tool_dispatch_duration};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::services::{
    action_policy::{
        ActionPolicyClient, PrepareActionReviewRequest, PreparedActionReview,
        PreparedActionReviewResponse,
    },
    action_reviews::{ActionReviewsClient, RequestActionReview},
    session_store::RestateSessionStoreClient,
    tool_executor::{ExecutionTaskToolCallRequest, ToolExecutorClient},
};
use crate::turn::util::{
    blocked_canary_tool_output, denied_tool_output, disallowed_tool_output, tool_input_leaks_canary,
};
use crate::workflows::turn_progress;

/// Workflow origin metadata for a governed tool invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GovernedInvocationOrigin<'a> {
    /// Tool call came from the root session turn.
    RootTurn,
    /// Tool call came from a worker turn.
    Worker {
        /// Worker object id.
        worker_id: &'a str,
        /// Worker turn id that produced the tool call.
        turn_id: &'a str,
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
    /// Active prompt-injection canary marker, when present.
    pub(crate) active_canary: Option<&'a str>,
    /// Trusted sandbox file manifest selected by the runtime that built this tool call.
    pub(crate) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    /// Root or worker origin metadata.
    pub(crate) origin: GovernedInvocationOrigin<'a>,
    /// Capability-level provenance, independent of execution-task ownership.
    pub(crate) capability_provenance: Option<&'a CapabilityProvenance>,
}

/// Completed governed tool invocation result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GovernedInvocationResult {
    /// Stable tool-call id used for session and worker records.
    pub(crate) tool_id: ToolCallId,
    /// Tool invocation copied from the provider tool call.
    pub(crate) invocation: ToolInvocation,
    /// Model-visible tool output.
    pub(crate) output: ToolOutput,
    /// Outcome classification for workflow-local recording.
    pub(crate) disposition: GovernedInvocationDisposition,
    /// Event ownership plan used for the output event.
    pub(crate) event_plan: GovernedInvocationEventPlan,
}

impl GovernedInvocationResult {
    /// Returns whether the caller should record a successful segment tool use.
    pub(crate) fn should_record_segment_tool_use(&self) -> bool {
        self.disposition == GovernedInvocationDisposition::Executed && !self.output.is_error
    }

    /// Returns whether a worker should record the result as denied.
    pub(crate) fn should_record_denied_worker_tool(&self) -> bool {
        matches!(
            self.disposition,
            GovernedInvocationDisposition::Disallowed
                | GovernedInvocationDisposition::Denied
                | GovernedInvocationDisposition::CanaryBlocked
                | GovernedInvocationDisposition::ReviewPending
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

    append_tool_call_event(ctx, &request).await?;

    let prepared_action = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ActionPolicyClient>()
            .prepare_action_review(Json(prepare_action_review_request(&request, &invocation))),
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
        return request_action_review(ctx, request, invocation, prepared_action).await;
    }

    execute_allowed_tool(
        ctx,
        request,
        invocation,
        session_limits,
        session_store,
        channel_adapters,
    )
    .await
}

async fn request_action_review(
    ctx: &WorkflowContext<'_>,
    request: GovernedInvocationRequest<'_>,
    invocation: ToolInvocation,
    prepared_action: PreparedActionReview,
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let tool_request = tool_call_request(&request, &invocation);
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
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let span = tool_dispatch_span(&invocation.name);
    if !matches!(
        request.origin,
        GovernedInvocationOrigin::ExecutionTask { .. }
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
    let tool_request = tool_call_request(&request, &invocation);
    let output = match request.origin {
        GovernedInvocationOrigin::ExecutionTask {
            run_uid,
            task_uid,
            generation,
        } => span
            .in_scope(|| {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution_task(Json::from(ExecutionTaskToolCallRequest {
                            call: tool_request,
                            origin: Some(ExecutionTaskOrigin {
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
            .into_inner(),
        GovernedInvocationOrigin::RootTurn | GovernedInvocationOrigin::Worker { .. } => span
            .in_scope(|| {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute(Json::from(tool_request)),
                )
            })
            .call()
            .instrument(span)
            .await?
            .into_inner(),
    };
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

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
    GovernedInvocationResult {
        tool_id,
        invocation,
        output,
        disposition,
        event_plan: GovernedInvocationEventPlan::WorkflowSyntheticResult { success: false },
    }
}

fn prepare_action_review_request(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
) -> PrepareActionReviewRequest {
    let (worker_id, default_provenance, execution_origin) = match request.origin {
        GovernedInvocationOrigin::RootTurn => (None, CapabilityProvenance::default(), None),
        GovernedInvocationOrigin::Worker { worker_id, turn_id } => (
            Some(WorkerId::from(worker_id)),
            CapabilityProvenance {
                kind: Some("worker".to_string()),
                id: Some(worker_id.to_string()),
                step_id: Some(turn_id.to_string()),
            },
            None,
        ),
        GovernedInvocationOrigin::ExecutionTask {
            run_uid,
            task_uid,
            generation,
        } => (
            None,
            CapabilityProvenance::default(),
            Some(ExecutionTaskOrigin {
                run_uid,
                task_uid,
                generation,
            }),
        ),
    };

    PrepareActionReviewRequest {
        session: request.session.clone(),
        invocation: invocation.clone(),
        review_id: request.tool_id.0,
        tool_call_id: request.tool_id,
        worker_id,
        capability_provenance: request
            .capability_provenance
            .cloned()
            .unwrap_or(default_provenance),
        execution_origin,
        idempotency_key: invocation.id.clone(),
    }
}

fn tool_call_request(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id: request.tool_id,
        caller_identity: request.identity.clone(),
        provider_tool_use_id: invocation.id.clone(),
        tool_name: invocation.name.clone(),
        input: invocation.input.clone(),
        active_canary: request.active_canary.map(ToOwned::to_owned),
        session_id: request.session_id,
        trusted_sandbox_manifest: request.trusted_sandbox_manifest.cloned(),
        worker_id: match request.origin {
            GovernedInvocationOrigin::RootTurn => None,
            GovernedInvocationOrigin::Worker { worker_id, .. } => Some(worker_id.to_string()),
            GovernedInvocationOrigin::ExecutionTask { .. } => None,
        },
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
    append_session_event(
        ctx,
        request.session_id,
        Event::ToolResult {
            tool_id: request.tool_id,
            provider_tool_use_id: invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: false,
            duration_ms: 0,
        },
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
        },
    )
    .await
    .map(|_| ())
}

fn owns_root_session_tool_events(origin: GovernedInvocationOrigin<'_>) -> bool {
    !matches!(origin, GovernedInvocationOrigin::ExecutionTask { .. })
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

    use moa_core::{
        traits::{Identity, IdentityType},
        types::action_policy::CapabilityProvenance,
        types::completion::ToolCallContent,
        types::completion::ToolInvocation,
        types::contact::ContactId,
        types::contact::ContactRef,
        types::contact::ContactVerificationState,
        types::contact::SessionActorRef,
        types::identifiers::TenantId,
        types::identifiers::ToolCallId,
        types::identifiers::UserId,
        types::session::SessionMeta,
        types::tools::TrustedSandboxFileEntry,
        types::tools::TrustedSandboxFileManifestRef,
    };
    use moa_test_support::fixtures::contact_ref_fixture;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        GovernedInvocationDisposition, GovernedInvocationEventPlan, GovernedInvocationOrigin,
        GovernedInvocationRequest, completed_result, owns_root_session_tool_events,
        pending_review_output, prepare_action_review_request, tool_call_request,
    };
    use crate::delegation::storage_user_id;

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
            active_canary: Some("canary"),
            trusted_sandbox_manifest: None,
            origin,
            capability_provenance: None,
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
            GovernedInvocationOrigin::RootTurn,
        );

        let policy_request = prepare_action_review_request(&request, &tool_call.invocation);

        assert_eq!(policy_request.session, session);
        assert_eq!(policy_request.invocation, tool_call.invocation);
        assert_eq!(policy_request.review_id, request.tool_id.0);
        assert_eq!(policy_request.tool_call_id, request.tool_id);
        assert_eq!(policy_request.worker_id, None);
        assert_eq!(policy_request.capability_provenance, Default::default());
        assert_eq!(policy_request.execution_origin, None);
        assert_eq!(
            policy_request.idempotency_key.as_deref(),
            Some("provider-tool-1")
        );
    }

    #[test]
    fn action_policy_worker_request_sets_origin_fields() {
        // Pins: worker review records remain traceable to the child turn.
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
            },
        );

        let policy_request = prepare_action_review_request(&request, &tool_call.invocation);

        assert_eq!(policy_request.worker_id.as_deref(), Some("worker-1"));
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
        assert_eq!(policy_request.execution_origin, None);
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

        let policy_request = prepare_action_review_request(&request, &tool_call.invocation);

        assert_eq!(policy_request.session, session);
        assert_eq!(policy_request.invocation, tool_call.invocation);
        assert_eq!(policy_request.worker_id, None);
        assert_eq!(policy_request.capability_provenance, capability);
        assert_eq!(
            policy_request.execution_origin,
            Some(moa_core::types::action_policy::ExecutionTaskOrigin {
                run_uid: Uuid::from_u128(40),
                task_uid: Uuid::from_u128(41),
                generation: 2,
            })
        );
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
            GovernedInvocationOrigin::RootTurn,
        );
        let worker = request(
            &session,
            &tool_call,
            &allowed_tools,
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "turn-1",
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
            .map(|request| prepare_action_review_request(request, &tool_call.invocation))
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
        assert_eq!(prepared[0].execution_origin, None);
        assert_eq!(prepared[1].execution_origin, None);
        assert!(prepared[2].execution_origin.is_some());
    }

    #[test]
    fn execution_task_origin_never_owns_root_session_tool_events() {
        // Pins: all governed root event appenders share one execution-task exclusion guard.
        assert!(owns_root_session_tool_events(
            GovernedInvocationOrigin::RootTurn
        ));
        assert!(owns_root_session_tool_events(
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "turn-1",
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
            GovernedInvocationOrigin::RootTurn,
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation);

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
            },
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation);

        assert_eq!(tool_request.worker_id.as_deref(), Some("worker-1"));
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
            GovernedInvocationOrigin::RootTurn,
        );

        let tool_request = tool_call_request(&request, &tool_call.invocation);

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
            GovernedInvocationOrigin::RootTurn,
        );
        request.trusted_sandbox_manifest = Some(&manifest);

        let tool_request = tool_call_request(&request, &tool_call.invocation);

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
            output: moa_core::types::tools::ToolOutput::text(
                "ok",
                std::time::Duration::from_millis(5),
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
}
