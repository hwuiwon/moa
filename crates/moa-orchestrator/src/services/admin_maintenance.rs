//! Hosted administrative maintenance APIs for vector promotion and checkpoints.

use std::sync::Arc;

use moa_authz_schema::{ObjectType, Relation};
use moa_config::MoaConfig;
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::memory::RlsContext;
use moa_core::{
    WORKSPACE_ID, traits::BranchManager, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_memory_vector::{
    PromotionOptions, PromotionReport, TurbopufferStore, VectorPartitionPromotion,
    VectorStoreFactory, finalize_promotion, rollback_promotion,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::admin::{
    CheckpointCleanupResponse, CheckpointCreateRequest, CheckpointCreateResponse,
    CheckpointListResponse, CheckpointRollbackRequest, CheckpointRollbackResponse,
    VectorPromoteRequest, VectorPromotionResponse, VectorPromotionUpdateRequest,
};
use restate_sdk::prelude::*;

use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{AuthzEnforcer, require_identity};

/// Restate service for user-facing administrative maintenance operations.
#[restate_sdk::service]
#[name = "AdminMaintenance"]
pub trait AdminMaintenance {
    /// Promotes one tenant's vector namespace to a new backend.
    async fn promote_tenant_vector(
        request: Json<VectorPromoteRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError>;

    /// Rolls back an in-flight vector promotion.
    async fn rollback_promotion(
        request: Json<VectorPromotionUpdateRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError>;

    /// Finalizes a successful vector promotion.
    async fn finalize_promotion(
        request: Json<VectorPromotionUpdateRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError>;

    /// Creates a managed Neon checkpoint branch.
    async fn checkpoint_create(
        request: Json<CheckpointCreateRequest>,
    ) -> Result<Json<CheckpointCreateResponse>, HandlerError>;

    /// Lists managed Neon checkpoint branches.
    async fn checkpoint_list(
        request: Json<serde_json::Value>,
    ) -> Result<Json<CheckpointListResponse>, HandlerError>;

    /// Prepares rollback to a managed Neon checkpoint branch.
    async fn checkpoint_rollback(
        request: Json<CheckpointRollbackRequest>,
    ) -> Result<Json<CheckpointRollbackResponse>, HandlerError>;

    /// Cleans up expired managed Neon checkpoint branches.
    async fn checkpoint_cleanup(
        request: Json<serde_json::Value>,
    ) -> Result<Json<CheckpointCleanupResponse>, HandlerError>;
}

/// Concrete administrative maintenance implementation.
#[derive(Clone)]
pub struct AdminMaintenanceImpl {
    pool: sqlx::PgPool,
    config: Arc<MoaConfig>,
    authz: AuthzEnforcer,
}

impl AdminMaintenanceImpl {
    /// Creates the maintenance adapter with its storage and backend configuration.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, config: Arc<MoaConfig>, authz: AuthzEnforcer) -> Self {
        Self {
            pool,
            config,
            authz,
        }
    }
}

impl AdminMaintenance for AdminMaintenanceImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn promote_tenant_vector(
        &self,
        ctx: Context<'_>,
        request: Json<VectorPromoteRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "promote_tenant_vector");
        let request = request.into_inner();
        authorize_tenant_admin_for_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let pool = self.pool.clone();
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let storage_partition_id =
                    StoragePartitionId::for_tenant(request.tenant_id).to_string();
                let scope = RlsContext::tenant(request.tenant_id);
                let pgvector = VectorStoreFactory::from_config(&config)
                    .pgvector_source_for_control_plane(pool.clone(), scope);
                let turbopuffer = Arc::new(
                    TurbopufferStore::from_config(&config)
                        .map_err(|error| {
                            TerminalError::new(format!("loading Turbopuffer client: {error}"))
                        })?
                        .with_storage_partition_id(storage_partition_id.clone()),
                );
                let promotion = VectorPartitionPromotion::new(pool, pgvector, turbopuffer);
                let report = promotion
                    .promote(PromotionOptions {
                        storage_partition_id,
                        target_backend: request.target_backend,
                        validate_percent: request.validate_percent,
                        dual_read_hours: request.dual_read_hours,
                    })
                    .await
                    .map_err(|error| {
                        TerminalError::new(format!("promote tenant vector: {error}"))
                    })?;
                Ok::<_, HandlerError>(Json(promotion_response_from_report(
                    request.tenant_id,
                    report,
                    Some(request.dual_read_hours),
                )))
            })
            .name("admin_promote_tenant_vector")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn rollback_promotion(
        &self,
        ctx: Context<'_>,
        request: Json<VectorPromotionUpdateRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "rollback_promotion");
        let request = request.into_inner();
        authorize_tenant_admin_for_tenant(&self.authz, &ctx, request.tenant_id).await?;
        validate_promotion_action(&request.action, "rollback")?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                let storage_partition_id =
                    StoragePartitionId::for_tenant(request.tenant_id).to_string();
                rollback_promotion(&pool, &storage_partition_id)
                    .await
                    .map_err(|error| TerminalError::new(format!("rollback promotion: {error}")))?;
                Ok::<_, HandlerError>(Json(promotion_update_response(
                    request.tenant_id,
                    "pgvector",
                    "steady",
                )))
            })
            .name("admin_rollback_promotion")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn finalize_promotion(
        &self,
        ctx: Context<'_>,
        request: Json<VectorPromotionUpdateRequest>,
    ) -> Result<Json<VectorPromotionResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "finalize_promotion");
        let request = request.into_inner();
        authorize_tenant_admin_for_tenant(&self.authz, &ctx, request.tenant_id).await?;
        validate_promotion_action(&request.action, "finalize")?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                let storage_partition_id =
                    StoragePartitionId::for_tenant(request.tenant_id).to_string();
                finalize_promotion(&pool, &storage_partition_id)
                    .await
                    .map_err(|error| TerminalError::new(format!("finalize promotion: {error}")))?;
                Ok::<_, HandlerError>(Json(promotion_update_response(
                    request.tenant_id,
                    "turbopuffer",
                    "steady",
                )))
            })
            .name("admin_finalize_promotion")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn checkpoint_create(
        &self,
        ctx: Context<'_>,
        request: Json<CheckpointCreateRequest>,
    ) -> Result<Json<CheckpointCreateResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "checkpoint_create");
        authorize_platform_maintenance(&self.authz, &ctx).await?;
        let request = request.into_inner();
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let manager = moa_session::NeonBranchManager::from_config(config.as_ref())
                    .map_err(|error| TerminalError::new(format!("neon manager init: {error}")))?;
                let handle = manager
                    .create_checkpoint(&request.label, request.session_id)
                    .await
                    .map_err(|error| TerminalError::new(format!("create checkpoint: {error}")))?;
                Ok::<_, HandlerError>(Json(CheckpointCreateResponse { handle }))
            })
            .name("admin_checkpoint_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    async fn checkpoint_list(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<Json<CheckpointListResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "checkpoint_list");
        authorize_platform_maintenance(&self.authz, &ctx).await?;
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let manager = moa_session::NeonBranchManager::from_config(config.as_ref())
                    .map_err(|error| TerminalError::new(format!("neon manager init: {error}")))?;
                let checkpoints = manager
                    .list_checkpoints()
                    .await
                    .map_err(|error| TerminalError::new(format!("list checkpoints: {error}")))?;
                Ok::<_, HandlerError>(Json(CheckpointListResponse { checkpoints }))
            })
            .name("admin_checkpoint_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn checkpoint_rollback(
        &self,
        ctx: Context<'_>,
        request: Json<CheckpointRollbackRequest>,
    ) -> Result<Json<CheckpointRollbackResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "checkpoint_rollback");
        authorize_platform_maintenance(&self.authz, &ctx).await?;
        let request = request.into_inner();
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let manager = moa_session::NeonBranchManager::from_config(config.as_ref())
                    .map_err(|error| TerminalError::new(format!("neon manager init: {error}")))?;
                let checkpoint = manager
                    .get_checkpoint(&request.id)
                    .await
                    .map_err(|error| TerminalError::new(format!("load checkpoint: {error}")))?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(
                            404,
                            format!("checkpoint {} not found", request.id),
                        )
                    })?;
                manager
                    .rollback_to(&checkpoint.handle)
                    .await
                    .map_err(|error| TerminalError::new(format!("rollback checkpoint: {error}")))?;
                Ok::<_, HandlerError>(Json(CheckpointRollbackResponse {
                    handle: checkpoint.handle,
                }))
            })
            .name("admin_checkpoint_rollback")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, _request))]
    async fn checkpoint_cleanup(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<Json<CheckpointCleanupResponse>, HandlerError> {
        annotate_restate_handler_span("AdminMaintenance", "checkpoint_cleanup");
        authorize_platform_maintenance(&self.authz, &ctx).await?;
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let manager = moa_session::NeonBranchManager::from_config(config.as_ref())
                    .map_err(|error| TerminalError::new(format!("neon manager init: {error}")))?;
                let deleted = manager
                    .cleanup_expired()
                    .await
                    .map_err(|error| TerminalError::new(format!("cleanup checkpoints: {error}")))?;
                Ok::<_, HandlerError>(Json(CheckpointCleanupResponse {
                    deleted_expired_checkpoints: u64::from(deleted),
                }))
            })
            .name("admin_checkpoint_cleanup")
            .await?)
    }
}

/// Converts a promotion report into the hosted API response DTO.
#[must_use]
pub fn promotion_response_from_report(
    tenant_id: TenantId,
    report: PromotionReport,
    dual_read_hours: Option<u32>,
) -> VectorPromotionResponse {
    VectorPromotionResponse {
        tenant_id,
        copied_vectors: u64::try_from(report.copied).unwrap_or(u64::MAX),
        validation_overlap: report.validation_overlap,
        vector_backend: report.vector_backend,
        vector_backend_state: report.vector_backend_state,
        dual_read_hours,
    }
}

/// Builds a response for a promotion state update.
#[must_use]
pub fn promotion_update_response(
    tenant_id: TenantId,
    backend: impl Into<String>,
    state: impl Into<String>,
) -> VectorPromotionResponse {
    VectorPromotionResponse {
        tenant_id,
        copied_vectors: 0,
        validation_overlap: 1.0,
        vector_backend: backend.into(),
        vector_backend_state: state.into(),
        dual_read_hours: None,
    }
}

fn validate_promotion_action(actual: &str, expected: &str) -> Result<(), HandlerError> {
    if actual != expected {
        return Err(TerminalError::new_with_code(
            400,
            format!("promotion action must be `{expected}`"),
        )
        .into());
    }
    Ok(())
}

async fn authorize_tenant_admin_for_tenant(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    authz
        .authorize_tenant(ctx, tenant_id, Relation::Admin)
        .await?;
    Ok(())
}

/// Authorizes deployment-global checkpoint maintenance.
///
/// Checkpoint branches are platform resources, so callers must be service
/// identities with the canonical deployment workspace admin relation.
pub async fn authorize_platform_maintenance(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
) -> Result<(), HandlerError> {
    let identity = platform_maintenance_identity(ctx)?;
    let fga = authz.require_fga_client()?;
    crate::handlers::authz_shim::journal_context_authz(
        ctx,
        fga,
        identity,
        ObjectType::Workspace,
        WORKSPACE_ID,
        Relation::Admin,
    )
    .await
}

/// Loads and validates the caller shape required before workspace admin authz.
pub fn platform_maintenance_identity(ctx: &impl RequestHeaders) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    if identity.identity_type != IdentityType::Service || identity.api_key_id.is_some() {
        return Err(TerminalError::new_with_code(
            403,
            "platform maintenance requires service workspace admin",
        )
        .into());
    }
    Ok(identity)
}
