//! Restate service for builtin human approval lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{ApprovalDecision, IdentityType};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::require_identity;

/// Approval summary returned to users.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSummary {
    /// Approval row id.
    pub id: Uuid,
    /// Session waiting on this approval.
    pub session_id: Uuid,
    /// One-line action summary.
    pub action_summary: String,
    /// Full action details.
    pub action_details: serde_json::Value,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Approval decision request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// Approval id to resolve.
    pub id: Uuid,
    /// Decision outcome: `approved` or `denied`.
    pub outcome: String,
    /// Optional denial reason.
    pub reason: Option<String>,
}

/// Restate service surface for builtin approvals.
#[restate_sdk::service]
#[name = "Approvals"]
pub trait Approvals {
    /// List pending approvals for the caller.
    async fn list_mine() -> Result<Json<Vec<ApprovalSummary>>, HandlerError>;

    /// Resolve one approval with an approve or deny decision.
    async fn decide(request: Json<DecisionRequest>) -> Result<(), HandlerError>;
}

/// Concrete approvals service implementation.
#[derive(Clone, Default)]
pub struct ApprovalsImpl;

impl Approvals for ApprovalsImpl {
    #[tracing::instrument(skip(self, ctx))]
    async fn list_mine(
        &self,
        ctx: Context<'_>,
    ) -> Result<Json<Vec<ApprovalSummary>>, HandlerError> {
        annotate_restate_handler_span("Approvals", "list_mine");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(TerminalError::new_with_code(403, "only users can list approvals").into());
        }
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { list_mine_inner(pool, identity.id).await.map(Json::from) })
            .name("approvals_list_mine")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide(
        &self,
        ctx: Context<'_>,
        request: Json<DecisionRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Approvals", "decide");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(
                TerminalError::new_with_code(403, "only users can resolve approvals").into(),
            );
        }
        let request = request.into_inner();
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let row = ctx
            .run(|| async move {
                decide_inner(pool, identity.id, request)
                    .await
                    .map(Json::from)
            })
            .name("approvals_decide")
            .await?
            .into_inner();

        let decision = match row.status.as_str() {
            "approved" => ApprovalDecision::Approved,
            "denied" => ApprovalDecision::Denied {
                reason: row.deny_reason,
            },
            other => {
                return Err(TerminalError::new(format!(
                    "unexpected approval status after decision: {other}"
                ))
                .into());
            }
        };
        ctx.resolve_awakeable(&row.awakeable_id, Json::from(decision));
        Ok(())
    }
}

async fn list_mine_inner(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
) -> Result<Vec<ApprovalSummary>, HandlerError> {
    let rows: Vec<BuiltinApprovalRow> = sqlx::query_as(
        r#"
        SELECT id, session_id, deciding_user_id, tenant_id, awakeable_id,
               action_summary, action_details, status, deny_reason,
               created_at, expires_at, decided_at, decided_by_user_id
        FROM builtin_pending_approvals
        WHERE deciding_user_id = $1 AND status = 'pending' AND expires_at > NOW()
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(deciding_user_id)
    .fetch_all(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("list approvals: {error}")))?;

    Ok(rows
        .into_iter()
        .map(|row| ApprovalSummary {
            id: row.id,
            session_id: row.session_id,
            action_summary: row.action_summary,
            action_details: row.action_details,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
        .collect())
}

async fn decide_inner(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
    request: DecisionRequest,
) -> Result<ResolvedApproval, HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row: BuiltinApprovalRow = sqlx::query_as(
        r#"
        SELECT id, session_id, deciding_user_id, tenant_id, awakeable_id,
               action_summary, action_details, status, deny_reason,
               created_at, expires_at, decided_at, decided_by_user_id
        FROM builtin_pending_approvals
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(request.id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("load approval: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "approval not found"))?;

    if row.deciding_user_id != deciding_user_id {
        return Err(TerminalError::new_with_code(403, "not your approval").into());
    }
    if row.status != "pending" {
        return Err(
            TerminalError::new_with_code(409, format!("approval already {}", row.status)).into(),
        );
    }
    if row.expires_at <= Utc::now() {
        return Err(TerminalError::new_with_code(410, "approval expired").into());
    }

    let (status, decision) = match request.outcome.as_str() {
        "approved" => ("approved", ApprovalDecision::Approved),
        "denied" => (
            "denied",
            ApprovalDecision::Denied {
                reason: request.reason.clone(),
            },
        ),
        other => {
            return Err(TerminalError::new_with_code(400, format!("bad outcome: {other}")).into());
        }
    };

    let updated: BuiltinApprovalRow = sqlx::query_as(
        r#"
        UPDATE builtin_pending_approvals
        SET status = $2,
            deny_reason = $3,
            decided_at = NOW(),
            decided_by_user_id = $4
        WHERE id = $1
        RETURNING id, session_id, deciding_user_id, tenant_id, awakeable_id,
                  action_summary, action_details, status, deny_reason,
                  created_at, expires_at, decided_at, decided_by_user_id
        "#,
    )
    .bind(request.id)
    .bind(status)
    .bind(match &decision {
        ApprovalDecision::Denied { reason } => reason.as_deref(),
        ApprovalDecision::Approved | ApprovalDecision::Timeout => None,
    })
    .bind(deciding_user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("update approval: {error}")))?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(ResolvedApproval {
        awakeable_id: updated.awakeable_id,
        status: updated.status,
        deny_reason: updated.deny_reason,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolvedApproval {
    awakeable_id: String,
    status: String,
    deny_reason: Option<String>,
}
