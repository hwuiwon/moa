//! Background timeout reaper for builtin async authorization challenges.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use moa_authz::{AwakeableResolveError, AwakeableResolver};
use moa_core::traits::ApprovalDecision;
use moa_observability::{
    record_builtin_approval_decision, record_builtin_approval_oldest_pending_age,
    record_builtin_approval_pending_depth,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::interval;

use crate::authz_challenges::store as authz_challenge_store;
use crate::services::durable_timeout::{
    AuthzChallengeTimeout, DURABLE_TIMEOUT_RECONCILIATION_INTERVAL,
};

/// Durable delivery selected after applying one exact authz-challenge timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthzChallengeTimeoutDelivery {
    /// The row no longer matches the delayed challenge incarnation.
    Stale,
    /// Resolution is complete or is already owned by another exact claim.
    AlreadyDelivered,
    /// The exact timeout claim must resolve its original awakeable.
    Resolve {
        /// Stable challenge row identifier.
        challenge_id: uuid::Uuid,
        /// Exact awakeable fenced by this delayed delivery.
        awakeable_id: String,
        /// Claim token that must fence the delivery acknowledgement.
        resolve_claim_token: uuid::Uuid,
        /// Whether this delivery changed the row from pending to timeout.
        newly_timed_out: bool,
    },
}

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
    /// The supervised reaper task could not be joined.
    #[error("authz-challenge reaper task join failed: {0}")]
    Join(String),
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
            sweep_interval: DURABLE_TIMEOUT_RECONCILIATION_INTERVAL,
        }
    }

    /// Spawn the reaper as a Tokio task.
    pub fn spawn(self, resolver: Arc<dyn AwakeableResolver>) -> AuthzChallengeReaperHandle {
        let health = Arc::new(AuthzChallengeReaperHealth {
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
                            tracing::info!("authz challenge reaper received shutdown");
                            return Ok(());
                        }
                        _ = tick.tick() => {}
                    }
                    self.sweep(resolver.as_ref()).await?;
                    // Queue sampling is observational and cannot invalidate a
                    // completed correctness pass.
                    let _ = self.sample_gauges().await;
                    set_authz_challenge_reaper_heartbeat(&task_health);
                }
            }
            .await;
            task_health.exited.store(true, Ordering::Release);
            result
        });
        AuthzChallengeReaperHandle {
            health,
            shutdown,
            task,
            heartbeat_maximum_age,
        }
    }

    /// Samples the pending builtin-approval queue depth and oldest-pending age.
    ///
    /// Returns the sampling error so tests can assert the query decodes
    /// against the real schema; the tick loop treats it as best-effort and a
    /// sampling error never fails a tick, matching the authz outbox backlog
    /// gauge.
    pub async fn sample_gauges(&self) -> Result<(), sqlx::Error> {
        match authz_challenge_store::builtin_approval_pending_stats(&self.pool).await {
            Ok(stats) => {
                record_builtin_approval_pending_depth(stats.pending_depth.max(0) as u64);
                record_builtin_approval_oldest_pending_age(stats.oldest_pending_age_seconds);
                Ok(())
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to sample builtin approval queue gauges");
                Err(error)
            }
        }
    }

    /// Run one timeout sweep.
    pub async fn sweep(&self, resolver: &dyn AwakeableResolver) -> Result<usize, ReaperError> {
        let sweep =
            authz_challenge_store::unresolved_terminal_builtin_challenges(&self.pool).await?;
        for _ in &sweep.timed_out {
            record_builtin_approval_decision("timeout");
        }
        let unresolved = sweep.unresolved;

        let mut resolved_count = 0usize;
        for challenge in &unresolved {
            let decision = decision_from_unresolved_challenge(challenge)?;
            let payload = serde_json::to_value(&decision)?;
            if let Err(error) = resolver.resolve(&challenge.awakeable_id, &payload).await {
                if missing_awakeable_error(&error) {
                    let marked = authz_challenge_store::mark_claimed_builtin_challenge_resolved(
                        &self.pool,
                        challenge.id,
                        challenge.resolve_claim_token,
                    )
                    .await?;
                    if marked {
                        tracing::warn!(
                            authz_challenge_id = %challenge.id,
                            awakeable_id = %challenge.awakeable_id,
                            error = %error,
                            "suppressing authz challenge retry for missing awakeable"
                        );
                    }
                    continue;
                }
                authz_challenge_store::release_builtin_challenge_resolution_claim(
                    &self.pool,
                    challenge.id,
                    challenge.resolve_claim_token,
                )
                .await?;
                tracing::warn!(
                    authz_challenge_id = %challenge.id,
                    awakeable_id = %challenge.awakeable_id,
                    error = %error,
                    "authz challenge awakeable resolve failed"
                );
                continue;
            }
            let marked = authz_challenge_store::mark_claimed_builtin_challenge_resolved(
                &self.pool,
                challenge.id,
                challenge.resolve_claim_token,
            )
            .await?;
            if marked {
                resolved_count += 1;
            } else {
                tracing::debug!(
                    authz_challenge_id = %challenge.id,
                    "authz challenge resolution claim was already completed elsewhere"
                );
            }
        }

        Ok(resolved_count)
    }

    /// Applies and claims one exact delayed authz-challenge timeout.
    ///
    /// Both the row id and original awakeable are compared. A late delivery
    /// from an older challenge incarnation therefore returns
    /// [`AuthzChallengeTimeoutDelivery::Stale`] without changing or resolving
    /// the current row.
    pub async fn apply_timeout(
        &self,
        timeout: &AuthzChallengeTimeout,
    ) -> Result<AuthzChallengeTimeoutDelivery, sqlx::Error> {
        let request = authz_challenge_store::BuiltinChallengeTimeoutLookup {
            challenge_id: timeout.challenge_id,
            awakeable_id: timeout.awakeable_id.clone(),
        };
        match authz_challenge_store::apply_builtin_challenge_timeout(&self.pool, &request).await? {
            authz_challenge_store::BuiltinChallengeTimeoutClaim::Resolve {
                challenge_id,
                awakeable_id,
                resolve_claim_token,
                newly_timed_out,
            } => Ok(AuthzChallengeTimeoutDelivery::Resolve {
                challenge_id,
                awakeable_id,
                resolve_claim_token,
                newly_timed_out,
            }),
            authz_challenge_store::BuiltinChallengeTimeoutClaim::AlreadyDelivered => {
                Ok(AuthzChallengeTimeoutDelivery::AlreadyDelivered)
            }
            authz_challenge_store::BuiltinChallengeTimeoutClaim::Stale => {
                Ok(AuthzChallengeTimeoutDelivery::Stale)
            }
        }
    }
}

fn missing_awakeable_error(error: &AwakeableResolveError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("missing awakeable")
        || message.contains("awakeable not found")
        || message.starts_with("http 404")
        || message.starts_with("http 410")
        || message.contains("http 404")
        || message.contains("http 410")
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
    health: Arc<AuthzChallengeReaperHealth>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), ReaperError>>,
    heartbeat_maximum_age: Duration,
}

impl AuthzChallengeReaperHandle {
    /// Returns a cloneable readiness projection for the supervised reaper.
    #[must_use]
    pub fn readiness(&self) -> AuthzChallengeReaperReadiness {
        AuthzChallengeReaperReadiness {
            health: Arc::clone(&self.health),
            heartbeat_maximum_age: self.heartbeat_maximum_age,
        }
    }

    /// Waits for the reaper task so unexpected failure can terminate its owner process.
    pub async fn task_result(&mut self) -> Result<(), ReaperError> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) => Err(ReaperError::Join(error.to_string())),
        }
    }

    /// Signals shutdown and waits for the task to exit.
    pub async fn shutdown(mut self) -> Result<(), ReaperError> {
        let _ = self.shutdown.send(true);
        self.task_result().await
    }
}

impl Drop for AuthzChallengeReaperHandle {
    fn drop(&mut self) {
        self.health.exited.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

/// Cloneable readiness projection for builtin-authz timeout reconciliation.
#[derive(Clone)]
pub struct AuthzChallengeReaperReadiness {
    health: Arc<AuthzChallengeReaperHealth>,
    heartbeat_maximum_age: Duration,
}

impl AuthzChallengeReaperReadiness {
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
            return Some("authz-challenge reaper exited".to_string());
        }
        super::action_reviews_reaper::reaper_heartbeat_reason(
            "authz-challenge reaper",
            self.health.started_at,
            &self.health.last_success,
            self.heartbeat_maximum_age,
        )
    }
}

#[derive(Debug)]
struct AuthzChallengeReaperHealth {
    started_at: Instant,
    last_success: RwLock<Option<Instant>>,
    exited: AtomicBool,
}

fn set_authz_challenge_reaper_heartbeat(health: &AuthzChallengeReaperHealth) {
    if let Ok(mut heartbeat) = health.last_success.write() {
        *heartbeat = Some(Instant::now());
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
        let response = crate::restate_identity::with_reqwest_trace_headers(
            self.client
                .post(format!(
                    "{}/restate/awakeables/{}/resolve",
                    self.base_url, awakeable_id
                ))
                .json(payload),
        )
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
            resolve_claim_token: Uuid::now_v7(),
        };
        let timeout = authz_challenge_store::UnresolvedBuiltinChallenge {
            id: Uuid::now_v7(),
            awakeable_id: "awakeable-timeout".to_string(),
            status: "timeout".to_string(),
            deny_reason: None,
            resolve_claim_token: Uuid::now_v7(),
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

    #[test]
    fn readiness_requires_a_complete_pass_and_rejects_exit() {
        // Pins: the maintenance role cannot report ready before this sole
        // reconciliation owner succeeds or after it exits.
        let health = Arc::new(AuthzChallengeReaperHealth {
            started_at: Instant::now(),
            last_success: RwLock::new(None),
            exited: AtomicBool::new(false),
        });
        let readiness = AuthzChallengeReaperReadiness {
            health: Arc::clone(&health),
            heartbeat_maximum_age: Duration::from_secs(60),
        };
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("authz-challenge reaper has not completed its first pass")
        );

        set_authz_challenge_reaper_heartbeat(&health);
        assert_eq!(readiness.unready_reason(), None);

        health.exited.store(true, Ordering::Release);
        assert_eq!(
            readiness.unready_reason().as_deref(),
            Some("authz-challenge reaper exited")
        );
    }
}
