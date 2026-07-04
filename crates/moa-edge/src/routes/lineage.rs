//! Direct edge lineage read routes.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::config::{ComplianceConfig, LineageAuditSigningProvider};
use moa_core::wire::lineage::{
    LineageExplainRequest, LineageExplainResponse, LineageQueryOrder, LineageQueryRequest,
    LineageQueryResponse, LineageRecordView, LineageVerifyRequest, LineageVerifyResponse,
};
use moa_core::{RlsContext, SessionId, StoragePartitionId, TenantId, UserId};
use moa_lineage_audit::admin as lineage_audit_admin;
use moa_lineage_audit::{AuditRootSigner, HttpAuditRootSigner, LocalAuditRootSigner, SigningKey};
use moa_lineage_sink::admin as lineage_sink_admin;
use sqlx::{Postgres, QueryBuilder, Row};

use super::{
    AppState, authenticate_direct_request, parse_json_body_with_tenant, require_direct_authz,
    route_error,
};

const LINEAGE_QUERY_MAX_LIMIT: u32 = 1000;

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

/// Handles direct typed lineage reads at the edge.
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
    match verify_inner(&state.pool, request, state.config.compliance.clone()).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
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
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let mut tx = moa_db::ScopedConn::begin(pool, &RlsContext::tenant(request.tenant_id))
        .await
        .map_err(route_error)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(tx.as_mut())
        .await
        .map_err(route_error)?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(tx.as_mut())
        .await
        .map_err(route_error)?;
    let rows = execute_typed_lineage_query(tx.as_mut(), &request, &storage_partition_id)
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

    let signing = configured_audit_root_signer(&config)?;
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
    let report = lineage_audit_admin::verify_audit_root_window(rows, &root, signing.as_ref())
        .await
        .map_err(route_error)?;
    Ok(LineageVerifyResponse {
        tenant_id: request.tenant_id,
        records: usize_to_u64(report.records),
        root_checked: true,
        status: verification_status(dead_letters),
        root_id: Some(root.root_id),
    })
}

fn storage_partition_id_for_tenant(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

async fn execute_typed_lineage_query(
    conn: &mut sqlx::PgConnection,
    request: &LineageQueryRequest,
    storage_partition_id: &StoragePartitionId,
) -> Result<Vec<LineageRecordView>, sqlx::Error> {
    let mut query = QueryBuilder::<Postgres>::new(
        "SELECT turn_id, session_id, user_id, storage_partition_id, ts, record_kind, payload, answer_text \
         FROM analytics.turn_lineage WHERE storage_partition_id = ",
    );
    query.push_bind(storage_partition_id.as_str());
    if let Some(turn_id) = request.filters.turn_id {
        query.push(" AND turn_id = ");
        query.push_bind(turn_id);
    }
    if let Some(session_id) = request.filters.session_id {
        query.push(" AND session_id = ");
        query.push_bind(session_id.0);
    }
    if let Some(user_id) = request.filters.user_id.as_ref() {
        query.push(" AND user_id = ");
        query.push_bind(user_id.as_str());
    }
    if let Some(record_kind) = request.filters.record_kind {
        query.push(" AND record_kind = ");
        query.push_bind(record_kind);
    }
    if let Some(from_time) = request.filters.from_time {
        query.push(" AND ts >= ");
        query.push_bind(from_time);
    }
    if let Some(to_time) = request.filters.to_time {
        query.push(" AND ts <= ");
        query.push_bind(to_time);
    }
    match request.order {
        LineageQueryOrder::TimestampDesc => {
            query.push(" ORDER BY ts DESC, record_kind ASC, turn_id ASC");
        }
        LineageQueryOrder::TimestampAsc => {
            query.push(" ORDER BY ts ASC, record_kind ASC, turn_id ASC");
        }
    }
    query.push(" LIMIT ");
    query.push_bind(normalized_query_limit(request.limit));

    let rows = query.build().fetch_all(conn).await?;
    rows.into_iter()
        .map(|row| lineage_record_from_row(row, request.tenant_id))
        .collect()
}

fn normalized_query_limit(limit: u32) -> i64 {
    i64::from(limit.clamp(1, LINEAGE_QUERY_MAX_LIMIT))
}

fn lineage_record_from_row(
    row: sqlx::postgres::PgRow,
    tenant_id: TenantId,
) -> Result<LineageRecordView, sqlx::Error> {
    Ok(LineageRecordView {
        turn_id: row.try_get("turn_id")?,
        session_id: Some(SessionId(row.try_get("session_id")?)),
        tenant_id: Some(tenant_id),
        user_id: Some(UserId::new(row.try_get::<String, _>("user_id")?)),
        ts: row.try_get("ts")?,
        record_kind: row.try_get("record_kind")?,
        payload: row.try_get("payload")?,
        summary: row.try_get("answer_text")?,
    })
}

fn configured_audit_root_signer(
    config: &ComplianceConfig,
) -> Result<Arc<dyn AuditRootSigner>, Response> {
    match config.lineage_audit_signing_provider {
        LineageAuditSigningProvider::Local => {
            let signing_key = configured_signing_key_from_config(
                "MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX",
                config.lineage_audit_signing_key_hex.as_deref(),
                config.lineage_audit_signing_key_id.clone(),
            )?;
            Ok(Arc::new(LocalAuditRootSigner::new(signing_key)))
        }
        LineageAuditSigningProvider::Http => configured_http_audit_root_signer(config),
    }
}

fn configured_http_audit_root_signer(
    config: &ComplianceConfig,
) -> Result<Arc<dyn AuditRootSigner>, Response> {
    let endpoint = required_signer_config(
        "MOA_LINEAGE_AUDIT_SIGNING_ENDPOINT",
        config.lineage_audit_signing_endpoint.as_deref(),
    )?;
    let token_env = required_signer_config(
        "MOA_LINEAGE_AUDIT_SIGNING_BEARER_TOKEN_ENV",
        config.lineage_audit_signing_bearer_token_env.as_deref(),
    )?;
    let token = std::env::var(&token_env).map_err(|_| {
        route_error(format!(
            "{token_env} is required when MOA_LINEAGE_AUDIT_SIGNING_PROVIDER=http"
        ))
    })?;
    if token.trim().is_empty() {
        return Err(route_error(format!(
            "{token_env} is required when MOA_LINEAGE_AUDIT_SIGNING_PROVIDER=http"
        )));
    }
    let signer =
        HttpAuditRootSigner::new(endpoint, config.lineage_audit_signing_key_id.clone(), token)
            .map_err(route_error)?;
    Ok(Arc::new(signer))
}

fn required_signer_config(env_name: &str, value: Option<&str>) -> Result<String, Response> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    value.map(ToOwned::to_owned).ok_or_else(|| {
        route_error(format!(
            "{env_name} is required when MOA_LINEAGE_AUDIT_SIGNING_PROVIDER=http"
        ))
    })
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
    use moa_core::config::{ComplianceConfig, LineageAuditSigningProvider};
    use moa_lineage_audit::SigningKey;

    use super::{configured_audit_root_signer, normalized_query_limit};

    #[test]
    fn normalized_query_limit_clamps_to_safe_range() {
        // Pins: lineage query limits stay bounded before being bound into SQL.
        assert_eq!(normalized_query_limit(0), 1);
        assert_eq!(normalized_query_limit(100), 100);
        assert_eq!(normalized_query_limit(10_000), 1000);
    }

    #[test]
    fn lineage_signer_config_uses_local_key_by_default() {
        // Pins: lineage root verification keeps the existing local signing key as the default provider.
        let seed = [21_u8; 32];
        let key = SigningKey::from_seed("lineage-local-key", seed);
        let mut config = ComplianceConfig::default();
        config.lineage_audit_signing_key_hex = Some(hex::encode(seed));
        config.lineage_audit_signing_key_id = key.label().to_string();

        let signer =
            configured_audit_root_signer(&config).expect("default local signer should build");

        assert_eq!(signer.key_id(), key.label());
        assert_eq!(
            signer
                .verifying_key()
                .expect("local signer should expose verifying key"),
            key.verifying_key_bytes()
        );
    }

    #[test]
    fn lineage_signer_config_rejects_http_without_endpoint() {
        // Pins: HTTP lineage signing fails closed before verifier work when no endpoint is configured.
        let mut config = ComplianceConfig::default();
        config.lineage_audit_signing_provider = LineageAuditSigningProvider::Http;
        config.lineage_audit_signing_bearer_token_env = Some("MOA_AUDIT_SIGNER_TOKEN".to_string());

        let response = match configured_audit_root_signer(&config) {
            Ok(_) => panic!("HTTP signer without endpoint should fail"),
            Err(response) => response,
        };

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
