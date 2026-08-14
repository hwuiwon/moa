//! Durable, self-scheduling terminal execution-detail retention.

use std::time::Duration;

use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    retention::{
        ExecutionRetentionClaimOutcome, ExecutionRetentionPageOutcome,
        ExecutionRetentionScheduleReceipt,
    },
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::workflows::errors::execution_error_to_handler_error;

const RETENTION_PAGE_SIZE: u32 = 64;
const BACKLOG_DELAY: Duration = Duration::from_secs(5);
const INITIAL_IDLE_DELAY: Duration = Duration::from_secs(60);
const MAXIMUM_IDLE_DELAY: Duration = Duration::from_secs(60 * 60);
const FAILURE_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Generation-fenced invocation accepted from bootstrap repair or the prior pass.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRetentionRequest {
    /// Exact scheduled generation; absent only for the coarse repair Cron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
}

/// Bounded pass response exposed to operational callers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRetentionResponse {
    /// Work performed by this pass, or `None` when another schedule owns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<ExecutionRetentionPageOutcome>,
    /// Durable generation accepted by the next delayed invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_generation: Option<u64>,
    /// Delay until the next normal pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_delay_seconds: Option<u64>,
}

/// Ingress-private bounded retention target.
#[restate_sdk::service]
#[name = "ExecutionRetention"]
pub trait ExecutionRetention {
    /// Archives or deletes at most one bounded page, then schedules the next generation.
    async fn run(
        request: Json<ExecutionRetentionRequest>,
    ) -> Result<Json<ExecutionRetentionResponse>, HandlerError>;
}

/// PostgreSQL-backed execution retention implementation.
#[derive(Clone)]
pub struct ExecutionRetentionImpl {
    repository: ExecutionRepository,
    retention_days: u64,
}

impl ExecutionRetentionImpl {
    /// Creates the retention target from the validated fleet policy.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: &moa_config::ExecutionConfig) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            retention_days: config.terminal_detail_retention_days,
        }
    }
}

impl ExecutionRetention for ExecutionRetentionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: ingress-private maintenance uses control-plane RLS and rechecks terminal and legal-hold fences.
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRetentionRequest>,
    ) -> Result<Json<ExecutionRetentionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionRetention",
            "run",
        );
        let repository = self.repository.clone();
        let expected_generation = request.into_inner().expected_generation;
        let claim = ctx
            .run(|| async move {
                repository
                    .claim_execution_retention(ExecutionScope::ControlPlane, expected_generation)
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_retention_claim")
            .await?
            .into_inner();
        let ExecutionRetentionClaimOutcome::Claimed {
            generation,
            previous_delay_seconds,
        } = claim
        else {
            return Ok(Json::from(ExecutionRetentionResponse {
                page: None,
                scheduled_generation: None,
                next_delay_seconds: None,
            }));
        };

        let repository = self.repository.clone();
        let retention_days = self.retention_days;
        let page = match ctx
            .run(|| async move {
                repository
                    .advance_execution_retention_page(
                        ExecutionScope::ControlPlane,
                        retention_days,
                        RETENTION_PAGE_SIZE,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name(format!("execution_retention_page_{generation}"))
            .await
        {
            Ok(page) => page.into_inner(),
            Err(error) => {
                schedule_after_failure(&ctx, &self.repository, generation, &error.to_string())
                    .await;
                return Err(error.into());
            }
        };
        let delay = retention_delay(&page, previous_delay_seconds);
        let receipt =
            persist_next_schedule(&ctx, &self.repository, generation, delay, None).await?;
        send_next(&ctx, &receipt, delay).await?;

        Ok(Json::from(ExecutionRetentionResponse {
            page: Some(page),
            scheduled_generation: Some(receipt.scheduled_generation),
            next_delay_seconds: Some(delay.as_secs()),
        }))
    }
}

async fn persist_next_schedule(
    ctx: &Context<'_>,
    repository: &ExecutionRepository,
    generation: u64,
    delay: Duration,
    failure: Option<String>,
) -> Result<ExecutionRetentionScheduleReceipt, HandlerError> {
    let repository = repository.clone();
    ctx.run(|| async move {
        repository
            .schedule_execution_retention(
                ExecutionScope::ControlPlane,
                generation,
                delay.as_secs(),
                failure.as_deref(),
            )
            .await
            .map(Json::from)
            .map_err(execution_error_to_handler_error)
    })
    .name(format!("execution_retention_schedule_{generation}"))
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

async fn send_next(
    ctx: &Context<'_>,
    receipt: &ExecutionRetentionScheduleReceipt,
    delay: Duration,
) -> Result<(), HandlerError> {
    let request = ExecutionRetentionRequest {
        expected_generation: Some(receipt.scheduled_generation),
    };
    let handle = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionRetentionClient>()
            .run(Json::from(request))
            .idempotency_key(format!(
                "execution-retention-generation-{}",
                receipt.scheduled_generation
            )),
    )
    .send_after(delay);
    let _invocation_id = handle.invocation_id().await?;
    Ok(())
}

async fn schedule_after_failure(
    ctx: &Context<'_>,
    repository: &ExecutionRepository,
    generation: u64,
    error: &str,
) {
    match persist_next_schedule(
        ctx,
        repository,
        generation,
        FAILURE_RETRY_DELAY,
        Some(error.to_string()),
    )
    .await
    {
        Ok(receipt) => {
            if let Err(schedule_error) = send_next(ctx, &receipt, FAILURE_RETRY_DELAY).await {
                tracing::warn!(
                    ?schedule_error,
                    "failed to accept delayed execution retention retry"
                );
            }
        }
        Err(schedule_error) => {
            tracing::warn!(
                ?schedule_error,
                "failed to persist execution retention retry"
            );
        }
    }
}

fn retention_delay(
    page: &ExecutionRetentionPageOutcome,
    previous_delay_seconds: Option<u64>,
) -> Duration {
    if !matches!(page, ExecutionRetentionPageOutcome::Idle) {
        return BACKLOG_DELAY;
    }
    let previous = Duration::from_secs(previous_delay_seconds.unwrap_or(0));
    if previous < INITIAL_IDLE_DELAY {
        INITIAL_IDLE_DELAY
    } else {
        previous.saturating_mul(2).min(MAXIMUM_IDLE_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn idle_retention_backs_off_but_backlog_resets_to_short_delay() {
        // Pins: empty fleets stop waking frequently while any archive/delete work
        // restores the short bounded cadence.
        assert_eq!(
            retention_delay(&ExecutionRetentionPageOutcome::Idle, None),
            INITIAL_IDLE_DELAY
        );
        assert_eq!(
            retention_delay(
                &ExecutionRetentionPageOutcome::Idle,
                Some(INITIAL_IDLE_DELAY.as_secs())
            ),
            INITIAL_IDLE_DELAY.saturating_mul(2)
        );
        assert_eq!(
            retention_delay(
                &ExecutionRetentionPageOutcome::Idle,
                Some(MAXIMUM_IDLE_DELAY.as_secs())
            ),
            MAXIMUM_IDLE_DELAY
        );
        assert_eq!(
            retention_delay(
                &ExecutionRetentionPageOutcome::SegmentArchived {
                    run_uid: Uuid::nil(),
                    segment_kind: "task".to_string(),
                    records: 1,
                },
                Some(MAXIMUM_IDLE_DELAY.as_secs())
            ),
            BACKLOG_DELAY
        );
    }
}
