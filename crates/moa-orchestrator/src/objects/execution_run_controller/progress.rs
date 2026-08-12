//! Product progress projection and Session delivery for controller activations.

use moa_core::{events::ExecutionProgress, traits::Identity, types::identifiers::SessionId};
use moa_execution::{
    repository::{ExecutionRepository, ExecutionScope},
    wire::execution_progress_from_run,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use super::{ExecutionRunAdvanceRequest, advance::ControllerAdvanceCommit};
use crate::objects::session::SessionClient;

pub(super) async fn deliver(
    ctx: &ObjectContext<'_>,
    repository: &ExecutionRepository,
    request: &ExecutionRunAdvanceRequest,
    committed: &ControllerAdvanceCommit,
) -> Result<(), HandlerError> {
    if !committed.publish_progress {
        return Ok(());
    }
    let repository = repository.clone();
    let run_uid = request.run_uid;
    let tenant_id = request.tenant_id;
    let terminal = committed.terminal_delivery.clone();
    let delivery = ctx
        .run(|| async move {
            let run = repository
                .load_run(ExecutionScope::ControlPlane, run_uid)
                .await
                .map_err(crate::workflows::errors::execution_error_to_handler_error)?
                .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
            if run.tenant_id != tenant_id || run.admitted_identity.tenant_id != tenant_id {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution progress owner does not match activation",
                )
                .into());
            }
            let progress = execution_progress_from_run(&run)
                .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
            Ok::<_, HandlerError>(Json::from(ControllerProgressDeliveryWire {
                identity: run.admitted_identity,
                session_id: run.session_id,
                progress,
                terminal,
            }))
        })
        .name(format!(
            "execution_controller_progress_{}_{}",
            request.controller_generation, request.wake_epoch
        ))
        .await?
        .into_inner();

    tracing::info!(
        session_id = %delivery.session_id,
        summary = %crate::workflows::progress_delivery::execution_controller_summary(
            &delivery.progress.status,
            delivery.progress.ready_tasks,
            delivery.progress.active_tasks,
            delivery.progress.parked_tasks,
            delivery.progress.completed,
            delivery.progress.next_wake_at,
        ),
        "execution controller progress committed"
    );

    let call = ctx
        .object_client::<SessionClient>(delivery.session_id.to_string())
        .execution_progress(Json::from(delivery.progress));
    let handle = crate::restate_identity::replay_safe_request(
        crate::restate_identity::with_identity_headers(call, &delivery.identity),
    )
    .send();
    let _progress_invocation_id = handle.invocation_id().await?;
    if let Some(terminal) = delivery.terminal {
        if !matches!(
            terminal.status,
            moa_execution::state::ExecutionRunStatus::Completed
                | moa_execution::state::ExecutionRunStatus::Cancelled
        ) {
            moa_execution::wire::execution_failure_disposition(terminal.status)
                .map_err(crate::workflows::errors::execution_error_to_handler_error)?;
        }
        let call = ctx
            .object_client::<SessionClient>(delivery.session_id.to_string())
            .execution_terminal(Json::from(terminal));
        let handle = crate::restate_identity::replay_safe_request(
            crate::restate_identity::with_identity_headers(call, &delivery.identity),
        )
        .send();
        let _terminal_invocation_id = handle.invocation_id().await?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControllerProgressDeliveryWire {
    identity: Identity,
    session_id: SessionId,
    progress: ExecutionProgress,
    terminal: Option<moa_execution::wire::ExecutionTerminalDelivery>,
}
