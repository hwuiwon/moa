//! Bounded, generation-fenced activations for one durable execution run.
//!
//! A controller activation performs a finite amount of database-backed scheduler
//! work and then returns. Parked runs retain only Postgres state and exact
//! delayed triggers; they never retain a promise, child join, or polling loop.

mod advance;
mod progress;
mod settlement;

#[cfg(test)]
mod tests;

use moa_core::types::identifiers::TenantId;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Exact durable dispatch accepted by one controller activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunAdvanceRequest {
    /// Immutable dispatch-outbox identity.
    pub dispatch_uid: Uuid,
    /// Tenant that owns the execution run.
    pub tenant_id: TenantId,
    /// Durable execution-run identifier and virtual-object key.
    pub run_uid: Uuid,
    /// Exact controller generation fenced by the dispatch.
    pub controller_generation: u64,
    /// Exact persisted scheduling wake claimed by this activation.
    pub wake_epoch: u64,
}

/// Durable disposition of one controller activation request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunAdvanceOutcome {
    /// The exact wake was claimed and bounded scheduler work committed.
    Advanced,
    /// The exact wake had already committed and was replayed as a no-op.
    Replayed,
    /// A newer generation or wake superseded this dispatch.
    Stale,
    /// The run was already terminal and could not be advanced.
    Terminal,
}

/// Bounded acknowledgement returned to the dispatch owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunAdvanceResponse {
    /// Durable request disposition.
    pub outcome: ExecutionRunAdvanceOutcome,
    /// Controller generation observed after the transaction.
    pub controller_generation: u64,
    /// Wake epoch observed after the transaction.
    pub wake_epoch: u64,
    /// Number of scheduler transitions performed by this activation.
    pub activation_steps: usize,
    /// Number of newly materialized logical tasks.
    pub materialized_tasks: usize,
    /// Whether the same transaction enqueued one bounded continuation.
    pub continuation_enqueued: bool,
}

/// Restate virtual object that serializes bounded advancement by execution run.
#[restate_sdk::object]
#[name = "ExecutionRunController"]
pub trait ExecutionRunController {
    /// Claims and advances one exact persisted scheduling wake.
    async fn advance(
        request: Json<ExecutionRunAdvanceRequest>,
    ) -> Result<Json<ExecutionRunAdvanceResponse>, HandlerError>;
}

/// PostgreSQL-backed bounded execution-run controller.
#[derive(Clone)]
pub struct ExecutionRunControllerImpl {
    repository: moa_execution::repository::ExecutionRepository,
    config: moa_config::ExecutionConfig,
}

impl ExecutionRunControllerImpl {
    /// Creates a bounded controller over the shared execution repository.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: moa_config::ExecutionConfig) -> Self {
        Self {
            repository: moa_execution::repository::ExecutionRepository::new(pool),
            config,
        }
    }
}

impl ExecutionRunController for ExecutionRunControllerImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(run_uid = %request.0.run_uid))]
    // SAFETY: ingress-private dispatch; the exact key and admitted owner scope are revalidated from Postgres.
    async fn advance(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ExecutionRunAdvanceRequest>,
    ) -> Result<Json<ExecutionRunAdvanceResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        moa_observability::restate_observability::annotate_restate_handler_span(
            "ExecutionRunController",
            "advance",
        );
        let request = request.into_inner();
        advance::validate_request(ctx.key(), &request)?;

        let repository = self.repository.clone();
        let config = self.config.clone();
        let operation = request.clone();
        let committed = ctx
            .run(|| async move {
                advance::advance(repository, config, operation)
                    .await
                    .map(Json::from)
                    .map_err(crate::workflows::errors::execution_error_to_handler_error)
            })
            .name(format!(
                "execution_controller_advance_{}_{}",
                request.controller_generation, request.wake_epoch
            ))
            .await?
            .into_inner();

        progress::deliver(&ctx, &self.repository, &request, &committed).await?;
        Ok(Json::from(committed.response))
    }
}
