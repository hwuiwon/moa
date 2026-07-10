//! Governed tool invocation coordination for turn workflows.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use moa_core::wire::session_store::{AppendEventRequest, RecordSegmentToolUseRequest};
use moa_core::{
    ActionPolicyEffect, Event, ProcedureTool, SessionId, SessionMeta, ToolCallContent, ToolCallId,
    ToolCallRequest, ToolInvocation, ToolOutput, TrustedSandboxFileManifestRef, WorkerId,
    is_delegation_tool_name, is_procedure_tool_name,
};
use moa_observability::restate_observability::{event_persist_span, tool_dispatch_span};
use moa_observability::{record_turn_event_persist_duration, record_turn_tool_dispatch_duration};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::delegation::storage_user_id;
use crate::services::{
    action_policy::{ActionPolicyClient, PrepareActionReviewRequest, PreparedActionReview},
    action_reviews::{ActionReviewsClient, RequestActionReview},
    session_store::RestateSessionStoreClient,
    tool_executor::ToolExecutorClient,
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
}

/// Request for coordinating one governed tool invocation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GovernedInvocationRequest<'a> {
    /// Session metadata used for policy and execution.
    pub(crate) session: &'a SessionMeta,
    /// Session event stream that receives tool events.
    pub(crate) session_id: SessionId,
    /// Stable tool call id for event correlation.
    pub(crate) tool_id: ToolCallId,
    /// Provider tool-call block.
    pub(crate) tool_call: &'a ToolCallContent,
    /// Allowed tool names selected for this turn.
    pub(crate) allowed_tools: &'a BTreeSet<String>,
    /// Normalized `skill://<name>` references of the procedure-capable skills selected
    /// for this turn. A `run_procedure` call may only target a skill in this set.
    pub(crate) selected_procedure_skills: &'a BTreeSet<String>,
    /// Active prompt-injection canary marker, when present.
    pub(crate) active_canary: Option<&'a str>,
    /// Trusted sandbox file manifest selected by the runtime that built this tool call.
    pub(crate) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    /// Root or worker origin metadata.
    pub(crate) origin: GovernedInvocationOrigin<'a>,
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

    // Procedure tools start/poll durable runs and, like delegation tools, must run
    // on the workflow-owned path with the Restate context rather than through the
    // stateless ToolExecutor. They are executed inline here so both the root and
    // worker turn loops keep their existing `Completed` handling; the run's own
    // node actions remain action-policy governed inside ProcedureExecution.
    if is_procedure_tool_name(&invocation.name) {
        return execute_procedure_tool(ctx, &request, invocation).await;
    }

    append_tool_call_event(ctx, &request).await?;

    let prepared_action = ctx
        .service_client::<ActionPolicyClient>()
        .prepare_action_review(Json(prepare_action_review_request(&request, &invocation)))
        .call()
        .await?
        .into_inner();

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

    execute_allowed_tool(ctx, request, invocation).await
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

    ctx.service_client::<ActionReviewsClient>()
        .request(Json::from(RequestActionReview {
            envelope: prepared_action.envelope,
            preview: prepared_action.preview,
            tool_request,
        }))
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
) -> Result<GovernedInvocationOutcome, HandlerError> {
    let span = tool_dispatch_span(&invocation.name);
    turn_progress::maybe_emit(
        ctx,
        request.session_id,
        turn_progress::running_tool_summary(&invocation.name),
    )
    .await?;
    let dispatch_started = Instant::now();
    let output = ctx
        .service_client::<ToolExecutorClient>()
        .execute(Json::from(tool_call_request(&request, &invocation)))
        .call()
        .instrument(span)
        .await?
        .into_inner();
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

/// Executes a procedure tool on the workflow-owned path and records its events.
///
/// The tool call event is appended here (procedure tools do not reach the
/// delegation short-circuit that would otherwise own it), the invocation is gated
/// by [`ActionPolicyClient::prepare_action_review`] exactly like a registered tool,
/// and only an `Allow` effect starts or polls the run through
/// [`crate::procedure_tools::execute_procedure_tool`]. The result event is appended
/// before returning a `Completed` outcome so the caller turn loops treat it like any
/// other executed tool.
///
/// `AdminReview` is turned into a terminal denial rather than a queued review: an
/// approved review resumes by re-dispatching the tool through the stateless
/// `ToolExecutor` (see `ActionReviews::decide`), which cannot run a workflow-owned
/// procedure tool. Deferring these would enqueue a review that could never execute,
/// so the model is told the action requires review it cannot obtain on this path.
async fn execute_procedure_tool(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
    invocation: ToolInvocation,
) -> Result<GovernedInvocationOutcome, HandlerError> {
    append_tool_call_event(ctx, request).await?;

    let prepared_action = ctx
        .service_client::<ActionPolicyClient>()
        .prepare_action_review(Json(prepare_action_review_request(request, &invocation)))
        .call()
        .await?
        .into_inner();

    if matches!(prepared_action.effect, ActionPolicyEffect::Deny) {
        let output = denied_action_output(&prepared_action, &invocation);
        append_procedure_tool_result(ctx, request, &invocation, &output).await?;
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
        let output = procedure_review_unsupported_output(&invocation);
        append_procedure_tool_result(ctx, request, &invocation, &output).await?;
        return Ok(GovernedInvocationOutcome::Completed(Box::new(
            completed_result(
                request.tool_id,
                invocation,
                output,
                GovernedInvocationDisposition::Denied,
            ),
        )));
    }

    let span = tool_dispatch_span(&invocation.name);
    turn_progress::maybe_emit(
        ctx,
        request.session_id,
        turn_progress::running_tool_summary(&invocation.name),
    )
    .await?;

    let dispatch_started = Instant::now();
    let output = match ProcedureTool::from_invocation(&invocation) {
        Ok(Some(tool)) => {
            crate::procedure_tools::execute_procedure_tool(
                ctx,
                request.session,
                request.session_id,
                tool,
                request.selected_procedure_skills,
            )
            .instrument(span)
            .await?
        }
        Ok(None) => ToolOutput::error(
            format!("unsupported procedure tool {}", invocation.name),
            Duration::ZERO,
        ),
        Err(error) => ToolOutput::error(
            format!("invalid {} arguments: {error}", invocation.name),
            Duration::ZERO,
        ),
    };
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_procedure_tool_result(ctx, request, &invocation, &output).await?;

    let success = !output.is_error;
    Ok(GovernedInvocationOutcome::Completed(Box::new(
        GovernedInvocationResult {
            tool_id: request.tool_id,
            invocation,
            output,
            disposition: GovernedInvocationDisposition::Executed,
            event_plan: GovernedInvocationEventPlan::WorkflowSyntheticResult { success },
        },
    )))
}

/// Records a successful segment tool use through the session-store service.
pub(crate) async fn record_segment_tool_use(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
            session_id,
            tool_name: tool_name.to_string(),
        }))
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
    let (worker_id, origin_kind, origin_id, origin_step_id) = match request.origin {
        GovernedInvocationOrigin::RootTurn => (None, None, None, None),
        GovernedInvocationOrigin::Worker { worker_id, turn_id } => (
            Some(WorkerId::from(worker_id)),
            Some("worker".to_string()),
            Some(worker_id.to_string()),
            Some(turn_id.to_string()),
        ),
    };

    PrepareActionReviewRequest {
        session: request.session.clone(),
        invocation: invocation.clone(),
        review_id: request.tool_id.0,
        tool_call_id: request.tool_id,
        worker_id,
        origin_kind,
        origin_id,
        origin_step_id,
        idempotency_key: invocation.id.clone(),
    }
}

fn tool_call_request(
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_call_id: request.tool_id,
        provider_tool_use_id: invocation.id.clone(),
        tool_name: invocation.name.clone(),
        input: invocation.input.clone(),
        active_canary: request.active_canary.map(ToOwned::to_owned),
        session_id: Some(request.session_id),
        tenant_id: request.session.tenant_id,
        user_id: storage_user_id(request.session),
        trusted_sandbox_manifest: request.trusted_sandbox_manifest.cloned(),
        worker_id: match request.origin {
            GovernedInvocationOrigin::RootTurn => None,
            GovernedInvocationOrigin::Worker { worker_id, .. } => Some(worker_id.to_string()),
        },
    }
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

/// Terminal output for a procedure tool that tenant policy routed to admin review.
///
/// Procedure tools run on the workflow-owned path and cannot be resumed through the
/// `ToolExecutor`-based review approval flow, so a review can never execute them.
/// The call is denied with a message the model can act on rather than being queued.
fn procedure_review_unsupported_output(invocation: &ToolInvocation) -> ToolOutput {
    ToolOutput::error(
        format!(
            "Tool {} requires tenant admin review, which is not supported for procedure tools; the action was not started.",
            invocation.name
        ),
        Duration::ZERO,
    )
}

async fn append_tool_call_event(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
) -> Result<(), HandlerError> {
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

async fn append_procedure_tool_result(
    ctx: &WorkflowContext<'_>,
    request: &GovernedInvocationRequest<'_>,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        request.session_id,
        Event::ToolResult {
            tool_id: request.tool_id,
            provider_tool_use_id: invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: !output.is_error,
            duration_ms: 0,
        },
    )
    .await
    .map(|_| ())
}

async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    let sequence_num = ctx
        .service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: None,
        }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(sequence_num)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use moa_core::{
        ContactId, ContactRef, ContactVerificationState, SessionActorRef, SessionMeta, TenantId,
        ToolCallContent, ToolCallId, ToolInvocation, TrustedSandboxFileEntry,
        TrustedSandboxFileManifestRef, UserId,
    };
    use moa_test_support::fixtures::contact_ref_fixture;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        GovernedInvocationDisposition, GovernedInvocationEventPlan, GovernedInvocationOrigin,
        GovernedInvocationRequest, completed_result, pending_review_output,
        prepare_action_review_request, tool_call_request,
    };
    use crate::delegation::storage_user_id;

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
        selected_procedure_skills: &'a BTreeSet<String>,
        origin: GovernedInvocationOrigin<'a>,
    ) -> GovernedInvocationRequest<'a> {
        GovernedInvocationRequest {
            session,
            session_id: session.id,
            tool_id: ToolCallId(Uuid::from_u128(30)),
            tool_call,
            allowed_tools,
            selected_procedure_skills,
            active_canary: Some("canary"),
            trusted_sandbox_manifest: None,
            origin,
        }
    }

    #[test]
    fn root_policy_request_has_no_origin_and_uses_provider_idempotency_key() {
        // Pins: root turns preserve the previous ActionPolicy request shape.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let no_procedures = BTreeSet::new();
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            &no_procedures,
            GovernedInvocationOrigin::RootTurn,
        );

        let policy_request = prepare_action_review_request(&request, &tool_call.invocation);

        assert_eq!(policy_request.session, session);
        assert_eq!(policy_request.invocation, tool_call.invocation);
        assert_eq!(policy_request.review_id, request.tool_id.0);
        assert_eq!(policy_request.tool_call_id, request.tool_id);
        assert_eq!(policy_request.worker_id, None);
        assert_eq!(policy_request.origin_kind, None);
        assert_eq!(policy_request.origin_id, None);
        assert_eq!(policy_request.origin_step_id, None);
        assert_eq!(
            policy_request.idempotency_key.as_deref(),
            Some("provider-tool-1")
        );
    }

    #[test]
    fn worker_policy_request_sets_origin_fields() {
        // Pins: worker review records remain traceable to the child turn.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let no_procedures = BTreeSet::new();
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            &no_procedures,
            GovernedInvocationOrigin::Worker {
                worker_id: "worker-1",
                turn_id: "child-turn-1",
            },
        );

        let policy_request = prepare_action_review_request(&request, &tool_call.invocation);

        assert_eq!(policy_request.worker_id.as_deref(), Some("worker-1"));
        assert_eq!(policy_request.origin_kind.as_deref(), Some("worker"));
        assert_eq!(policy_request.origin_id.as_deref(), Some("worker-1"));
        assert_eq!(
            policy_request.origin_step_id.as_deref(),
            Some("child-turn-1")
        );
    }

    #[test]
    fn tool_request_preserves_session_identity_and_idempotency() {
        // Pins: deferred review and direct execution share one durable request shape.
        let mut session = test_session_meta();
        let contact_id = ContactId::new();
        session.contact = Some(contact_ref(session.tenant_id, contact_id));
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let no_procedures = BTreeSet::new();
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            &no_procedures,
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
        assert_eq!(tool_request.session_id, Some(session.id));
        assert_eq!(tool_request.tenant_id, session.tenant_id);
        assert_eq!(tool_request.user_id, UserId::new(contact_id.to_string()));
        assert_eq!(tool_request.trusted_sandbox_manifest, None);
        assert_eq!(tool_request.worker_id, None);
    }

    #[test]
    fn worker_origin_request_carries_worker_hand_scope() {
        // Pins: a worker tool call provisions a hand scoped to its worker_id.
        let session = test_session_meta();
        let tool_call = tool_call();
        let allowed_tools = BTreeSet::from(["file_read".to_string()]);
        let no_procedures = BTreeSet::new();
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            &no_procedures,
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
        let no_procedures = BTreeSet::new();
        let request = request(
            &session,
            &tool_call,
            &allowed_tools,
            &no_procedures,
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
        let no_procedures = BTreeSet::new();
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
            &no_procedures,
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
    fn procedure_admin_review_becomes_terminal_denial() {
        // Pins: an AdminReview effect on a procedure tool is turned into a terminal
        // denial the model can read, because a procedure tool cannot be resumed through
        // the ToolExecutor-based review approval path.
        let invocation = ToolInvocation {
            id: None,
            name: "run_procedure".to_string(),
            input: json!({}),
        };
        let output = super::procedure_review_unsupported_output(&invocation);

        assert!(output.is_error);
        let text = output.to_text();
        assert!(text.contains("run_procedure"), "names the tool: {text}");
        assert!(
            text.contains("review"),
            "explains the review limitation: {text}"
        );
    }

    #[test]
    fn denied_procedure_result_is_workflow_owned_and_not_segment_success() {
        // Pins: a denied procedure invocation is a workflow-synthetic result (the turn
        // workflow owns appending both the tool-call and result events, not the
        // ToolExecutor), counts as a denied worker tool, and is never segment success.
        let invocation = ToolInvocation {
            id: None,
            name: "run_procedure".to_string(),
            input: json!({}),
        };
        let result = completed_result(
            ToolCallId(Uuid::from_u128(2)),
            invocation.clone(),
            super::procedure_review_unsupported_output(&invocation),
            GovernedInvocationDisposition::Denied,
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
            output: moa_core::ToolOutput::text("ok", std::time::Duration::from_millis(5)),
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
