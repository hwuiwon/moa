//! Background timeout reaper and gauge sampler for tenant action reviews.
//!
//! Pending tenant action reviews (`tenant_action_reviews`) fail closed: a row
//! that is not decided before its `expires_at` is transitioned to `timeout` so
//! the gated tool never executes. The same periodic tick publishes the
//! pending-queue depth and oldest-pending-age gauges, mirroring the builtin
//! authz challenge reaper's poll-and-emit shape.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use moa_core::{
    events::Event,
    types::action_policy::{
        ActionReviewOwner, ActionReviewRelease, ActionReviewStatus,
        action_review_timed_out_dedupe_key,
    },
};
use moa_observability::propagation::{ValidatedTraceContext, with_reqwest_validated_trace_headers};
use moa_observability::{
    record_action_review_decision, record_action_review_oldest_pending_age,
    record_action_review_pending_depth, record_approval_wait,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::interval;

use crate::action_reviews::store as action_review_store;
use crate::services::action_review_dispatcher::{
    DispatchActionReviewsRequest, DispatchActionReviewsResponse,
};
use crate::services::durable_timeout::{
    ActionReviewTimeout, DURABLE_TIMEOUT_RECONCILIATION_INTERVAL,
};
use moa_wire::session_store::AppendEventRequest;

const OWNER_RELEASE_DISPATCH_BATCH_SIZE: i64 = 32;

/// Durable delivery selected after applying one exact action-review timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionReviewTimeoutDelivery {
    /// The row no longer matches the delayed owner generation/incarnation.
    Stale,
    /// The exact timeout was already delivered to its conversational owner.
    AlreadyDelivered,
    /// A conversational owner must have its lifecycle hold released.
    Conversational {
        /// Timestamp persisted when the review failed closed.
        timed_out_at: chrono::DateTime<chrono::Utc>,
        /// Exact owner-generation release payload.
        release: ActionReviewRelease,
    },
    /// An execution outbox row is ready for the Restate-owned dispatcher.
    Execution,
}

/// Action-review reaper failures.
#[derive(Debug, Error)]
pub enum ActionReviewReaperError {
    /// Database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The Restate-owned execution-review dispatcher could not be awakened.
    #[error("Restate action-review dispatcher error: {0}")]
    Dispatcher(String),
    /// The supervised reaper task could not be joined.
    #[error("action-review reaper task join failed: {0}")]
    Join(String),
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
            sweep_interval: DURABLE_TIMEOUT_RECONCILIATION_INTERVAL,
            restate_ingress_url: None,
            client: reqwest::Client::new(),
        }
    }

    /// Builds the production reaper with execution-review outbox delivery enabled.
    #[must_use]
    pub fn with_restate_ingress(pool: PgPool, restate_ingress_url: String) -> Self {
        Self {
            pool,
            sweep_interval: DURABLE_TIMEOUT_RECONCILIATION_INTERVAL,
            restate_ingress_url: Some(restate_ingress_url.trim_end_matches('/').to_string()),
            client: reqwest::Client::new(),
        }
    }

    /// Spawn the reaper as a Tokio task.
    pub fn spawn(self) -> ActionReviewReaperHandle {
        let health = Arc::new(ActionReviewReaperHealth {
            started_at: Instant::now(),
            last_success: RwLock::new(None),
            exited: AtomicBool::new(false),
        });
        let heartbeat_maximum_age = self.sweep_interval.saturating_mul(3);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let task_health = Arc::clone(&health);
        let task = tokio::spawn(async move {
            let result = async {
                let mut tick = interval(self.sweep_interval);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => {
                            tracing::info!("action review reaper received shutdown");
                            return Ok(());
                        }
                        _ = tick.tick() => {}
                    }
                    self.sweep().await?;
                    // Queue sampling is observational and cannot invalidate a
                    // completed correctness pass.
                    let _ = self.sample_gauges().await;
                    set_action_review_reaper_heartbeat(&task_health);
                }
            }
            .await;
            task_health.exited.store(true, Ordering::Release);
            result
        });
        ActionReviewReaperHandle {
            health,
            shutdown,
            task,
            heartbeat_maximum_age,
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
        record_timed_out_reviews(&timed_out);
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

    /// Applies one exact delayed timeout and selects its durable delivery.
    ///
    /// The full persisted owner is compared before any expiry sweep runs. A
    /// timer from an older turn, task attempt, or compensation generation is a
    /// successful no-op and cannot use a newer row incarnation as its target.
    pub async fn apply_timeout(
        &self,
        timeout: &ActionReviewTimeout,
    ) -> Result<ActionReviewTimeoutDelivery, sqlx::Error> {
        match load_action_review_timeout_state(&self.pool, timeout).await? {
            ActionReviewTimeoutState::Stale => return Ok(ActionReviewTimeoutDelivery::Stale),
            ActionReviewTimeoutState::Pending => {
                let resolution_trace_context = current_trace_context();
                let timed_out = action_review_store::timeout_expired_reviews(
                    &self.pool,
                    resolution_trace_context.as_ref(),
                )
                .await?;
                record_timed_out_reviews(&timed_out);
            }
            ActionReviewTimeoutState::TimedOut => {}
        }
        load_action_review_timeout_delivery(&self.pool, timeout).await
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionReviewTimeoutState {
    Stale,
    Pending,
    TimedOut,
}

async fn load_action_review_timeout_state(
    pool: &PgPool,
    timeout: &ActionReviewTimeout,
) -> Result<ActionReviewTimeoutState, sqlx::Error> {
    let Some(row) = action_review_store::load_action_review_timeout_snapshot(
        pool,
        action_review_store::ActionReviewTimeoutLookup {
            tenant_id: timeout.tenant_id,
            review_id: timeout.review_id,
        },
    )
    .await?
    else {
        return Ok(ActionReviewTimeoutState::Stale);
    };
    if row.owner != timeout.owner {
        return Ok(ActionReviewTimeoutState::Stale);
    }
    match row.status {
        ActionReviewStatus::Pending
            if row.is_due && row.owner_registered && row.execution_requested_at.is_none() =>
        {
            Ok(ActionReviewTimeoutState::Pending)
        }
        ActionReviewStatus::Timeout => Ok(ActionReviewTimeoutState::TimedOut),
        _ => Ok(ActionReviewTimeoutState::Stale),
    }
}

async fn load_action_review_timeout_delivery(
    pool: &PgPool,
    timeout: &ActionReviewTimeout,
) -> Result<ActionReviewTimeoutDelivery, sqlx::Error> {
    let Some(row) = action_review_store::load_action_review_timeout_snapshot(
        pool,
        action_review_store::ActionReviewTimeoutLookup {
            tenant_id: timeout.tenant_id,
            review_id: timeout.review_id,
        },
    )
    .await?
    else {
        return Ok(ActionReviewTimeoutDelivery::Stale);
    };
    if row.owner != timeout.owner || row.status != ActionReviewStatus::Timeout {
        return Ok(ActionReviewTimeoutDelivery::Stale);
    }
    if row.owner.is_conversational() {
        if row.owner_release_delivered_at.is_some() {
            return Ok(ActionReviewTimeoutDelivery::AlreadyDelivered);
        }
        let Some(timed_out_at) = row.decided_at else {
            return Err(sqlx::Error::Protocol(
                "timed-out action review has no decision timestamp".to_string(),
            ));
        };
        return Ok(ActionReviewTimeoutDelivery::Conversational {
            timed_out_at,
            release: ActionReviewRelease {
                review_id: timeout.review_id,
                owner: row.owner,
                resume_queued: false,
            },
        });
    }
    Ok(ActionReviewTimeoutDelivery::Execution)
}

fn record_timed_out_reviews(reviews: &[action_review_store::TimedOutReview]) {
    for review in reviews {
        record_action_review_decision(ActionReviewStatus::Timeout, review.action_class);
        let wait = (review.decided_at - review.created_at)
            .to_std()
            .unwrap_or_default();
        record_approval_wait(review.action_class, wait);
    }
    if !reviews.is_empty() {
        tracing::warn!(
            count = reviews.len(),
            "tenant action reviews timed out and failed closed"
        );
    }
}

fn current_trace_context() -> Option<ValidatedTraceContext> {
    let headers = moa_observability::current_trace_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

/// Handle used to stop the action-review reaper.
pub struct ActionReviewReaperHandle {
    health: Arc<ActionReviewReaperHealth>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), ActionReviewReaperError>>,
    heartbeat_maximum_age: Duration,
}

impl ActionReviewReaperHandle {
    /// Returns a cloneable readiness projection for the supervised reaper.
    #[must_use]
    pub fn readiness(&self) -> ActionReviewReaperReadiness {
        ActionReviewReaperReadiness {
            health: Arc::clone(&self.health),
            heartbeat_maximum_age: self.heartbeat_maximum_age,
        }
    }

    /// Waits for the reaper task so unexpected failure can terminate its owner process.
    pub async fn task_result(&mut self) -> Result<(), ActionReviewReaperError> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) => Err(ActionReviewReaperError::Join(error.to_string())),
        }
    }

    /// Signals shutdown and waits for the task to exit.
    pub async fn shutdown(mut self) -> Result<(), ActionReviewReaperError> {
        let _ = self.shutdown.send(true);
        self.task_result().await
    }
}

impl Drop for ActionReviewReaperHandle {
    fn drop(&mut self) {
        self.health.exited.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

/// Cloneable readiness projection for action-review timeout reconciliation.
#[derive(Clone)]
pub struct ActionReviewReaperReadiness {
    health: Arc<ActionReviewReaperHealth>,
    heartbeat_maximum_age: Duration,
}

impl ActionReviewReaperReadiness {
    /// Returns the age of the most recent complete successful reconciliation pass.
    #[must_use]
    pub fn heartbeat_age(&self) -> Duration {
        let heartbeat = self
            .health
            .last_success
            .read()
            .ok()
            .and_then(|value| *value);
        heartbeat.map_or_else(|| self.health.started_at.elapsed(), |value| value.elapsed())
    }

    /// Returns why the reaper must not be considered ready.
    #[must_use]
    pub fn unready_reason(&self) -> Option<String> {
        if self.health.exited.load(Ordering::Acquire) {
            return Some("action-review reaper exited".to_string());
        }
        reaper_heartbeat_reason(
            "action-review reaper",
            self.health.started_at,
            &self.health.last_success,
            self.heartbeat_maximum_age,
        )
    }
}

#[derive(Debug)]
struct ActionReviewReaperHealth {
    started_at: Instant,
    last_success: RwLock<Option<Instant>>,
    exited: AtomicBool,
}

fn set_action_review_reaper_heartbeat(health: &ActionReviewReaperHealth) {
    if let Ok(mut heartbeat) = health.last_success.write() {
        *heartbeat = Some(Instant::now());
    }
}

/// Returns a bounded readiness reason for a periodic reconciliation owner.
pub(super) fn reaper_heartbeat_reason(
    name: &str,
    started_at: Instant,
    last_success: &RwLock<Option<Instant>>,
    maximum_age: Duration,
) -> Option<String> {
    let heartbeat = last_success.read().ok().and_then(|value| *value);
    let age = heartbeat.map_or_else(|| started_at.elapsed(), |value| value.elapsed());
    if heartbeat.is_none() {
        return Some(format!("{name} has not completed its first pass"));
    }
    (age > maximum_age).then(|| format!("{name} heartbeat is stale by {:.3}s", age.as_secs_f64()))
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn readiness_requires_a_complete_pass_and_rejects_exit() {
        // Pins: the maintenance role cannot report ready before this sole
        // reconciliation owner succeeds or after it exits.
        let health = Arc::new(ActionReviewReaperHealth {
            started_at: Instant::now(),
            last_success: RwLock::new(None),
            exited: AtomicBool::new(false),
        });
        let readiness = ActionReviewReaperReadiness {
            health: Arc::clone(&health),
            heartbeat_maximum_age: Duration::from_secs(60),
        };
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("action-review reaper has not completed its first pass")
        );

        set_action_review_reaper_heartbeat(&health);
        assert_eq!(readiness.unready_reason(), None);

        health.exited.store(true, Ordering::Release);
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("action-review reaper exited")
        );
    }
}
