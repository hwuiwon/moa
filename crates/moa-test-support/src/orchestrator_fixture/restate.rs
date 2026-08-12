//! Restate container, deployment registration, and URL helpers.

use super::*;

pub(super) async fn start_restate_container() -> Result<(ContainerAsync<GenericImage>, u16, u16)> {
    start_restate_container_on_ports(None).await
}

pub(super) async fn start_restate_container_on_ports(
    host_ports: Option<(u16, u16)>,
) -> Result<(ContainerAsync<GenericImage>, u16, u16)> {
    let mut failures = Vec::new();
    for attempt in 1..=3 {
        let image = GenericImage::new(RESTATE_IMAGE, RESTATE_TAG)
            .with_exposed_port(8080.tcp())
            .with_exposed_port(9070.tcp())
            .with_wait_for(WaitFor::seconds(1))
            .with_env_var("DO_NOT_TRACK", "1")
            // Recovery tests hard-kill the SDK endpoint while a durable step is
            // blocked. Keep Restate's fixture-level stall detection below the
            // service-test deadline; handlers with explicit longer policies
            // (for example LLM/tool work) still override these server defaults.
            .with_env_var("RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT", "1s")
            .with_env_var("RESTATE_WORKER__INVOKER__ABORT_TIMEOUT", "1s")
            .with_host("host.docker.internal", Host::HostGateway)
            .with_cmd(["--node-name=restate-test"]);
        let image = match host_ports {
            Some((ingress, admin)) => image
                .with_mapped_port(ingress, 8080.tcp())
                .with_mapped_port(admin, 9070.tcp()),
            None => image,
        };
        let container = match image.start().await {
            Ok(container) => container,
            Err(error) => {
                failures.push(format!("attempt {attempt} failed to start: {error}"));
                continue;
            }
        };

        let ports = async {
            let ingress = fixture_host_port_ipv4(&container, "restate ingress", 8080.tcp()).await?;
            let admin = fixture_host_port_ipv4(&container, "restate admin", 9070.tcp()).await?;
            Ok::<_, anyhow::Error>((ingress, admin))
        }
        .await;
        match ports {
            Ok((ingress, admin)) => return Ok((container, ingress, admin)),
            Err(error) => {
                failures.push(format!(
                    "attempt {attempt} exposed incomplete ports: {error:#}"
                ));
                tracing::warn!(
                    attempt,
                    container_id = %container.id(),
                    %error,
                    "restarting Restate fixture after incomplete Docker port publication"
                );
                if let Err(remove_error) = container.rm().await {
                    tracing::warn!(
                        attempt,
                        %remove_error,
                        "failed to remove incomplete Restate fixture container"
                    );
                }
            }
        }
    }

    bail!(
        "start Restate testcontainer with ingress and admin ports failed after 3 attempts: {}",
        failures.join("; ")
    )
}

pub(super) async fn wait_for_restate_admin(admin_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{admin_url}/health")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for Restate admin health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Restate admin health");
            }
            Ok(response) => bail!(
                "Restate admin did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("Restate admin did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) async fn register_deployment(admin_url: &str, deployment_uri: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "uri": deployment_uri });
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client
            .post(format!("{admin_url}/deployments"))
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if response.status() == StatusCode::CONFLICT => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                tracing::debug!(%status, body = %text, "waiting to register Restate deployment");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting to register Restate deployment");
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("register deployment returned {status}: {text}");
            }
            Err(error) => return Err(error).context("register deployment with Restate"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub(super) async fn find_deployment(
    admin_url: &str,
    deployment_uri: &str,
) -> Result<(String, String)> {
    let client = reqwest::Client::new();
    let expected = deployment_uri.trim_end_matches('/');
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let payload = client
            .get(format!("{admin_url}/deployments"))
            .send()
            .await
            .context("list Restate fixture deployments")?
            .error_for_status()
            .context("Restate fixture deployment list failed")?
            .json::<DeploymentsResponse>()
            .await
            .context("decode Restate fixture deployment list")?;
        if let Some(deployment) = payload.deployments.into_iter().find(|deployment| {
            deployment
                .uri
                .as_deref()
                .is_some_and(|uri| uri.trim_end_matches('/') == expected)
        }) {
            return Ok((
                deployment.id,
                deployment.uri.unwrap_or_else(|| deployment_uri.to_string()),
            ));
        }
        if Instant::now() >= deadline {
            bail!("Restate did not expose registered deployment URI `{deployment_uri}`");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) async fn pinned_invocation_count(admin_url: &str, deployment_id: &str) -> Result<u64> {
    #[derive(Deserialize)]
    struct CountRow {
        pinned_count: u64,
    }
    #[derive(Deserialize)]
    struct QueryResponse {
        rows: Vec<CountRow>,
    }

    let escaped = deployment_id.replace('\'', "''");
    let query = format!(
        "SELECT COUNT(*) AS pinned_count FROM sys_invocation \
         WHERE (pinned_deployment_id = '{escaped}' OR last_attempt_deployment_id = '{escaped}') \
         AND status NOT IN ('completed', 'killed')"
    );
    let response = reqwest::Client::new()
        .post(format!("{admin_url}/query"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .context("query Restate pinned fixture invocations")?
        .error_for_status()
        .context("Restate pinned-invocation query failed")?
        .json::<QueryResponse>()
        .await
        .context("decode Restate pinned-invocation query")?;
    response
        .rows
        .first()
        .map(|row| row.pinned_count)
        .context("Restate pinned-invocation query returned no aggregate row")
}

pub(super) async fn delete_deployment(admin_url: &str, deployment_id: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .delete(format!(
            "{admin_url}/deployments/{deployment_id}?force=true"
        ))
        .send()
        .await
        .context("delete drained Restate fixture deployment")?;
    if response.status() != StatusCode::ACCEPTED {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("delete Restate deployment {deployment_id} returned {status}: {body}");
    }
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let payload = client
            .get(format!("{admin_url}/deployments"))
            .send()
            .await
            .context("list Restate deployments after delete")?
            .error_for_status()
            .context("Restate deployment list failed after delete")?
            .json::<DeploymentsResponse>()
            .await
            .context("decode Restate deployments after delete")?;
        if payload
            .deployments
            .iter()
            .all(|deployment| deployment.id != deployment_id)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Restate deployment {deployment_id} remained registered after delete");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn wait_for_registered_services(admin_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{admin_url}/deployments")).send().await {
            Ok(response) if response.status().is_success() => {
                let payload = response
                    .json::<DeploymentsResponse>()
                    .await
                    .context("decode Restate deployment list")?;
                if payload.deployments.iter().any(deployment_is_routable) {
                    return Ok(());
                }
            }
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for registered services");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for registered services");
            }
            Ok(response) => bail!(
                "registered services did not appear; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("registered services did not appear"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Deserialize)]
struct DeploymentsResponse {
    deployments: Vec<Deployment>,
}

#[derive(Deserialize)]
struct Deployment {
    id: String,
    services: Vec<RegisteredService>,
    uri: Option<String>,
}

#[derive(Deserialize)]
struct RegisteredService {
    name: String,
}

fn deployment_is_routable(deployment: &Deployment) -> bool {
    const REQUIRED_SERVICES: [&str; 9] = [
        "Session",
        "ActionReviewDispatcher",
        "Execution",
        "ExecutionDispatcher",
        "ExecutionTrigger",
        "ExecutionRunController",
        "ExecutionTaskAttempt",
        "LLMGateway",
        "ToolExecutor",
    ];

    REQUIRED_SERVICES.iter().all(|required| {
        deployment
            .services
            .iter()
            .any(|service| service.name == *required)
    })
}

pub(super) fn trim_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("URL must be non-empty");
    }
    url::Url::parse(trimmed).with_context(|| format!("invalid URL {trimmed}"))?;
    Ok(trimmed.to_string())
}

pub(super) fn derive_admin_url(ingress_url: &str) -> String {
    url::Url::parse(ingress_url)
        .ok()
        .and_then(|mut url| {
            url.set_port(Some(10011)).ok()?;
            Some(url.to_string().trim_end_matches('/').to_string())
        })
        .unwrap_or_else(|| "http://127.0.0.1:10011".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_readiness_requires_every_route_used_during_startup() {
        // Pins: Session discovery alone cannot release the fixture while the
        // action-review reaper and execution workflows would still receive 404.
        let service = |name: &str| RegisteredService {
            name: name.to_string(),
        };
        let mut deployment = Deployment {
            id: "fixture-deployment".to_string(),
            services: vec![service("Session")],
            uri: Some("http://127.0.0.1:8080".to_string()),
        };
        assert!(!deployment_is_routable(&deployment));

        deployment.services.extend([
            service("ActionReviewDispatcher"),
            service("Execution"),
            service("ExecutionDispatcher"),
            service("ExecutionTrigger"),
            service("ExecutionRunController"),
            service("ExecutionTaskAttempt"),
            service("LLMGateway"),
            service("ToolExecutor"),
        ]);
        assert!(deployment_is_routable(&deployment));
    }
}
