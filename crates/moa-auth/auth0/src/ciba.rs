//! Auth0 Client-Initiated Backchannel Authentication provider.
//!
//! The provider starts a CIBA request at `/bc-authorize`, then polls
//! `/oauth/token` until Auth0 reports approval, denial, or timeout. Final
//! decisions resolve the Restate awakeable supplied in the approval request.

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use moa_authz::AwakeableResolver;
use moa_core::traits::{
    ApprovalDecision, ApprovalHandle, ApprovalRequest, AsyncAuthzError, AsyncAuthzProvider,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, sleep};
use uuid::Uuid;

const CIBA_RECOVERY_INTERVAL: Duration = Duration::from_secs(10);
const CIBA_LEASE_DURATION: Duration = Duration::from_secs(120);

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
        let provider = Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            issuer,
            client_id,
            client_secret,
            pool,
            resolver,
        };
        provider.spawn_recovery_loop();
        Ok(provider)
    }

    fn worker(&self) -> CibaWorker {
        CibaWorker {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            pool: self.pool.clone(),
            resolver: self.resolver.clone(),
        }
    }

    fn spawn_recovery_loop(&self) {
        let worker = self.worker();
        tokio::spawn(async move {
            let mut tick = interval(CIBA_RECOVERY_INTERVAL);
            loop {
                tick.tick().await;
                if let Err(error) = worker.recover_due_work().await {
                    tracing::debug!(error = %error, "CIBA recovery sweep failed");
                }
            }
        });
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
        let approval_id = Uuid::new_v4();
        let interval_seconds = response.interval.unwrap_or(5).max(1);
        let expires_at = Utc::now() + ChronoDuration::seconds(response.expires_in);
        sqlx::query(
            r#"
            INSERT INTO auth0_ciba_approvals
                (id, session_id, deciding_user_id, awakeable_id, auth_req_id,
                 poll_interval_ms, next_poll_at, expires_at)
            VALUES
                ($1, $2, $3, $4, $5, $6,
                 NOW() + ($7 || ' milliseconds')::INTERVAL, $8)
            "#,
        )
        .bind(approval_id)
        .bind(request.session_id)
        .bind(request.deciding_user_id)
        .bind(&request.awakeable_id)
        .bind(&auth_req_id)
        .bind(duration_millis_i32(Duration::from_secs(interval_seconds))?)
        .bind(duration_millis_string(Duration::from_secs(
            interval_seconds,
        )))
        .bind(expires_at)
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("persist ciba request: {error}")))?;

        let handle = ApprovalHandle {
            id: approval_id,
            awakeable_id: request.awakeable_id.clone(),
            provider_specific: serde_json::json!({
                "kind": "auth0_ciba",
                "auth_req_id": auth_req_id.clone(),
                "interval": interval_seconds,
                "expires_in": response.expires_in,
            }),
        };

        let poller = CibaPoller {
            worker: self.worker(),
            approval_id,
        };
        tokio::spawn(async move {
            poller.run().await;
        });

        Ok(handle)
    }

    async fn poll_decision(
        &self,
        handle: &ApprovalHandle,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        self.worker().poll_approval(handle.id, true, false).await
    }

    fn name(&self) -> &'static str {
        "auth0_ciba"
    }
}

struct CibaPoller {
    worker: CibaWorker,
    approval_id: Uuid,
}

impl CibaPoller {
    async fn run(self) {
        loop {
            match self
                .worker
                .poll_approval(self.approval_id, false, true)
                .await
            {
                Ok(Some(_)) => return,
                Ok(None) => sleep(Duration::from_secs(1)).await,
                Err(error) => {
                    tracing::warn!(
                        approval_id = %self.approval_id,
                        error = %error,
                        "CIBA poller failed; retrying"
                    );
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

#[derive(Clone)]
struct CibaWorker {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    client_secret: SecretString,
    pool: Arc<PgPool>,
    resolver: Arc<dyn AwakeableResolver>,
}

impl CibaWorker {
    async fn recover_due_work(&self) -> Result<(), AsyncAuthzError> {
        for _ in 0..32 {
            let Some(row) = self.claim_work(None, false).await? else {
                return Ok(());
            };
            self.process_claim(row, true).await?;
        }
        Ok(())
    }

    async fn poll_approval(
        &self,
        approval_id: Uuid,
        force: bool,
        resolve_awakeable: bool,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        let Some(row) = self.claim_work(Some(approval_id), force).await? else {
            return Ok(None);
        };
        self.process_claim(row, resolve_awakeable).await
    }

    async fn process_claim(
        &self,
        row: CibaWorkRow,
        resolve_awakeable: bool,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        if let Some(decision) = decision_from_ciba_status(&row.status, row.deny_reason.clone())? {
            self.finish_terminal_row(&row, &decision, resolve_awakeable)
                .await?;
            return Ok(Some(decision));
        }

        if Utc::now() >= row.expires_at {
            let decision = ApprovalDecision::Timeout;
            if self.mark_terminal(&row, &decision).await? {
                self.finish_terminal_row(&row, &decision, resolve_awakeable)
                    .await?;
                return Ok(Some(decision));
            }
            return Ok(None);
        }

        match self.poll_once(&row.auth_req_id).await {
            PollOutcome::Pending => {
                self.reschedule(&row, row.poll_interval()).await?;
                Ok(None)
            }
            PollOutcome::SlowDown => {
                let interval = row.poll_interval() + Duration::from_secs(5);
                self.reschedule(&row, interval).await?;
                Ok(None)
            }
            PollOutcome::Approved => {
                let decision = ApprovalDecision::Approved;
                if self.mark_terminal(&row, &decision).await? {
                    self.finish_terminal_row(&row, &decision, resolve_awakeable)
                        .await?;
                    Ok(Some(decision))
                } else {
                    Ok(None)
                }
            }
            PollOutcome::Denied(reason) => {
                let decision = ApprovalDecision::Denied { reason };
                if self.mark_terminal(&row, &decision).await? {
                    self.finish_terminal_row(&row, &decision, resolve_awakeable)
                        .await?;
                    Ok(Some(decision))
                } else {
                    Ok(None)
                }
            }
            PollOutcome::Timeout => {
                let decision = ApprovalDecision::Timeout;
                if self.mark_terminal(&row, &decision).await? {
                    self.finish_terminal_row(&row, &decision, resolve_awakeable)
                        .await?;
                    Ok(Some(decision))
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn claim_work(
        &self,
        approval_id: Option<Uuid>,
        force: bool,
    ) -> Result<Option<CibaWorkRow>, AsyncAuthzError> {
        let lease_token = Uuid::new_v4();
        sqlx::query_as(
            r#"
            WITH candidate AS (
                SELECT id
                FROM auth0_ciba_approvals
                WHERE ($1::UUID IS NULL OR id = $1)
                  AND (
                    (
                        status = 'pending'
                        AND ($2 OR next_poll_at <= NOW() OR expires_at <= NOW())
                    )
                    OR (
                        status IN ('approved', 'denied', 'timeout')
                        AND resolved_at IS NULL
                    )
                  )
                  AND (lease_expires_at IS NULL OR lease_expires_at <= NOW())
                ORDER BY next_poll_at, updated_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE auth0_ciba_approvals AS approval
            SET lease_token = $3,
                lease_expires_at = NOW() + ($4 || ' milliseconds')::INTERVAL,
                updated_at = NOW()
            FROM candidate
            WHERE approval.id = candidate.id
            RETURNING approval.id, approval.awakeable_id, approval.auth_req_id,
                      approval.status, approval.deny_reason, approval.poll_interval_ms,
                      approval.expires_at, approval.lease_token
            "#,
        )
        .bind(approval_id)
        .bind(force)
        .bind(lease_token)
        .bind(duration_millis_string(CIBA_LEASE_DURATION))
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("claim ciba work: {error}")))
    }

    async fn poll_once(&self, auth_req_id: &str) -> PollOutcome {
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
                ("auth_req_id", auth_req_id),
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

    async fn mark_terminal(
        &self,
        row: &CibaWorkRow,
        decision: &ApprovalDecision,
    ) -> Result<bool, AsyncAuthzError> {
        let (status, deny_reason) = ciba_status_from_decision(decision);
        let result = sqlx::query(
            r#"
            UPDATE auth0_ciba_approvals
            SET status = $3,
                deny_reason = $4,
                updated_at = NOW()
            WHERE id = $1
              AND lease_token = $2
            "#,
        )
        .bind(row.id)
        .bind(row.lease_token)
        .bind(status)
        .bind(deny_reason)
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("mark ciba terminal: {error}")))?;
        Ok(result.rows_affected() == 1)
    }

    async fn finish_terminal_row(
        &self,
        row: &CibaWorkRow,
        decision: &ApprovalDecision,
        resolve_awakeable: bool,
    ) -> Result<(), AsyncAuthzError> {
        if !resolve_awakeable {
            self.release_claim(row).await?;
            return Ok(());
        }

        let payload = serde_json::to_value(decision)
            .map_err(|error| AsyncAuthzError::Internal(format!("serialize decision: {error}")))?;
        if let Err(error) = self.resolver.resolve(&row.awakeable_id, &payload).await {
            self.release_claim(row).await?;
            tracing::warn!(
                approval_id = %row.id,
                awakeable_id = %row.awakeable_id,
                error = %error,
                "CIBA awakeable resolve failed"
            );
            return Ok(());
        }
        sqlx::query(
            r#"
            UPDATE auth0_ciba_approvals
            SET resolved_at = COALESCE(resolved_at, NOW()),
                lease_token = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND lease_token = $2
            "#,
        )
        .bind(row.id)
        .bind(row.lease_token)
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("mark ciba resolved: {error}")))?;
        Ok(())
    }

    async fn reschedule(
        &self,
        row: &CibaWorkRow,
        next_interval: Duration,
    ) -> Result<(), AsyncAuthzError> {
        sqlx::query(
            r#"
            UPDATE auth0_ciba_approvals
            SET poll_interval_ms = $3,
                next_poll_at = NOW() + ($4 || ' milliseconds')::INTERVAL,
                lease_token = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND lease_token = $2
            "#,
        )
        .bind(row.id)
        .bind(row.lease_token)
        .bind(duration_millis_i32(next_interval)?)
        .bind(duration_millis_string(next_interval))
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("reschedule ciba poll: {error}")))?;
        Ok(())
    }

    async fn release_claim(&self, row: &CibaWorkRow) -> Result<(), AsyncAuthzError> {
        sqlx::query(
            r#"
            UPDATE auth0_ciba_approvals
            SET lease_token = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND lease_token = $2
            "#,
        )
        .bind(row.id)
        .bind(row.lease_token)
        .execute(&*self.pool)
        .await
        .map_err(|error| AsyncAuthzError::Internal(format!("release ciba claim: {error}")))?;
        Ok(())
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct CibaWorkRow {
    id: Uuid,
    awakeable_id: String,
    auth_req_id: String,
    status: String,
    deny_reason: Option<String>,
    poll_interval_ms: i32,
    expires_at: DateTime<Utc>,
    lease_token: Uuid,
}

impl CibaWorkRow {
    fn poll_interval(&self) -> Duration {
        u64::try_from(self.poll_interval_ms)
            .ok()
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(5))
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

fn decision_from_ciba_status(
    status: &str,
    deny_reason: Option<String>,
) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
    match status {
        "pending" => Ok(None),
        "approved" => Ok(Some(ApprovalDecision::Approved)),
        "denied" => Ok(Some(ApprovalDecision::Denied {
            reason: deny_reason,
        })),
        "timeout" => Ok(Some(ApprovalDecision::Timeout)),
        other => Err(AsyncAuthzError::Internal(format!(
            "unknown ciba status: {other}"
        ))),
    }
}

fn ciba_status_from_decision(decision: &ApprovalDecision) -> (&'static str, Option<&str>) {
    match decision {
        ApprovalDecision::Approved => ("approved", None),
        ApprovalDecision::Denied { reason } => ("denied", reason.as_deref()),
        ApprovalDecision::Timeout => ("timeout", None),
    }
}

fn duration_millis_i32(duration: Duration) -> Result<i32, AsyncAuthzError> {
    i32::try_from(duration.as_millis())
        .map_err(|_| AsyncAuthzError::Internal("duration exceeds i32 milliseconds".to_string()))
}

fn duration_millis_string(duration: Duration) -> String {
    duration.as_millis().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use httpmock::{Method::POST, MockServer};
    use moa_authz::{AwakeableResolveError, AwakeableResolver};
    use sqlx::{PgPool, postgres::PgPoolOptions};

    #[tokio::test]
    async fn ciba_poll_decision_resumes_persisted_auth_req_id() {
        // Pins: a restarted provider can poll Auth0 from the persisted auth_req_id row.
        let pool = test_pool().await;
        let server = MockServer::start();
        let poll = server.mock(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "approved-token" }));
        });
        let provider = Auth0AsyncAuthzProvider::new_with_base_url(
            server.base_url(),
            "https://issuer.example.test/".to_string(),
            "client-1".to_string(),
            SecretString::new("secret-1".to_string().into_boxed_str()),
            Arc::new(pool.clone()),
            Arc::new(NoopResolver),
        )
        .expect("provider should build");
        let approval_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO auth0_ciba_approvals
                (id, session_id, deciding_user_id, awakeable_id, auth_req_id,
                 poll_interval_ms, next_poll_at, expires_at)
            VALUES
                ($1, $2, $3, 'awakeable-resume', 'persisted-auth-req',
                 1000, NOW() + INTERVAL '5 minutes', NOW() + INTERVAL '10 minutes')
            "#,
        )
        .bind(approval_id)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .expect("persisted CIBA row should insert");
        let handle = ApprovalHandle {
            id: approval_id,
            awakeable_id: "awakeable-resume".to_string(),
            provider_specific: serde_json::json!({
                "kind": "auth0_ciba",
                "auth_req_id": "persisted-auth-req"
            }),
        };

        let decision = provider
            .poll_decision(&handle)
            .await
            .expect("poll_decision should query Auth0");

        assert_eq!(decision, Some(ApprovalDecision::Approved));
        poll.assert_hits(1);
        let (status, resolved_at): (String, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT status, resolved_at FROM auth0_ciba_approvals WHERE id = $1")
                .bind(approval_id)
                .fetch_one(&pool)
                .await
                .expect("CIBA row should remain readable");
        assert_eq!(status, "approved");
        assert_eq!(
            resolved_at, None,
            "poll_decision reports the decision but does not resolve the awakeable itself"
        );
    }

    async fn test_pool() -> PgPool {
        let database_url = std::env::var("MOA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
        let schema_name = format!("auth0_ciba_unit_test_{}", Uuid::new_v4().simple());
        let search_path = format!("{}, public", quote_identifier(&schema_name));
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _meta| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("test Postgres should be reachable");
        moa_migrations::run_auth_schema(&pool, &schema_name)
            .await
            .expect("auth schema should apply");
        pool
    }

    fn quote_identifier(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }

    struct NoopResolver;

    #[async_trait]
    impl AwakeableResolver for NoopResolver {
        async fn resolve(
            &self,
            _awakeable_id: &str,
            _payload: &serde_json::Value,
        ) -> Result<(), AwakeableResolveError> {
            Ok(())
        }
    }
}
