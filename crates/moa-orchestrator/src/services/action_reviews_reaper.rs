//! Background timeout reaper and gauge sampler for tenant action reviews.
//!
//! Pending tenant action reviews (`tenant_action_reviews`) fail closed: a row
//! that is not decided before its `expires_at` is transitioned to `timeout` so
//! the gated tool never executes. The same periodic tick publishes the
//! pending-queue depth and oldest-pending-age gauges, mirroring the builtin
//! authz challenge reaper's poll-and-emit shape.

use std::time::Duration;

use moa_core::{
    events::Event,
    types::action_policy::{
        ActionReviewOwner, ActionReviewStatus, action_review_timed_out_dedupe_key,
    },
};
use moa_observability::propagation::{ValidatedTraceContext, with_reqwest_validated_trace_headers};
use moa_observability::{
    record_action_review_decision, record_action_review_oldest_pending_age,
    record_action_review_pending_depth, record_approval_wait,
};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::time::interval;

use crate::action_reviews::store as action_review_store;
use crate::services::action_review_dispatcher::{
    DispatchActionReviewsRequest, DispatchActionReviewsResponse,
};
use moa_wire::session_store::AppendEventRequest;

const OWNER_RELEASE_DISPATCH_BATCH_SIZE: i64 = 32;

/// Action-review reaper failures.
#[derive(Debug, Error)]
pub enum ActionReviewReaperError {
    /// Database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The Restate-owned execution-review dispatcher could not be awakened.
    #[error("Restate action-review dispatcher error: {0}")]
    Dispatcher(String),
}

/// Background worker that times out expired reviews and samples queue gauges.
pub struct ActionReviewReaper {
    pool: PgPool,
    sweep_interval: Duration,
    restate_ingress_url: Option<String>,
    client: reqwest::Client,
}

impl ActionReviewReaper {
    /// Build a reaper with the default sweep interval.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            sweep_interval: Duration::from_secs(30),
            restate_ingress_url: None,
            client: reqwest::Client::new(),
        }
    }

    /// Builds the production reaper with execution-review outbox delivery enabled.
    #[must_use]
    pub fn with_restate_ingress(pool: PgPool, restate_ingress_url: String) -> Self {
        Self {
            pool,
            sweep_interval: Duration::from_secs(30),
            restate_ingress_url: Some(restate_ingress_url.trim_end_matches('/').to_string()),
            client: reqwest::Client::new(),
        }
    }

    /// Spawn the reaper as a Tokio task.
    pub fn spawn(self) -> ActionReviewReaperHandle {
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let mut tick = interval(self.sweep_interval);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("action review reaper received shutdown");
                        break;
                    }
                    _ = tick.tick() => {}
                }
                if let Err(error) = self.sweep().await {
                    tracing::error!(error = %error, "action review reaper sweep failed");
                }
                // Best-effort in the tick loop; the error is logged inside.
                let _ = self.sample_gauges().await;
            }
        });
        ActionReviewReaperHandle {
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Run one timeout sweep, returning how many reviews failed closed.
    pub async fn sweep(&self) -> Result<usize, ActionReviewReaperError> {
        let resolution_trace_context = current_trace_context();
        let timed_out = action_review_store::timeout_expired_reviews(
            &self.pool,
            resolution_trace_context.as_ref(),
        )
        .await?;
        for review in &timed_out {
            record_action_review_decision(ActionReviewStatus::Timeout, review.action_class);
            let wait = (review.decided_at - review.created_at)
                .to_std()
                .unwrap_or_default();
            record_approval_wait(review.action_class, wait);
        }
        if !timed_out.is_empty() {
            tracing::warn!(
                count = timed_out.len(),
                "tenant action reviews timed out and failed closed"
            );
        }
        if self.restate_ingress_url.is_some() {
            let released = self.dispatch_action_review_releases().await?;
            if released > 0 {
                tracing::info!(count = released, "action-review owner releases dispatched");
            }
            let dispatched = self.trigger_execution_review_dispatch().await?;
            if dispatched > 0 {
                tracing::info!(
                    count = dispatched,
                    "execution action-review resolutions dispatched"
                );
            }
        }
        Ok(timed_out.len())
    }

    /// Attempts one bounded batch of persisted owner releases.
    pub async fn dispatch_action_review_releases(&self) -> Result<usize, ActionReviewReaperError> {
        let Some(ingress_url) = self.restate_ingress_url.as_deref() else {
            return Ok(0);
        };
        let pending = action_review_store::pending_action_review_releases(
            &self.pool,
            OWNER_RELEASE_DISPATCH_BATCH_SIZE,
        )
        .await?;
        let pending_count = pending.len();
        let dispatch_trace_context = current_trace_context();
        for delivery in pending {
            let event_request = AppendEventRequest {
                session_id: delivery.release.owner.session_id(),
                event: Event::ActionReviewTimedOut {
                    review_id: delivery.release.review_id,
                    timed_out_at: delivery.timed_out_at,
                },
                dedupe_key: Some(action_review_timed_out_dedupe_key(
                    delivery.release.review_id,
                )),
            };
            let event_response = with_reqwest_validated_trace_headers(
                self.client
                    .post(format!(
                        "{ingress_url}/restate/call/SessionStore/append_event"
                    ))
                    .header(
                        "idempotency-key",
                        format!("action-review-timeout-event:{}", delivery.release.review_id),
                    )
                    .json(&event_request),
                dispatch_trace_context.as_ref(),
            )
            .send()
            .await;
            match event_response {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    tracing::warn!(
                        review_id = %delivery.release.review_id,
                        status = %response.status(),
                        "action-review timeout event append failed; owner release deferred"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        review_id = %delivery.release.review_id,
                        error = %error,
                        "action-review timeout event append failed; owner release deferred"
                    );
                    continue;
                }
            }
            let endpoint = match &delivery.release.owner {
                ActionReviewOwner::Coordinator { session_id, .. } => {
                    format!("{ingress_url}/restate/call/Session/{session_id}/release_action_review")
                }
                ActionReviewOwner::Worker { worker_id, .. } => {
                    format!("{ingress_url}/restate/call/Worker/{worker_id}/release_action_review")
                }
                ActionReviewOwner::ExecutionTask { .. }
                | ActionReviewOwner::ExecutionCompensation { .. } => {
                    tracing::error!(
                        review_id = %delivery.release.review_id,
                        "execution action review reached conversational release dispatch"
                    );
                    continue;
                }
            };
            let request = with_reqwest_validated_trace_headers(
                self.client
                    .post(endpoint)
                    .header("idempotency-key", delivery.release.review_id.to_string())
                    .json(&delivery.release),
                dispatch_trace_context.as_ref(),
            );
            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    action_review_store::mark_action_review_release_delivered(
                        &self.pool,
                        delivery.release.review_id,
                    )
                    .await?;
                }
                Ok(response) => {
                    tracing::warn!(
                        review_id = %delivery.release.review_id,
                        status = %response.status(),
                        "action-review owner release failed; retry remains pending"
                    );
                }
                Err(error) => tracing::warn!(
                    review_id = %delivery.release.review_id,
                    error = %error,
                    "action-review owner release failed; retry remains pending"
                ),
            }
        }
        Ok(pending_count)
    }

    /// Wakes the Restate-owned dispatcher that drains execution-review outbox rows.
    pub async fn trigger_execution_review_dispatch(
        &self,
    ) -> Result<usize, ActionReviewReaperError> {
        let Some(ingress_url) = self.restate_ingress_url.as_deref() else {
            return Ok(0);
        };
        let response = crate::restate_identity::with_reqwest_trace_headers(
            self.client
                .post(format!(
                    "{ingress_url}/restate/call/ActionReviewDispatcher/dispatch"
                ))
                .json(&DispatchActionReviewsRequest::default()),
        )
        .send()
        .await
        .map_err(|error| ActionReviewReaperError::Dispatcher(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<unreadable body: {error}>"));
            return Err(ActionReviewReaperError::Dispatcher(format!(
                "Restate returned {status}: {body}"
            )));
        }
        response
            .json::<DispatchActionReviewsResponse>()
            .await
            .map(|response| response.claimed)
            .map_err(|error| ActionReviewReaperError::Dispatcher(error.to_string()))
    }

    /// Sample the pending-review queue and publish operator gauges.
    ///
    /// Returns the sampling error so tests can assert the query decodes
    /// against the real schema; the tick loop treats it as best-effort and a
    /// sampling error never fails a tick, matching the authz outbox backlog
    /// gauge.
    pub async fn sample_gauges(&self) -> Result<(), sqlx::Error> {
        match action_review_store::pending_review_stats(&self.pool).await {
            Ok(stats) => {
                record_action_review_pending_depth(&stats.depth_by_risk);
                record_action_review_oldest_pending_age(stats.oldest_pending_age_seconds);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to sample action review queue gauges");
                Err(error)
            }
        }
    }
}

fn current_trace_context() -> Option<ValidatedTraceContext> {
    let headers = moa_observability::current_trace_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

/// Handle used to stop the action-review reaper.
pub struct ActionReviewReaperHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ActionReviewReaperHandle {
    /// Signal shutdown and wait for the task to exit.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}
