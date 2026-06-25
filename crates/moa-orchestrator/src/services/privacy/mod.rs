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
pub use erase::run_privacy_erase;
pub use export::write_export_readme;
pub use manifest::{Ed25519ManifestSigner, finalize_archive_to_bytes, write_manifest};

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::TenantId;
use moa_core::traits::Identity;
use moa_core::wire::privacy::{
    PrivacyEraseRequest, PrivacyEraseResponse, PrivacyExportRequest, PrivacyExportResponse,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

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
}

/// Concrete privacy service implementation.
#[derive(Clone, Default)]
pub struct PrivacyImpl;

impl Privacy for PrivacyImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<PrivacyExportRequest>,
    ) -> Result<Json<PrivacyExportResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "export");
        let request = request.into_inner();
        authorize_tenant_admin(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let config = OrchestratorCtx::current_config();
        let claims = ApprovalTokenVerifier::from_config(&config.compliance)?.verify(
            &request.approval_token,
            "export",
            &subject_user_id,
            request.tenant_id,
        )?;
        let pool = OrchestratorCtx::current_graph_pool();
        let compliance_config = config.compliance.clone();

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
        authorize_tenant_admin(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let config = OrchestratorCtx::current_config();
        let claims = ApprovalTokenVerifier::from_config(&config.compliance)?.verify(
            &request.approval_token,
            "erase",
            &subject_user_id,
            request.tenant_id,
        )?;
        let pool = OrchestratorCtx::current_graph_pool();
        let erase_ctx =
            PrivacyEraseContext::from_request(pool, request, claims, &config.compliance)?;

        Ok(ctx
            .run(|| async move { run_privacy_erase(erase_ctx).await.map(Json::from) })
            .name("privacy_erase")
            .await?)
    }
}

async fn authorize_tenant_admin(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .map_err(translate_authz_error)?;
    Ok(identity)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
