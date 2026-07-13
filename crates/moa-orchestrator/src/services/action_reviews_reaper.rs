//! Background timeout reaper and gauge sampler for tenant action reviews.
//!
//! Pending tenant action reviews (`tenant_action_reviews`) fail closed: a row
//! that is not decided before its `expires_at` is transitioned to `timeout` so
//! the gated tool never executes. The same periodic tick publishes the
//! pending-queue depth and oldest-pending-age gauges, mirroring the builtin
//! authz challenge reaper's poll-and-emit shape.

use std::time::Duration;

use moa_core::types::action_policy::ActionReviewStatus;
use moa_observability::{
    record_action_review_decision, record_action_review_oldest_pending_age,
    record_action_review_pending_depth, record_approval_wait,
};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::time::interval;

use crate::action_reviews::store as action_review_store;

/// Action-review reaper failures.
#[derive(Debug, Error)]
pub enum ActionReviewReaperError {
    /// Database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Background worker that times out expired reviews and samples queue gauges.
pub struct ActionReviewReaper {
    pool: PgPool,
    sweep_interval: Duration,
}

impl ActionReviewReaper {
    /// Build a reaper with the default sweep interval.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            sweep_interval: Duration::from_secs(30),
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
        let timed_out = action_review_store::timeout_expired_reviews(&self.pool).await?;
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
        Ok(timed_out.len())
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
