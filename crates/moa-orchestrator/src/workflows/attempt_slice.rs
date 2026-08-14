//! Helpers shared by the bounded task- and compensation-attempt slices.
//!
//! Restate models the exclusive and shared halves of a workflow as two distinct
//! context types. Both satisfy the SDK's blanket-implemented [`ContextSideEffects`]
//! and [`ContextClient`] traits, but bounding a helper on `ContextSideEffects<'ctx>`
//! makes the journal lifetime early-bound while `ContextSideEffects::run` requires
//! the journaled closure to outlive that same `'ctx`, so the generated handlers fail
//! rustc's higher-ranked check (`rust-lang/rust#100013`). Helpers that journal are
//! therefore written per context type; helpers that do not stay generic.

use chrono::{DateTime, Utc};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::services::execution_dispatcher::{DispatchExecutionsRequest, ExecutionDispatcherClient};

/// Idempotency-key family for task-attempt dispatcher wakes.
pub(crate) const TASK_ATTEMPT_DISPATCH_KICK: &str = "task-attempt-dispatch";

/// Idempotency-key family for compensation-attempt dispatcher wakes.
pub(crate) const COMPENSATION_ATTEMPT_DISPATCH_KICK: &str = "compensation-attempt-dispatch";

/// Journals the current wall clock from a shared watchdog or cancellation handler.
///
/// Shared twin of [`crate::workflows::durable_utc_now`]; `step_name` becomes the
/// Restate journal entry name and must stay stable, because renaming a durable step
/// changes the replay journal key.
pub(crate) async fn durable_utc_now_shared(
    ctx: &SharedWorkflowContext<'_>,
    step_name: &'static str,
) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name(step_name)
        .await?
        .into_inner())
}

/// Wakes the fleet dispatcher immediately after an attempt boundary commits.
///
/// Attempt workflows run asynchronously from the dispatcher, which indexes the
/// persisted future head and owns the only delayed wake. `prefix` keeps the task
/// and compensation families on disjoint idempotency keys so one family's kick can
/// never attach to the other's completed invocation.
pub(crate) async fn kick_dispatcher<'ctx, C>(
    ctx: &C,
    prefix: &'static str,
    dispatch_uid: Uuid,
    boundary: &'static str,
) -> Result<(), HandlerError>
where
    C: ContextClient<'ctx>,
{
    let handle = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionDispatcherClient>()
            .dispatch(Json::from(DispatchExecutionsRequest::default()))
            .idempotency_key(format!("{prefix}:{dispatch_uid}:{boundary}")),
    )
    .send();
    let _invocation_id = handle.invocation_id().await?;
    Ok(())
}

/// Rejects any delivery whose workflow key is not the immutable dispatch identity.
///
/// `mismatch_message` names the owning attempt family so an operator can tell which
/// durable surface rejected the delivery.
pub(crate) fn require_dispatch_key(
    key: &str,
    dispatch_uid: Uuid,
    mismatch_message: &'static str,
) -> Result<(), HandlerError> {
    if key == dispatch_uid.to_string() {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(404, mismatch_message).into())
    }
}
