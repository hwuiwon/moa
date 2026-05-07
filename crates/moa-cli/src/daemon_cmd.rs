//! Daemon command status helper.

use super::*;

pub(crate) async fn daemon_status_report(config: &MoaConfig) -> Result<String> {
    let info = daemon::daemon_info(config).await?;
    Ok(format!(
        "daemon: running\npid: {}\nsocket: {}\nlog: {}\nstarted_at: {}\nsessions: {}\nactive_sessions: {}\n",
        info.pid,
        info.socket_path,
        info.log_path,
        info.started_at,
        info.session_count,
        info.active_session_count
    ))
}
