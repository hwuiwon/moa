//! Bounded self-healing amendment planning for runs parked in `WaitingReplan`.
//!
//! A task that returns `NeedsReplan` parks its run. Nothing else in the product
//! proposes a replacement plan, so without this slice the run waits until its
//! absolute deadline kills it. The controller activation that observes the park
//! is required to stay short and must never issue a model call, so it only
//! *selects* the work: it sends one durable invocation here, and this service
//! performs the paid planner call and submits the resulting candidate through
//! the ordinary `Execution/apply_amendment` boundary, which owns validation,
//! the three replan-stop conditions, and the revision fence.
//!
//! Exactly one planning invocation exists per `(run_uid, plan_revision)`. Every
//! accepted amendment advances the revision, so the number of paid planner calls
//! is bounded by the number of revisions, which the replan-stop evaluation and
//! the pre-call exhaustion check both bound.

mod preparation;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_artifacts::execution_plan::PlanAmendment;
use moa_brain::execution_planning::{
    ExecutionAmendmentPlanningRequest, ExecutionAmendmentPlanningResultKind, plan_amendment,
};
use moa_config::ExecutionConfig;
use moa_core::{
    traits::{Identity, LLMProvider},
    types::{
        completion::{CompletionRequest, CompletionStream, SharedCompletionRequest},
        execution_planning::ExecutionPlanningAuditEnvelope,
        identifiers::ModelId,
        model::ModelCapabilities,
        resource::{ResourceAmounts, ResourceBudget},
    },
};
use moa_execution::{
    ReplanStopReason,
    capability::amendment_hash,
    repository::{
        ExecutionRepository, ExecutionRunRecord, ExecutionScope,
        planning_budget::{
            AmendmentPlanningCallReconcileOutcome, AmendmentPlanningCallReconcileRequest,
            AmendmentPlanningCallReservation, AmendmentPlanningCallReservationOutcome,
            AmendmentPlanningCallReservationRequest, PlanningUsage,
        },
        replan_stop::{NewExecutionReplanStopIntent, ReplanStopIntentWriteOutcome},
    },
    state::ExecutionRunStatus,
    wire::{ExecutionAmendmentRequest, ExecutionMutationResponse, ExecutionRunRequest},
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::services::{
    execution::ExecutionClient,
    llm_gateway::{
        BoundedCompletionRequest, LLMCompletionAction, LLMGatewayClient, completion_idempotency_key,
    },
};

pub use preparation::{
    AmendmentPlanningInputs, AmendmentPlanningOrigin, AmendmentPlanningTarget,
    PreparedAmendmentPlanning, prepare_amendment_planning,
};

/// Closed disposition of one bounded amendment-planning slice.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionAmendmentPlanningResponse {
    /// A planner-authored amendment reached the public amendment boundary.
    Submitted {
        /// Exact durable disposition returned by `Execution/apply_amendment`.
        response: Box<ExecutionMutationResponse>,
    },
    /// Replanning stopped without a candidate and the run was fenced to terminalize.
    Stopped {
        /// Typed deterministic stop reason.
        reason: ReplanStopReason,
    },
    /// The revision was no longer the live `WaitingReplan` revision.
    Skipped,
}

/// Restate service that owns the paid amendment-planning slice for one run revision.
#[restate_sdk::service]
#[name = "ExecutionAmendmentPlanner"]
pub trait ExecutionAmendmentPlanner {
    /// Plans and submits at most one amendment for the exact parked run revision.
    async fn plan(
        request: Json<AmendmentPlanningTarget>,
    ) -> Result<Json<ExecutionAmendmentPlanningResponse>, HandlerError>;
}

/// PostgreSQL-backed bounded amendment planner.
#[derive(Clone)]
pub struct ExecutionAmendmentPlannerImpl {
    repository: ExecutionRepository,
    config: ExecutionConfig,
    planner_model: ModelId,
}

impl ExecutionAmendmentPlannerImpl {
    /// Creates the planner over the shared execution repository and auxiliary model.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: ExecutionConfig, planner_model: ModelId) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            config,
            planner_model,
        }
    }
}

impl ExecutionAmendmentPlanner for ExecutionAmendmentPlannerImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(run_uid = %request.0.run_uid))]
    // SAFETY: ingress-private slice selected only by the trusted controller activation; every
    // authority it uses is reloaded from the locked run, and the amendment it proposes is
    // submitted under the run's own admitted identity through the authorized public boundary.
    async fn plan(
        &self,
        ctx: Context<'_>,
        request: Json<AmendmentPlanningTarget>,
    ) -> Result<Json<ExecutionAmendmentPlanningResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ExecutionAmendmentPlanner", "plan");
        let target = request.into_inner();
        let scope = target.scope();

        let repository = self.repository.clone();
        let config = self.config.clone();
        let inputs = ctx
            .run(|| async move {
                prepare_amendment_planning(&repository, &config, target, chrono::Utc::now())
                    .await
                    .map(Json::from)
            })
            .name(format!(
                "execution_amendment_inputs_{}_{}",
                target.run_uid, target.base_plan_revision
            ))
            .await?
            .into_inner();

        let prepared = match inputs {
            AmendmentPlanningInputs::Skip => {
                return Ok(Json::from(ExecutionAmendmentPlanningResponse::Skipped));
            }
            AmendmentPlanningInputs::Exhausted { origin, exhaustion } => {
                return self
                    .record_planner_stop(
                        &ctx,
                        scope,
                        origin,
                        exhaustion.reason,
                        exhaustion.description,
                    )
                    .await
                    .map(Json::from);
            }
            AmendmentPlanningInputs::Ready(prepared) => *prepared,
        };

        let provider = RestateAmendmentPlannerProvider {
            ctx: &ctx,
            repository: self.repository.clone(),
            scope,
            config: &self.config,
            deadline_at: prepared.remaining_budget.deadline_at,
            run_uid: target.run_uid,
            plan_revision: target.base_plan_revision,
            next_attempt: AtomicUsize::new(0),
        };
        let planned = plan_amendment(
            &provider,
            ExecutionAmendmentPlanningRequest {
                run_uid: target.run_uid,
                base_plan_revision: target.base_plan_revision,
                context: prepared.context,
                evidence: prepared.evidence,
                remaining_budget: prepared.remaining_budget,
                planner_model: self.planner_model.clone(),
                config: self.config.clone(),
                now: prepared.now,
            },
        )
        .await
        .map_err(crate::workflows::errors::moa_error_to_handler_error)?;
        for (ordinal, audit) in planned.audits.into_iter().enumerate() {
            self.persist_audit(&ctx, scope, &target, ordinal, audit)
                .await?;
        }

        let amendment = match planned.kind {
            ExecutionAmendmentPlanningResultKind::Ready { amendment, .. } => amendment,
            ExecutionAmendmentPlanningResultKind::NeedsInput { message }
            | ExecutionAmendmentPlanningResultKind::Unsupported { message } => {
                // Planner-authored verdict text is safe to carry into the replan-stop reason.
                return self
                    .record_planner_stop(
                        &ctx,
                        scope,
                        prepared.origin,
                        ReplanStopReason::NoProgress,
                        format!("amendment planner stopped: {message}"),
                    )
                    .await
                    .map(Json::from);
            }
            ExecutionAmendmentPlanningResultKind::ProviderFailure { message } => {
                // Infrastructure failure, not a semantic verdict: the raw provider string must not
                // reach the user-surfaced replan-stop gaps. Record the detail for operators (the
                // persisted planner audit already carries the ProviderError outcome) and stop the
                // replan with a bounded, user-safe description.
                tracing::error!(
                    run_uid = %target.run_uid,
                    plan_revision = target.base_plan_revision,
                    detail = %message,
                    "amendment planner provider failure"
                );
                return self
                    .record_planner_stop(
                        &ctx,
                        scope,
                        prepared.origin,
                        ReplanStopReason::NoProgress,
                        "an internal error interrupted amendment planning".to_string(),
                    )
                    .await
                    .map(Json::from);
            }
            ExecutionAmendmentPlanningResultKind::BudgetExhausted { message } => {
                return self
                    .record_planner_stop(
                        &ctx,
                        scope,
                        prepared.origin,
                        ReplanStopReason::BudgetExhausted,
                        message,
                    )
                    .await
                    .map(Json::from);
            }
        };

        let response = submit_amendment(&ctx, &target, &prepared.admitted_identity, amendment)
            .await?
            .into_inner();
        Ok(Json::from(ExecutionAmendmentPlanningResponse::Submitted {
            response: Box::new(response),
        }))
    }
}

impl ExecutionAmendmentPlannerImpl {
    /// Persists one planner or compile audit produced by the amendment operation.
    async fn persist_audit(
        &self,
        ctx: &Context<'_>,
        scope: ExecutionScope,
        target: &AmendmentPlanningTarget,
        ordinal: usize,
        audit: ExecutionPlanningAuditEnvelope,
    ) -> Result<(), HandlerError> {
        let repository = self.repository.clone();
        ctx.run(|| {
            let repository = repository.clone();
            let audit = audit.clone();
            async move {
                preparation::persist_amendment_audit(&repository, scope, &audit)
                    .await
                    .map(Json::from)
            }
        })
        .name(format!(
            "execution_amendment_audit_{}_{}_{ordinal}",
            target.run_uid, target.base_plan_revision
        ))
        .await?;
        Ok(())
    }

    /// Fences the run to terminalize when no further amendment may be attempted.
    async fn record_planner_stop(
        &self,
        ctx: &Context<'_>,
        scope: ExecutionScope,
        origin: AmendmentPlanningOrigin,
        reason: ReplanStopReason,
        description: String,
    ) -> Result<ExecutionAmendmentPlanningResponse, HandlerError> {
        let amendment_digest =
            amendment_hash(&planner_stop_amendment(origin, reason, &description))
                .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
        let repository = self.repository.clone();
        let config = self.config.clone();
        let detail = description.clone();
        let write = ctx
            .run(|| {
                let repository = repository.clone();
                let config = config.clone();
                let detail = detail.clone();
                async move {
                    repository
                        .request_replan_stop(
                            scope,
                            &config,
                            NewExecutionReplanStopIntent {
                                run_uid: origin.run_uid,
                                session_id: origin.session_id,
                                base_plan_revision: origin.base_plan_revision,
                                origin_task_id: origin.task_id,
                                task_generation: origin.task_generation,
                                amendment_hash: amendment_digest,
                                stop_reason: reason,
                                detail: Some(detail),
                            },
                        )
                        .await
                        .map(|outcome| Json::from(JournaledReplanStopWrite::from(&outcome)))
                        .map_err(crate::workflows::errors::execution_error_to_handler_error)
                }
            })
            .name(format!(
                "execution_amendment_stop_{}_{}",
                origin.run_uid, origin.base_plan_revision
            ))
            .await?
            .into_inner();
        match write {
            JournaledReplanStopWrite::Applied | JournaledReplanStopWrite::Replayed => {
                // The intent committed its own exact controller wake; this only shortens the
                // latency before the fleet drain delivers it.
                crate::services::execution::handlers::kick_execution_dispatcher(
                    ctx,
                    origin.run_uid,
                    origin.base_plan_revision,
                    "amendment-planner-stop",
                )
                .await?;
                Ok(ExecutionAmendmentPlanningResponse::Stopped { reason })
            }
            JournaledReplanStopWrite::NotFound | JournaledReplanStopWrite::Conflict => {
                Ok(ExecutionAmendmentPlanningResponse::Skipped)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledReplanStopWrite {
    Applied,
    Replayed,
    NotFound,
    Conflict,
}

impl From<&ReplanStopIntentWriteOutcome> for JournaledReplanStopWrite {
    fn from(value: &ReplanStopIntentWriteOutcome) -> Self {
        match value {
            ReplanStopIntentWriteOutcome::Applied(_) => Self::Applied,
            ReplanStopIntentWriteOutcome::Replayed(_) => Self::Replayed,
            ReplanStopIntentWriteOutcome::NotFound => Self::NotFound,
            ReplanStopIntentWriteOutcome::Conflict => Self::Conflict,
        }
    }
}

/// Submits one planner-authored amendment through the authorized public boundary.
///
/// The run's admitted identity travels with the call so `Execution/apply_amendment`
/// performs the same participant check an external amendment would, and the planner
/// never gains authority the run itself did not already hold.
async fn submit_amendment(
    ctx: &Context<'_>,
    target: &AmendmentPlanningTarget,
    identity: &Identity,
    amendment: PlanAmendment,
) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
    let call = ctx
        .service_client::<ExecutionClient>()
        .apply_amendment(Json::from(ExecutionAmendmentRequest {
            run: ExecutionRunRequest {
                tenant_id: target.tenant_id,
                contact_id: target.contact_id,
                session_id: target.session_id,
                run_uid: target.run_uid,
            },
            expected_plan_revision: target.base_plan_revision,
            amendment,
        }));
    crate::restate_identity::with_identity_headers(call, identity)
        .call()
        .await
        .map_err(HandlerError::from)
}

/// Builds the deterministic empty amendment identifying one planner-authored stop.
///
/// `request_replan_stop` keys replay on the amendment hash, so a stop that carries no
/// candidate still needs a stable identity derived only from its own frozen evidence.
fn planner_stop_amendment(
    origin: AmendmentPlanningOrigin,
    reason: ReplanStopReason,
    description: &str,
) -> PlanAmendment {
    PlanAmendment {
        base_plan_revision: origin.base_plan_revision,
        reason: description.to_string(),
        evidence: json!({
            "source": "amendment_planner",
            "stop_reason": reason.as_str(),
            "origin_task_id": origin.task_id,
        }),
        operations: Vec::new(),
    }
}

/// Returns the exact planning target for a run parked in `WaitingReplan`.
///
/// Nothing else may reach the planner: a run holding a terminal intent or awaiting manual
/// repair is on its way out, and re-planning it would race the settlement that owns it.
fn waiting_replan_target(run: &ExecutionRunRecord) -> Option<AmendmentPlanningTarget> {
    parked_run_needs_amendment(
        run.status,
        run.pending_terminal.is_some(),
        run.manual_repair_required,
        run.waiting_replan_task_count,
    )
    .then_some(AmendmentPlanningTarget {
        tenant_id: run.tenant_id,
        contact_id: run.contact_id,
        session_id: run.session_id,
        run_uid: run.run_uid,
        base_plan_revision: run.plan_revision,
    })
}

/// Decides whether one parked run is a legitimate amendment-planning candidate.
const fn parked_run_needs_amendment(
    status: ExecutionRunStatus,
    has_pending_terminal: bool,
    manual_repair_required: bool,
    waiting_replan_task_count: u64,
) -> bool {
    matches!(status, ExecutionRunStatus::WaitingReplan)
        && !has_pending_terminal
        && !manual_repair_required
        // The compiler admits an amendment that supersedes exactly one WaitingReplan node, so a
        // run holding any other count cannot be repaired by this path.
        && waiting_replan_task_count == 1
}

/// Returns the stable Restate identity of the sole planning slice for one run revision.
fn amendment_planning_identity(run_uid: uuid::Uuid, base_plan_revision: u64) -> String {
    format!("moa:execution-amendment-plan:v1:{run_uid}:{base_plan_revision}")
}

/// Selects the bounded planning slice for one controller activation that parked a run.
///
/// This is the controller's only participation in self-healing: it performs one indexed
/// read and one durable send, and never waits for the planner. If the run is not parked in
/// `WaitingReplan` the read is the whole cost.
pub(crate) async fn dispatch_parked_replan_planning(
    ctx: &ObjectContext<'_>,
    repository: &ExecutionRepository,
    tenant_id: moa_core::types::identifiers::TenantId,
    run_uid: uuid::Uuid,
    controller_generation: u64,
    wake_epoch: u64,
) -> Result<(), HandlerError> {
    if automatic_amendment_planner_paused() {
        return Ok(());
    }
    let repository = repository.clone();
    let target = ctx
        .run(|| {
            let repository = repository.clone();
            async move {
                let run = repository
                    .load_run(ExecutionScope::ControlPlane, run_uid)
                    .await
                    .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
                let Some(run) = run else {
                    return Ok(Json::from(None));
                };
                if run.tenant_id != tenant_id {
                    return Err(TerminalError::new_with_code(
                        409,
                        "amendment planning owner does not match activation",
                    )
                    .into());
                }
                Ok::<_, HandlerError>(Json::from(waiting_replan_target(&run)))
            }
        })
        .name(format!(
            "execution_amendment_planning_select_{controller_generation}_{wake_epoch}"
        ))
        .await?
        .into_inner();
    let Some(target) = target else {
        return Ok(());
    };
    let handle = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionAmendmentPlannerClient>()
            .plan(Json::from(target))
            .idempotency_key(amendment_planning_identity(
                target.run_uid,
                target.base_plan_revision,
            )),
    )
    .send();
    let _planning_invocation_id = handle.invocation_id().await?;
    Ok(())
}

/// Restate-journaled planner provider that routes every model call through the gateway.
struct RestateAmendmentPlannerProvider<'a, 'ctx> {
    ctx: &'a Context<'ctx>,
    repository: ExecutionRepository,
    scope: ExecutionScope,
    config: &'a ExecutionConfig,
    deadline_at: Option<chrono::DateTime<chrono::Utc>>,
    run_uid: uuid::Uuid,
    plan_revision: u64,
    next_attempt: AtomicUsize,
}

#[async_trait]
impl LLMProvider for RestateAmendmentPlannerProvider<'_, '_> {
    fn name(&self) -> &'static str {
        "restate-llm-gateway"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        request: SharedCompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let request = CompletionRequest::from_view(&request);
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        let ordinal = u8::try_from(attempt).map_err(|_| {
            moa_core::error::MoaError::ValidationError(
                "amendment planner call ordinal exceeds u8".to_string(),
            )
        })?;
        let estimate = amendment_planning_call_estimate(&request, self.config)?;
        let repository = self.repository.clone();
        let scope = self.scope;
        let run_uid = self.run_uid;
        let plan_revision = self.plan_revision;
        let reservation = self
            .ctx
            .run(|| {
                let repository = repository.clone();
                async move {
                    let outcome = repository
                        .reserve_amendment_planning_call(
                            scope,
                            AmendmentPlanningCallReservationRequest {
                                run_uid,
                                base_plan_revision: plan_revision,
                                call_ordinal: ordinal,
                                reservation: estimate,
                                now: chrono::Utc::now(),
                            },
                        )
                        .await
                        .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
                    Ok::<_, HandlerError>(Json::from(JournaledPlanningReservation::from(&outcome)))
                }
            })
            .name(format!(
                "execution_amendment_reserve_{}_{}_{}",
                self.run_uid, self.plan_revision, attempt
            ))
            .await
            .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?
            .into_inner();
        match reservation {
            JournaledPlanningReservation::Proceed => {}
            JournaledPlanningReservation::Denied => {
                return Err(moa_core::error::MoaError::BudgetExhausted(
                    "automatic amendment planning exhausted the approved run budget".to_string(),
                ));
            }
            JournaledPlanningReservation::Conflict | JournaledPlanningReservation::NotFound => {
                return Err(moa_core::error::MoaError::ProviderError(
                    "automatic amendment planning reservation conflicted with durable state"
                        .to_string(),
                ));
            }
        }

        let response = crate::restate_identity::replay_safe_request(
            self.ctx
                .service_client::<LLMGatewayClient>()
                .complete_bounded(Json::from(BoundedCompletionRequest {
                    request,
                    budget: ResourceBudget::new(
                        self.deadline_at,
                        Some(ResourceAmounts {
                            cost_micro_usd: estimate.cost_microusd,
                            tokens: estimate.tokens,
                            turns: 0,
                            model_calls: 1,
                            tool_calls: 0,
                        }),
                    ),
                }))
                .idempotency_key(completion_idempotency_key(
                    self.ctx.invocation_id(),
                    LLMCompletionAction::ExecutionAmendment {
                        run_uid: self.run_uid,
                        plan_revision: self.plan_revision,
                        attempt,
                    },
                )),
        )
        .call()
        .await;
        let response = match response {
            Ok(response) => response.into_inner(),
            Err(error) => {
                self.reconcile_call(ordinal, attempt, PlanningUsage::default())
                    .await?;
                return Err(moa_core::error::MoaError::ProviderError(error.to_string()));
            }
        };
        let actual = amendment_planning_call_usage(&response)?;
        self.reconcile_call(ordinal, attempt, actual).await?;
        Ok(CompletionStream::from_response(response))
    }
}

impl RestateAmendmentPlannerProvider<'_, '_> {
    async fn reconcile_call(
        &self,
        ordinal: u8,
        attempt: usize,
        actual: PlanningUsage,
    ) -> moa_core::error::Result<()> {
        let repository = self.repository.clone();
        let scope = self.scope;
        let run_uid = self.run_uid;
        let plan_revision = self.plan_revision;
        let reconcile = self
            .ctx
            .run(|| {
                let repository = repository.clone();
                async move {
                    let outcome = repository
                        .reconcile_amendment_planning_call(
                            scope,
                            AmendmentPlanningCallReconcileRequest {
                                run_uid,
                                base_plan_revision: plan_revision,
                                call_ordinal: ordinal,
                                actual,
                                settled_at: chrono::Utc::now(),
                            },
                        )
                        .await
                        .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
                    Ok::<_, HandlerError>(Json::from(JournaledPlanningReconcile::from(&outcome)))
                }
            })
            .name(format!(
                "execution_amendment_reconcile_{}_{}_{}",
                self.run_uid, self.plan_revision, attempt
            ))
            .await
            .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?
            .into_inner();
        if reconcile != JournaledPlanningReconcile::Settled {
            return Err(moa_core::error::MoaError::ProviderError(
                "automatic amendment planning reconciliation conflicted with durable state"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledPlanningReservation {
    Proceed,
    Denied,
    Conflict,
    NotFound,
}

impl From<&AmendmentPlanningCallReservationOutcome> for JournaledPlanningReservation {
    fn from(value: &AmendmentPlanningCallReservationOutcome) -> Self {
        match value {
            AmendmentPlanningCallReservationOutcome::Granted(_)
            | AmendmentPlanningCallReservationOutcome::ReplayedOpen(_)
            | AmendmentPlanningCallReservationOutcome::AlreadySettled(_) => Self::Proceed,
            AmendmentPlanningCallReservationOutcome::Denied(_) => Self::Denied,
            AmendmentPlanningCallReservationOutcome::Conflict => Self::Conflict,
            AmendmentPlanningCallReservationOutcome::NotFound => Self::NotFound,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum JournaledPlanningReconcile {
    Settled,
    Conflict,
    NotFound,
}

impl From<&AmendmentPlanningCallReconcileOutcome> for JournaledPlanningReconcile {
    fn from(value: &AmendmentPlanningCallReconcileOutcome) -> Self {
        match value {
            AmendmentPlanningCallReconcileOutcome::Applied(_)
            | AmendmentPlanningCallReconcileOutcome::Replayed(_) => Self::Settled,
            AmendmentPlanningCallReconcileOutcome::Conflict => Self::Conflict,
            AmendmentPlanningCallReconcileOutcome::NotFound => Self::NotFound,
        }
    }
}

fn amendment_planning_call_estimate(
    request: &CompletionRequest,
    config: &ExecutionConfig,
) -> moa_core::error::Result<AmendmentPlanningCallReservation> {
    // The shared estimator sizes the provider-visible messages and tools. A twofold headroom keeps
    // this authorization conservative across model tokenizers without charging DTO serialization
    // syntax that the provider never sees.
    let input_token_count = moa_core::types::context::sum_message_tokens(&request.messages)
        .saturating_add(
            request
                .tools
                .iter()
                .map(|tool| moa_core::types::context::estimate_text_tokens(&tool.to_string()))
                .sum(),
        )
        .saturating_mul(2);
    let input_tokens = u64::try_from(input_token_count).map_err(|_| {
        moa_core::error::MoaError::ValidationError(
            "amendment planner request token estimate exceeds u64".to_string(),
        )
    })?;
    let output_tokens =
        u64::try_from(request.max_output_tokens.unwrap_or_default()).map_err(|_| {
            moa_core::error::MoaError::ValidationError(
                "amendment planner output limit exceeds u64".to_string(),
            )
        })?;
    let tokens = input_tokens.checked_add(output_tokens).ok_or_else(|| {
        moa_core::error::MoaError::ValidationError(
            "amendment planner token reservation overflow".to_string(),
        )
    })?;
    let priced_cost = request
        .model
        .as_ref()
        .and_then(|model| moa_providers::pricing_for_model(model.as_str()))
        .map(|pricing| {
            let input_rate = pricing
                .input_per_mtok
                .max(pricing.cached_input_per_mtok.unwrap_or_default())
                .max(pricing.cache_write_per_mtok());
            let dollars = input_tokens as f64 / 1_000_000.0 * input_rate
                + output_tokens as f64 / 1_000_000.0 * pricing.output_per_mtok;
            moa_core::types::resource::ResourceAmounts::cost_micro_usd_from_dollars(dollars)
                .unwrap_or(u64::MAX)
        })
        .unwrap_or_default();
    Ok(AmendmentPlanningCallReservation {
        cost_microusd: priced_cost.max(config.agent_turn_cost_microusd),
        tokens,
    })
}

fn amendment_planning_call_usage(
    response: &moa_core::types::completion::CompletionResponse,
) -> moa_core::error::Result<PlanningUsage> {
    let input = u64::try_from(response.usage.total_input_tokens()).map_err(|_| {
        moa_core::error::MoaError::ValidationError(
            "amendment planner input usage exceeds u64".to_string(),
        )
    })?;
    let output = u64::try_from(response.usage.output_tokens).map_err(|_| {
        moa_core::error::MoaError::ValidationError(
            "amendment planner output usage exceeds u64".to_string(),
        )
    })?;
    Ok(PlanningUsage {
        cost_microusd: moa_providers::pricing_for_model(response.model.as_str())
            .map(|pricing| pricing.cost_micros(&response.usage))
            .unwrap_or_default(),
        tokens: input.saturating_add(output),
    })
}

#[cfg(feature = "integration")]
fn automatic_amendment_planner_paused() -> bool {
    std::env::var("MOA_EXECUTION_TEST_PAUSE_AMENDMENT_PLANNER").as_deref() == Ok("true")
}

#[cfg(not(feature = "integration"))]
const fn automatic_amendment_planner_paused() -> bool {
    false
}
