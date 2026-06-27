//! OpenFGA container bootstrap and external fixture client discovery.

use super::*;

async fn response_json_or_error(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("read OpenFGA {operation} response"))?;
    if !status.is_success() {
        bail!("OpenFGA {operation} returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("decode OpenFGA {operation} response"))
}

pub(super) async fn start_openfga_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(OPENFGA_IMAGE, OPENFGA_TAG)
        .with_exposed_port(8080.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("OPENFGA_DATASTORE_ENGINE", "memory")
        .with_env_var("OPENFGA_AUTHN_METHOD", "preshared")
        .with_env_var("OPENFGA_AUTHN_PRESHARED_KEYS", OPENFGA_PRESHARED_KEY)
        .with_env_var("OPENFGA_LOG_FORMAT", "json")
        .with_cmd(["run"])
        .start()
        .await
        .context("start OpenFGA testcontainer")
}

pub(super) async fn wait_for_openfga(openfga_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{openfga_url}/healthz")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for OpenFGA health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for OpenFGA health");
            }
            Ok(response) => bail!(
                "OpenFGA did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("OpenFGA did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) async fn bootstrap_openfga(openfga_url: &str, preshared_key: &str) -> Result<FgaConfig> {
    let client = reqwest::Client::new();
    let store_name = format!("moa-test-{}", Uuid::now_v7().simple());
    let store = response_json_or_error(
        client
            .post(format!("{openfga_url}/stores"))
            .bearer_auth(preshared_key)
            .json(&json!({ "name": store_name }))
            .send()
            .await
            .context("create fixture OpenFGA store")?,
        "CreateStore",
    )
    .await?;
    let store_id = store
        .get("id")
        .and_then(|value| value.as_str())
        .context("CreateStore response missing id")?
        .to_string();

    let model = serde_json::from_str::<serde_json::Value>(SCHEMA_V1_JSON)
        .context("parse embedded OpenFGA model")?;
    let model_response = response_json_or_error(
        client
            .post(format!(
                "{openfga_url}/stores/{store_id}/authorization-models"
            ))
            .bearer_auth(preshared_key)
            .json(&model)
            .send()
            .await
            .context("write fixture OpenFGA authorization model")?,
        "WriteAuthorizationModel",
    )
    .await?;
    let model_id = model_response
        .get("authorization_model_id")
        .and_then(|value| value.as_str())
        .context("WriteAuthorizationModel response missing authorization_model_id")?
        .to_string();

    Ok(FgaConfig {
        url: openfga_url.to_string(),
        preshared_key: preshared_key.to_string(),
        store_id,
        model_id,
        timeout_ms: 5000,
    })
}

pub(super) fn external_fga_client(repo_root: &Path) -> Result<Option<FgaClient>> {
    let values = fga_env_values(repo_root);
    let Some(store_id) = fga_value(&values, "MOA_AUTHZ_OPENFGA_STORE_ID") else {
        return Ok(None);
    };
    let Some(model_id) = fga_value(&values, "MOA_AUTHZ_OPENFGA_MODEL_ID") else {
        return Ok(None);
    };
    let url = fga_value(&values, "MOA_AUTHZ_OPENFGA_URL")
        .unwrap_or_else(|| "http://127.0.0.1:10030".to_string());
    let preshared_key = fga_value(&values, "MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
        .unwrap_or_else(|| OPENFGA_PRESHARED_KEY.to_string());
    FgaClient::new(FgaConfig {
        url,
        preshared_key,
        store_id,
        model_id,
        timeout_ms: 5000,
    })
    .map(Some)
    .context("build external OpenFGA client")
}

fn fga_env_values(repo_root: &Path) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for key in [
        "MOA_AUTHZ_OPENFGA_URL",
        "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
        "MOA_AUTHZ_OPENFGA_STORE_ID",
        "MOA_AUTHZ_OPENFGA_MODEL_ID",
    ] {
        if let Ok(value) = std::env::var(key) {
            values.insert(key.to_string(), value);
        }
    }

    if let Ok(contents) = std::fs::read_to_string(repo_root.join(".env.fga")) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            values
                .entry(key.trim().to_string())
                .or_insert_with(|| value.trim().trim_matches('"').to_string());
        }
    }
    values
}

fn fga_value(values: &HashMap<String, String>, key: &str) -> Option<String> {
    values
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
}
