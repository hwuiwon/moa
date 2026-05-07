//! Daemon filesystem path helpers.

use super::*;

pub(super) async fn read_pid_file(config: &MoaConfig) -> Result<u32> {
    let content = fs::read_to_string(daemon_pid_path(config)).await?;
    content
        .trim()
        .parse::<u32>()
        .context("parsing daemon pid file")
}

pub(super) async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    Ok(())
}

pub(super) fn daemon_socket_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.socket_path)
}

pub(super) fn daemon_pid_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.pid_file)
}

pub(super) fn daemon_log_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.log_file)
}

fn expand_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }

    PathBuf::from(path)
}
