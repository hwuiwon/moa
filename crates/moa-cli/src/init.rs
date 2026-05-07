//! Workspace initialization command.

use super::*;

pub(crate) async fn init_workspace(config: &MoaConfig) -> Result<()> {
    let config_path = MoaConfig::default_path()?;
    if !config_path.exists() {
        config.save_async().await?;
    }
    let workspace_id = current_workspace_id();
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let workspace_memory = Path::new(&home)
        .join(".moa")
        .join("workspaces")
        .join(workspace_id.as_str())
        .join("memory");
    fs::create_dir_all(workspace_memory).await?;
    fs::create_dir_all(expand_tilde(&config.local.sandbox_dir)).await?;
    if config.cloud.enabled
        && let Some(memory_dir) = config.cloud.memory_dir.as_deref()
    {
        fs::create_dir_all(expand_tilde(memory_dir)).await?;
    }
    Ok(())
}
