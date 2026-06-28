//! Direct edge security-audit verification route.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    AppState, authenticate_direct_request, parse_json_body_with_tenant, require_direct_authz,
    route_error,
};

/// Public audit verification request.
#[derive(Debug, Deserialize)]
pub struct AuditVerifyRequest {
    /// Security event to verify.
    pub event_id: Uuid,
    /// Tenant that owns the security event.
    pub tenant_id: TenantId,
}

/// Audit verification result.
#[derive(Debug, Clone, Serialize)]
pub struct AuditVerifyResponse {
    /// Event id that was verified.
    pub event_id: Uuid,
    /// Tenant that owns the event.
    pub tenant_id: Uuid,
    /// Whether the HMAC matched the stored canonical bytes.
    pub valid: bool,
}

/// Handles direct security-audit event signature verification at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/audit/verify").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let request: AuditVerifyRequest = match parse_json_body_with_tenant(&body, identity.tenant_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }

    match verify_event(&state.pool, request.event_id, request.tenant_id).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

async fn verify_event(
    pool: &sqlx::PgPool,
    event_id: Uuid,
    tenant_id: TenantId,
) -> Result<AuditVerifyResponse, Response> {
    let row: Option<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        r#"
        SELECT signing_key_id, event_jcs, signature_hex
        FROM security_events
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(event_id)
    .bind(tenant_id.0)
    .fetch_optional(pool)
    .await
    .map_err(route_error)?;
    let Some((signing_key_id, event_jcs, signature_hex)) = row else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "security event not found",
        )
            .into_response());
    };
    let valid = moa_ocsf::verify(pool, signing_key_id, &event_jcs, &signature_hex)
        .await
        .map_err(|error| route_error(format!("verify signature: {error}")))?;
    Ok(AuditVerifyResponse {
        event_id,
        tenant_id: tenant_id.0,
        valid,
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use moa_core::TenantId;
    use uuid::Uuid;

    use super::AuditVerifyRequest;

    #[test]
    fn audit_verify_request_injects_authenticated_tenant() {
        // Pins: direct audit verify derives tenant scope from the authenticated edge identity.
        let tenant_id = TenantId::from(
            Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("tenant id fixture should parse"),
        );
        let event_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("event id fixture should parse");
        let body = Bytes::from(serde_json::json!({ "event_id": event_id }).to_string());

        let request: AuditVerifyRequest =
            match crate::routes::parse_json_body_with_tenant(&body, tenant_id) {
                Ok(request) => request,
                Err(response) => panic!("request should parse, got {}", response.status()),
            };

        assert_eq!(request.event_id, event_id);
        assert_eq!(request.tenant_id, tenant_id);
    }
}
