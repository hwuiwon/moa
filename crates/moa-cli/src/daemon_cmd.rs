//! Orchestrator endpoint status command helper.

use super::*;

pub(crate) async fn daemon_status_report(config: &MoaConfig) -> Result<String> {
    let endpoint = daemon::orchestrator_endpoint(config);
    let health_url = daemon::orchestrator_health_url(config);
    daemon::health_check(config).await?;
    Ok(format!(
        "orchestrator endpoint: {endpoint}\nstatus: healthy ({health_url})\n"
    ))
}
