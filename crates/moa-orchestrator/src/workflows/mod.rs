//! Restate workflow modules hosted by the orchestrator binary.

use chrono::{DateTime, Utc};
use restate_sdk::prelude::*;

pub mod artifact_workflow_execution;
pub mod consolidate;
pub(crate) mod errors;
#[cfg(feature = "experiments")]
pub(crate) mod experiment_errors;
#[cfg(feature = "experiments")]
pub mod experiment_run;
#[cfg(feature = "experiments")]
pub mod experiment_trial_run;
pub mod knowledge_sync_ingestion;
pub(crate) mod progress_delivery;
#[cfg(feature = "skill-learning")]
pub mod skill_learning;
pub mod sub_agent_turn_execution;
pub mod turn_execution;
pub(crate) mod turn_progress;
pub(crate) mod turn_responsiveness;
pub mod workflow_node_actions;

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
