//! Async initialization flow for the MOA backend services.

use anyhow::{Context, Result};
use moa_core::{MoaConfig, Platform};
use moa_runtime::ChatRuntime;

/// Result of a successful backend boot.
pub struct InitializedServices {
    pub config: MoaConfig,
    pub chat_runtime: ChatRuntime,
}

/// Loads config from `~/.moa/config.toml` (falling back to defaults + env vars)
/// and constructs the [`ChatRuntime`] facade.
pub async fn initialize_services() -> Result<InitializedServices> {
    let config = MoaConfig::load().context("failed to load MOA configuration")?;
    let chat_runtime = ChatRuntime::from_config(config.clone(), Platform::Desktop)
        .await
        .context("failed to initialize MOA chat runtime")?;
    Ok(InitializedServices {
        config,
        chat_runtime,
    })
}
