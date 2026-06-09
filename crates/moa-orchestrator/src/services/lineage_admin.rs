//! Restate service for protected lineage explain, query, export, verify, and erase operations.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::wire::{
    LineageEraseRequest, LineageEraseResponse, LineageExplainRequest, LineageExplainResponse,
    LineageExportRequest, LineageExportResponse, LineageQueryRequest, LineageQueryResponse,
    LineageRecordView, LineageVerifyRequest, LineageVerifyResponse,
};
use moa_core::{SessionId, UserId, WorkspaceId};
use moa_lineage_audit::{
    DsarExporter, HashChain, PiiVault, SigningKey, blake3_merkle_root, hash_from_slice,
};
use restate_sdk::prelude::*;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

const PII_VAULT_SECRET_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET";
const PII_VAULT_SECRET_HEX_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET_HEX";

/// Restate service surface for protected lineage administration.
#[restate_sdk::service]
#[name = "LineageAdmin"]
pub trait LineageAdmin {
    /// Explains lineage records for one workspace-scoped session or turn.
    async fn explain(
        request: Json<LineageExplainRequest>,
    ) -> Result<Json<LineageExplainResponse>, HandlerError>;

    /// Runs a read-only workspace-scoped lineage query.
    async fn query(
        request: Json<LineageQueryRequest>,
    ) -> Result<Json<LineageQueryResponse>, HandlerError>;

    /// Exports a workspace-scoped lineage DSAR bundle.
    async fn export(
        request: Json<LineageExportRequest>,
    ) -> Result<Json<LineageExportResponse>, HandlerError>;

    /// Verifies workspace-scoped lineage hash-chain integrity.
    async fn verify(
        request: Json<LineageVerifyRequest>,
    ) -> Result<Json<LineageVerifyResponse>, HandlerError>;

    /// Marks a workspace-scoped lineage subject pseudonym as erased.
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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { erase_inner(pool, request).await.map(Json::from) })
            .name("lineage_erase")
            .await?)
    }
}

/// One compliance lineage row used by hash-chain verification.
#[derive(Debug, Clone)]
pub struct ComplianceRow {
    /// Turn identifier for the row.
    pub turn_id: Uuid,
    /// Numeric lineage record kind.
    pub record_kind: i16,
    /// Timestamp when the row was captured.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Canonical lineage payload.
    pub payload: Value,
    /// Stored integrity hash.
    pub integrity_hash: Vec<u8>,
    /// Stored previous-row hash.
    pub prev_hash: Option<Vec<u8>>,
}

/// Verification result for a lineage hash-chain window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationReport {
    /// Number of records verified.
    pub records: usize,
}

#[derive(Debug)]
struct AuditRootRow {
    root_id: Uuid,
    workspace_id: String,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
    merkle_root: Vec<u8>,
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        relation,
    )
    .await
    .map_err(translate_authz_error)
}

async fn explain_inner(
    pool: PgPool,
    request: LineageExplainRequest,
) -> Result<LineageExplainResponse, HandlerError> {
    let rows = sqlx::query(
        r#"
        SELECT turn_id, session_id, user_id, workspace_id, ts, record_kind, payload
        FROM analytics.turn_lineage
        WHERE workspace_id = $1 AND (session_id = $2 OR turn_id = $2)
        ORDER BY ts ASC, record_kind ASC
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(request.id)
    .fetch_all(&pool)
    .await
    .map_err(handler_error)?;

    let records = rows
        .into_iter()
        .map(lineage_record_from_row)
        .collect::<Result<Vec<_>, HandlerError>>()?;
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
    let mut tx = pool.begin().await.map_err(handler_error)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    let rows: Value = sqlx::query_scalar(&format!(
        "SELECT COALESCE(jsonb_agg(row_to_json(lineage_query)), '[]'::jsonb) \
         FROM ({prepared}) lineage_query"
    ))
    .bind(request.workspace_id.as_str())
    .bind(&request.since)
    .fetch_one(&mut *tx)
    .await
    .map_err(handler_error)?;
    tx.commit().await.map_err(handler_error)?;
    Ok(LineageQueryResponse { rows })
}

async fn export_inner(
    pool: PgPool,
    request: LineageExportRequest,
) -> Result<LineageExportResponse, HandlerError> {
    let pattern = format!("%{}%", request.subject);
    let records: Vec<Value> = sqlx::query_scalar(
        r#"
        SELECT row_to_json(lineage_row)::jsonb
        FROM (
            SELECT turn_id, session_id, user_id, workspace_id, ts, record_kind, payload,
                   integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1 AND payload::text ILIKE $2
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            LIMIT 10000
        ) lineage_row
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(pattern)
    .fetch_all(&pool)
    .await
    .map_err(handler_error)?;

    let export_dir = create_temp_dir("moa-lineage-export").await?;
    let bundle_path = export_dir.join("lineage-dsar.zip");
    let signing = service_signing_key("dsar-export");
    let exporter = DsarExporter::new(signing);
    let subject_pseudonym = request.subject.as_bytes().to_vec();
    let bundle = exporter
        .export_records(
            request.workspace_id.as_str(),
            subject_pseudonym,
            records,
            Vec::new(),
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
    if request.window == "hot" || request.window == "db" {
        let rows =
            load_compliance_rows_for_interval(&pool, &request.workspace_id, &request.since).await?;
        let report = verify_compliance_rows(rows, None)?;
        return Ok(LineageVerifyResponse {
            workspace_id: request.workspace_id,
            records: usize_to_u64(report.records),
            root_checked: false,
            status: "ok".to_string(),
            root_id: None,
        });
    }

    let root = load_audit_root(&pool, &request.workspace_id, &request.window).await?;
    let rows = load_compliance_rows_for_window(
        &pool,
        &request.workspace_id,
        root.window_start,
        root.window_end,
    )
    .await?;
    let report = verify_compliance_rows(rows, Some(root.merkle_root))?;
    Ok(LineageVerifyResponse {
        workspace_id: WorkspaceId::new(root.workspace_id),
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
    let subject = hex::decode(request.subject.trim()).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("subject must be a hex-encoded pseudonym: {error}"),
        )
    })?;
    let secret = pii_vault_secret_from_env()?.unwrap_or_default();
    let vault = PiiVault::with_pool(pool, secret, "lineage-erase");
    let subjects = vault
        .erase_subject(request.workspace_id.as_str(), &subject)
        .await
        .map_err(handler_error)?;
    Ok(LineageEraseResponse {
        workspace_id: request.workspace_id,
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

/// Verifies hash-chain links and an optional Merkle root for compliance rows.
pub fn verify_compliance_rows(
    rows: Vec<ComplianceRow>,
    expected_root: Option<Vec<u8>>,
) -> Result<VerificationReport, HandlerError> {
    let mut leaves = Vec::with_capacity(rows.len());
    let mut previous_integrity: Option<&[u8]> = None;
    for row in &rows {
        if let (Some(previous), Some(prev_hash)) = (previous_integrity, row.prev_hash.as_deref())
            && prev_hash != previous
        {
            return Err(TerminalError::new(format!(
                "chain link mismatch at turn={} kind={} ts={}",
                row.turn_id, row.record_kind, row.ts
            ))
            .into());
        }
        let prev = row
            .prev_hash
            .as_deref()
            .map(hash_from_slice)
            .transpose()
            .map_err(handler_error)?;
        let (actual, _) = HashChain::link(prev, &row.payload).map_err(handler_error)?;
        if actual.as_bytes() != row.integrity_hash.as_slice() {
            return Err(TerminalError::new(format!(
                "chain mismatch at turn={} kind={} ts={}",
                row.turn_id, row.record_kind, row.ts
            ))
            .into());
        }
        previous_integrity = Some(&row.integrity_hash);
        leaves.push(row.integrity_hash.clone());
    }
    if let Some(expected_root) = expected_root {
        let actual_root = blake3_merkle_root(&leaves).map_err(handler_error)?;
        if actual_root.as_bytes() != expected_root.as_slice() {
            return Err(TerminalError::new("merkle root mismatch for verified window").into());
        }
    }
    Ok(VerificationReport {
        records: rows.len(),
    })
}

async fn load_audit_root(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    id_or_uri: &str,
) -> Result<AuditRootRow, HandlerError> {
    let row = if let Ok(root_id) = Uuid::parse_str(id_or_uri) {
        sqlx::query(
            r#"
            SELECT root_id, workspace_id, window_start, window_end, merkle_root
            FROM analytics.audit_roots
            WHERE workspace_id = $1 AND root_id = $2
            "#,
        )
        .bind(workspace_id.as_str())
        .bind(root_id)
        .fetch_one(pool)
        .await
        .map_err(handler_error)?
    } else {
        sqlx::query(
            r#"
            SELECT root_id, workspace_id, window_start, window_end, merkle_root
            FROM analytics.audit_roots
            WHERE workspace_id = $1 AND s3_object_uri = $2
            "#,
        )
        .bind(workspace_id.as_str())
        .bind(id_or_uri)
        .fetch_one(pool)
        .await
        .map_err(handler_error)?
    };
    Ok(AuditRootRow {
        root_id: row.try_get("root_id").map_err(handler_error)?,
        workspace_id: row.try_get("workspace_id").map_err(handler_error)?,
        window_start: row.try_get("window_start").map_err(handler_error)?,
        window_end: row.try_get("window_end").map_err(handler_error)?,
        merkle_root: row.try_get("merkle_root").map_err(handler_error)?,
    })
}

async fn load_compliance_rows_for_interval(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    since: &str,
) -> Result<Vec<ComplianceRow>, HandlerError> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1
              AND prev_hash IS NOT NULL
              AND ts > now() - ($2::text)::interval
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(workspace_id.as_str())
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(handler_error)?,
    )
}

async fn load_compliance_rows_for_window(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<ComplianceRow>, HandlerError> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1
              AND prev_hash IS NOT NULL
              AND ts >= $2
              AND ts <= $3
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(workspace_id.as_str())
        .bind(window_start)
        .bind(window_end)
        .fetch_all(pool)
        .await
        .map_err(handler_error)?,
    )
}

fn load_compliance_rows(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<ComplianceRow>, HandlerError> {
    rows.into_iter()
        .map(|row| {
            Ok(ComplianceRow {
                turn_id: row.try_get("turn_id").map_err(handler_error)?,
                record_kind: row.try_get("record_kind").map_err(handler_error)?,
                ts: row.try_get("ts").map_err(handler_error)?,
                payload: row.try_get("payload").map_err(handler_error)?,
                integrity_hash: row.try_get("integrity_hash").map_err(handler_error)?,
                prev_hash: row.try_get("prev_hash").map_err(handler_error)?,
            })
        })
        .collect()
}

fn lineage_record_from_row(row: sqlx::postgres::PgRow) -> Result<LineageRecordView, HandlerError> {
    let session_id: Uuid = row.try_get("session_id").map_err(handler_error)?;
    let user_id: String = row.try_get("user_id").map_err(handler_error)?;
    let workspace_id: String = row.try_get("workspace_id").map_err(handler_error)?;
    Ok(LineageRecordView {
        turn_id: row.try_get("turn_id").map_err(handler_error)?,
        session_id: Some(SessionId(session_id)),
        workspace_id: Some(WorkspaceId::new(workspace_id)),
        user_id: Some(UserId::new(user_id)),
        ts: row.try_get("ts").map_err(handler_error)?,
        record_kind: row.try_get("record_kind").map_err(handler_error)?,
        payload: row.try_get("payload").map_err(handler_error)?,
        summary: None,
    })
}

fn service_signing_key(label: &str) -> SigningKey {
    SigningKey::from_seed(
        label.to_string(),
        *blake3::hash(format!("moa-orchestrator-local-signing:{label}").as_bytes()).as_bytes(),
    )
}

fn pii_vault_secret_from_env() -> Result<Option<Vec<u8>>, HandlerError> {
    if let Ok(secret_hex) = std::env::var(PII_VAULT_SECRET_HEX_ENV) {
        return hex::decode(secret_hex.trim()).map(Some).map_err(|error| {
            TerminalError::new_with_code(
                400,
                format!("{PII_VAULT_SECRET_HEX_ENV} must be hex-encoded: {error}"),
            )
            .into()
        });
    }
    Ok(std::env::var(PII_VAULT_SECRET_ENV)
        .ok()
        .map(|secret| secret.into_bytes()))
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
