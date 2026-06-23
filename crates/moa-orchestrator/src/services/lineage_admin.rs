//! Restate service for protected lineage explain, query, export, verify, and erase operations.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::wire::{
    LineageEraseRequest, LineageEraseResponse, LineageExplainRequest, LineageExplainResponse,
    LineageExportRequest, LineageExportResponse, LineageQueryRequest, LineageQueryResponse,
    LineageVerifyRequest, LineageVerifyResponse,
};
use moa_core::{TenantId, WorkspaceId};
use moa_lineage_audit::admin as lineage_audit_admin;
use moa_lineage_sink::admin as lineage_sink_admin;
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

const PII_VAULT_SECRET_HEX_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET_HEX";

/// Restate service surface for protected lineage administration.
#[restate_sdk::service]
#[name = "LineageAdmin"]
pub trait LineageAdmin {
    /// Explains lineage records for one tenant-scoped session or turn.
    async fn explain(
        request: Json<LineageExplainRequest>,
    ) -> Result<Json<LineageExplainResponse>, HandlerError>;

    /// Runs a read-only tenant-scoped lineage query.
    async fn query(
        request: Json<LineageQueryRequest>,
    ) -> Result<Json<LineageQueryResponse>, HandlerError>;

    /// Exports a tenant-scoped lineage DSAR bundle.
    async fn export(
        request: Json<LineageExportRequest>,
    ) -> Result<Json<LineageExportResponse>, HandlerError>;

    /// Verifies tenant-scoped lineage hash-chain integrity.
    async fn verify(
        request: Json<LineageVerifyRequest>,
    ) -> Result<Json<LineageVerifyResponse>, HandlerError>;

    /// Marks a tenant-scoped lineage subject pseudonym as erased.
    async fn erase(
        request: Json<LineageEraseRequest>,
    ) -> Result<Json<LineageEraseResponse>, HandlerError>;
}

/// Concrete lineage administration service implementation.
#[derive(Clone, Default)]
pub struct LineageAdminImpl;

impl LineageAdmin for LineageAdminImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn explain(
        &self,
        ctx: Context<'_>,
        request: Json<LineageExplainRequest>,
    ) -> Result<Json<LineageExplainResponse>, HandlerError> {
        annotate_restate_handler_span("LineageAdmin", "explain");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { explain_inner(pool, request).await.map(Json::from) })
            .name("lineage_explain")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn query(
        &self,
        ctx: Context<'_>,
        request: Json<LineageQueryRequest>,
    ) -> Result<Json<LineageQueryResponse>, HandlerError> {
        annotate_restate_handler_span("LineageAdmin", "query");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { query_inner(pool, request).await.map(Json::from) })
            .name("lineage_query")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<LineageExportRequest>,
    ) -> Result<Json<LineageExportResponse>, HandlerError> {
        annotate_restate_handler_span("LineageAdmin", "export");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { export_inner(pool, request).await.map(Json::from) })
            .name("lineage_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn verify(
        &self,
        ctx: Context<'_>,
        request: Json<LineageVerifyRequest>,
    ) -> Result<Json<LineageVerifyResponse>, HandlerError> {
        annotate_restate_handler_span("LineageAdmin", "verify");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { verify_inner(pool, request).await.map(Json::from) })
            .name("lineage_verify")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn erase(
        &self,
        ctx: Context<'_>,
        request: Json<LineageEraseRequest>,
    ) -> Result<Json<LineageEraseResponse>, HandlerError> {
        annotate_restate_handler_span("LineageAdmin", "erase");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { erase_inner(pool, request).await.map(Json::from) })
            .name("lineage_erase")
            .await?)
    }
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
    relation: Relation,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .map_err(translate_authz_error)
}

fn storage_workspace_id(tenant_id: TenantId) -> WorkspaceId {
    WorkspaceId::new(tenant_id.to_string())
}

async fn explain_inner(
    pool: PgPool,
    request: LineageExplainRequest,
) -> Result<LineageExplainResponse, HandlerError> {
    let workspace_id = storage_workspace_id(request.tenant_id);
    let records = lineage_sink_admin::explain_records(&pool, &workspace_id, request.id)
        .await
        .map_err(handler_error)?;
    Ok(LineageExplainResponse {
        id: request.id,
        records,
    })
}

async fn query_inner(
    pool: PgPool,
    request: LineageQueryRequest,
) -> Result<LineageQueryResponse, HandlerError> {
    if request.cold {
        return Err(TerminalError::new_with_code(
            400,
            "cold lineage query is not supported until a tenant-admin cold-object API exists",
        )
        .into());
    }
    let prepared = prepare_lineage_sql(&request.sql)?;
    let workspace_id = storage_workspace_id(request.tenant_id);
    let mut tx = pool.begin().await.map_err(handler_error)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    let rows = lineage_sink_admin::execute_prepared_lineage_query(
        &mut tx,
        &prepared,
        &workspace_id,
        &request.since,
    )
    .await
    .map_err(handler_error)?;
    tx.commit().await.map_err(handler_error)?;
    Ok(LineageQueryResponse { rows })
}

async fn export_inner(
    pool: PgPool,
    request: LineageExportRequest,
) -> Result<LineageExportResponse, HandlerError> {
    let workspace_id = storage_workspace_id(request.tenant_id);
    let records =
        lineage_sink_admin::load_dsar_export_records(&pool, &workspace_id, &request.subject)
            .await
            .map_err(handler_error)?;
    let export_dir = create_temp_dir("moa-lineage-export").await?;
    let bundle_path = export_dir.join("lineage-dsar.zip");
    let bundle = lineage_audit_admin::export_dsar_bundle(
        workspace_id.as_str(),
        &request.subject,
        records,
        &bundle_path,
    )
    .await
    .map_err(handler_error)?;
    let archive = tokio::fs::read(&bundle_path).await.map_err(handler_error)?;
    cleanup_temp_dir(&export_dir).await;

    Ok(LineageExportResponse {
        bundle_uri: "inline:lineage-dsar.zip".to_string(),
        record_count: bundle.record_count,
        subject_hash: blake3::hash(&bundle.subject_pseudonym).to_hex().to_string(),
        archive_base64: Some(BASE64_STANDARD.encode(archive)),
    })
}

async fn verify_inner(
    pool: PgPool,
    request: LineageVerifyRequest,
) -> Result<LineageVerifyResponse, HandlerError> {
    let workspace_id = storage_workspace_id(request.tenant_id);
    if request.window == "hot" || request.window == "db" {
        let rows = lineage_audit_admin::load_compliance_rows_for_interval(
            &pool,
            workspace_id.as_str(),
            &request.since,
        )
        .await
        .map_err(handler_error)?;
        let report =
            lineage_audit_admin::verify_compliance_rows(rows, None).map_err(handler_error)?;
        return Ok(LineageVerifyResponse {
            tenant_id: request.tenant_id,
            records: usize_to_u64(report.records),
            root_checked: false,
            status: "ok".to_string(),
            root_id: None,
        });
    }

    let root = lineage_audit_admin::load_audit_root(&pool, workspace_id.as_str(), &request.window)
        .await
        .map_err(handler_error)?;
    let rows = lineage_audit_admin::load_compliance_rows_for_window(
        &pool,
        workspace_id.as_str(),
        root.window_start,
        root.window_end,
    )
    .await
    .map_err(handler_error)?;
    let report = lineage_audit_admin::verify_compliance_rows(rows, Some(root.merkle_root))
        .map_err(handler_error)?;
    Ok(LineageVerifyResponse {
        tenant_id: request.tenant_id,
        records: usize_to_u64(report.records),
        root_checked: true,
        status: "ok".to_string(),
        root_id: Some(root.root_id),
    })
}

async fn erase_inner(
    pool: PgPool,
    request: LineageEraseRequest,
) -> Result<LineageEraseResponse, HandlerError> {
    let workspace_id = storage_workspace_id(request.tenant_id);
    let subject = hex::decode(request.subject.trim()).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("subject must be a hex-encoded pseudonym: {error}"),
        )
    })?;
    let secret = pii_vault_secret_from_env()?.unwrap_or_default();
    let subjects = lineage_audit_admin::erase_subject_pseudonym(
        &pool,
        workspace_id.as_str(),
        &subject,
        secret,
        "lineage-erase",
    )
    .await
    .map_err(handler_error)?;
    Ok(LineageEraseResponse {
        tenant_id: request.tenant_id,
        subjects,
        status: "scheduled".to_string(),
    })
}

/// Prepares a read-only logical lineage SQL query against a scoped hot-store subquery.
pub fn prepare_lineage_sql(sql: &str) -> Result<String, HandlerError> {
    let trimmed = sql.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select ") || lower.starts_with("with ")) {
        return Err(
            TerminalError::new_with_code(400, "only SELECT or WITH queries are permitted").into(),
        );
    }
    if trimmed.contains(';') {
        return Err(TerminalError::new_with_code(
            400,
            "semicolon-separated statements are not permitted",
        )
        .into());
    }
    let Some(idx) = lower.find("from lineage") else {
        return Err(TerminalError::new_with_code(
            400,
            "query must use `FROM lineage` as the source table",
        )
        .into());
    };
    let replacement = "FROM (SELECT * FROM analytics.turn_lineage \
        WHERE workspace_id = $1 AND ts > now() - ($2::text)::interval) lineage";
    let mut prepared = String::with_capacity(trimmed.len() + replacement.len());
    prepared.push_str(&trimmed[..idx]);
    prepared.push_str(replacement);
    prepared.push_str(&trimmed[idx + "from lineage".len()..]);
    Ok(prepared)
}

fn pii_vault_secret_from_env() -> Result<Option<Vec<u8>>, HandlerError> {
    std::env::var(PII_VAULT_SECRET_HEX_ENV)
        .ok()
        .map(|secret_hex| {
            hex::decode(secret_hex.trim()).map_err(|error| {
                TerminalError::new_with_code(
                    400,
                    format!("{PII_VAULT_SECRET_HEX_ENV} must be hex-encoded: {error}"),
                )
                .into()
            })
        })
        .transpose()
}

async fn create_temp_dir(prefix: &str) -> Result<std::path::PathBuf, HandlerError> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::now_v7()));
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(handler_error)?;
    Ok(path)
}

async fn cleanup_temp_dir(path: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        tracing::warn!(path = %path.display(), %error, "failed to remove lineage export staging directory");
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
