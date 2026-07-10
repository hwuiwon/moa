//! Shared cancellation fan-out for experiment run and trial workflows.
//!
//! Cancelling an experiment run must stop live work, not only update database
//! projections. Both the parent [`ExperimentRun`](crate::workflows::experiment_run)
//! workflow (for single-target runs) and each
//! [`ExperimentTrialRun`](crate::workflows::experiment_trial_run) workflow drive a
//! child target — a target session (agent-loop) or a durable procedure run. This
//! module owns the replay-safe logic that maps a workflow's durable child links to
//! the concrete cancellation surfaces, so both workflows forward cancellation the
//! same way through the existing `Session` and `ProcedureExecution` `request_cancel`
//! handlers. Cancelling the child makes the workflow's own durable wait resolve as
//! `Cancelled`, so it stops promptly instead of waiting out the target timeout.

use moa_core::SessionId;
use moa_core::traits::{Identity, IdentityType};
use restate_sdk::context::Request;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::objects::session::SessionClient;
use crate::workflows::procedure_execution::ProcedureExecutionClient;

/// Durable state key holding the identity that created the experiment's child
/// work, used to authorize the target-session cancellation forward.
pub(crate) const K_CANCEL_IDENTITY: &str = "cancel_identity";

/// Durable state key holding the trial/run's target session id, if any.
///
/// Matches the `"session_id"` key both workflows set when they attach a session.
const K_SESSION_ID: &str = "session_id";

/// Durable state key holding the trial/run's procedure run id, if any.
///
/// Matches the `"procedure_run_uid"` key both workflows set when they start a
/// procedure target.
const K_PROCEDURE_RUN_UID: &str = "procedure_run_uid";

/// A child execution surface an experiment cancellation must stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChildCancelTarget {
    /// An agent-loop target session, cancelled through `Session/request_cancel`.
    Session(SessionId),
    /// A durable procedure run, cancelled through `ProcedureExecution/request_cancel`.
    Procedure(Uuid),
}

/// Returns the child execution surfaces to cancel for a workflow's durable links.
///
/// A workflow may have started a target session, a procedure run, both (never in
/// current paths), or neither (nothing to cancel yet). The order is
/// session-before-procedure so the transcript-bearing surface is signaled first.
pub(crate) fn child_cancel_targets(
    session_id: Option<SessionId>,
    procedure_run_uid: Option<Uuid>,
) -> Vec<ChildCancelTarget> {
    let mut targets = Vec::new();
    if let Some(session_id) = session_id {
        targets.push(ChildCancelTarget::Session(session_id));
    }
    if let Some(procedure_run_uid) = procedure_run_uid {
        targets.push(ChildCancelTarget::Procedure(procedure_run_uid));
    }
    targets
}

/// Forwards cancellation from an experiment run/trial workflow to its live child
/// target work.
///
/// Reads the workflow's durable child links and creator identity, then signals
/// the existing `Session` / `ProcedureExecution` `request_cancel` handlers with a
/// one-way send. Best-effort: unset links are skipped, and the target session
/// cancel carries the creator identity headers because `Session/request_cancel`
/// authorizes the caller as a session participant.
pub(crate) async fn forward_child_cancellation(
    ctx: &SharedWorkflowContext<'_>,
    reason: String,
) -> Result<(), HandlerError> {
    let session_id = ctx
        .get::<Json<SessionId>>(K_SESSION_ID)
        .await?
        .map(Json::into_inner);
    let procedure_run_uid = ctx
        .get::<Json<Uuid>>(K_PROCEDURE_RUN_UID)
        .await?
        .map(Json::into_inner);
    let identity = ctx
        .get::<Json<Identity>>(K_CANCEL_IDENTITY)
        .await?
        .map(Json::into_inner);
    for target in child_cancel_targets(session_id, procedure_run_uid) {
        match target {
            ChildCancelTarget::Session(session_id) => {
                let request = ctx
                    .object_client::<SessionClient>(session_id.to_string())
                    .request_cancel(Json::from(reason.clone()));
                match &identity {
                    Some(identity) => {
                        with_identity_headers(request, identity).send();
                    }
                    None => {
                        request.send();
                    }
                }
            }
            ChildCancelTarget::Procedure(run_uid) => {
                ctx.workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
                    .request_cancel(Json::from(reason.clone()))
                    .send();
            }
        }
    }
    Ok(())
}

/// Attaches the caller identity headers required by `Session/request_cancel`.
fn with_identity_headers<'a, Req, Res>(
    request: Request<'a, Req, Res>,
    identity: &Identity,
) -> Request<'a, Req, Res> {
    let request = request
        .header(
            "x-moa-identity-type".to_string(),
            identity_type_header(identity.identity_type).to_string(),
        )
        .header("x-moa-identity-id".to_string(), identity.id.to_string())
        .header(
            "x-moa-tenant-id".to_string(),
            identity.tenant_id.to_string(),
        );
    let request = if let Some(api_key_id) = identity.api_key_id {
        request.header("x-moa-api-key-id".to_string(), api_key_id.to_string())
    } else {
        request
    };
    if let Some(user_id) = identity.acting_on_behalf_of {
        request.header("x-moa-acting-on-behalf-of".to_string(), user_id.to_string())
    } else {
        request
    }
}

fn identity_type_header(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::Operator => "operator",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
        IdentityType::Contact => "contact",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_child_links_yield_no_cancellation_targets_offline() {
        // Pins: a run/trial that has not started a child target has nothing to cancel.
        assert!(child_cancel_targets(None, None).is_empty());
    }

    #[test]
    fn session_link_yields_session_cancellation_target_offline() {
        // Pins: an agent-loop target session is cancelled through the Session surface.
        let session_id = SessionId::new();
        assert_eq!(
            child_cancel_targets(Some(session_id), None),
            vec![ChildCancelTarget::Session(session_id)]
        );
    }

    #[test]
    fn procedure_link_yields_procedure_cancellation_target_offline() {
        // Pins: a procedure target is cancelled through the ProcedureExecution surface.
        let run_uid = Uuid::new_v4();
        assert_eq!(
            child_cancel_targets(None, Some(run_uid)),
            vec![ChildCancelTarget::Procedure(run_uid)]
        );
    }

    #[test]
    fn session_is_cancelled_before_procedure_offline() {
        // Pins: the transcript-bearing session is signaled before the procedure run.
        let session_id = SessionId::new();
        let run_uid = Uuid::new_v4();
        assert_eq!(
            child_cancel_targets(Some(session_id), Some(run_uid)),
            vec![
                ChildCancelTarget::Session(session_id),
                ChildCancelTarget::Procedure(run_uid),
            ]
        );
    }
}
