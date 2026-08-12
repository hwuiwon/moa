//! Shared Restate-delayed timeout delivery for durable approval waits.
//!
//! Normal expiry delivery is one delayed Restate call per persisted wait. The
//! payload carries the immutable persisted owner incarnation, so a late call
//! can only fail closed the wait that originally scheduled it. Process reapers
//! remain a lower-frequency repair path for scheduling or delivery gaps.

use std::time::Duration;

use moa_core::types::{
    action_policy::{ActionReviewOwner, ActionReviewRelease},
    identifiers::TenantId,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::services::{
    action_review_dispatcher::{ActionReviewDispatcherClient, DispatchActionReviewsRequest},
    action_reviews_reaper::{ActionReviewReaper, ActionReviewTimeoutDelivery},
    authz_challenges_reaper::{AuthzChallengeReaper, AuthzChallengeTimeoutDelivery},
    session_store::RestateSessionStoreClient,
};
use crate::workflows::errors::sqlx_error_to_handler_error;
use moa_core::{events::Event, traits::ApprovalDecision};
use moa_wire::session_store::AppendEventRequest;

/// Low-frequency scanner cadence used only to reconcile missed durable timers.
pub(crate) const DURABLE_TIMEOUT_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(300);

/// Exact action-review incarnation carried by its delayed timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionReviewTimeout {
    /// Tenant that owns the persisted review.
    pub tenant_id: TenantId,
    /// Stable review identifier.
    pub review_id: Uuid,
    /// Full owner fence, including the originating turn or execution generation.
    pub owner: ActionReviewOwner,
}

/// Exact builtin-authz challenge incarnation carried by its delayed timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthzChallengeTimeout {
    /// Stable challenge row identifier.
    pub challenge_id: Uuid,
    /// Exact Restate awakeable created for this challenge incarnation.
    pub awakeable_id: String,
}

/// One supported durable timeout target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableTimeoutTarget {
    /// Fail one tenant action review closed.
    ActionReview(ActionReviewTimeout),
    /// Fail one builtin async-authz challenge closed.
    AuthzChallenge(AuthzChallengeTimeout),
}

/// Delayed request accepted by `DurableTimeout/expire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableTimeoutRequest {
    /// Immutable idempotency identity for this delayed trigger.
    pub trigger_id: Uuid,
    /// Persisted wait incarnation that may be expired.
    pub target: DurableTimeoutTarget,
}

impl DurableTimeoutRequest {
    /// Builds the timeout trigger for one action-review incarnation.
    #[must_use]
    pub fn action_review(tenant_id: TenantId, review_id: Uuid, owner: ActionReviewOwner) -> Self {
        Self {
            trigger_id: review_id,
            target: DurableTimeoutTarget::ActionReview(ActionReviewTimeout {
                tenant_id,
                review_id,
                owner,
            }),
        }
    }

    /// Builds the timeout trigger for one builtin-authz challenge incarnation.
    #[must_use]
    pub fn authz_challenge(challenge_id: Uuid, awakeable_id: String) -> Self {
        Self {
            trigger_id: challenge_id,
            target: DurableTimeoutTarget::AuthzChallenge(AuthzChallengeTimeout {
                challenge_id,
                awakeable_id,
            }),
        }
    }

    fn has_matching_trigger_id(&self) -> bool {
        match &self.target {
            DurableTimeoutTarget::ActionReview(timeout) => self.trigger_id == timeout.review_id,
            DurableTimeoutTarget::AuthzChallenge(timeout) => {
                self.trigger_id == timeout.challenge_id
            }
        }
    }
}

/// Schedules one replay-safe delayed timeout call.
pub(crate) fn schedule_durable_timeout(
    ctx: &Context<'_>,
    request: DurableTimeoutRequest,
    delay: Duration,
) {
    let idempotency_key = format!("durable-timeout:{}", request.trigger_id);
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<DurableTimeoutClient>()
            .expire(Json::from(request))
            .idempotency_key(idempotency_key),
    )
    .send_after(delay);
}

/// Restate service that owns delayed approval timeout delivery.
#[restate_sdk::service]
#[name = "DurableTimeout"]
pub trait DurableTimeout {
    /// Delivers one generation/incarnation-fenced timeout.
    async fn expire(request: Json<DurableTimeoutRequest>) -> Result<(), HandlerError>;
}

/// PostgreSQL-backed durable timeout implementation.
#[derive(Clone)]
pub struct DurableTimeoutImpl {
    pool: sqlx::PgPool,
}

impl DurableTimeoutImpl {
    /// Creates the timeout service over the shared product database.
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl DurableTimeout for DurableTimeoutImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(trigger_id = %request.0.trigger_id))]
    // SAFETY: ingress-private delayed delivery can only fail closed an exact persisted incarnation; mismatches are successful no-ops.
    async fn expire(
        &self,
        ctx: Context<'_>,
        request: Json<DurableTimeoutRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("DurableTimeout", "expire");
        let request = request.into_inner();
        if !request.has_matching_trigger_id() {
            tracing::debug!(
                trigger_id = %request.trigger_id,
                "ignored durable timeout with mismatched trigger identity"
            );
            return Ok(());
        }

        match request.target {
            DurableTimeoutTarget::ActionReview(timeout) => {
                deliver_action_review_timeout(&ctx, self.pool.clone(), timeout).await
            }
            DurableTimeoutTarget::AuthzChallenge(timeout) => {
                deliver_authz_challenge_timeout(&ctx, self.pool.clone(), timeout).await
            }
        }
    }
}

async fn deliver_action_review_timeout(
    ctx: &Context<'_>,
    pool: sqlx::PgPool,
    timeout: ActionReviewTimeout,
) -> Result<(), HandlerError> {
    let release_pool = pool.clone();
    let delivery = ctx
        .run(|| async move {
            ActionReviewReaper::new(pool)
                .apply_timeout(&timeout)
                .await
                .map(Json::from)
                .map_err(sqlx_error_to_handler_error)
        })
        .name("durable_timeout_action_review")
        .await?
        .into_inner();

    match delivery {
        ActionReviewTimeoutDelivery::Stale | ActionReviewTimeoutDelivery::AlreadyDelivered => {
            Ok(())
        }
        ActionReviewTimeoutDelivery::Execution => {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<ActionReviewDispatcherClient>()
                    .dispatch(Json::from(DispatchActionReviewsRequest::default())),
            )
            .call()
            .await?;
            Ok(())
        }
        ActionReviewTimeoutDelivery::Conversational {
            timed_out_at,
            release,
        } => deliver_action_review_release(ctx, release_pool, release, timed_out_at).await,
    }
}

async fn deliver_action_review_release(
    ctx: &Context<'_>,
    pool: sqlx::PgPool,
    release: ActionReviewRelease,
    timed_out_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), HandlerError> {
    let review_id = release.review_id;
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id: release.owner.session_id(),
                event: Event::ActionReviewTimedOut {
                    review_id,
                    timed_out_at,
                },
                dedupe_key: Some(
                    moa_core::types::action_policy::action_review_timed_out_dedupe_key(review_id),
                ),
            })),
    )
    .call()
    .await?;

    crate::services::action_reviews::release_timed_out_conversational_review(ctx, release).await?;
    ctx.run(|| async move {
        crate::action_reviews::store::mark_action_review_release_delivered(&pool, review_id)
            .await
            .map_err(sqlx_error_to_handler_error)
    })
    .name("durable_timeout_action_review_mark_released")
    .await?;
    Ok(())
}

async fn deliver_authz_challenge_timeout(
    ctx: &Context<'_>,
    pool: sqlx::PgPool,
    timeout: AuthzChallengeTimeout,
) -> Result<(), HandlerError> {
    let mark_resolved_pool = pool.clone();
    let delivery = ctx
        .run(|| async move {
            AuthzChallengeReaper::new(pool)
                .apply_timeout(&timeout)
                .await
                .map(Json::from)
                .map_err(sqlx_error_to_handler_error)
        })
        .name("durable_timeout_authz_challenge")
        .await?
        .into_inner();

    match delivery {
        AuthzChallengeTimeoutDelivery::Stale | AuthzChallengeTimeoutDelivery::AlreadyDelivered => {
            Ok(())
        }
        AuthzChallengeTimeoutDelivery::Resolve {
            challenge_id,
            awakeable_id,
            resolve_claim_token,
            newly_timed_out,
        } => {
            if newly_timed_out {
                moa_observability::record_builtin_approval_decision("timeout");
            }
            ctx.resolve_awakeable(&awakeable_id, Json::from(ApprovalDecision::Timeout));
            ctx.run(|| async move {
                let marked =
                    crate::authz_challenges::store::mark_claimed_builtin_challenge_resolved(
                        &mark_resolved_pool,
                        challenge_id,
                        resolve_claim_token,
                    )
                    .await
                    .map_err(sqlx_error_to_handler_error)?;
                if !marked {
                    tracing::debug!(
                        authz_challenge_id = %challenge_id,
                        "durable authz timeout acknowledgement lost its exact claim"
                    );
                }
                Ok::<_, HandlerError>(())
            })
            .name("durable_timeout_authz_challenge_mark_resolved")
            .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::types::identifiers::SessionId;

    use super::*;

    #[test]
    fn trigger_identity_is_bound_to_the_persisted_target() {
        // Pins: a malformed or replay-substituted trigger id cannot target a different wait.
        let request = DurableTimeoutRequest::action_review(
            TenantId::new(),
            Uuid::from_u128(10),
            ActionReviewOwner::Coordinator {
                session_id: SessionId::new(),
                turn_id: "turn-1".to_string(),
                generation: 4,
            },
        );
        assert!(request.has_matching_trigger_id());

        let mut mismatched = request;
        mismatched.trigger_id = Uuid::from_u128(11);
        assert!(!mismatched.has_matching_trigger_id());
    }
}
