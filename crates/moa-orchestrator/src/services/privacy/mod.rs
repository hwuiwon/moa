//! Restate service for protected privacy export and erasure operations.

mod approval;
mod context;
mod erase;
mod export;
mod manifest;
pub mod repository;

pub use approval::{ApprovalClaims, ApprovalTokenVerifier, ensure_jti_inserted};
pub use context::{
    PrivacyEraseContext, PrivacyExportContext, PrivacySubject, PrivacySubjectProvenance,
};
pub use erase::{
    DUAL_CONTROL_OPERATION_ERASE, ensure_erase_dual_control, erase_operation_ref, run_privacy_erase,
};
pub use export::write_export_readme;
pub use manifest::{Ed25519ManifestSigner, finalize_archive_to_bytes, write_manifest};

use moa_authz_schema::Relation;
use moa_core::config::ComplianceConfig;
use moa_core::wire::privacy::{
    ApproveDualControlRequest, ApproveDualControlResponse, PlaceLegalHoldRequest,
    PlaceLegalHoldResponse, PrivacyEraseRequest, PrivacyEraseResponse, PrivacyExportRequest,
    PrivacyExportResponse, ReleaseLegalHoldRequest, ReleaseLegalHoldResponse,
    RequestErasureApprovalRequest, RequestErasureApprovalResponse,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use std::sync::Arc;

use crate::handlers::authz_shim::authorize_tenant;

use self::export::execute_privacy_export;

/// Restate service surface for protected privacy administration.
#[restate_sdk::service]
#[name = "Privacy"]
pub trait Privacy {
    /// Exports privacy data for one subject after admin authorization.
    async fn export(
        request: Json<PrivacyExportRequest>,
    ) -> Result<Json<PrivacyExportResponse>, HandlerError>;

    /// Erases privacy data for one subject after admin authorization.
    async fn erase(
        request: Json<PrivacyEraseRequest>,
    ) -> Result<Json<PrivacyEraseResponse>, HandlerError>;

    /// Raises a four-eyes dual-control request for a privacy erasure (first admin).
    async fn request_erasure_approval(
        request: Json<RequestErasureApprovalRequest>,
    ) -> Result<Json<RequestErasureApprovalResponse>, HandlerError>;

    /// Approves a pending dual-control request as a distinct second admin.
    async fn approve_dual_control(
        request: Json<ApproveDualControlRequest>,
    ) -> Result<Json<ApproveDualControlResponse>, HandlerError>;

    /// Places a litigation/finance legal hold after admin authorization.
    async fn place_legal_hold(
        request: Json<PlaceLegalHoldRequest>,
    ) -> Result<Json<PlaceLegalHoldResponse>, HandlerError>;

    /// Releases an active legal hold after admin authorization.
    async fn release_legal_hold(
        request: Json<ReleaseLegalHoldRequest>,
    ) -> Result<Json<ReleaseLegalHoldResponse>, HandlerError>;
}

/// Concrete privacy service implementation.
#[derive(Clone)]
pub struct PrivacyImpl {
    pool: sqlx::PgPool,
    compliance: ComplianceConfig,
    kms: Arc<dyn moa_crypto::KeyManagementProvider>,
}

impl PrivacyImpl {
    /// Creates the privacy adapter with its repository pool and approval configuration.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        compliance: ComplianceConfig,
        kms: Arc<dyn moa_crypto::KeyManagementProvider>,
    ) -> Self {
        Self {
            pool,
            compliance,
            kms,
        }
    }
}

impl Privacy for PrivacyImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<PrivacyExportRequest>,
    ) -> Result<Json<PrivacyExportResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "export");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let claims = ApprovalTokenVerifier::from_config(&self.compliance)?.verify(
            &request.approval_token,
            "export",
            &subject_user_id,
            request.tenant_id,
        )?;
        let pool = self.pool.clone();
        let compliance_config = self.compliance.clone();

        Ok(ctx
            .run(|| async move {
                execute_privacy_export(pool, request.tenant_id, request, claims, compliance_config)
                    .await
                    .map(Json::from)
            })
            .name("privacy_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn erase(
        &self,
        ctx: Context<'_>,
        request: Json<PrivacyEraseRequest>,
    ) -> Result<Json<PrivacyEraseResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "erase");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let claims = ApprovalTokenVerifier::from_config(&self.compliance)?.verify(
            &request.approval_token,
            "erase",
            &subject_user_id,
            request.tenant_id,
        )?;
        let pool = self.pool.clone();
        let erase_ctx = PrivacyEraseContext::from_request(
            pool,
            request,
            claims,
            &self.compliance,
            self.kms.clone(),
        )?;

        Ok(ctx
            .run(|| async move { run_privacy_erase(erase_ctx).await.map(Json::from) })
            .name("privacy_erase")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn request_erasure_approval(
        &self,
        ctx: Context<'_>,
        request: Json<RequestErasureApprovalRequest>,
    ) -> Result<Json<RequestErasureApprovalResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "request_erasure_approval");
        let request = request.into_inner();
        // Raising a dual-control request is privileged; gate it on tenant admin,
        // exactly like the erasure it guards. The distinct-approver (SoD) rule is
        // enforced at approval time, not here.
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let requested_by = identity.id.to_string();
        let operation_ref = erase_operation_ref(
            request.tenant_id,
            request.subject_user_id.as_str(),
            request.contact_erasure_scope,
            &request.reason,
        );

        Ok(ctx
            .run(|| async move {
                let request_id = crate::services::dual_control::request(
                    &pool,
                    request.tenant_id,
                    DUAL_CONTROL_OPERATION_ERASE,
                    &operation_ref,
                    &requested_by,
                )
                .await
                .map_err(crate::services::dual_control::DualControlError::into_handler_error)?;
                Ok(Json::from(RequestErasureApprovalResponse { request_id }))
            })
            .name("privacy_request_erasure_approval")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn approve_dual_control(
        &self,
        ctx: Context<'_>,
        request: Json<ApproveDualControlRequest>,
    ) -> Result<Json<ApproveDualControlResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "approve_dual_control");
        let request = request.into_inner();
        // Approving is privileged; gate it on tenant admin. The dual-control
        // registry additionally rejects an approver that is the requester
        // (segregation of duties).
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let approver = identity.id.to_string();

        Ok(ctx
            .run(|| async move {
                crate::services::dual_control::approve(
                    &pool,
                    request.tenant_id,
                    request.request_id,
                    &approver,
                )
                .await
                .map_err(crate::services::dual_control::DualControlError::into_handler_error)?;
                Ok(Json::from(ApproveDualControlResponse { approved: true }))
            })
            .name("privacy_approve_dual_control")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn place_legal_hold(
        &self,
        ctx: Context<'_>,
        request: Json<PlaceLegalHoldRequest>,
    ) -> Result<Json<PlaceLegalHoldResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "place_legal_hold");
        let request = request.into_inner();
        // Placing a legal hold is a privileged compliance mutation; the registry
        // performs no authorization, so gate it on tenant admin here.
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let placed_by = identity.id.to_string();

        Ok(ctx
            .run(|| async move {
                let hold = moa_memory_pii::legal_hold::place_hold(
                    &pool,
                    request.tenant_id,
                    request.subject_id,
                    &request.reason,
                    &placed_by,
                )
                .await
                .map_err(handler_error)?;
                Ok(Json::from(PlaceLegalHoldResponse {
                    hold_id: hold.id,
                    tenant_id: hold.tenant_id,
                    subject_id: hold.subject_id,
                }))
            })
            .name("privacy_place_legal_hold")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn release_legal_hold(
        &self,
        ctx: Context<'_>,
        request: Json<ReleaseLegalHoldRequest>,
    ) -> Result<Json<ReleaseLegalHoldResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "release_legal_hold");
        let request = request.into_inner();
        // Releasing a legal hold is privileged; gate it on tenant admin.
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        let released_by = identity.id.to_string();

        Ok(ctx
            .run(|| async move {
                let released = moa_memory_pii::legal_hold::release_hold(
                    &pool,
                    request.tenant_id,
                    request.hold_id,
                    &released_by,
                )
                .await
                .map_err(handler_error)?;
                Ok(Json::from(ReleaseLegalHoldResponse {
                    hold_id: request.hold_id,
                    released,
                }))
            })
            .name("privacy_release_legal_hold")
            .await?)
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
