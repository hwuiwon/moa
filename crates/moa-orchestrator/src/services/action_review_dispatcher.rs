//! Restate-owned delivery for execution action-review resolutions.

use chrono::Utc;
use moa_config::ExecutionConfig;
use moa_execution::repository::compensation::CompensationReviewResolutionOutcome;
use moa_execution::repository::task::{
    ResolveTaskAttemptReviewRequest, TaskAttemptReviewResolutionOutcome,
};
use moa_execution::repository::{ExecutionRepository, ExecutionScope};
use moa_execution::wire::{
    ExecutionActionReviewResolutionRequest, ExecutionCompensationReviewResolutionRequest,
};
use moa_observability::propagation::{ValidatedTraceContext, link_validated_context};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::action_reviews::store as action_review_store;
use crate::services::execution_dispatcher::{DispatchExecutionsRequest, ExecutionDispatcherClient};
use crate::workflows::errors::sqlx_error_to_handler_error;

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

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum StorageResolutionDisposition {
    Delivered,
    NotReady,
    Failed { message: String },
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
    config: ExecutionConfig,
}

impl ActionReviewDispatcherImpl {
    /// Creates a dispatcher over the shared control-plane pool.
    #[must_use]
    pub fn new(pool: PgPool, config: ExecutionConfig) -> Self {
        Self { pool, config }
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
            deliver_one(&ctx, &self.pool, &self.config, delivery).await?;
        }

        Ok(Json(DispatchActionReviewsResponse { claimed }))
    }
}

async fn deliver_one(
    ctx: &Context<'_>,
    pool: &PgPool,
    config: &ExecutionConfig,
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
    if let Some(context) = resolution_context.as_ref() {
        let _ = link_validated_context(&tracing::Span::current(), context);
    }
    if let Some(context) = task_context.as_ref() {
        let _ = link_validated_context(&tracing::Span::current(), context);
    }
    let acknowledged = match delivery.request {
        JournaledExecutionReviewRequest::Task(request) => {
            let repository = ExecutionRepository::new(pool.clone());
            let disposition = ctx
                .run(|| async move {
                    let disposition = match repository
                        .resolve_task_attempt_review(
                            config,
                            ResolveTaskAttemptReviewRequest {
                                scope: ExecutionScope::ControlPlane,
                                run_uid: request.run_uid,
                                task_id: request.task_id,
                                expected_task_generation: request.generation,
                                review_uid: request.review_uid,
                                resolution: request.resolution,
                                resolved_at: Utc::now(),
                            },
                        )
                        .await
                    {
                        Ok(
                            TaskAttemptReviewResolutionOutcome::Applied { .. }
                            | TaskAttemptReviewResolutionOutcome::Replayed { .. }
                            | TaskAttemptReviewResolutionOutcome::NotFound
                            | TaskAttemptReviewResolutionOutcome::Stale,
                        ) => StorageResolutionDisposition::Delivered,
                        Ok(TaskAttemptReviewResolutionOutcome::NotReady) => {
                            StorageResolutionDisposition::NotReady
                        }
                        Err(error) => StorageResolutionDisposition::Failed {
                            message: error.to_string(),
                        },
                    };
                    Ok::<_, HandlerError>(Json::from(disposition))
                })
                .name(format!(
                    "resolve_task_attempt_review_{}",
                    delivery.review_uid
                ))
                .await?
                .into_inner();
            match disposition {
                StorageResolutionDisposition::Delivered => Ok(()),
                StorageResolutionDisposition::NotReady => {
                    Err("execution task review owner is not durably parked".to_string())
                }
                StorageResolutionDisposition::Failed { message } => Err(message),
            }
        }
        JournaledExecutionReviewRequest::Compensation(request) => {
            let repository = ExecutionRepository::new(pool.clone());
            let disposition = ctx
                .run(|| async move {
                    let disposition = match repository
                        .resolve_current_compensation_review(
                            ExecutionScope::ControlPlane,
                            request.run_uid,
                            request.compensation_id,
                            request.generation,
                            request.review_uid,
                            &request.resolution,
                            Utc::now(),
                        )
                        .await
                    {
                        Ok(
                            CompensationReviewResolutionOutcome::Applied { .. }
                            | CompensationReviewResolutionOutcome::Replayed { .. }
                            | CompensationReviewResolutionOutcome::NotFound
                            | CompensationReviewResolutionOutcome::Stale,
                        ) => StorageResolutionDisposition::Delivered,
                        Ok(CompensationReviewResolutionOutcome::NotReady) => {
                            StorageResolutionDisposition::NotReady
                        }
                        Err(error) => StorageResolutionDisposition::Failed {
                            message: error.to_string(),
                        },
                    };
                    Ok::<_, HandlerError>(Json::from(disposition))
                })
                .name(format!(
                    "resolve_compensation_attempt_review_{}",
                    delivery.review_uid
                ))
                .await?
                .into_inner();
            match disposition {
                StorageResolutionDisposition::Delivered => Ok(()),
                StorageResolutionDisposition::NotReady => {
                    Err("execution compensation review owner is not durably parked".to_string())
                }
                StorageResolutionDisposition::Failed { message } => Err(message),
            }
        }
    };

    match acknowledged {
        Ok(()) => {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<ExecutionDispatcherClient>()
                    .dispatch(Json::from(DispatchExecutionsRequest::default()))
                    .idempotency_key(format!("execution-action-review:{}", delivery.review_uid)),
            )
            .send();
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
