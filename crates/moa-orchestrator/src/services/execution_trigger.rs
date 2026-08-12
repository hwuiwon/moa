//! Restate delivery for exact, generation-fenced execution triggers.

use std::time::Duration;

use moa_execution::repository::{
    ExecutionRepository, ExecutionScope, TransitionOutcome,
    terminal::PendingTerminalAdvanceOutcome,
    trigger::{
        ExecutionExternalReconcileTriggerOutcome, ExecutionExternalStartRecoveryTriggerOutcome,
        ExecutionRunDeadlineTriggerOutcome, ExecutionTriggerFireOutcome, ExecutionTriggerKind,
        ExecutionTriggerNoOp, ExecutionWatchdogTriggerOutcome,
    },
};
use moa_execution::wire::{
    ExecutionAttemptWatchdogResponseOutcome, ExecutionCompensationAttemptWatchdogRequest,
    ExecutionExternalJobReconcileRequest, ExecutionExternalJobReconcileResponseOutcome,
    ExecutionExternalJobStartRecoveryRequest, ExecutionExternalJobStartRecoveryResponseOutcome,
    ExecutionTaskAttemptWatchdogRequest,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    runtime::execution_dispatch::ExecutionTriggerDeliveryRequest,
    services::execution_schedule::{ExecutionScheduleClient, ExecutionScheduleFireResponse},
    services::tool_executor::ToolExecutorClient,
    workflows::{
        errors::execution_error_to_handler_error,
        execution_compensation_attempt::ExecutionCompensationAttemptClient,
        execution_task_attempt::ExecutionTaskAttemptClient,
    },
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum ExternalReconcileRoute {
    Ready {
        request: ExecutionExternalJobReconcileRequest,
    },
    NoOp {
        response: ExecutionTriggerFireResponse,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum ExternalStartRecoveryRoute {
    Ready {
        request: Box<ExecutionExternalJobStartRecoveryRequest>,
    },
    NoOp {
        response: ExecutionTriggerFireResponse,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum RunDeadlineRoute {
    Fenced {
        response: ExecutionTriggerFireResponse,
    },
    NoOp {
        response: ExecutionTriggerFireResponse,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("run deadline fence changed while its trigger delivery was in flight")]
struct RunDeadlineFenceRace;

#[derive(Debug, thiserror::Error)]
#[error("run deadline delivery arrived before its canonical database due time")]
struct RunDeadlineNotDue;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum WatchdogRoute {
    Task {
        request: ExecutionTaskAttemptWatchdogRequest,
    },
    Compensation {
        request: ExecutionCompensationAttemptWatchdogRequest,
    },
    NoOp {
        response: ExecutionTriggerFireResponse,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("watchdog receiver remains current after requesting redelivery")]
struct WatchdogReceiverStillCurrent;

/// Successful disposition of one immutable trigger delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExecutionTriggerFireResponse {
    /// Canonical trigger state advanced and may have enqueued a run activation.
    Delivered {
        /// Same-transaction run-activation dispatch, when the trigger owns a run.
        activation_dispatch_uid: Option<Uuid>,
    },
    /// No visible trigger has the supplied immutable identity.
    NotFound,
    /// The same trigger was already delivered.
    Duplicate,
    /// Cancellation or supersession fenced the trigger.
    Inactive,
    /// A newer run, task, compensation, or schedule incarnation won.
    StaleGeneration,
    /// Delivery arrived before the canonical absolute due time.
    NotDue,
}

/// Restate service that owns temporal-trigger delivery.
#[restate_sdk::service]
#[name = "ExecutionTrigger"]
pub trait ExecutionTrigger {
    /// Fires one exact trigger after reloading its canonical database state.
    async fn fire(
        request: Json<ExecutionTriggerDeliveryRequest>,
    ) -> Result<Json<ExecutionTriggerFireResponse>, HandlerError>;
}

/// PostgreSQL-backed exact trigger delivery.
#[derive(Clone)]
pub struct ExecutionTriggerImpl {
    repository: ExecutionRepository,
    config: moa_config::ExecutionConfig,
}

impl ExecutionTriggerImpl {
    /// Creates a trigger service over the shared execution repository.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: &moa_config::ExecutionConfig) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            config: config.clone(),
        }
    }
}

impl ExecutionTrigger for ExecutionTriggerImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(
        dispatch_uid = %request.0.dispatch_uid,
        trigger_uid = %request.0.trigger_uid,
    ))]
    // SAFETY: ingress-private outbox delivery; the handler reloads the exact tenant-scoped trigger and every persisted generation fence.
    async fn fire(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionTriggerDeliveryRequest>,
    ) -> Result<Json<ExecutionTriggerFireResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionTrigger",
            "fire",
        );
        let request = request.into_inner();
        let delivery_dispatch_uid = request.dispatch_uid;
        let trigger_uid = request.trigger_uid;
        let tenant_id = request.tenant_id;
        let repository = self.repository.clone();
        let trigger_kind = ctx
            .run(|| async move {
                repository
                    .load_trigger(ExecutionScope::Tenant { tenant_id }, trigger_uid)
                    .await
                    .map(|trigger| {
                        Json::from(trigger.map(|trigger| trigger.kind.as_str().to_string()))
                    })
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_trigger_route_{trigger_uid}"))
            .await?
            .into_inner();
        if trigger_kind.is_none() {
            return Ok(Json::from(ExecutionTriggerFireResponse::NotFound));
        }
        if trigger_kind.as_deref() == Some(ExecutionTriggerKind::ScheduleOccurrence.as_str()) {
            let response = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ExecutionScheduleClient>()
                    .fire_occurrence(Json::from(request.clone()))
                    .idempotency_key(request.dispatch_uid.to_string()),
            )
            .call()
            .await?
            .into_inner();
            let response = schedule_trigger_response(response);
            return Ok(Json::from(response));
        }
        if trigger_kind.as_deref() == Some(ExecutionTriggerKind::RunDeadline.as_str()) {
            let repository = self.repository.clone();
            let config = self.config.clone();
            let page_limit =
                u32::try_from(self.config.maximum_activation_steps).unwrap_or(u32::MAX);
            let route = ctx
                .run(move || async move {
                    let prepared = repository
                        .prepare_run_deadline_trigger(
                            ExecutionScope::Tenant { tenant_id },
                            trigger_uid,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?;
                    let route = match prepared {
                        ExecutionRunDeadlineTriggerOutcome::Ready {
                            run_uid,
                            controller_generation,
                            wake_epoch,
                            observed_at,
                        } => {
                            let outcome = repository
                                .fence_deadline_and_enqueue_settlement(
                                    &config,
                                    ExecutionScope::Tenant { tenant_id },
                                    run_uid,
                                    controller_generation,
                                    wake_epoch,
                                    observed_at,
                                    page_limit,
                                )
                                .await
                                .map_err(execution_error_to_handler_error)?;
                            run_deadline_fence_route(outcome).map_err(HandlerError::from)?
                        }
                        ExecutionRunDeadlineTriggerOutcome::NoOp(reason) => {
                            run_deadline_noop_route(reason).map_err(HandlerError::from)?
                        }
                    };
                    Ok::<_, HandlerError>(Json::from(route))
                })
                .name(format!("execution_run_deadline_fence_{trigger_uid}"))
                .retry_policy(run_deadline_retry_policy())
                .await?
                .into_inner();
            let response = match route {
                RunDeadlineRoute::Fenced { response } => response,
                RunDeadlineRoute::NoOp { response } => return Ok(Json::from(response)),
            };
            let repository = self.repository.clone();
            ctx.run(|| async move {
                repository
                    .settle_run_deadline_trigger(ExecutionScope::Tenant { tenant_id }, trigger_uid)
                    .await
                    .map(|_| Json::from(()))
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_run_deadline_settle_{trigger_uid}"))
            .await?;
            return Ok(Json::from(response));
        }
        if matches!(
            trigger_kind.as_deref(),
            Some(kind)
                if kind == ExecutionTriggerKind::TaskTimer.as_str()
                    || kind == ExecutionTriggerKind::WaitExpiry.as_str()
        ) {
            // These deliveries are accepted synchronously by ExecutionDispatcher. Their commit is
            // visible when that owning dispatcher selects its next outbox head; another kick here
            // would only create duplicate, unkeyed dispatcher invocations.
            let repository = self.repository.clone();
            let config = self.config.clone();
            let response = ctx
                .run(|| async move {
                    repository
                        .fire_wait_trigger(
                            ExecutionScope::Tenant {
                                tenant_id: request.tenant_id,
                            },
                            &config,
                            request.trigger_uid,
                        )
                        .await
                        .map(wait_trigger_response)
                        .map(Json::from)
                        .map_err(execution_error_to_handler_error)
                })
                .name(format!("execution_wait_trigger_fire_{trigger_uid}"))
                .await?;
            return Ok(response);
        }
        if matches!(
            trigger_kind.as_deref(),
            Some(kind)
                if kind == ExecutionTriggerKind::TaskWatchdog.as_str()
                    || kind == ExecutionTriggerKind::CompensationWatchdog.as_str()
        ) {
            let repository = self.repository.clone();
            let prepared = ctx
                .run(|| async move {
                    let outcome = repository
                        .prepare_watchdog_trigger(ExecutionScope::Tenant { tenant_id }, trigger_uid)
                        .await
                        .map_err(execution_error_to_handler_error)?;
                    let route = watchdog_route(outcome);
                    Ok::<_, HandlerError>(Json::from(route))
                })
                .name(format!("execution_watchdog_prepare_{trigger_uid}"))
                .await?
                .into_inner();
            let watchdog_outcome = match prepared {
                WatchdogRoute::Task { request } => {
                    crate::restate_identity::replay_safe_request(
                        ctx.workflow_client::<ExecutionTaskAttemptClient>(
                            request.dispatch_uid.to_string(),
                        )
                        .watchdog(Json::from(request))
                        .idempotency_key(delivery_dispatch_uid.to_string()),
                    )
                    .call()
                    .await?
                    .into_inner()
                    .outcome
                }
                WatchdogRoute::Compensation { request } => {
                    crate::restate_identity::replay_safe_request(
                        ctx.workflow_client::<ExecutionCompensationAttemptClient>(
                            request.dispatch_uid.to_string(),
                        )
                        .watchdog(Json::from(request))
                        .idempotency_key(delivery_dispatch_uid.to_string()),
                    )
                    .call()
                    .await?
                    .into_inner()
                    .outcome
                }
                WatchdogRoute::NoOp { response } => return Ok(Json::from(response)),
            };
            if watchdog_outcome == ExecutionAttemptWatchdogResponseOutcome::RetryDelivery {
                // The receiver can race a different durable owner transition. Revalidate after
                // its response so a watchdog superseded by that transition completes as a stale
                // delivery instead of blocking the fleet-serialized drain behind endless retry.
                let repository = self.repository.clone();
                let response = ctx
                    .run(|| async move {
                        let refreshed = repository
                            .prepare_watchdog_trigger(
                                ExecutionScope::Tenant { tenant_id },
                                trigger_uid,
                            )
                            .await
                            .map(watchdog_route)
                            .map_err(execution_error_to_handler_error)?;
                        watchdog_retry_response(refreshed).map(Json::from)
                    })
                    .name(format!("execution_watchdog_revalidate_{trigger_uid}"))
                    .retry_policy(watchdog_revalidation_retry_policy())
                    .await?
                    .into_inner();
                return Ok(Json::from(response));
            }
            let repository = self.repository.clone();
            ctx.run(|| async move {
                repository
                    .settle_watchdog_trigger(ExecutionScope::Tenant { tenant_id }, trigger_uid)
                    .await
                    .map(|_| Json::from(()))
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_watchdog_settle_{trigger_uid}"))
            .await?;
            return Ok(Json::from(ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: None,
            }));
        }
        if trigger_kind.as_deref() == Some(ExecutionTriggerKind::ExternalStartRecovery.as_str()) {
            let repository = self.repository.clone();
            let prepared = ctx
                .run(|| async move {
                    let outcome = repository
                        .prepare_external_start_recovery_trigger(
                            ExecutionScope::Tenant { tenant_id },
                            trigger_uid,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?;
                    let route = match outcome {
                        ExecutionExternalStartRecoveryTriggerOutcome::Ready(request) => {
                            ExternalStartRecoveryRoute::Ready {
                                request: Box::new(request),
                            }
                        }
                        ExecutionExternalStartRecoveryTriggerOutcome::NoOp(reason) => {
                            ExternalStartRecoveryRoute::NoOp {
                                response: trigger_response(ExecutionTriggerFireOutcome::NoOp(
                                    reason,
                                )),
                            }
                        }
                    };
                    Ok::<_, HandlerError>(Json::from(route))
                })
                .name(format!(
                    "execution_external_start_recovery_prepare_{trigger_uid}"
                ))
                .await?
                .into_inner();
            let recovery_request = match prepared {
                ExternalStartRecoveryRoute::Ready { request } => *request,
                ExternalStartRecoveryRoute::NoOp { response } => {
                    return Ok(Json::from(response));
                }
            };
            let recovery_response = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .recover_external_job_start(Json::from(recovery_request))
                    .idempotency_key(delivery_dispatch_uid.to_string()),
            )
            .call()
            .await?
            .into_inner();
            if recovery_response.outcome
                == ExecutionExternalJobStartRecoveryResponseOutcome::UnknownPreserved
            {
                // ToolExecutor atomically rearmed the same durable outbox row. The owning
                // dispatcher observes its new head deadline after this synchronous call returns.
                return Ok(Json::from(ExecutionTriggerFireResponse::NotDue));
            }
            let repository = self.repository.clone();
            ctx.run(|| async move {
                repository
                    .settle_external_start_recovery_trigger(
                        ExecutionScope::Tenant { tenant_id },
                        trigger_uid,
                    )
                    .await
                    .map(|_| Json::from(()))
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!(
                "execution_external_start_recovery_settle_{trigger_uid}"
            ))
            .await?;
            return Ok(Json::from(match recovery_response.outcome {
                ExecutionExternalJobStartRecoveryResponseOutcome::NotStartedReleased
                | ExecutionExternalJobStartRecoveryResponseOutcome::StartedBound => {
                    ExecutionTriggerFireResponse::Delivered {
                        activation_dispatch_uid: None,
                    }
                }
                ExecutionExternalJobStartRecoveryResponseOutcome::StaleDelivery => {
                    ExecutionTriggerFireResponse::StaleGeneration
                }
                ExecutionExternalJobStartRecoveryResponseOutcome::AlreadySettled => {
                    ExecutionTriggerFireResponse::Inactive
                }
                ExecutionExternalJobStartRecoveryResponseOutcome::UnknownPreserved => {
                    return Err(crate::workflows::errors::moa_error_to_handler_error(
                        moa_core::error::MoaError::ProviderTransport(
                            "external start recovery remained ambiguous without durable rearm"
                                .to_string(),
                        ),
                    ));
                }
            }));
        }
        if trigger_kind.as_deref() == Some(ExecutionTriggerKind::ExternalReconcile.as_str()) {
            let repository = self.repository.clone();
            let prepared = ctx
                .run(|| async move {
                    let outcome = repository
                        .prepare_external_reconcile_trigger(
                            ExecutionScope::Tenant { tenant_id },
                            trigger_uid,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?;
                    let route = match outcome {
                        ExecutionExternalReconcileTriggerOutcome::Ready(request) => {
                            ExternalReconcileRoute::Ready { request }
                        }
                        ExecutionExternalReconcileTriggerOutcome::NoOp(reason) => {
                            ExternalReconcileRoute::NoOp {
                                response: trigger_response(ExecutionTriggerFireOutcome::NoOp(
                                    reason,
                                )),
                            }
                        }
                    };
                    Ok::<_, HandlerError>(Json::from(route))
                })
                .name(format!(
                    "execution_external_reconcile_prepare_{trigger_uid}"
                ))
                .await?
                .into_inner();
            let reconcile_request = match prepared {
                ExternalReconcileRoute::Ready { request } => request,
                ExternalReconcileRoute::NoOp { response } => {
                    return Ok(Json::from(response));
                }
            };
            let reconcile_response = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .reconcile_external_job(Json::from(reconcile_request))
                    .idempotency_key(delivery_dispatch_uid.to_string()),
            )
            .call()
            .await?
            .into_inner();
            let repository = self.repository.clone();
            ctx.run(|| async move {
                repository
                    .settle_external_reconcile_trigger(
                        ExecutionScope::Tenant { tenant_id },
                        trigger_uid,
                    )
                    .await
                    .map(|_| Json::from(()))
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_external_reconcile_settle_{trigger_uid}"))
            .await?;
            let response = external_reconcile_response(reconcile_response.outcome);
            return Ok(Json::from(response));
        }
        let repository = self.repository.clone();
        let response = ctx
            .run(|| async move {
                repository
                    .fire_trigger(
                        ExecutionScope::Tenant {
                            tenant_id: request.tenant_id,
                        },
                        request.trigger_uid,
                    )
                    .await
                    .map(trigger_response)
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_trigger_fire_{trigger_uid}"))
            .await?;
        Ok(Json::from(response.into_inner()))
    }
}

fn watchdog_route(outcome: ExecutionWatchdogTriggerOutcome) -> WatchdogRoute {
    match outcome {
        ExecutionWatchdogTriggerOutcome::Task(request) => WatchdogRoute::Task { request },
        ExecutionWatchdogTriggerOutcome::Compensation(request) => {
            WatchdogRoute::Compensation { request }
        }
        ExecutionWatchdogTriggerOutcome::NoOp(reason) => WatchdogRoute::NoOp {
            response: trigger_response(ExecutionTriggerFireOutcome::NoOp(reason)),
        },
    }
}

fn watchdog_retry_response(
    route: WatchdogRoute,
) -> Result<ExecutionTriggerFireResponse, HandlerError> {
    match route {
        WatchdogRoute::Task { .. } | WatchdogRoute::Compensation { .. } => {
            Err(WatchdogReceiverStillCurrent.into())
        }
        WatchdogRoute::NoOp { response } => Ok(response),
    }
}

fn watchdog_revalidation_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(10))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(1))
}

fn run_deadline_fence_route(
    outcome: PendingTerminalAdvanceOutcome,
) -> Result<RunDeadlineRoute, RunDeadlineFenceRace> {
    match outcome {
        PendingTerminalAdvanceOutcome::Applied(commit) => Ok(RunDeadlineRoute::Fenced {
            response: ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: commit.continuation.map(|dispatch| dispatch.dispatch_uid),
            },
        }),
        PendingTerminalAdvanceOutcome::Replayed(_) => Ok(RunDeadlineRoute::Fenced {
            response: ExecutionTriggerFireResponse::Duplicate,
        }),
        PendingTerminalAdvanceOutcome::NotFound => Ok(RunDeadlineRoute::NoOp {
            response: ExecutionTriggerFireResponse::NotFound,
        }),
        PendingTerminalAdvanceOutcome::Conflict => Err(RunDeadlineFenceRace),
    }
}

fn run_deadline_noop_route(
    reason: ExecutionTriggerNoOp,
) -> Result<RunDeadlineRoute, RunDeadlineNotDue> {
    if reason == ExecutionTriggerNoOp::NotDue {
        return Err(RunDeadlineNotDue);
    }
    Ok(RunDeadlineRoute::NoOp {
        response: trigger_response(ExecutionTriggerFireOutcome::NoOp(reason)),
    })
}

fn run_deadline_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(10))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(1))
}

fn wait_trigger_response(
    outcome: (
        TransitionOutcome,
        Option<moa_execution::repository::outbox::ExecutionDispatchRecord>,
    ),
) -> ExecutionTriggerFireResponse {
    match outcome {
        (TransitionOutcome::Applied(_), activation) => ExecutionTriggerFireResponse::Delivered {
            activation_dispatch_uid: activation.map(|dispatch| dispatch.dispatch_uid),
        },
        (TransitionOutcome::AlreadyApplied(_), _) => ExecutionTriggerFireResponse::Duplicate,
        (TransitionOutcome::NotFound, _) => ExecutionTriggerFireResponse::NotFound,
        (TransitionOutcome::Rejected(_), _) => ExecutionTriggerFireResponse::StaleGeneration,
        (TransitionOutcome::RunApplied(_), _) | (TransitionOutcome::RunAlreadyApplied(_), _) => {
            ExecutionTriggerFireResponse::StaleGeneration
        }
    }
}

fn external_reconcile_response(
    outcome: ExecutionExternalJobReconcileResponseOutcome,
) -> ExecutionTriggerFireResponse {
    match outcome {
        ExecutionExternalJobReconcileResponseOutcome::Applied { .. } => {
            ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: None,
            }
        }
        ExecutionExternalJobReconcileResponseOutcome::StaleDelivery => {
            ExecutionTriggerFireResponse::StaleGeneration
        }
        ExecutionExternalJobReconcileResponseOutcome::AlreadyTerminal => {
            ExecutionTriggerFireResponse::Inactive
        }
        ExecutionExternalJobReconcileResponseOutcome::NotFound => {
            ExecutionTriggerFireResponse::NotFound
        }
    }
}

fn schedule_trigger_response(
    outcome: ExecutionScheduleFireResponse,
) -> ExecutionTriggerFireResponse {
    match outcome {
        ExecutionScheduleFireResponse::Admitted {
            activation_dispatch_uid,
            ..
        } => ExecutionTriggerFireResponse::Delivered {
            activation_dispatch_uid: Some(activation_dispatch_uid),
        },
        ExecutionScheduleFireResponse::Skipped => ExecutionTriggerFireResponse::Delivered {
            activation_dispatch_uid: None,
        },
        ExecutionScheduleFireResponse::Replayed { .. } => ExecutionTriggerFireResponse::Duplicate,
        ExecutionScheduleFireResponse::Stale => ExecutionTriggerFireResponse::StaleGeneration,
    }
}

fn trigger_response(outcome: ExecutionTriggerFireOutcome) -> ExecutionTriggerFireResponse {
    match outcome {
        ExecutionTriggerFireOutcome::Delivered { activation } => {
            ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: activation.map(|dispatch| dispatch.dispatch_uid),
            }
        }
        ExecutionTriggerFireOutcome::NoOp(reason) => match reason {
            ExecutionTriggerNoOp::NotFound => ExecutionTriggerFireResponse::NotFound,
            ExecutionTriggerNoOp::Duplicate => ExecutionTriggerFireResponse::Duplicate,
            ExecutionTriggerNoOp::Inactive => ExecutionTriggerFireResponse::Inactive,
            ExecutionTriggerNoOp::StaleGeneration => ExecutionTriggerFireResponse::StaleGeneration,
            ExecutionTriggerNoOp::NotDue => ExecutionTriggerFireResponse::NotDue,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stale_delivery_is_a_successful_no_op_response() {
        // Pins: Restate retries cannot turn stale trigger delivery into product failure.
        let cases = [
            (
                ExecutionTriggerNoOp::NotFound,
                ExecutionTriggerFireResponse::NotFound,
            ),
            (
                ExecutionTriggerNoOp::Duplicate,
                ExecutionTriggerFireResponse::Duplicate,
            ),
            (
                ExecutionTriggerNoOp::Inactive,
                ExecutionTriggerFireResponse::Inactive,
            ),
            (
                ExecutionTriggerNoOp::StaleGeneration,
                ExecutionTriggerFireResponse::StaleGeneration,
            ),
            (
                ExecutionTriggerNoOp::NotDue,
                ExecutionTriggerFireResponse::NotDue,
            ),
        ];
        for (reason, expected) in cases {
            assert_eq!(
                trigger_response(ExecutionTriggerFireOutcome::NoOp(reason)),
                expected
            );
        }
    }

    #[test]
    fn watchdog_retry_revalidation_completes_a_superseded_delivery_offline()
    -> Result<(), HandlerError> {
        // Pins: a receiver RetryDelivery is retried only while repository revalidation still
        // resolves an active receiver; a concurrently superseded watchdog is a successful no-op.
        assert_eq!(
            watchdog_retry_response(watchdog_route(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::Inactive,
            )))?,
            ExecutionTriggerFireResponse::Inactive
        );
        assert_eq!(
            watchdog_retry_response(watchdog_route(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            )))?,
            ExecutionTriggerFireResponse::StaleGeneration
        );
        assert!(
            watchdog_retry_response(WatchdogRoute::Task {
                request: ExecutionTaskAttemptWatchdogRequest {
                    dispatch_uid: Uuid::from_u128(1),
                    capacity_reservation_uid: Uuid::from_u128(2),
                    watchdog_trigger_uid: Uuid::from_u128(3),
                    run_uid: Uuid::from_u128(4),
                    task_id: moa_execution::state::ExecutionTaskId::from_uuid(Uuid::from_u128(5)),
                    controller_generation: 1,
                    attempt_generation: 1,
                    tenant_id: moa_core::types::identifiers::TenantId::new(),
                },
            })
            .is_err(),
            "an exact current receiver must keep the ctx.run operation retrying"
        );
        Ok::<_, HandlerError>(())
    }

    #[test]
    fn schedule_occurrence_dispositions_preserve_delivery_and_no_op_semantics() {
        // Pins: schedule occurrence routing never turns overlap, replay, or an
        // obsolete schedule incarnation into a failed trigger delivery.
        let activation_dispatch_uid = Uuid::from_u128(1);
        assert_eq!(
            schedule_trigger_response(ExecutionScheduleFireResponse::Admitted {
                run_uid: Uuid::from_u128(2),
                activation_dispatch_uid,
            }),
            ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: Some(activation_dispatch_uid),
            }
        );
        assert_eq!(
            schedule_trigger_response(ExecutionScheduleFireResponse::Skipped),
            ExecutionTriggerFireResponse::Delivered {
                activation_dispatch_uid: None,
            }
        );
        assert_eq!(
            schedule_trigger_response(ExecutionScheduleFireResponse::Replayed {
                run_uid: None,
                activation_dispatch_uid: None,
            }),
            ExecutionTriggerFireResponse::Duplicate
        );
        assert_eq!(
            schedule_trigger_response(ExecutionScheduleFireResponse::Stale),
            ExecutionTriggerFireResponse::StaleGeneration
        );
    }

    #[test]
    fn run_deadline_conflict_retries_without_exposing_a_settlement_route() {
        // Pins: a pause or wake transition between deadline preparation and fencing is a
        // retryable race, never a stale-success response that allows the trigger to settle.
        assert!(matches!(
            run_deadline_fence_route(PendingTerminalAdvanceOutcome::Conflict),
            Err(RunDeadlineFenceRace)
        ));
        assert!(matches!(
            run_deadline_fence_route(PendingTerminalAdvanceOutcome::NotFound),
            Ok(RunDeadlineRoute::NoOp {
                response: ExecutionTriggerFireResponse::NotFound,
            })
        ));
    }

    #[test]
    fn early_run_deadline_delivery_retries_until_database_due_time() {
        // Pins: an application/Restate clock that reaches a delayed invocation slightly before
        // PostgreSQL must not memoize NotDue under the immutable trigger-delivery identity.
        assert!(matches!(
            run_deadline_noop_route(ExecutionTriggerNoOp::NotDue),
            Err(RunDeadlineNotDue)
        ));
        assert!(matches!(
            run_deadline_noop_route(ExecutionTriggerNoOp::StaleGeneration),
            Ok(RunDeadlineRoute::NoOp {
                response: ExecutionTriggerFireResponse::StaleGeneration,
            })
        ));
    }
}
