//! Shared cancellation fan-out for experiment run and trial workflows.

use moa_core::traits::Identity;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::contact::ContactId;
use moa_core::types::experiments::ExperimentCancelSignal;
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_execution::wire::{ExecutionCancelRequest, ExecutionRunRequest};
use moa_experiments::store::ExperimentStore;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::execution::ExecutionClient;

/// Durable state key holding the effective execution contact scope.
pub(crate) const K_EXECUTION_CONTACT_ID: &str = "execution_contact_id";
/// Durable state key holding the linked execution run.
pub(crate) const K_EXECUTION_RUN_UID: &str = "execution_run_uid";

const K_SESSION_ID: &str = "session_id";

/// Child surface selected by an experiment cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChildCancelTarget {
    /// Agent-loop Session cancellation.
    Session(SessionId),
    /// Execution cancellation under its exact parent Session and scope.
    Execution {
        /// Owning tenant.
        tenant_id: TenantId,
        /// Optional owning contact.
        contact_id: Option<ContactId>,
        /// Parent Session.
        session_id: SessionId,
        /// Durable execution run.
        run_uid: Uuid,
    },
}

/// Selects exactly one child cancellation surface.
///
/// An execution-template target owns both a Session link and an execution link,
/// but cancellation must stop only the execution run. A Session-only link is an
/// agent-loop target and continues through `Session/request_cancel`.
#[must_use]
pub(crate) fn child_cancel_target(
    identity: Option<&Identity>,
    contact_id: Option<ContactId>,
    session_id: Option<SessionId>,
    execution_run_uid: Option<Uuid>,
) -> Option<ChildCancelTarget> {
    match (identity, session_id, execution_run_uid) {
        (Some(identity), Some(session_id), Some(run_uid)) => Some(ChildCancelTarget::Execution {
            tenant_id: identity.tenant_id,
            contact_id,
            session_id,
            run_uid,
        }),
        (_, Some(session_id), None) => Some(ChildCancelTarget::Session(session_id)),
        _ => None,
    }
}

/// Forwards cancellation from an experiment workflow to its live child.
pub(crate) async fn forward_child_cancellation(
    ctx: &SharedWorkflowContext<'_>,
    signal: ExperimentCancelSignal,
) -> Result<(), HandlerError> {
    let session_id = ctx
        .get::<Json<SessionId>>(K_SESSION_ID)
        .await?
        .map(Json::into_inner);
    let execution_run_uid = ctx
        .get::<Json<Uuid>>(K_EXECUTION_RUN_UID)
        .await?
        .map(Json::into_inner);
    let contact_id = ctx
        .get::<Json<ContactId>>(K_EXECUTION_CONTACT_ID)
        .await?
        .map(Json::into_inner);
    let Some(target) = child_cancel_target(
        Some(&signal.identity),
        contact_id,
        session_id,
        execution_run_uid,
    ) else {
        return Ok(());
    };
    match target {
        ChildCancelTarget::Session(session_id) => {
            let request = ctx
                .object_client::<SessionClient>(session_id.to_string())
                .request_cancel(Json::from(signal.reason));
            with_identity_headers(request, &signal.identity).send();
        }
        ChildCancelTarget::Execution {
            tenant_id,
            contact_id,
            session_id,
            run_uid,
        } => {
            let request = ctx.service_client::<ExecutionClient>().cancel(Json::from(
                ExecutionCancelRequest {
                    run: ExecutionRunRequest {
                        tenant_id,
                        contact_id,
                        session_id,
                        run_uid,
                    },
                    reason: signal.reason,
                },
            ));
            with_identity_headers(request, &signal.identity).send();
        }
    }
    Ok(())
}

/// Returns whether an authorized cancellation was durably recorded.
pub(crate) async fn has_pending_cancellation(
    ctx: &WorkflowContext<'_>,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<bool, HandlerError> {
    Ok(load_cancel_signal(ctx, scope, run_uid, pool)
        .await?
        .is_some())
}

/// Forwards a previously recorded cancellation after a child link becomes known.
///
/// Returns `true` when a cancellation fence exists, including before a child is
/// linked. Callers use that result to avoid starting paid child work.
pub(crate) async fn forward_pending_child_cancellation(
    ctx: &WorkflowContext<'_>,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<bool, HandlerError> {
    let Some(signal) = load_cancel_signal(ctx, scope, run_uid, pool).await? else {
        return Ok(false);
    };
    forward_child_cancellation_signal(ctx, &signal).await?;
    Ok(true)
}

/// Forwards one cancellation signal to whichever child this workflow started.
///
/// Split out from the pending-signal path so a locally-decided stop — a trial that
/// has run past its envelope deadline — can cancel the child it is waiting on
/// without first persisting an operator cancellation it never received. The child
/// target is resolved from this workflow's own journaled keys, so a replay forwards
/// to the same child rather than re-deciding from live state.
pub(crate) async fn forward_child_cancellation_signal(
    ctx: &WorkflowContext<'_>,
    signal: &ExperimentCancelSignal,
) -> Result<(), HandlerError> {
    let session_id = ctx
        .get::<Json<SessionId>>(K_SESSION_ID)
        .await?
        .map(Json::into_inner);
    let execution_run_uid = ctx
        .get::<Json<Uuid>>(K_EXECUTION_RUN_UID)
        .await?
        .map(Json::into_inner);
    let contact_id = ctx
        .get::<Json<ContactId>>(K_EXECUTION_CONTACT_ID)
        .await?
        .map(Json::into_inner);
    let Some(target) = child_cancel_target(
        Some(&signal.identity),
        contact_id,
        session_id,
        execution_run_uid,
    ) else {
        return Ok(());
    };
    match target {
        ChildCancelTarget::Session(session_id) => {
            with_identity_headers(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .request_cancel(Json::from(signal.reason.clone())),
                &signal.identity,
            )
            .send();
        }
        ChildCancelTarget::Execution {
            tenant_id,
            contact_id,
            session_id,
            run_uid,
        } => {
            with_identity_headers(
                ctx.service_client::<ExecutionClient>().cancel(Json::from(
                    ExecutionCancelRequest {
                        run: ExecutionRunRequest {
                            tenant_id,
                            contact_id,
                            session_id,
                            run_uid,
                        },
                        reason: signal.reason.clone(),
                    },
                )),
                &signal.identity,
            )
            .send();
        }
    }
    Ok(())
}

async fn load_cancel_signal(
    ctx: &WorkflowContext<'_>,
    scope: &ActionRuleScope,
    run_uid: Uuid,
    pool: &sqlx::PgPool,
) -> Result<Option<ExperimentCancelSignal>, HandlerError> {
    let pool = pool.clone();
    let scope = *scope;
    Ok(ctx
        .run(|| async move {
            ExperimentStore::new(pool)
                .load_run_cancel_signal(&scope, run_uid)
                .await
                .map(Json::from)
                .map_err(crate::workflows::errors::moa_error_to_handler_error)
        })
        .name("experiment_load_cancel_signal")
        .await?
        .into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::traits::IdentityType;

    fn identity() -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(1),
            tenant_id: TenantId(Uuid::from_u128(2)),
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    #[test]
    fn execution_link_selects_only_execution_cancellation_offline() {
        // Pins: execution templates do not cancel their parent user/internal Session.
        let identity = identity();
        let session_id = SessionId(Uuid::from_u128(3));
        let run_uid = Uuid::from_u128(4);
        assert_eq!(
            child_cancel_target(Some(&identity), None, Some(session_id), Some(run_uid)),
            Some(ChildCancelTarget::Execution {
                tenant_id: identity.tenant_id,
                contact_id: None,
                session_id,
                run_uid,
            })
        );
    }

    #[test]
    fn session_only_link_selects_agent_loop_cancellation_offline() {
        // Pins: an agent-loop target retains Session cancellation.
        let session_id = SessionId(Uuid::from_u128(3));
        assert_eq!(
            child_cancel_target(None, None, Some(session_id), None),
            Some(ChildCancelTarget::Session(session_id))
        );
    }

    #[test]
    fn incomplete_execution_authority_yields_no_cancellation_offline() {
        // Pins: an execution run is never cancelled with fabricated scope or parent authority.
        assert_eq!(
            child_cancel_target(None, None, Some(SessionId::new()), Some(Uuid::new_v4())),
            None
        );
    }
}
