//! Authentication and local API-key CLI commands.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use moa_auth_providers::Env;
use moa_core::traits::Identity;
use moa_orchestrator_client::OrchestratorClient;
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
    let path = crate::client::credentials_path()?;
    let parent = path
        .parent()
        .context("credentials path must have parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create {}", parent.display()))?;
    let payload = serde_json::json!({ "api_key": key });
    tokio::fs::write(&path, serde_json::to_vec_pretty(&payload)?)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    set_owner_only_permissions(&path).await?;
    Ok(format!(
        "Stored API key at {}.\nCLI will present this key on subsequent edge requests.\n",
        path.display()
    ))
}

async fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
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
