//! Tenant-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::ExecutionFailureClass;
use moa_authz_schema::Relation;
use moa_core::{
    events::Event, events::EventType, types::action_policy::ActionClass,
    types::action_policy::ActionEnvelope, types::action_policy::ActionReviewFailureClass,
    types::action_policy::ActionReviewOutcome, types::action_policy::ActionReviewOwner,
    types::action_policy::ActionReviewPreview, types::action_policy::ActionReviewReceipt,
    types::action_policy::ActionReviewRegistration, types::action_policy::ActionReviewStatus,
    types::action_policy::ActionReviewTerminalEvent, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId,
    types::security::ToolCapabilityId, types::security::ToolOutputAssessment,
    types::tools::SecuredToolOutput, types::tools::ToolCallRequest,
};
use moa_observability::propagation::ValidatedTraceContext;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_observability::{
    record_action_review_decision, record_action_review_requested, record_approval_wait,
};
use moa_wire::session_store::AppendEventRequest;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::action_reviews::app as action_review_app;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::authorize_tenant;
use crate::objects::session::SessionClient;
use crate::objects::worker::WorkerClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::{ExecutionTaskToolCallRequest, ToolExecutorClient};
use moa_core::traits::SessionRepository;
use moa_execution::wire::ExecutionActionReviewResolution;

/// Summary returned for one tenant action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReviewSummary {
    /// Review identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Original tool call identifier.
    pub tool_call_id: ToolCallId,
    /// Tool name.
    pub tool_name: String,
    /// Action class.
    pub action_class: ActionClass,
    /// Risk level.
    pub risk_level: moa_core::types::action_policy::RiskLevel,
    /// Concise input summary.
    pub input_summary: String,
    /// Durable action envelope.
    pub envelope: ActionEnvelope,
    /// Human-readable preview.
    pub preview: ActionReviewPreview,
    /// Review status.
    pub status: ActionReviewStatus,
    /// User that requested the action.
    pub requested_by: String,
    /// User that decided the action, when present.
    pub decided_by: Option<String>,
    /// Denial reason, when present.
    pub deny_reason: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Decision timestamp, when present.
    pub decided_at: Option<DateTime<Utc>>,
}

/// Request payload for `ActionReviews/request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestActionReview {
    /// Durable policy-facing action envelope.
    pub envelope: ActionEnvelope,
    /// Human-readable preview rendered to admins.
    pub preview: ActionReviewPreview,
    /// Stored tool request to execute if the review is cleared.
    pub tool_request: ToolCallRequest,
}

/// Request payload for `ActionReviews/list_pending`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListActionReviewsRequest {
    /// Tenant whose pending action reviews should be listed.
    pub tenant_id: TenantId,
}

/// Request payload for `ActionReviews/decide`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideActionReviewRequest {
    /// Tenant that owns the review.
    pub tenant_id: TenantId,
    /// Review identifier.
    pub review_id: Uuid,
    /// Decision kind.
    pub decision: ActionReviewDecisionKind,
    /// Optional denial reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Wire decision kind for `ActionReviews/decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReviewDecisionKind {
    /// Clear the action for execution.
    Cleared,
    /// Deny the action.
    Denied,
}

/// Restate service surface for tenant-admin action reviews.
#[restate_sdk::service]
#[name = "ActionReviews"]
pub trait ActionReviews {
    /// Queue one action for tenant-admin review.
    async fn request(
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError>;

    /// List pending tenant action reviews.
    async fn list_pending(
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError>;

    /// Decide one tenant action review.
    async fn decide(request: Json<DecideActionReviewRequest>) -> Result<(), HandlerError>;
}

/// Concrete action-review service implementation.
#[derive(Clone)]
pub struct ActionReviewsImpl {
    pool: sqlx::PgPool,
    session_store: Arc<dyn SessionRepository>,
    review_timeout_secs: i64,
}

impl ActionReviewsImpl {
    /// Creates the action-review adapter with its persistence dependencies.
    ///
    /// `review_timeout_secs` sets how long a queued review may stay pending
    /// before the reaper fails it closed.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<dyn SessionRepository>,
        review_timeout_secs: i64,
    ) -> Self {
        Self {
            pool,
            session_store,
            review_timeout_secs,
        }
    }
}

impl ActionReviews for ActionReviewsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: request is an internal workflow call after the owning session or worker has already checked participant authorization before tool execution.
    async fn request(
        &self,
        ctx: Context<'_>,
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "request");
        let mut request = request.into_inner();
        let execution_task_trace_context = request
            .envelope
            .owner
            .execution_origin()
            .is_some()
            .then(|| incoming_trace_context(&ctx))
            .flatten();
        action_review_app::prepare_request(&mut request)?;
        let event = action_review_app::requested_event(&request);
        let pool = self.pool.clone();
        let owner = request.envelope.owner.clone();
        let session_id = owner.session_id();
        let action_class = request.envelope.action_class;
        let review_timeout_secs = self.review_timeout_secs;

        let stored = ctx
            .run(|| async move {
                action_review_app::request_review(
                    pool,
                    request,
                    review_timeout_secs,
                    execution_task_trace_context,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_request")
            .await?
            .into_inner();
        if stored.record_requested_event {
            let event_exists = prior_action_review_event_exists(
                &ctx,
                &storage_partition_id(stored.summary.tenant_id),
                session_id,
                EventType::ActionReviewRequested,
                stored.summary.id,
                &self.session_store,
            )
            .await?;
            if !event_exists {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<RestateSessionStoreClient>()
                        .append_event(Json(AppendEventRequest {
                            session_id,
                            event,
                            dedupe_key: None,
                        })),
                )
                .call()
                .await?;
            }
            let pool = self.pool.clone();
            let storage_partition_id = storage_partition_id(stored.summary.tenant_id);
            let review_id = stored.summary.id;
            ctx.run(|| async move {
                action_review_app::mark_requested_event_recorded(
                    pool,
                    storage_partition_id,
                    review_id,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_mark_requested_event_recorded")
            .await?;
        }
        // Registration happens before this handler returns Pending, so the owning
        // Session or Worker durably knows it has an outstanding review before the
        // requesting turn ever sees the pending-review tool output. Doing it after the
        // return would leave a window in which a worker could finish, deliver its
        // parent result, and self-clean while a review it raised was still open.
        register_conversational_review(&ctx, &owner, stored.summary.id).await?;
        if stored.newly_inserted {
            record_action_review_requested(
                moa_core::types::action_policy::ActionPolicyEffect::AdminReview,
                action_class,
            );
        }
        Ok(Json::from(stored.summary))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_pending(
        &self,
        ctx: Context<'_>,
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "list_pending");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let storage_partition_id = storage_partition_id(request.tenant_id);

        Ok(ctx
            .run(|| async move {
                action_review_app::list_pending_reviews(pool, storage_partition_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_list_pending")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide(
        &self,
        ctx: Context<'_>,
        request: Json<DecideActionReviewRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "decide");
        let resolution_trace_context = current_trace_context();
        let request = request.into_inner();
        let tenant_id = request.tenant_id;
        let review_id = request.review_id;
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let decision_trace_context = resolution_trace_context.clone();
        let mut decided = ctx
            .run(|| async move {
                action_review_app::decide_review(
                    pool,
                    request,
                    identity.id.to_string(),
                    decision_trace_context,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_decide")
            .await?
            .into_inner();

        if let (Some(tool_request), Some(origin)) = (
            decided.tool_request.clone(),
            decided.owner.execution_origin(),
        ) {
            let execution = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .execute_execution_task(Json::from(ExecutionTaskToolCallRequest {
                        call: tool_request,
                        origin: Some(origin),
                    })),
            )
            .call()
            .await;
            let resolution = match execution {
                Ok(secured) => {
                    // The reviewed tool's output was classified inside the
                    // executor's `ctx.run`. This path consumes that envelope
                    // whole — it never re-derives a summary from raw bytes and
                    // never reclassifies.
                    let secured = secured.into_inner();
                    if secured.is_error() {
                        ExecutionActionReviewResolution::Failed {
                            class: ExecutionFailureClass::Terminal,
                            message: secured.safe_output.to_text(),
                        }
                    } else {
                        ExecutionActionReviewResolution::Completed {
                            tool_output: serde_json::to_value(&secured).map_err(|error| {
                                TerminalError::new(format!(
                                    "serialize execution review tool output: {error}"
                                ))
                            })?,
                        }
                    }
                }
                Err(error) => ExecutionActionReviewResolution::Failed {
                    class: ExecutionFailureClass::Terminal,
                    message: format!("{error:?}"),
                },
            };
            let pool = self.pool.clone();
            decided = ctx
                .run(|| async move {
                    action_review_app::finalize_execution_review(
                        pool,
                        tenant_id,
                        review_id,
                        resolution,
                        resolution_trace_context,
                    )
                    .await
                    .map(Json::from)
                })
                .name("action_reviews_finalize_execution_review")
                .await?
                .into_inner();
        }

        if decided.record_decision_event {
            let event_exists = prior_action_review_event_exists(
                &ctx,
                &decided.storage_partition_id,
                decided.owner.session_id(),
                EventType::ActionReviewDecided,
                decided.review_id,
                &self.session_store,
            )
            .await?;
            if !event_exists {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<RestateSessionStoreClient>()
                        .append_event(Json(AppendEventRequest {
                            session_id: decided.owner.session_id(),
                            event: Event::ActionReviewDecided {
                                review_id: decided.review_id,
                                decision: decided.decision.clone(),
                                decided_by: decided.decided_by.clone(),
                                decided_at: decided.decided_at,
                            },
                            dedupe_key: None,
                        })),
                )
                .call()
                .await?;
            }
            let pool = self.pool.clone();
            let storage_partition_id = decided.storage_partition_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                action_review_app::mark_decision_event_recorded(
                    pool,
                    storage_partition_id,
                    review_id,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_mark_decision_event_recorded")
            .await?;
        }
        if decided.newly_decided {
            record_action_review_decision(decided.status, decided.action_class);
            let wait = (decided.decided_at - decided.created_at)
                .to_std()
                .unwrap_or_default();
            record_approval_wait(decided.action_class, wait);
        }

        // Executed exactly once per cleared review. `None` means either a denial or a
        // replay of an already-dispatched clear; both fall through to receipt
        // reconstruction from durable facts below.
        let mut executed_output: Option<SecuredToolOutput> = None;
        if let Some(tool_request) = decided.tool_request.as_ref() {
            if let Some(execution_request) =
                execution_task_review_request(decided.owner.execution_origin(), tool_request)
            {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution_task(Json::from(execution_request)),
                )
                .call()
                .await?;
            } else if prior_tool_terminal_event_exists(
                &ctx,
                &decided,
                tool_request.tool_call_id,
                &self.session_store,
            )
            .await?
            .is_none()
            {
                let execution = crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute(Json::from(tool_request.clone())),
                )
                .call()
                .await;
                match execution {
                    Ok(output) => executed_output = Some(output.into_inner()),
                    // A pre-durability infrastructure failure leaves nothing to report:
                    // the invocation retries and no owner callback is sent. A failure
                    // that already recorded its terminal tool event is recovered from
                    // that durable fact instead of re-running a reviewed side effect.
                    Err(error) => {
                        if prior_tool_terminal_event_exists(
                            &ctx,
                            &decided,
                            tool_request.tool_call_id,
                            &self.session_store,
                        )
                        .await?
                        .is_none()
                        {
                            return Err(error.into());
                        }
                    }
                }
            }
            let pool = self.pool.clone();
            let storage_partition_id = decided.storage_partition_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                action_review_app::mark_execution_requested(pool, storage_partition_id, review_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_mark_execution_requested")
            .await?;
        }

        // An execution-task owner keeps its existing run/task outbox and ack path and
        // receives zero conversational callbacks.
        if decided.owner.is_conversational()
            && let Some(receipt) = conversational_receipt(
                &ctx,
                &decided,
                executed_output.as_ref(),
                &self.session_store,
            )
            .await?
        {
            deliver_conversational_resolution(&ctx, receipt).await?;
        }
        Ok(())
    }
}

/// Registers one pending conversational review on its typed owner.
///
/// An `ExecutionTask` owner is routed nowhere: it is resumed by the durable
/// execution outbox, and sending it to a Session or Worker handler would resume a
/// conversation that never issued the action.
async fn register_conversational_review(
    ctx: &Context<'_>,
    owner: &ActionReviewOwner,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    let registration = ActionReviewRegistration {
        review_id,
        owner: owner.clone(),
    };
    match owner {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .register_action_review(Json::from(registration)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id.clone())
                    .register_action_review(Json::from(registration)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. } => {}
    }
    Ok(())
}

/// Delivers one typed resolution receipt to its conversational owner.
async fn deliver_conversational_resolution(
    ctx: &Context<'_>,
    receipt: ActionReviewReceipt,
) -> Result<(), HandlerError> {
    match receipt.owner.clone() {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .action_review_resolved(Json::from(receipt)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id)
                    .action_review_resolved(Json::from(receipt)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. } => {}
    }
    Ok(())
}

/// Builds the typed receipt for a conversational owner, or `None` when the
/// terminal facts a callback depends on are not durable yet.
///
/// A denial resolves on the decision alone. A clear additionally requires the
/// executed tool's terminal `ToolResult`/`ToolError`: either the one this
/// invocation just produced, or — on replay — the one already in the durable log.
async fn conversational_receipt(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    executed_output: Option<&SecuredToolOutput>,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<Option<ActionReviewReceipt>, HandlerError> {
    let mut terminal_events = vec![ActionReviewTerminalEvent::Decided];
    let outcome = match decided.status {
        ActionReviewStatus::Denied => ActionReviewOutcome::Denied {
            reason: decided
                .deny_reason
                .as_deref()
                .map(ActionReviewReceipt::bounded_summary),
        },
        ActionReviewStatus::Cleared => {
            let Some(executed_tool_call_id) = decided.executed_tool_call_id else {
                return Ok(None);
            };
            match executed_output {
                Some(output) => {
                    let (outcome, terminal) = executed_outcome(output);
                    terminal_events.push(terminal);
                    outcome
                }
                None => {
                    let Some(terminal) = prior_tool_terminal_event_exists(
                        ctx,
                        decided,
                        executed_tool_call_id,
                        session_store,
                    )
                    .await?
                    else {
                        return Ok(None);
                    };
                    terminal_events.push(terminal);
                    // The output itself is gone, but the assessment the circuit
                    // acted on is durable on the ToolResult. Read it back rather
                    // than stamping a fresh "safe" verdict nothing re-checked.
                    let recorded = durable_tool_security_metadata(
                        ctx,
                        decided,
                        executed_tool_call_id,
                        session_store,
                    )
                    .await?;
                    recovered_outcome(terminal, recorded)
                }
            }
        }
        ActionReviewStatus::Pending | ActionReviewStatus::Timeout => return Ok(None),
    };

    Ok(Some(ActionReviewReceipt {
        review_id: decided.review_id,
        owner: decided.owner.clone(),
        tool_name: decided.tool_name.clone(),
        requested_tool_call_id: decided.requested_tool_call_id,
        executed_tool_call_id: decided.executed_tool_call_id,
        outcome,
        terminal_events,
    }))
}

/// Classifies the output this invocation just produced and its durable terminal fact.
///
/// A tool that returned an error output still produced a durable `ToolResult`; the
/// distinction from an `ExecutionError` is what tells the continuation whether the
/// action ran at all.
fn executed_outcome(
    secured: &SecuredToolOutput,
) -> (ActionReviewOutcome, ActionReviewTerminalEvent) {
    // Bounded from the *classified* output. The classifier has already redacted
    // or destroyed unsafe carriers, so this is a length bound on safe text — not
    // a security control, and never a substitute for one.
    let summary = ActionReviewReceipt::bounded_summary(&secured.safe_output.to_text());
    let assessment = secured.assessment.clone();
    let capability = secured.capability.clone();
    if secured.is_error() {
        (
            ActionReviewOutcome::ClearedToolError {
                failure_class: ActionReviewFailureClass::ToolError,
                summary,
                assessment,
                capability,
            },
            ActionReviewTerminalEvent::ToolResult,
        )
    } else {
        (
            ActionReviewOutcome::ClearedSuccess {
                summary,
                assessment,
                capability,
            },
            ActionReviewTerminalEvent::ToolResult,
        )
    }
}

/// Rebuilds the outcome from an already-durable terminal fact, without re-execution.
///
/// Reached on replay and after an infrastructure failure that still recorded its
/// terminal event. The summary is intentionally generic: the exact output is
/// already in the session's own tool history, and re-reading it here would add a
/// second copy of the same bytes to the continuation event.
fn recovered_outcome(
    terminal: ActionReviewTerminalEvent,
    recorded: Option<(ToolOutputAssessment, ToolCapabilityId)>,
) -> ActionReviewOutcome {
    // A `ToolError` never produced a classified output, so its capability is the
    // reviewed tool under the built-in namespace and its assessment is the
    // detector's verdict on the absence of output, not a claim about bytes.
    let (assessment, capability) = recorded.unwrap_or_else(|| {
        (
            ToolOutputAssessment::safe(),
            ToolCapabilityId::builtin("unknown"),
        )
    });
    match terminal {
        ActionReviewTerminalEvent::ToolResult => ActionReviewOutcome::ClearedSuccess {
            summary: "The reviewed action completed; its result is in this session's tool history."
                .to_string(),
            assessment,
            capability,
        },
        ActionReviewTerminalEvent::ToolError | ActionReviewTerminalEvent::Decided => {
            ActionReviewOutcome::ClearedToolError {
                failure_class: ActionReviewFailureClass::ExecutionError,
                summary: "The reviewed action failed before producing a tool result.".to_string(),
                assessment,
                capability,
            }
        }
    }
}

/// Reads back the security metadata one durable `ToolResult` recorded.
async fn durable_tool_security_metadata(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    tool_call_id: ToolCallId,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<Option<(ToolOutputAssessment, ToolCapabilityId)>, HandlerError> {
    let store = session_store.clone();
    let storage_partition_id = decided.storage_partition_id.clone();
    let session_id = decided.owner.session_id();
    Ok(ctx
        .run(|| async move {
            store
                .tool_result_security_metadata(&storage_partition_id, session_id, tool_call_id)
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("action_reviews_tool_result_security_metadata")
        .await?
        .into_inner())
}

fn execution_task_review_request(
    origin: Option<moa_core::types::action_policy::ExecutionTaskOrigin>,
    tool_request: &moa_core::types::tools::ToolCallRequest,
) -> Option<ExecutionTaskToolCallRequest> {
    origin.map(|origin| ExecutionTaskToolCallRequest {
        call: tool_request.clone(),
        origin: Some(origin),
    })
}

/// Returns which terminal tool event is already durable for one reviewed call.
///
/// `ToolResult` is checked first because a tool that produced a model-visible
/// output — successful or not — has a result, while `ToolError` records a failure
/// that never reached one.
async fn prior_tool_terminal_event_exists(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    tool_call_id: ToolCallId,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<Option<ActionReviewTerminalEvent>, HandlerError> {
    for (event_type, terminal) in [
        (EventType::ToolResult, ActionReviewTerminalEvent::ToolResult),
        (EventType::ToolError, ActionReviewTerminalEvent::ToolError),
    ] {
        let store = session_store.clone();
        let storage_partition_id = decided.storage_partition_id.clone();
        let session_id = decided.owner.session_id();
        let exists = ctx
            .run(|| async move {
                store
                    .tool_event_exists(&storage_partition_id, session_id, event_type, tool_call_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name(format!(
                "action_reviews_tool_terminal_exists:{}",
                terminal.as_str()
            ))
            .await?
            .into_inner();
        if exists {
            return Ok(Some(terminal));
        }
    }
    Ok(None)
}

async fn prior_action_review_event_exists(
    ctx: &Context<'_>,
    storage_partition_id: &StoragePartitionId,
    session_id: moa_core::types::identifiers::SessionId,
    event_type: EventType,
    review_id: Uuid,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<bool, HandlerError> {
    let store = session_store.clone();
    let storage_partition_id = storage_partition_id.clone();
    Ok(ctx
        .run(|| async move {
            store
                .action_review_event_exists(
                    &storage_partition_id,
                    session_id,
                    event_type,
                    review_id,
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("action_reviews_event_exists")
        .await?
        .into_inner())
}

fn storage_partition_id(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

fn incoming_trace_context(ctx: &impl RequestHeaders) -> Option<ValidatedTraceContext> {
    let headers = ctx.request_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

fn current_trace_context() -> Option<ValidatedTraceContext> {
    let headers = moa_observability::current_trace_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

#[cfg(test)]
mod tests {
    use moa_core::traits::{Identity, IdentityType};
    use moa_core::types::{
        action_policy::ExecutionTaskOrigin,
        identifiers::{TenantId, ToolCallId},
        tools::ToolCallRequest,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{executed_outcome, execution_task_review_request, recovered_outcome};
    use moa_core::types::action_policy::{
        ActionReviewFailureClass, ActionReviewOutcome, ActionReviewTerminalEvent,
    };

    #[test]
    fn action_review_receipt_distinguishes_success_tool_error_and_execution_error() {
        // Pins: the continuation must be able to tell the user which of three things
        // happened. A tool that ran and returned an error output still produced a
        // durable ToolResult, and it must not be reported the same way as an execution
        // failure that never produced one.
        let (success, success_terminal) =
            executed_outcome(&moa_core::types::tools::SecuredToolOutput::assessed_safe(
                moa_core::types::tools::ToolOutput::text("ok", std::time::Duration::from_millis(1)),
                moa_core::types::security::ToolCapabilityId::builtin("noop"),
            ));
        assert_eq!(success_terminal, ActionReviewTerminalEvent::ToolResult);
        assert!(matches!(
            success,
            ActionReviewOutcome::ClearedSuccess { .. }
        ));

        let (tool_error, tool_error_terminal) =
            executed_outcome(&moa_core::types::tools::SecuredToolOutput::assessed_safe(
                moa_core::types::tools::ToolOutput::error(
                    "exit 1",
                    std::time::Duration::from_millis(1),
                ),
                moa_core::types::security::ToolCapabilityId::builtin("noop"),
            ));
        assert_eq!(tool_error_terminal, ActionReviewTerminalEvent::ToolResult);
        assert_eq!(
            match tool_error {
                ActionReviewOutcome::ClearedToolError { failure_class, .. } => failure_class,
                other => panic!("expected a cleared tool error, got {other:?}"),
            },
            ActionReviewFailureClass::ToolError
        );

        assert_eq!(
            match recovered_outcome(ActionReviewTerminalEvent::ToolError, None) {
                ActionReviewOutcome::ClearedToolError { failure_class, .. } => failure_class,
                other => panic!("expected an execution error, got {other:?}"),
            },
            ActionReviewFailureClass::ExecutionError
        );
        assert!(matches!(
            recovered_outcome(ActionReviewTerminalEvent::ToolResult, None),
            ActionReviewOutcome::ClearedSuccess { .. }
        ));
    }

    #[test]
    fn action_policy_clear_preserves_execution_task_review_provenance() {
        // Pins: approval replay cannot downgrade an execution task to root-session dispatch.
        let origin = ExecutionTaskOrigin {
            run_uid: Uuid::from_u128(10),
            task_uid: Uuid::from_u128(20),
            generation: 3,
        };
        let call = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            caller_identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::from_u128(2),
                tenant_id: TenantId::from(Uuid::from_u128(1)),
                api_key_id: None,
                acting_on_behalf_of: None,
            },
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            input: json!({"cmd": "printf reviewed"}),
            active_canary: None,
            session_id: moa_core::types::identifiers::SessionId::new(),
            trusted_sandbox_manifest: None,
            worker_id: None,
        };

        let request = execution_task_review_request(Some(origin), &call)
            .expect("execution provenance should select execution-task dispatch");

        assert_eq!(request.call, call);
        assert_eq!(request.origin, Some(origin));
        assert!(execution_task_review_request(None, &call).is_none());
    }
}
