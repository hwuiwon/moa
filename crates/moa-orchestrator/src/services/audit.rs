//! Restate service for security-audit verification helpers.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
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
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let row: (Uuid, Uuid, Vec<u8>, String) = sqlx::query_as(
            r#"
            SELECT tenant_id, signing_key_id, event_jcs, signature_hex
            FROM security_events
            WHERE id = $1
            "#,
        )
        .bind(event_id)
        .fetch_optional(&pool)
        .await
        .map_err(|error| TerminalError::new(format!("load security event: {error}")))?
        .ok_or_else(|| TerminalError::new_with_code(404, "security event not found"))?;

        let fga = require_fga_client()?;
        require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, row.0, Relation::Admin)
            .await
            .map_err(translate_authz_error)?;

        Ok(ctx
            .run(|| async move {
                let valid = moa_ocsf::verify(&pool, row.1, &row.2, &row.3)
                    .await
                    .map_err(|error| TerminalError::new(format!("verify signature: {error}")))?;
                Ok(Json(AuditVerifyResponse {
                    event_id,
                    tenant_id: row.0,
                    valid,
                }))
            })
            .name("audit_verify")
            .await?)
    }
}
