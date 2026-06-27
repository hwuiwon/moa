//! Direct edge lineage read routes.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::config::ComplianceConfig;
use moa_core::wire::lineage::{
    LineageExplainRequest, LineageExplainResponse, LineageQueryRequest, LineageQueryResponse,
    LineageVerifyRequest, LineageVerifyResponse,
};
use moa_core::{StoragePartitionId, TenantId};
use moa_lineage_audit::SigningKey;
use moa_lineage_audit::admin as lineage_audit_admin;
use moa_lineage_sink::admin as lineage_sink_admin;

use super::{
    AppState, authenticate_direct_request, load_moa_config, parse_json_body_with_tenant,
    require_direct_authz, route_error,
};

/// Handles direct lineage explanation reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_explain(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/lineage/explain").await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let request: LineageExplainRequest =
        match parse_json_body_with_tenant(&body, identity.tenant_id) {
            Ok(request) => request,
            Err(response) => return response,
        };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match explain_inner(&state.pool, request).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

/// Handles direct lineage SQL reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/lineage/query").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let request: LineageQueryRequest = match parse_json_body_with_tenant(&body, identity.tenant_id)
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match query_inner(&state.pool, request).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

/// Handles direct lineage integrity verification reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/lineage/verify").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let request: LineageVerifyRequest = match parse_json_body_with_tenant(&body, identity.tenant_id)
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    let config = match load_moa_config() {
        Ok(config) => config.compliance,
        Err(response) => return response,
    };
    match verify_inner(&state.pool, request, config).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

/// Rejects lineage export on the direct read surface.
#[tracing::instrument(skip(state, headers))]
pub async fn handle_export_deferred(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticate_direct_request(&state, &headers, "/v1/lineage/export").await
    {
        return response;
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        "lineage export is not a direct read handler",
    )
        .into_response()
}

/// Rejects lineage erase on the direct read surface.
#[tracing::instrument(skip(state, headers))]
pub async fn handle_erase_deferred(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authenticate_direct_request(&state, &headers, "/v1/lineage/erase").await
    {
        return response;
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        "lineage erase is not a direct read handler",
    )
        .into_response()
}

async fn explain_inner(
    pool: &sqlx::PgPool,
    request: LineageExplainRequest,
) -> Result<LineageExplainResponse, Response> {
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let records = lineage_sink_admin::explain_records(pool, &storage_partition_id, request.id)
        .await
        .map_err(route_error)?;
    Ok(LineageExplainResponse {
        id: request.id,
        records,
    })
}

async fn query_inner(
    pool: &sqlx::PgPool,
    request: LineageQueryRequest,
) -> Result<LineageQueryResponse, Response> {
    if request.cold {
        return Err((
            StatusCode::BAD_REQUEST,
            "cold lineage query is not supported until a tenant-admin cold-object API exists",
        )
            .into_response());
    }
    let prepared = prepare_lineage_sql(&request.sql)?;
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let mut tx = pool.begin().await.map_err(route_error)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(route_error)?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .map_err(route_error)?;
    let rows = lineage_sink_admin::execute_prepared_lineage_query(
        &mut tx,
        &prepared,
        &storage_partition_id,
        &request.since,
    )
    .await
    .map_err(route_error)?;
    tx.commit().await.map_err(route_error)?;
    Ok(LineageQueryResponse { rows })
}

async fn verify_inner(
    pool: &sqlx::PgPool,
    request: LineageVerifyRequest,
    config: ComplianceConfig,
) -> Result<LineageVerifyResponse, Response> {
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    if request.window == "hot" || request.window == "db" {
        let rows = lineage_audit_admin::load_compliance_rows_for_interval(
            pool,
            storage_partition_id.as_str(),
            &request.since,
        )
        .await
        .map_err(route_error)?;
        let report =
            lineage_audit_admin::verify_compliance_rows(rows, None).map_err(route_error)?;
        let dead_letters = lineage_audit_admin::count_lineage_dead_letter_rows(
            pool,
            storage_partition_id.as_str(),
            None,
        )
        .await
        .map_err(route_error)?;
        return Ok(LineageVerifyResponse {
            tenant_id: request.tenant_id,
            records: usize_to_u64(report.records),
            root_checked: false,
            status: verification_status(dead_letters),
            root_id: None,
        });
    }

    let signing = configured_audit_root_signing_key(&config)?;
    let root =
        lineage_audit_admin::load_audit_root(pool, storage_partition_id.as_str(), &request.window)
            .await
            .map_err(route_error)?;
    let rows = lineage_audit_admin::load_compliance_rows_for_window(
        pool,
        storage_partition_id.as_str(),
        root.window_start,
        root.window_end,
    )
    .await
    .map_err(route_error)?;
    let dead_letters = lineage_audit_admin::count_lineage_dead_letter_rows(
        pool,
        storage_partition_id.as_str(),
        Some((root.window_start, root.window_end)),
    )
    .await
    .map_err(route_error)?;
    let report = lineage_audit_admin::verify_audit_root_window(rows, &root, &signing)
        .map_err(route_error)?;
    Ok(LineageVerifyResponse {
        tenant_id: request.tenant_id,
        records: usize_to_u64(report.records),
        root_checked: true,
        status: verification_status(dead_letters),
        root_id: Some(root.root_id),
    })
}

/// Prepares a read-only logical lineage SQL query against a scoped hot-store subquery.
pub fn prepare_lineage_sql(sql: &str) -> Result<String, Response> {
    let trimmed = sql.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select ") || lower.starts_with("with ")) {
        return Err((
            StatusCode::BAD_REQUEST,
            "only SELECT or WITH queries are permitted",
        )
            .into_response());
    }
    if trimmed.contains(';') {
        return Err((
            StatusCode::BAD_REQUEST,
            "semicolon-separated statements are not permitted",
        )
            .into_response());
    }
    let Some(idx) = lower.find("from lineage") else {
        return Err((
            StatusCode::BAD_REQUEST,
            "query must use `FROM lineage` as the source table",
        )
            .into_response());
    };
    let replacement = "FROM (SELECT * FROM analytics.turn_lineage \
        WHERE storage_partition_id = $1 AND ts > now() - ($2::text)::interval) lineage";
    let mut prepared = String::with_capacity(trimmed.len() + replacement.len());
    prepared.push_str(&trimmed[..idx]);
    prepared.push_str(replacement);
    prepared.push_str(&trimmed[idx + "from lineage".len()..]);
    Ok(prepared)
}

fn storage_partition_id_for_tenant(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

fn configured_audit_root_signing_key(config: &ComplianceConfig) -> Result<SigningKey, Response> {
    configured_signing_key_from_config(
        "MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX",
        config.lineage_audit_signing_key_hex.as_deref(),
        config.lineage_audit_signing_key_id.clone(),
    )
}

fn configured_signing_key_from_config(
    key_env: &str,
    raw: Option<&str>,
    label: String,
) -> Result<SigningKey, Response> {
    let raw = raw.ok_or_else(|| route_error(format!("{key_env} is required")))?;
    signing_key_from_material(key_env, label, raw)
}

fn signing_key_from_material(
    key_env: &str,
    label: String,
    raw: &str,
) -> Result<SigningKey, Response> {
    let bytes = decode_signing_key_material(key_env, raw)?;
    let seed = match bytes.len() {
        32 => bytes,
        64 => bytes[..32].to_vec(),
        len => {
            return Err(route_error(format!(
                "{key_env} must be 32 or 64 bytes, got {len}"
            )));
        }
    };
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| route_error(format!("{key_env} must be 32 bytes")))?;
    Ok(SigningKey::from_seed(label, seed))
}

fn decode_signing_key_material(key_env: &str, raw: &str) -> Result<Vec<u8>, Response> {
    let trimmed = raw.trim();
    if let Ok(bytes) = hex::decode(trimmed) {
        return Ok(bytes);
    }
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, trimmed).map_err(|error| {
        route_error(format!(
            "{key_env} must be hex or standard base64 key material: {error}"
        ))
    })
}

fn verification_status(dead_letter_rows: u64) -> String {
    if dead_letter_rows > 0 {
        "incomplete".to_string()
    } else {
        "ok".to_string()
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::prepare_lineage_sql;

    #[test]
    fn prepare_lineage_sql_scopes_logical_source_to_tenant_and_since() {
        // Pins: direct lineage query rewrites the logical source to a tenant-scoped hot-store subquery.
        let sql = prepare_lineage_sql("SELECT count(*) FROM lineage WHERE record_kind = 4")
            .expect("lineage query should prepare");

        assert!(sql.contains("analytics.turn_lineage"));
        assert!(sql.contains("storage_partition_id = $1"));
        assert!(sql.contains("($2::text)::interval"));
        assert!(sql.contains("record_kind = 4"));
    }

    #[test]
    fn prepare_lineage_sql_rejects_mutating_statement() {
        // Pins: direct lineage query rejects mutating SQL before any database query runs.
        let error = prepare_lineage_sql("DELETE FROM lineage")
            .expect_err("mutating lineage query should fail");

        let status = error.status();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
