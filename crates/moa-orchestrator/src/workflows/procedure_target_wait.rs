//! Shared durable wait for a direct procedure experiment target.
//!
//! Both the parent experiment run
//! ([`experiment_run`](crate::workflows::experiment_run)) and the behavior-lab
//! trial ([`experiment_trial_run`](crate::workflows::experiment_trial_run))
//! execute a procedure target by starting a durable procedure run and then
//! waiting for it to reach a terminal state before reporting completion. This
//! module owns that wait so both paths bound it identically and never resolve
//! their parent while the procedure is still executing. Each caller maps the
//! returned [`ProcedureWaitOutcome`] into its own status vocabulary.

use std::time::Duration;

use moa_artifacts::registry::ArtifactRunStatus;
use moa_core::traits::Identity;
use moa_core::{types::identifiers::SessionId, types::identifiers::TenantId};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::workflows::procedure_execution::{
    ProcedureExecutionClient, ProcedureOutcome, RunProcedureRequest,
};

/// Maximum time a direct procedure target may run before the awaiting workflow
/// records a timeout.
///
/// Shared by the experiment run and trial direct-target paths so both bound the
/// wait with the same deadline as the agent-loop turn wait.
pub(crate) const TARGET_WAIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Outcome of durably awaiting a direct procedure target.
pub(crate) enum ProcedureWaitOutcome {
    /// The procedure `run` handler returned a terminal artifact-run status.
    /// Callers map the [`ArtifactRunStatus`] into their own status vocabulary.
    Terminal(ArtifactRunStatus, ProcedureOutcome),
    /// The `run` handler returned before reaching a terminal status. The handler
    /// only returns terminal outcomes, so callers treat this as a failure.
    NonTerminal(ProcedureOutcome),
    /// The wait exceeded [`TARGET_WAIT_TIMEOUT`] before the procedure finished,
    /// for example a review-gated run that blocked inside the `run` handler.
    TimedOut,
}

/// Durably waits for a procedure run to reach a terminal state, bounded by
/// [`TARGET_WAIT_TIMEOUT`].
///
/// Races a request-response `.call()` to the procedure
/// [`ProcedureExecution`](crate::workflows::procedure_execution::ProcedureExecution)
/// `run` handler against a durable timer. A `CallFuture` is a `DurableFuture`, so
/// the `select!` combinator journals the completion order deterministically and
/// the wait is replay-safe, mirroring the awakeable-vs-timer race used for
/// agent-loop turns.
///
/// The procedure `run` handler blocks internally while a run is paused on a
/// `Review` or `WaitSignal` node, so a review-gated run never resolves the call
/// and the wait times out instead. On timeout the wait requests cancellation of
/// the abandoned child procedure (best-effort, via the shared `request_cancel`
/// handler) so an experiment target does not keep executing after its parent
/// recorded the timeout as a failure.
pub(crate) async fn wait_for_procedure_outcome(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    identity: Identity,
    run_uid: Uuid,
    session_id: Option<SessionId>,
) -> Result<ProcedureWaitOutcome, HandlerError> {
    let outcome_fut = ctx
        .workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
        .run(Json::from(RunProcedureRequest {
            tenant_id,
            run_uid,
            identity,
            session_id,
        }))
        .call();

    restate_sdk::select! {
        outcome = outcome_fut => {
            let outcome = outcome?.into_inner();
            match artifact_run_status_from_label(&outcome.status) {
                Some(status) if artifact_run_status_is_terminal(&status) => {
                    Ok(ProcedureWaitOutcome::Terminal(status, outcome))
                }
                _ => Ok(ProcedureWaitOutcome::NonTerminal(outcome)),
            }
        },
        _ = ctx.sleep(TARGET_WAIT_TIMEOUT) => {
            ctx.workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
                .request_cancel(Json::from(format!(
                    "experiment target wait timed out after {TARGET_WAIT_TIMEOUT:?}"
                )))
                .send();
            Ok(ProcedureWaitOutcome::TimedOut)
        },
    }
}

/// Parses the durable artifact-run status label emitted by the procedure `run`
/// handler back into an [`ArtifactRunStatus`].
pub(crate) fn artifact_run_status_from_label(label: &str) -> Option<ArtifactRunStatus> {
    match label {
        "queued" => Some(ArtifactRunStatus::Queued),
        "running" => Some(ArtifactRunStatus::Running),
        "pending_review" => Some(ArtifactRunStatus::PendingReview),
        "completed" => Some(ArtifactRunStatus::Completed),
        "failed" => Some(ArtifactRunStatus::Failed),
        "cancelled" => Some(ArtifactRunStatus::Cancelled),
        _ => None,
    }
}

/// Returns true when an artifact-run status is terminal and should resolve the
/// awaiting workflow.
fn artifact_run_status_is_terminal(status: &ArtifactRunStatus) -> bool {
    matches!(
        status,
        ArtifactRunStatus::Completed | ArtifactRunStatus::Failed | ArtifactRunStatus::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_run_status_label_round_trips_every_status_offline() {
        // Pins: every status label emitted by the procedure run handler parses back to
        // the matching ArtifactRunStatus so terminal detection never silently drops a state.
        for status in [
            ArtifactRunStatus::Queued,
            ArtifactRunStatus::Running,
            ArtifactRunStatus::PendingReview,
            ArtifactRunStatus::Completed,
            ArtifactRunStatus::Failed,
            ArtifactRunStatus::Cancelled,
        ] {
            assert_eq!(
                artifact_run_status_from_label(status.as_str()),
                Some(status.clone()),
                "label {} should round-trip",
                status.as_str()
            );
        }
        assert_eq!(artifact_run_status_from_label("nonsense"), None);
    }

    #[test]
    fn only_finished_runs_are_terminal_offline() {
        // Pins: a queued, running, or review-paused procedure is never terminal, so the
        // shared wait keeps blocking until the run finishes or the timeout fires.
        assert!(!artifact_run_status_is_terminal(&ArtifactRunStatus::Queued));
        assert!(!artifact_run_status_is_terminal(
            &ArtifactRunStatus::Running
        ));
        assert!(!artifact_run_status_is_terminal(
            &ArtifactRunStatus::PendingReview
        ));
        assert!(artifact_run_status_is_terminal(
            &ArtifactRunStatus::Completed
        ));
        assert!(artifact_run_status_is_terminal(&ArtifactRunStatus::Failed));
        assert!(artifact_run_status_is_terminal(
            &ArtifactRunStatus::Cancelled
        ));
    }
}
