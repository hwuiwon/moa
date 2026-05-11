//! Shared CLI client construction helpers.

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use moa_orchestrator_client::OrchestratorClient;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const REFRESH_SKEW: TimeDelta = TimeDelta::seconds(60);

/// Build an orchestrator client that authenticates to `moa-edge` with stored credentials.
pub(crate) async fn client_from_credentials() -> Result<OrchestratorClient> {
    let path = credentials_path()?;
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}; did you run `moa auth use-key`?", path.display()))?;
    let mut credentials: StoredCredentials =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    if let Some(key) = credentials.api_key.as_deref() {
        let base =
            std::env::var("MOA_EDGE_URL").unwrap_or_else(|_| "http://localhost:10000".into());
        return Ok(OrchestratorClient::new(base)?.with_bearer(key.to_string()));
    }

    if credentials.expires_at.is_none()
        && let Some(access_token) = credentials.access_token.as_deref()
    {
        credentials.expires_at = jwt_expiration(access_token);
    }

    if should_refresh(credentials.expires_at) {
        refresh_stored_credentials(&mut credentials).await?;
        write_credentials(&credentials).await?;
    }

    let token = credentials.access_token.clone().context(
        "access_token missing from credentials.json; run `moa auth login` or `moa auth use-key`",
    )?;
    let base = std::env::var("MOA_EDGE_URL").unwrap_or_else(|_| "http://localhost:10000".into());
    Ok(OrchestratorClient::new(base)?.with_bearer(token))
}

/// Return the default MOA CLI credentials path.
pub(crate) fn credentials_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("home directory unavailable")?;
    Ok(home.join(".moa").join("credentials.json"))
}

/// Persist CLI credentials using owner-only file permissions on Unix.
pub(crate) async fn write_credentials(credentials: &StoredCredentials) -> Result<()> {
    let path = credentials_path()?;
    let parent = path
        .parent()
        .context("credentials path must have parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create {}", parent.display()))?;
    tokio::fs::write(&path, serde_json::to_vec_pretty(credentials)?)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    set_owner_only_permissions(&path).await
}

/// Credentials stored by `moa auth use-key` and `moa auth login`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct StoredCredentials {
    /// Local MOA API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_key: Option<String>,
    /// OIDC access token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) access_token: Option<String>,
    /// OIDC refresh token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    /// OIDC token endpoint used for refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_endpoint: Option<String>,
    /// Public OAuth client id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    /// OIDC issuer URL used for discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) issuer: Option<String>,
    /// Access token expiration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OAuthTokenResponse {
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_in: Option<u64>,
}

impl OAuthTokenResponse {
    pub(crate) fn expires_at(&self) -> Option<DateTime<Utc>> {
        expires_at_from_now(self.expires_in?)
    }
}

fn should_refresh(expires_at: Option<DateTime<Utc>>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= Utc::now() + REFRESH_SKEW)
}

async fn refresh_stored_credentials(credentials: &mut StoredCredentials) -> Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .context("refresh_token missing from credentials.json; run `moa auth login` again")?;
    let token_endpoint = credentials
        .token_endpoint
        .as_deref()
        .context("token_endpoint missing from credentials.json; run `moa auth login` again")?;
    let client_id = credentials
        .client_id
        .as_deref()
        .context("client_id missing from credentials.json; run `moa auth login` again")?;

    let response = reqwest::Client::new()
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("refresh OIDC token")?;

    let status = response.status();
    let body = response.text().await.context("read refresh response")?;
    if !status.is_success() {
        bail!("refresh OIDC token failed with {status}: {body}");
    }
    let token: OAuthTokenResponse =
        serde_json::from_str(&body).context("parse refresh token response")?;
    let expires_at = token.expires_at();
    credentials.access_token = Some(token.access_token);
    if let Some(refresh_token) = token.refresh_token {
        credentials.refresh_token = Some(refresh_token);
    }
    credentials.expires_at =
        expires_at.or_else(|| credentials.access_token.as_deref().and_then(jwt_expiration));
    Ok(())
}

pub(crate) fn expires_at_from_now(seconds: u64) -> Option<DateTime<Utc>> {
    let duration = TimeDelta::from_std(Duration::from_secs(seconds)).ok()?;
    Utc::now().checked_add_signed(duration)
}

fn jwt_expiration(token: &str) -> Option<DateTime<Utc>> {
    #[derive(Debug, Deserialize)]
    struct Claims {
        exp: i64,
    }

    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Claims = serde_json::from_slice(&decoded).ok()?;
    DateTime::<Utc>::from_timestamp(claims.exp, 0)
}

async fn set_owner_only_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        tokio::fs::set_permissions(path, permissions)
            .await
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
