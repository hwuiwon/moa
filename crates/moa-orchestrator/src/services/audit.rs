//! Restate service for security-audit verification helpers.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Audit verification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditVerifyResponse {
    /// Event id that was verified.
    pub event_id: Uuid,
    /// Tenant that owns the event.
    pub tenant_id: Uuid,
    /// Whether the HMAC matched the stored canonical bytes.
    pub valid: bool,
}

/// Security-audit service.
#[restate_sdk::service]
#[name = "Audit"]
pub trait Audit {
    /// Verify one signed security event.
    async fn verify(event_id: Json<Uuid>) -> Result<Json<AuditVerifyResponse>, HandlerError>;
}

/// Concrete security-audit implementation.
#[derive(Clone, Default)]
pub struct AuditImpl;

impl Audit for AuditImpl {
    #[tracing::instrument(skip(self, ctx, event_id))]
    async fn verify(
        &self,
        ctx: Context<'_>,
        event_id: Json<Uuid>,
    ) -> Result<Json<AuditVerifyResponse>, HandlerError> {
        annotate_restate_handler_span("Audit", "verify");
        let identity = require_identity(&ctx)?;
        let event_id = event_id.into_inner();
        let pool = OrchestratorCtx::current_graph_pool();
        let metadata = load_security_event_metadata(&pool, event_id).await?;

        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            metadata.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;

        Ok(ctx
            .run(|| async move {
                let payload =
                    load_security_event_payload(&pool, event_id, metadata.tenant_id).await?;
                let valid = moa_ocsf::verify(
                    &pool,
                    payload.signing_key_id,
                    &payload.event_jcs,
                    &payload.signature_hex,
                )
                .await
                .map_err(|error| TerminalError::new(format!("verify signature: {error}")))?;
                Ok(Json(AuditVerifyResponse {
                    event_id,
                    tenant_id: metadata.tenant_id,
                    valid,
                }))
            })
            .name("audit_verify")
            .await?)
    }
}

#[derive(Debug, Clone, Copy)]
struct SecurityEventMetadata {
    tenant_id: Uuid,
}

#[derive(Debug)]
struct SecurityEventPayload {
    signing_key_id: Uuid,
    event_jcs: Vec<u8>,
    signature_hex: String,
}

async fn load_security_event_metadata(
    pool: &sqlx::PgPool,
    event_id: Uuid,
) -> Result<SecurityEventMetadata, HandlerError> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT tenant_id
        FROM security_events
        WHERE id = $1
        "#,
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load security event tenant: {error}")))?;
    let Some((tenant_id,)) = row else {
        return Err(TerminalError::new_with_code(404, "security event not found").into());
    };
    Ok(SecurityEventMetadata { tenant_id })
}

async fn load_security_event_payload(
    pool: &sqlx::PgPool,
    event_id: Uuid,
    tenant_id: Uuid,
) -> Result<SecurityEventPayload, HandlerError> {
    let row: Option<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        r#"
        SELECT signing_key_id, event_jcs, signature_hex
        FROM security_events
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(event_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load security event payload: {error}")))?;
    let Some((signing_key_id, event_jcs, signature_hex)) = row else {
        return Err(TerminalError::new_with_code(404, "security event not found").into());
    };
    Ok(SecurityEventPayload {
        signing_key_id,
        event_jcs,
        signature_hex,
    })
}
