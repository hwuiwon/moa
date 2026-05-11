//! Auth0 Client-Initiated Backchannel Authentication provider.
//!
//! The provider starts a CIBA request at `/bc-authorize`, then polls
//! `/oauth/token` until Auth0 reports approval, denial, or timeout. Final
//! decisions resolve the Restate awakeable supplied in the approval request.

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use moa_authz::AwakeableResolver;
use moa_core::traits::{
    ApprovalDecision, ApprovalHandle, ApprovalRequest, AsyncAuthzError, AsyncAuthzProvider,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Auth0 CIBA-backed implementation of [`AsyncAuthzProvider`].
pub struct Auth0AsyncAuthzProvider {
    http: reqwest::Client,
    base_url: String,
    issuer: String,
    client_id: String,
    client_secret: SecretString,
    pool: Arc<PgPool>,
    resolver: Arc<dyn AwakeableResolver>,
}

impl Auth0AsyncAuthzProvider {
    /// Construct a provider for an Auth0 tenant domain.
    pub fn new(
        domain: String,
        client_id: String,
        client_secret: SecretString,
        pool: Arc<PgPool>,
        resolver: Arc<dyn AwakeableResolver>,
    ) -> Result<Self, AsyncAuthzError> {
        let trimmed = domain.trim_end_matches('/').to_string();
        Self::new_with_base_url(
            format!("https://{trimmed}"),
            format!("https://{trimmed}/"),
            client_id,
            client_secret,
            pool,
            resolver,
        )
    }

    /// Construct a provider with an explicit Auth0 base URL and issuer.
    pub fn new_with_base_url(
        base_url: String,
        issuer: String,
        client_id: String,
        client_secret: SecretString,
        pool: Arc<PgPool>,
        resolver: Arc<dyn AwakeableResolver>,
    ) -> Result<Self, AsyncAuthzError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| AsyncAuthzError::Unavailable(format!("http client: {error}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            issuer,
            client_id,
            client_secret,
            pool,
            resolver,
        })
    }
}

#[async_trait]
impl AsyncAuthzProvider for Auth0AsyncAuthzProvider {
    async fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalHandle, AsyncAuthzError> {
        let sub: Option<(String,)> =
            sqlx::query_as("SELECT sub FROM auth0_user_map WHERE user_id = $1 LIMIT 1")
                .bind(request.deciding_user_id)
                .fetch_optional(&*self.pool)
                .await
                .map_err(|error| AsyncAuthzError::Internal(format!("db: {error}")))?;
        let Some((sub,)) = sub else {
            return Err(AsyncAuthzError::Internal(
                "user has no Auth0 subject mapping; cannot start CIBA".to_string(),
            ));
        };

        #[derive(Debug, Deserialize)]
        struct BackchannelResponse {
            auth_req_id: String,
            expires_in: i64,
            #[serde(default)]
            interval: Option<u64>,
        }

        let login_hint = serde_json::json!({
            "format": "iss_sub",
            "iss": self.issuer,
            "sub": sub,
        })
        .to_string();
        let binding_message = binding_message(&request.action_summary);
        let requested_expiry = request.timeout.as_secs().clamp(1, 300).to_string();
        let response: BackchannelResponse = self
            .http
            .post(format!("{}/bc-authorize", self.base_url))
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.expose_secret()),
                ("scope", "openid"),
                ("login_hint", login_hint.as_str()),
                ("binding_message", binding_message.as_str()),
                ("requested_expiry", requested_expiry.as_str()),
            ])
            .send()
            .await
            .map_err(|error| AsyncAuthzError::Unavailable(format!("ciba: {error}")))?
            .error_for_status()
            .map_err(|error| AsyncAuthzError::Unavailable(format!("ciba status: {error}")))?
            .json()
            .await
            .map_err(|error| AsyncAuthzError::Unavailable(format!("ciba parse: {error}")))?;

        let auth_req_id = response.auth_req_id.clone();
        let handle = ApprovalHandle {
            id: Uuid::new_v4(),
            awakeable_id: request.awakeable_id.clone(),
            provider_specific: serde_json::json!({
                "kind": "auth0_ciba",
                "auth_req_id": auth_req_id.clone(),
                "interval": response.interval.unwrap_or(5),
                "expires_in": response.expires_in,
            }),
        };

        let poller = CibaPoller {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            resolver: self.resolver.clone(),
            awakeable_id: request.awakeable_id,
            auth_req_id,
            interval: Duration::from_secs(response.interval.unwrap_or(5).max(1)),
            deadline: Utc::now() + ChronoDuration::seconds(response.expires_in),
        };
        tokio::spawn(async move {
            poller.run().await;
        });

        Ok(handle)
    }

    async fn poll_decision(
        &self,
        _handle: &ApprovalHandle,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "auth0_ciba"
    }
}

struct CibaPoller {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    client_secret: SecretString,
    resolver: Arc<dyn AwakeableResolver>,
    awakeable_id: String,
    auth_req_id: String,
    interval: Duration,
    deadline: chrono::DateTime<Utc>,
}

impl CibaPoller {
    async fn run(mut self) {
        loop {
            if Utc::now() >= self.deadline {
                self.resolve(serde_json::json!({ "outcome": "timeout" }))
                    .await;
                return;
            }
            sleep(self.interval).await;
            match self.poll_once().await {
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => {
                    self.interval += Duration::from_secs(5);
                }
                PollOutcome::Approved => {
                    self.resolve(serde_json::json!({ "outcome": "approved" }))
                        .await;
                    return;
                }
                PollOutcome::Denied(reason) => {
                    self.resolve(serde_json::json!({
                        "outcome": "denied",
                        "reason": reason,
                    }))
                    .await;
                    return;
                }
                PollOutcome::Timeout => {
                    self.resolve(serde_json::json!({ "outcome": "timeout" }))
                        .await;
                    return;
                }
            }
        }
    }

    async fn poll_once(&self) -> PollOutcome {
        #[derive(Debug, Deserialize)]
        struct PollResponse {
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            error_description: Option<String>,
            #[serde(default)]
            access_token: Option<String>,
        }

        let response = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .form(&[
                ("grant_type", "urn:openid:params:grant-type:ciba"),
                ("auth_req_id", self.auth_req_id.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.expose_secret()),
            ])
            .send()
            .await;
        let Ok(response) = response else {
            return PollOutcome::Pending;
        };
        let body = response.json::<PollResponse>().await;
        let Ok(body) = body else {
            return PollOutcome::Pending;
        };
        match (body.error.as_deref(), body.access_token.as_deref()) {
            (None, Some(_)) => PollOutcome::Approved,
            (Some("authorization_pending"), _) => PollOutcome::Pending,
            (Some("slow_down"), _) => PollOutcome::SlowDown,
            (Some("expired_token"), _) => PollOutcome::Timeout,
            (Some("access_denied"), _) => PollOutcome::Denied(body.error_description),
            (Some(other), _) => PollOutcome::Denied(Some(format!("ciba error: {other}"))),
            (None, None) => PollOutcome::Pending,
        }
    }

    async fn resolve(&self, payload: serde_json::Value) {
        if let Err(error) = self.resolver.resolve(&self.awakeable_id, &payload).await {
            tracing::warn!(
                awakeable_id = %self.awakeable_id,
                error = %error,
                "CIBA awakeable resolve failed"
            );
        }
    }
}

enum PollOutcome {
    Pending,
    SlowDown,
    Approved,
    Denied(Option<String>),
    Timeout,
}

fn binding_message(summary: &str) -> String {
    let sanitized: String = summary
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '_' | '.' | ',' | ':' | '#') {
                Some(ch)
            } else if ch.is_whitespace() {
                Some('_')
            } else {
                None
            }
        })
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "MOA_approval".to_string()
    } else {
        sanitized
    }
}
