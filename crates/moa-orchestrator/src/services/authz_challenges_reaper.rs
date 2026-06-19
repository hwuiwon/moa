//! Background timeout reaper for builtin async authorization challenges.

use std::sync::Arc;
use std::time::Duration;

use moa_authz::{AwakeableResolveError, AwakeableResolver};
use moa_core::traits::ApprovalDecision;
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::time::interval;

use crate::authz_challenges::store as authz_challenge_store;

/// Authz challenge reaper failures.
#[derive(Debug, Error)]
pub enum ReaperError {
    /// Database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Decision payload serialization failed.
    #[error("serialize authz challenge decision: {0}")]
    Serde(#[from] serde_json::Error),
    /// Persisted challenge status cannot be resolved.
    #[error("invalid terminal authz challenge status: {0}")]
    InvalidStatus(String),
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
    /// Awakeable resolution through the shared resolver trait failed.
    #[error("resolve awakeable: {0}")]
    Resolve(#[from] AwakeableResolveError),
}

/// Background worker that marks expired authz challenges as timed out.
pub struct AuthzChallengeReaper {
    pool: PgPool,
    sweep_interval: Duration,
}

impl AuthzChallengeReaper {
    /// Build a reaper with the default sweep interval.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            sweep_interval: Duration::from_secs(30),
        }
    }

    /// Spawn the reaper as a Tokio task.
    pub fn spawn(self, resolver: Arc<dyn AwakeableResolver>) -> AuthzChallengeReaperHandle {
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let mut tick = interval(self.sweep_interval);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("authz challenge reaper received shutdown");
                        break;
                    }
                    _ = tick.tick() => {}
                }
                if let Err(error) = self.sweep(resolver.as_ref()).await {
                    tracing::error!(error = %error, "authz challenge reaper sweep failed");
                }
            }
        });
        AuthzChallengeReaperHandle {
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Run one timeout sweep.
    pub async fn sweep(&self, resolver: &dyn AwakeableResolver) -> Result<usize, ReaperError> {
        let unresolved =
            authz_challenge_store::unresolved_terminal_builtin_challenges(&self.pool).await?;

        for challenge in &unresolved {
            let decision = decision_from_unresolved_challenge(challenge)?;
            let payload = serde_json::to_value(&decision)?;
            if let Err(error) = resolver.resolve(&challenge.awakeable_id, &payload).await {
                tracing::warn!(
                    authz_challenge_id = %challenge.id,
                    awakeable_id = %challenge.awakeable_id,
                    error = %error,
                    "authz challenge awakeable resolve failed"
                );
                continue;
            }
            authz_challenge_store::mark_builtin_challenge_resolved(&self.pool, challenge.id)
                .await?;
        }

        Ok(unresolved.len())
    }
}

fn decision_from_unresolved_challenge(
    challenge: &authz_challenge_store::UnresolvedBuiltinChallenge,
) -> Result<ApprovalDecision, ReaperError> {
    match challenge.status.as_str() {
        "approved" => Ok(ApprovalDecision::Approved),
        "denied" => Ok(ApprovalDecision::Denied {
            reason: challenge.deny_reason.clone(),
        }),
        "timeout" => Ok(ApprovalDecision::Timeout),
        other => Err(ReaperError::InvalidStatus(other.to_string())),
    }
}

/// Handle used to stop the authz challenge reaper.
pub struct AuthzChallengeReaperHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl AuthzChallengeReaperHandle {
    /// Signal shutdown and wait for the task to exit.
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
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

#[async_trait::async_trait]
impl AwakeableResolver for HttpAwakeableResolver {
    async fn resolve(
        &self,
        awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AwakeableResolveError> {
        let response = self
            .client
            .post(format!(
                "{}/restate/awakeables/{}/resolve",
                self.base_url, awakeable_id
            ))
            .json(payload)
            .send()
            .await
            .map_err(|error| AwakeableResolveError::message(format!("transport: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => format!("<failed to read body: {error}>"),
            };
            return Err(AwakeableResolveError::message(format!(
                "HTTP {status}: {body}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn unresolved_challenge_status_maps_to_awakeable_decision() {
        // Pins: the reaper reconstructs the original terminal decision before retrying delivery.
        let denied = authz_challenge_store::UnresolvedBuiltinChallenge {
            id: Uuid::now_v7(),
            awakeable_id: "awakeable-denied".to_string(),
            status: "denied".to_string(),
            deny_reason: Some("policy denied".to_string()),
        };
        let timeout = authz_challenge_store::UnresolvedBuiltinChallenge {
            id: Uuid::now_v7(),
            awakeable_id: "awakeable-timeout".to_string(),
            status: "timeout".to_string(),
            deny_reason: None,
        };

        assert_eq!(
            decision_from_unresolved_challenge(&denied).expect("denial maps"),
            ApprovalDecision::Denied {
                reason: Some("policy denied".to_string())
            }
        );
        assert_eq!(
            decision_from_unresolved_challenge(&timeout).expect("timeout maps"),
            ApprovalDecision::Timeout
        );
    }
}
