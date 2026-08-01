//! Restate container, deployment registration, and URL helpers.

use super::*;

pub(super) async fn start_restate_container() -> Result<(ContainerAsync<GenericImage>, u16, u16)> {
    let mut failures = Vec::new();
    for attempt in 1..=3 {
        let container = match GenericImage::new(RESTATE_IMAGE, RESTATE_TAG)
            .with_exposed_port(8080.tcp())
            .with_exposed_port(9070.tcp())
            .with_wait_for(WaitFor::seconds(1))
            .with_env_var("DO_NOT_TRACK", "1")
            .with_env_var("RESTATE_EXPERIMENTAL_ENABLE_VQUEUES", "true")
            .with_host("host.docker.internal", Host::HostGateway)
            .with_cmd(["--node-name=restate-test"])
            .start()
            .await
        {
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
                if payload.deployments.iter().any(|deployment| {
                    deployment
                        .services
                        .iter()
                        .any(|service| service.name == "Session")
                }) {
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
    services: Vec<RegisteredService>,
}

#[derive(Deserialize)]
struct RegisteredService {
    name: String,
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
