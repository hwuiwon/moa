//! Background timeout reaper for builtin approvals.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::traits::ApprovalDecision;
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::time::interval;
use uuid::Uuid;

/// Approval reaper failures.
#[derive(Debug, Error)]
pub enum ReaperError {
    /// Database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Decision payload serialization failed.
    #[error("serialize approval decision: {0}")]
    Serde(#[from] serde_json::Error),
    /// HTTP client construction failed.
    #[error("build awakeable resolver HTTP client: {0}")]
    BuildClient(reqwest::Error),
    /// HTTP request to Restate failed.
    #[error("send awakeable resolve request: {0}")]
    Transport(reqwest::Error),
    /// Restate rejected the awakeable resolution request.
    #[error("awakeable resolve failed {status}: {body}")]
    ResolveHttp {
        /// HTTP status returned by Restate.
        status: reqwest::StatusCode,
        /// Response body returned by Restate.
        body: String,
    },
}

/// Background worker that marks expired approvals as timed out.
pub struct ApprovalReaper {
    pool: PgPool,
    sweep_interval: Duration,
}

impl ApprovalReaper {
    /// Build a reaper with the default sweep interval.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            sweep_interval: Duration::from_secs(30),
        }
    }

    /// Spawn the reaper as a Tokio task.
    pub fn spawn(self, resolver: Arc<dyn AwakeableResolver>) -> ApprovalReaperHandle {
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let mut tick = interval(self.sweep_interval);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("approval reaper received shutdown");
                        break;
                    }
                    _ = tick.tick() => {}
                }
                if let Err(error) = self.sweep(resolver.as_ref()).await {
                    tracing::error!(error = %error, "approval reaper sweep failed");
                }
            }
        });
        ApprovalReaperHandle {
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Run one timeout sweep.
    pub async fn sweep(&self, resolver: &dyn AwakeableResolver) -> Result<usize, ReaperError> {
        let expired: Vec<(Uuid, String)> = sqlx::query_as(
            r#"
            UPDATE builtin_pending_approvals
            SET status = 'timeout',
                decided_at = NOW()
            WHERE status = 'pending' AND expires_at <= NOW()
            RETURNING id, awakeable_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        for (id, awakeable_id) in &expired {
            let payload = serde_json::to_value(&ApprovalDecision::Timeout)?;
            if let Err(error) = resolver.resolve(awakeable_id, &payload).await {
                tracing::warn!(
                    approval_id = %id,
                    awakeable_id = %awakeable_id,
                    error = %error,
                    "timeout awakeable resolve failed"
                );
            }
        }

        Ok(expired.len())
    }
}

/// Handle used to stop the approval reaper.
pub struct ApprovalReaperHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ApprovalReaperHandle {
    /// Signal shutdown and wait for the task to exit.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

/// Resolve a Restate awakeable from outside the waiting handler context.
#[async_trait]
pub trait AwakeableResolver: Send + Sync {
    /// Resolve `awakeable_id` with `payload`.
    async fn resolve(
        &self,
        awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), ReaperError>;
}

/// HTTP implementation that calls Restate's awakeable resolve API.
pub struct HttpAwakeableResolver {
    base_url: String,
    client: reqwest::Client,
}

impl HttpAwakeableResolver {
    /// Build an HTTP resolver over a Restate ingress base URL.
    pub fn new(base_url: impl Into<String>) -> Result<Self, ReaperError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(ReaperError::BuildClient)?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }
}

#[async_trait]
impl AwakeableResolver for HttpAwakeableResolver {
    async fn resolve(
        &self,
        awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), ReaperError> {
        let response = self
            .client
            .post(format!(
                "{}/restate/awakeables/{}/resolve",
                self.base_url, awakeable_id
            ))
            .json(payload)
            .send()
            .await
            .map_err(ReaperError::Transport)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => format!("<failed to read body: {error}>"),
            };
            return Err(ReaperError::ResolveHttp { status, body });
        }
        Ok(())
    }
}
