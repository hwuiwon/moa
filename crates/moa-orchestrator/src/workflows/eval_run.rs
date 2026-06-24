//! Restate workflow that executes one hosted eval run.

use moa_core::wire::{
    EvalRunRequest, EvalRunResponse, EvalRunStatus, EvalRunStatusRequest, EvalRunStatusResponse,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::services::eval::{EvalServiceError, failed_eval_run_response};
use crate::services::eval::{
    execute_eval_run_request_isolated, status_response_from_run_response, verify_run_status_tenant,
};

const K_TENANT_ID: &str = "tenant_id";
const K_STATUS: &str = "status";
const K_RESPONSE: &str = "response";

/// Workflow input for one hosted eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunWorkflowRequest {
    /// Server-assigned hosted run identifier.
    pub run_id: Uuid,
    /// Client eval run request.
    pub request: EvalRunRequest,
}

/// Restate workflow surface for one hosted eval run.
#[restate_sdk::workflow]
pub trait EvalRun {
    /// Executes the hosted eval run.
    async fn run(
        request: Json<EvalRunWorkflowRequest>,
    ) -> Result<Json<EvalRunResponse>, HandlerError>;

    /// Reads current hosted eval run status.
    #[shared]
    async fn status(
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError>;
}

/// Concrete hosted eval workflow implementation.
pub struct EvalRunImpl;

impl EvalRun for EvalRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Eval/run after the tenant operator check.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<EvalRunWorkflowRequest>,
    ) -> Result<Json<EvalRunResponse>, HandlerError> {
        annotate_restate_handler_span("EvalRun", "run");
        let request = request.into_inner();
        ctx.set(K_TENANT_ID, Json(request.request.tenant_id));
        ctx.set(K_STATUS, Json(EvalRunStatus::Running));
        let tenant_id = request.request.tenant_id;
        let run_id = request.run_id;
        let response = execute_eval_run_request_isolated(request.run_id, request.request).await;
        let response = if response.run_id == run_id {
            response
        } else {
            failed_eval_run_response(
                tenant_id,
                run_id,
                format!(
                    "eval workflow produced mismatched run id {}",
                    response.run_id
                ),
            )
        };
        ctx.set(K_STATUS, Json(response.status));
        ctx.set(K_RESPONSE, Json(response.clone()));
        Ok(Json(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Eval/run_status after the tenant operator check.
    async fn status(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("EvalRun", "status");
        let request = request.into_inner();
        if request.run_id.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "eval run id mismatch").into());
        }
        let tenant_id = ctx
            .get::<Json<moa_core::TenantId>>(K_TENANT_ID)
            .await?
            .map(Json::into_inner)
            .ok_or_else(|| TerminalError::new_with_code(404, "eval run not found"))?;
        let response = if let Some(response) = ctx
            .get::<Json<EvalRunResponse>>(K_RESPONSE)
            .await?
            .map(Json::into_inner)
        {
            status_response_from_run_response(&response)
        } else {
            let status = ctx
                .get::<Json<EvalRunStatus>>(K_STATUS)
                .await?
                .map(Json::into_inner)
                .unwrap_or(EvalRunStatus::Pending);
            EvalRunStatusResponse {
                tenant_id,
                run_id: request.run_id,
                status,
                suite_name: None,
                exit_code: None,
                summary: None,
                results: Vec::new(),
                error: None,
            }
        };
        verify_run_status_tenant(request.tenant_id, &response)
            .map_err(eval_error_to_handler_error)?;
        Ok(Json(response))
    }
}

fn eval_error_to_handler_error(error: EvalServiceError) -> HandlerError {
    TerminalError::new_with_code(404, error.to_string()).into()
}
