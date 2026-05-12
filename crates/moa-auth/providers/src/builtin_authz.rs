//! Builtin async authorization backed by Postgres and Restate awakeables.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use moa_core::traits::{
    ApprovalDecision, ApprovalHandle, ApprovalRequest, AsyncAuthzError, AsyncAuthzProvider,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Builtin async-authorization provider for local/self-hosted deployments.
pub struct BuiltinAsyncAuthzProvider {
    pool: Arc<PgPool>,
}

impl BuiltinAsyncAuthzProvider {
    /// Build a builtin provider over an existing Postgres pool.
    #[must_use]
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AsyncAuthzProvider for BuiltinAsyncAuthzProvider {
    async fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalHandle, AsyncAuthzError> {
        let id = Uuid::new_v4();
        let timeout = ChronoDuration::from_std(request.timeout)
            .map_err(|error| AsyncAuthzError::Internal(format!("timeout: {error}")))?;
        let expires_at = Utc::now() + timeout;
        let tenant_id = request
            .action_details
            .get("_tenant_id")
            .and_then(|value| value.as_str())
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                AsyncAuthzError::Internal("request.action_details._tenant_id required".to_string())
            })?;

        sqlx::query(
            r#"
            INSERT INTO builtin_pending_approvals
                (id, session_id, deciding_user_id, tenant_id, awakeable_id,
                 action_summary, action_details, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(id)
        .bind(request.session_id)
        .bind(request.deciding_user_id)
        .bind(tenant_id)
        .bind(&request.awakeable_id)
        .bind(&request.action_summary)
        .bind(&request.action_details)
        .bind(expires_at)
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("db: {error}")))?;

        Ok(ApprovalHandle {
            id,
            awakeable_id: request.awakeable_id,
            provider_specific: json!({ "kind": "builtin" }),
        })
    }

    async fn poll_decision(
        &self,
        _handle: &ApprovalHandle,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "builtin"
    }
}

/// Full builtin approval row shape used by orchestrator approval handlers.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BuiltinApprovalRow {
    /// Approval row id.
    pub id: Uuid,
    /// Session waiting on the decision.
    pub session_id: Uuid,
    /// User who must decide.
    pub deciding_user_id: Uuid,
    /// Tenant owning the approval.
    pub tenant_id: Uuid,
    /// Restate awakeable id.
    pub awakeable_id: String,
    /// One-line action summary.
    pub action_summary: String,
    /// Full action payload.
    pub action_details: serde_json::Value,
    /// Approval status.
    pub status: String,
    /// Optional denial reason.
    pub deny_reason: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
    /// Decision timestamp.
    pub decided_at: Option<DateTime<Utc>>,
    /// User who decided.
    pub decided_by_user_id: Option<Uuid>,
}
