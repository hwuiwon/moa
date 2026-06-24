//! Restate service for builtin async authorization challenge lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use moa_core::traits::IdentityType;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::authz_challenges::app as authz_challenge_app;
use crate::authz_challenges::store as authz_challenge_store;
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
    // SAFETY: Lists only builtin challenges keyed to the trusted user identity.
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                authz_challenge_store::list_pending_builtin_challenges(pool, identity.id)
                    .await
                    .map(|rows| {
                        rows.into_iter()
                            .map(summary_from_builtin_row)
                            .collect::<Vec<_>>()
                    })
                    .map(Json::from)
            })
            .name("authz_challenges_list_mine")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Resolves only builtin challenges keyed to the trusted user identity.
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
        let pool = OrchestratorCtx::current_graph_pool();
        let mark_resolved_pool = pool.clone();
        let resolved = ctx
            .run(|| async move {
                authz_challenge_app::decide_builtin_challenge(pool, identity.id, request)
                    .await
                    .map(Json::from)
            })
            .name("authz_challenges_decide")
            .await?
            .into_inner();

        ctx.resolve_awakeable(&resolved.awakeable_id, Json::from(resolved.decision));
        ctx.run(|| async move {
            authz_challenge_store::mark_builtin_challenge_resolved(&mark_resolved_pool, resolved.id)
                .await
                .map_err(|error| {
                    HandlerError::from(TerminalError::new(format!(
                        "mark authz challenge resolved: {error}"
                    )))
                })
        })
        .name("authz_challenges_mark_resolved")
        .await?;
        Ok(())
    }
}

fn summary_from_builtin_row(row: BuiltinApprovalRow) -> AuthzChallengeSummary {
    AuthzChallengeSummary {
        id: row.id,
        session_id: row.session_id,
        action_summary: row.action_summary,
        action_details: row.action_details,
        created_at: row.created_at,
        expires_at: row.expires_at,
    }
}
