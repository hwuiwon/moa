//! Authentication and local API-key CLI commands.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use moa_auth_providers::Env;
use moa_core::traits::Identity;
use moa_orchestrator_client::OrchestratorClient;
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

/// Top-level auth command.
#[derive(Debug, Args)]
pub(crate) struct AuthCommand {
    /// Auth action to run.
    #[command(subcommand)]
    pub(crate) action: AuthAction,
}

/// Auth actions.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthAction {
    /// Manage API keys.
    Keys {
        /// API-key action to run.
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Store an API key for subsequent CLI requests through moa-edge.
    UseKey {
        /// Full API key value.
        key: String,
    },
    /// Authenticate via an Auth0/OIDC device-code flow.
    Login {
        /// OIDC issuer URL; defaults to MOA_OIDC_ISSUER or MOA_EDGE_URL.
        #[arg(long)]
        issuer: Option<String>,
        /// OAuth client id; defaults to MOA_AUTH_CLIENT_ID, MOA_AUTH0_CLIENT_ID, or MOA_OIDC_CLIENT_ID.
        #[arg(long)]
        client_id: Option<String>,
    },
}

/// API-key management actions.
#[derive(Debug, Subcommand)]
pub(crate) enum KeysAction {
    /// Create a new API key.
    Create {
        /// Human-readable key name.
        #[arg(long)]
        name: String,
        /// API key environment.
        #[arg(long, default_value_t = Env::Dev)]
        env: Env,
        /// Optional key description.
        #[arg(long)]
        description: Option<String>,
        /// Create the key for an agent instead of the caller user.
        #[arg(long)]
        for_agent: Option<Uuid>,
    },
    /// List active API keys owned by the caller.
    List,
    /// Rotate an API key.
    Rotate {
        /// API key ID to rotate.
        id: Uuid,
    },
    /// Revoke an API key.
    Revoke {
        /// API key ID to revoke.
        id: Uuid,
    },
}

/// Run an auth command.
pub(crate) async fn handle_auth_command(command: AuthCommand) -> Result<String> {
    match command.action {
        AuthAction::Keys { action } => handle_keys_action(action).await,
        AuthAction::UseKey { key } => run_use_key(&key).await,
        AuthAction::Login { issuer, client_id } => run_login(issuer, client_id).await,
    }
}

async fn handle_keys_action(action: KeysAction) -> Result<String> {
    let client = build_client_with_identity_from_config().await?;
    match action {
        KeysAction::Create {
            name,
            env,
            description,
            for_agent,
        } => {
            let response = client
                .api_keys_create(name, env, description, for_agent)
                .await?;
            Ok(format!(
                "Created API key {}\nPrefix: {}\n\n    {}\n\nThis is the only time the full key will be displayed. Store it now.\n",
                response.id, response.prefix, response.key
            ))
        }
        KeysAction::List => {
            let keys = client.api_keys_list().await?;
            let mut output = format!("{:<38} {:<5} {:<18} {}\n", "ID", "ENV", "PREFIX", "NAME");
            for key in keys {
                output.push_str(&format!(
                    "{:<38} {:<5} {:<18} {}\n",
                    key.id, key.env, key.prefix, key.name
                ));
            }
            Ok(output)
        }
        KeysAction::Rotate { id } => {
            let response = client.api_keys_rotate(id).await?;
            Ok(format!(
                "Rotated API key; new id {}\nPrefix: {}\n\n    {}\n\nThe old key is revoked. Reapply any manually narrowed FGA scope tuples to the new key.\n",
                response.id, response.prefix, response.key
            ))
        }
        KeysAction::Revoke { id } => {
            client.api_keys_revoke(id).await?;
            Ok(format!("Revoked API key {id}\n"))
        }
    }
}

async fn build_client_with_identity_from_config() -> Result<OrchestratorClient> {
    let path = cli_identity_path()?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read CLI identity file {}", path.display()))?;
    let identity: Identity = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse CLI identity file {}", path.display()))?;
    Ok(OrchestratorClient::from_env()?.with_identity(identity))
}

fn cli_identity_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("MOA_CLI_IDENTITY_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME").context("HOME is required for default CLI identity path")?;
    Ok(PathBuf::from(home).join(".moa").join("cli-identity.json"))
}

async fn run_use_key(key: &str) -> Result<String> {
    moa_auth_providers::parse_parts(key)
        .map_err(|error| anyhow::anyhow!("invalid API key format: {error}"))?;
    let credentials = crate::client::StoredCredentials {
        api_key: Some(key.to_string()),
        ..Default::default()
    };
    crate::client::write_credentials(&credentials).await?;
    let path = crate::client::credentials_path()?;
    Ok(format!(
        "Stored API key at {}.\nCLI will present this key on subsequent edge requests.\n",
        path.display()
    ))
}

async fn run_login(issuer: Option<String>, client_id: Option<String>) -> Result<String> {
    let issuer = issuer
        .or_else(|| std::env::var("MOA_OIDC_ISSUER").ok())
        .or_else(|| std::env::var("MOA_AUTH0_ISSUER").ok())
        .or_else(|| std::env::var("MOA_EDGE_URL").ok())
        .context("issuer required; pass --issuer or set MOA_OIDC_ISSUER")?;
    let client_id = client_id
        .or_else(|| std::env::var("MOA_AUTH_CLIENT_ID").ok())
        .or_else(|| std::env::var("MOA_AUTH0_CLIENT_ID").ok())
        .or_else(|| std::env::var("MOA_OIDC_CLIENT_ID").ok())
        .context("client id required; pass --client-id or set MOA_AUTH_CLIENT_ID")?;
    let discovery_url = discovery_url(&issuer);
    let http = reqwest::Client::new();
    let discovery: OidcDiscovery = http
        .get(&discovery_url)
        .send()
        .await
        .with_context(|| format!("fetch OIDC discovery {discovery_url}"))?
        .error_for_status()
        .with_context(|| format!("OIDC discovery {discovery_url}"))?
        .json()
        .await
        .context("parse OIDC discovery")?;

    let device: DeviceAuthorizationResponse = http
        .post(&discovery.device_authorization_endpoint)
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "openid offline_access"),
        ])
        .send()
        .await
        .context("start device authorization")?
        .error_for_status()
        .context("device authorization rejected")?
        .json()
        .await
        .context("parse device authorization response")?;

    let verification = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device.verification_uri);
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "Open this URL to authorize MOA CLI:\n\n  {verification}\n"
    )?;
    writeln!(stdout, "User code: {}\n", device.user_code)?;
    stdout.flush()?;
    try_open_browser(verification).await;

    let token = poll_device_token(&http, &discovery.token_endpoint, &client_id, &device).await?;
    let expires_at = token.expires_at();
    let credentials = crate::client::StoredCredentials {
        access_token: Some(token.access_token),
        refresh_token: token.refresh_token,
        token_endpoint: Some(discovery.token_endpoint),
        client_id: Some(client_id),
        issuer: Some(issuer),
        expires_at,
        ..Default::default()
    };
    crate::client::write_credentials(&credentials).await?;
    let path = crate::client::credentials_path()?;
    Ok(format!(
        "Stored OIDC credentials at {}.\nCLI will present the access token on subsequent edge requests.\n",
        path.display()
    ))
}

fn discovery_url(issuer: &str) -> String {
    if issuer.contains("/.well-known/") {
        return issuer.to_string();
    }
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

async fn poll_device_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    device: &DeviceAuthorizationResponse,
) -> Result<crate::client::OAuthTokenResponse> {
    let mut interval = device.interval.unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        if Instant::now() >= deadline {
            bail!("device authorization expired before approval");
        }
        sleep(Duration::from_secs(interval)).await;
        let response = http
            .post(token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await
            .context("poll device token")?;

        let status = response.status();
        let body = response.text().await.context("read token response")?;
        if status.is_success() {
            return serde_json::from_str(&body).context("parse token response");
        }

        let error: OAuthErrorResponse =
            serde_json::from_str(&body).with_context(|| format!("parse token error {status}"))?;
        match error.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            "access_denied" => bail!("device authorization denied"),
            "expired_token" => bail!("device authorization expired"),
            other => bail!(
                "device authorization failed: {}{}",
                other,
                error
                    .error_description
                    .as_deref()
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default()
            ),
        }
    }
}

async fn try_open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(not(unix))]
    let program = "";

    if program.is_empty() {
        return;
    }

    if let Err(error) = tokio::process::Command::new(program).arg(url).spawn() {
        tracing::debug!(%error, "failed to open browser for device authorization");
    }
}

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}
