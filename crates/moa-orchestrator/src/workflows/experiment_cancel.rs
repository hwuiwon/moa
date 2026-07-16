//! Shared cancellation fan-out for experiment run and trial workflows.

use moa_core::traits::{Identity, IdentityType};
use moa_core::types::contact::ContactId;
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_execution::wire::{ExecutionCancelRequest, ExecutionRunRequest};
use restate_sdk::context::Request;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::objects::session::SessionClient;
use crate::services::execution::ExecutionClient;

/// Durable state key holding the identity that created the experiment child work.
pub(crate) const K_CANCEL_IDENTITY: &str = "cancel_identity";
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
    reason: String,
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
    let identity = ctx
        .get::<Json<Identity>>(K_CANCEL_IDENTITY)
        .await?
        .map(Json::into_inner);
    let Some(target) =
        child_cancel_target(identity.as_ref(), contact_id, session_id, execution_run_uid)
    else {
        return Ok(());
    };
    match target {
        ChildCancelTarget::Session(session_id) => {
            let request = ctx
                .object_client::<SessionClient>(session_id.to_string())
                .request_cancel(Json::from(reason));
            match identity.as_ref() {
                Some(identity) => with_identity_headers(request, identity).send(),
                None => crate::restate_identity::replay_safe_request(request).send(),
            };
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
                    reason,
                },
            ));
            if let Some(identity) = identity.as_ref() {
                with_identity_headers(request, identity).send();
            } else {
                crate::restate_identity::replay_safe_request(request).send();
            };
        }
    }
    Ok(())
}

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
