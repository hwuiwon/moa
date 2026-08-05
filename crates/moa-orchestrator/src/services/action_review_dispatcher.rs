//! Restate-owned delivery for execution action-review resolutions.

use moa_execution::wire::{
    ExecutionActionReviewAcknowledgement, ExecutionActionReviewResolutionRequest,
    ExecutionCompensationReviewAcknowledgement, ExecutionCompensationReviewResolutionRequest,
};
use moa_observability::propagation::{
    TRACE_LINK_TRACEPARENT_HEADER, TRACE_LINK_TRACESTATE_HEADER, ValidatedTraceContext,
    with_validated_trace_headers,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::action_reviews::store as action_review_store;
use crate::workflows::{
    errors::sqlx_error_to_handler_error, execution_compensation::ExecutionCompensationClient,
    execution_task::ExecutionTaskClient,
};

const EXECUTION_REVIEW_DISPATCH_BATCH_SIZE: i64 = 32;

/// Empty request used by the process-level timeout sweep to wake durable delivery.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DispatchActionReviewsRequest {}

/// Summary of one bounded dispatcher pass.
#[derive(Debug, Deserialize, Serialize)]
pub struct DispatchActionReviewsResponse {
    /// Number of outbox rows claimed by this pass.
    pub claimed: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct JournaledExecutionReviewDelivery {
    review_uid: Uuid,
    request: JournaledExecutionReviewRequest,
    attempt_count: i32,
    resolution_traceparent: Option<String>,
    resolution_tracestate: Option<String>,
    task_traceparent: Option<String>,
    task_tracestate: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "owner",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum JournaledExecutionReviewRequest {
    Task(ExecutionActionReviewResolutionRequest),
    Compensation(ExecutionCompensationReviewResolutionRequest),
}

/// Restate service that owns private workflow delivery for action-review outbox rows.
#[restate_sdk::service]
#[name = "ActionReviewDispatcher"]
pub trait ActionReviewDispatcher {
    /// Claims and delivers one bounded outbox batch.
    async fn dispatch(
        request: Json<DispatchActionReviewsRequest>,
    ) -> Result<Json<DispatchActionReviewsResponse>, HandlerError>;
}

/// PostgreSQL-backed action-review dispatcher.
#[derive(Clone)]
pub struct ActionReviewDispatcherImpl {
    pool: PgPool,
}

impl ActionReviewDispatcherImpl {
    /// Creates a dispatcher over the shared control-plane pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ActionReviewDispatcher for ActionReviewDispatcherImpl {
    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: this is an operational wake-up surface; it accepts no caller-owned data and only drains already-authorized durable outbox rows.
    async fn dispatch(
        &self,
        ctx: Context<'_>,
        _request: Json<DispatchActionReviewsRequest>,
    ) -> Result<Json<DispatchActionReviewsResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        let pool = self.pool.clone();
        let deliveries = ctx
            .run(|| async move {
                let claimed = action_review_store::claim_execution_review_resolutions(
                    &pool,
                    EXECUTION_REVIEW_DISPATCH_BATCH_SIZE,
                )
                .await
                .map_err(sqlx_error_to_handler_error)?;
                Ok(Json(
                    claimed
                        .into_iter()
                        .map(|delivery| JournaledExecutionReviewDelivery {
                            review_uid: delivery.review_uid,
                            request: match delivery.request {
                                action_review_store::ClaimedExecutionReviewRequest::Task(request) => {
                                    JournaledExecutionReviewRequest::Task(request)
                                }
                                action_review_store::ClaimedExecutionReviewRequest::Compensation(
                                    request,
                                ) => JournaledExecutionReviewRequest::Compensation(request),
                            },
                            attempt_count: delivery.attempt_count,
                            resolution_traceparent: delivery
                                .resolution_trace_context
                                .as_ref()
                                .map(|context| context.traceparent().to_string()),
                            resolution_tracestate: delivery
                                .resolution_trace_context
                                .as_ref()
                                .and_then(ValidatedTraceContext::tracestate)
                                .map(str::to_string),
                            task_traceparent: delivery
                                .task_trace_context
                                .as_ref()
                                .map(|context| context.traceparent().to_string()),
                            task_tracestate: delivery
                                .task_trace_context
                                .as_ref()
                                .and_then(ValidatedTraceContext::tracestate)
                                .map(str::to_string),
                        })
                        .collect::<Vec<_>>(),
                ))
            })
            .name("action_review_dispatch_claim")
            .await?
            .into_inner();
        let claimed = deliveries.len();

        for delivery in deliveries {
            deliver_one(&ctx, &self.pool, delivery).await?;
        }

        Ok(Json(DispatchActionReviewsResponse { claimed }))
    }
}

async fn deliver_one(
    ctx: &Context<'_>,
    pool: &PgPool,
    delivery: JournaledExecutionReviewDelivery,
) -> Result<(), HandlerError> {
    let resolution_context = ValidatedTraceContext::new(
        delivery.resolution_traceparent.as_deref(),
        delivery.resolution_tracestate.as_deref(),
    );
    let task_context = ValidatedTraceContext::new(
        delivery.task_traceparent.as_deref(),
        delivery.task_tracestate.as_deref(),
    );
    macro_rules! with_delivery_trace_headers {
        ($request:expr) => {{
            let request = with_validated_trace_headers(
                $request,
                resolution_context.as_ref(),
                |request, name, value| request.header(name, value),
            );
            match task_context.as_ref() {
                Some(context) => {
                    let request = request.header(
                        TRACE_LINK_TRACEPARENT_HEADER.to_string(),
                        context.traceparent().to_string(),
                    );
                    match context.tracestate() {
                        Some(tracestate) => request.header(
                            TRACE_LINK_TRACESTATE_HEADER.to_string(),
                            tracestate.to_string(),
                        ),
                        None => request,
                    }
                }
                None => request,
            }
        }};
    }
    let acknowledged = match delivery.request {
        JournaledExecutionReviewRequest::Task(request) => {
            let request = ctx
                .workflow_client::<ExecutionTaskClient>(request.task_id.to_string())
                .resolve_action_review(Json(request))
                .idempotency_key(delivery.review_uid.to_string());
            let request = with_delivery_trace_headers!(request);
            crate::restate_identity::replay_safe_request(request)
                .call()
                .await
                .map(|Json(acknowledgement)| match acknowledgement {
                    ExecutionActionReviewAcknowledgement::Applied
                    | ExecutionActionReviewAcknowledgement::Replayed
                    | ExecutionActionReviewAcknowledgement::AuditedStale => {}
                })
        }
        JournaledExecutionReviewRequest::Compensation(request) => {
            let request = ctx
                .workflow_client::<ExecutionCompensationClient>(request.compensation_id.to_string())
                .resolve_action_review(Json(request))
                .idempotency_key(delivery.review_uid.to_string());
            let request = with_delivery_trace_headers!(request);
            crate::restate_identity::replay_safe_request(request)
                .call()
                .await
                .map(|Json(acknowledgement)| match acknowledgement {
                    ExecutionCompensationReviewAcknowledgement::Applied
                    | ExecutionCompensationReviewAcknowledgement::Replayed
                    | ExecutionCompensationReviewAcknowledgement::AuditedStale => {}
                })
        }
    };

    match acknowledged {
        Ok(()) => {
            let pool = pool.clone();
            let review_uid = delivery.review_uid;
            let attempt_count = delivery.attempt_count;
            let marked = ctx
                .run(|| async move {
                    action_review_store::mark_execution_review_delivered(
                        &pool,
                        review_uid,
                        attempt_count,
                    )
                    .await
                    .map(Json::from)
                    .map_err(sqlx_error_to_handler_error)
                })
                .name("action_review_dispatch_acknowledge")
                .await?
                .into_inner();
            if !marked {
                tracing::warn!(
                    review_uid = %delivery.review_uid,
                    attempt = delivery.attempt_count,
                    "execution review acknowledgement lost its outbox claim fence"
                );
            }
        }
        Err(error) => {
            let error = error.to_string();
            let pool = pool.clone();
            let review_uid = delivery.review_uid;
            let attempt_count = delivery.attempt_count;
            ctx.run(|| async move {
                action_review_store::mark_execution_review_failed(
                    &pool,
                    review_uid,
                    attempt_count,
                    &error,
                )
                .await
                .map(Json::from)
                .map_err(sqlx_error_to_handler_error)
            })
            .name("action_review_dispatch_reschedule")
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use moa_execution::{
        state::CompensationId,
        wire::{ExecutionActionReviewResolution, ExecutionCompensationReviewResolutionRequest},
    };
    use uuid::Uuid;

    use super::JournaledExecutionReviewRequest;

    #[test]
    fn journaled_compensation_delivery_preserves_exact_workflow_coordinates() {
        // Pins: replay of the dispatcher's Restate journal targets the same
        // compensation generation and cannot be decoded as a forward task.
        let request = JournaledExecutionReviewRequest::Compensation(
            ExecutionCompensationReviewResolutionRequest {
                run_uid: Uuid::from_u128(1),
                compensation_id: CompensationId::from_uuid(Uuid::from_u128(2)),
                generation: 7,
                review_uid: Uuid::from_u128(3),
                resolution: ExecutionActionReviewResolution::TimedOut {
                    reason: "review expired".to_string(),
                },
            },
        );
        let encoded = serde_json::to_value(&request)
            .expect("compensation dispatcher request should serialize");
        let decoded = serde_json::from_value::<JournaledExecutionReviewRequest>(encoded)
            .expect("compensation dispatcher request should deserialize");

        let JournaledExecutionReviewRequest::Compensation(decoded) = decoded else {
            panic!("compensation request must not decode as a forward task");
        };
        assert_eq!(decoded.run_uid, Uuid::from_u128(1));
        assert_eq!(
            decoded.compensation_id,
            CompensationId::from_uuid(Uuid::from_u128(2))
        );
        assert_eq!(decoded.generation, 7);
        assert_eq!(decoded.review_uid, Uuid::from_u128(3));
    }
}
