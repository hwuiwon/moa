//! Restate workflow modules hosted by the orchestrator binary.

use chrono::{DateTime, Utc};
use restate_sdk::prelude::*;

pub mod consolidate;
pub(crate) mod errors;
pub mod execution_node_actions;
pub mod execution_run;
pub mod execution_task;
pub(crate) mod experiment_cancel;
pub(crate) mod experiment_errors;
pub mod experiment_run;
pub mod experiment_trial_run;
pub mod knowledge_index_rebuild;
pub mod knowledge_sync_ingestion;
pub(crate) mod progress_delivery;
pub mod session_retention;
pub mod skill_learning;
pub mod tenant_purge;
pub mod turn_events;
pub mod turn_execution;
pub(crate) mod turn_progress;
pub(crate) mod turn_responsiveness;
pub mod worker_turn_execution;

/// Durably samples the current UTC time inside a workflow as a named replayable step.
///
/// `step_name` becomes the Restate journal entry name; callers must keep it stable
/// across versions because renaming a durable step changes the replay journal key.
pub(crate) async fn durable_utc_now(
    ctx: &WorkflowContext<'_>,
    step_name: &'static str,
) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name(step_name)
        .await?
        .into_inner())
}
