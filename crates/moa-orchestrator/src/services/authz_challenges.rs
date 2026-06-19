//! Restate service for builtin async authorization challenge lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{ApprovalDecision as AsyncApprovalDecision, IdentityType};
use moa_ocsf::ActorInput;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::require_identity;

/// Async authorization challenge summary returned to users.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzChallengeSummary {
    /// Challenge row id.
    pub id: Uuid,
    /// Session associated with this challenge.
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

/// Async authorization challenge decision request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzChallengeDecisionRequest {
    /// Challenge id to resolve.
    pub id: Uuid,
    /// Decision outcome: `approved` or `denied`.
    pub outcome: String,
    /// Optional denial reason.
    pub reason: Option<String>,
}

/// Restate service surface for builtin async authorization challenges.
#[restate_sdk::service]
#[name = "AuthzChallenges"]
pub trait AuthzChallenges {
    /// List pending async authorization challenges for the caller.
    async fn list_mine() -> Result<Json<Vec<AuthzChallengeSummary>>, HandlerError>;

    /// Resolve one async authorization challenge with an approve or deny decision.
    async fn decide(request: Json<AuthzChallengeDecisionRequest>) -> Result<(), HandlerError>;
}

/// Concrete async authorization challenge service implementation.
#[derive(Clone, Default)]
pub struct AuthzChallengesImpl;

impl AuthzChallenges for AuthzChallengesImpl {
    #[tracing::instrument(skip(self, ctx))]
    async fn list_mine(
        &self,
        ctx: Context<'_>,
    ) -> Result<Json<Vec<AuthzChallengeSummary>>, HandlerError> {
        annotate_restate_handler_span("AuthzChallenges", "list_mine");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(
                TerminalError::new_with_code(403, "only users can list authz challenges").into(),
            );
        }
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                list_builtin_challenges(pool, identity.id)
                    .await
                    .map(Json::from)
            })
            .name("authz_challenges_list_mine")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide(
        &self,
        ctx: Context<'_>,
        request: Json<AuthzChallengeDecisionRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("AuthzChallenges", "decide");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(TerminalError::new_with_code(
                403,
                "only users can resolve authz challenges",
            )
            .into());
        }
        let request = request.into_inner();
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let resolved = ctx
            .run(|| async move {
                decide_builtin_challenge(pool, identity.id, request)
                    .await
                    .map(Json::from)
            })
            .name("authz_challenges_decide")
            .await?
            .into_inner();

        let decision = match resolved.status.as_str() {
            "approved" => AsyncApprovalDecision::Approved,
            "denied" => AsyncApprovalDecision::Denied {
                reason: resolved.deny_reason,
            },
            other => {
                return Err(TerminalError::new(format!(
                    "unexpected authz challenge status after decision: {other}"
                ))
                .into());
            }
        };
        ctx.resolve_awakeable(&resolved.awakeable_id, Json::from(decision));
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolvedAuthzChallenge {
    awakeable_id: String,
    status: String,
    deny_reason: Option<String>,
}

async fn list_builtin_challenges(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
) -> Result<Vec<AuthzChallengeSummary>, HandlerError> {
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
    .map_err(|error| TerminalError::new(format!("list authz challenges: {error}")))?;

    Ok(rows
        .into_iter()
        .map(|row| AuthzChallengeSummary {
            id: row.id,
            session_id: row.session_id,
            action_summary: row.action_summary,
            action_details: row.action_details,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
        .collect())
}

async fn decide_builtin_challenge(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
    request: AuthzChallengeDecisionRequest,
) -> Result<ResolvedAuthzChallenge, HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row: Option<BuiltinApprovalRow> = sqlx::query_as(
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
    .map_err(|error| TerminalError::new(format!("load authz challenge: {error}")))?;
    let Some(row) = row else {
        return Err(TerminalError::new_with_code(404, "authz challenge not found").into());
    };

    if row.deciding_user_id != deciding_user_id {
        return Err(TerminalError::new_with_code(403, "not your authz challenge").into());
    }
    if row.status != "pending" {
        return Err(TerminalError::new_with_code(
            409,
            format!("authz challenge already {}", row.status),
        )
        .into());
    }
    if row.expires_at <= Utc::now() {
        return Err(TerminalError::new_with_code(410, "authz challenge expired").into());
    }

    let (status, decision) = match request.outcome.as_str() {
        "approved" => ("approved", AsyncApprovalDecision::Approved),
        "denied" => (
            "denied",
            AsyncApprovalDecision::Denied {
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
        AsyncApprovalDecision::Denied { reason } => reason.as_deref(),
        AsyncApprovalDecision::Approved | AsyncApprovalDecision::Timeout => None,
    })
    .bind(deciding_user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("update authz challenge: {error}")))?;

    moa_ocsf::emit_approval_decided_tx(
        &mut transaction,
        updated.tenant_id,
        ActorInput::user(deciding_user_id),
        updated.id,
        updated.status == "approved",
    )
    .await
    .map_err(|error| TerminalError::new(format!("audit authz challenge decision: {error}")))?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(ResolvedAuthzChallenge {
        awakeable_id: updated.awakeable_id,
        status: updated.status,
        deny_reason: updated.deny_reason,
    })
}
