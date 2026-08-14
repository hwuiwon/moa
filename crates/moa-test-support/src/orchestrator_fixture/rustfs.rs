//! Digest-pinned RustFS namespace owned by one orchestrator fixture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt as _;
use hmac::{Hmac, Mac as _};
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use reqwest::Method;
use sha2::{Digest as _, Sha256};
use testcontainers::core::IntoContainerPort;

use super::*;

const RUSTFS_IMAGE: &str = "rustfs/rustfs";
const RUSTFS_TAG: &str =
    "latest@sha256:fa19210ac4697c79d7ccca1ec9b0eb91aebacc6691991ffb14014bb3c67e6cc3";
const RUSTFS_REGION: &str = "us-east-1";
const RUSTFS_PORT: u16 = 9000;
type HmacSha256 = Hmac<Sha256>;

/// One authenticated, per-fixture RustFS bucket and non-root checkpoint prefix.
pub struct RustFsFixture {
    endpoint: String,
    bucket: String,
    prefix: String,
    access_key: String,
    secret_key: String,
    store: Arc<dyn ObjectStore>,
    _container: ContainerAsync<GenericImage>,
}

impl RustFsFixture {
    /// Starts a digest-pinned RustFS container and creates one unique bucket.
    pub async fn start() -> Result<Self> {
        let fixture_id = Uuid::now_v7().simple().to_string();
        let bucket = format!("moa-workspace-{fixture_id}");
        let prefix = format!("checkpoint-fixture/{fixture_id}");
        let access_key = format!("moa{fixture_id}");
        let mut secret_bytes = [0_u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret_bytes);
        let secret_key = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            secret_bytes,
        );
        let (container, host_port) = start_rustfs_container(&access_key, &secret_key).await?;
        let endpoint = format!("http://127.0.0.1:{host_port}");
        create_bucket_with_retry(&endpoint, &bucket, &access_key, &secret_key).await?;
        let store: Arc<dyn ObjectStore> = Arc::new(
            AmazonS3Builder::new()
                .with_bucket_name(&bucket)
                .with_region(RUSTFS_REGION)
                .with_endpoint(&endpoint)
                .with_access_key_id(&access_key)
                .with_secret_access_key(&secret_key)
                .with_allow_http(true)
                .with_virtual_hosted_style_request(false)
                .with_conditional_put(S3ConditionalPut::ETagMatch)
                .build()
                .context("build authenticated RustFS fixture client")?,
        );

        let fixture = Self {
            endpoint,
            bucket,
            prefix,
            access_key,
            secret_key,
            store,
            _container: container,
        };
        fixture.assert_available().await?;
        Ok(fixture)
    }

    /// Returns the loopback S3-compatible endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the unique bucket owned by this fixture.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Returns the non-root key prefix reserved for portable checkpoints.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Applies this fixture's isolated authenticated checkpoint namespace to a
    /// typed configuration without exposing credentials or mutating process-global state.
    pub fn apply_checkpoint_config(&self, config: &mut moa_config::MoaConfig) {
        config.object_store.backend = moa_config::ObjectStoreBackend::S3;
        config.object_store.credential_mode = moa_config::ObjectStoreCredentialMode::Static;
        config.object_store.region = Some(RUSTFS_REGION.to_string());
        config.object_store.endpoint = Some(self.endpoint.clone());
        config.object_store.access_key_id = Some(self.access_key.clone());
        config.object_store.secret_access_key = Some(self.secret_key.clone());
        config.object_store.allow_http = true;
        config.object_store.virtual_hosted_style = false;
        config.sandbox_checkpoints.enabled = true;
        config.sandbox_checkpoints.storage.bucket = self.bucket.clone();
        config.sandbox_checkpoints.storage.prefix = self.prefix.clone();
        config.sandbox_checkpoints.bucket_versioning =
            moa_config::CheckpointBucketVersioningPolicy::UnversionedRequired;
    }

    /// Confirms that the exact authenticated bucket remains reachable.
    pub async fn assert_available(&self) -> Result<()> {
        let response = send_bucket_request(
            Method::HEAD,
            &self.endpoint,
            &self.bucket,
            &self.access_key,
            &self.secret_key,
        )
        .await?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("RustFS fixture bucket health failed with {status}: {body}")
    }

    /// Stores exact bytes beneath this fixture's reserved prefix.
    pub async fn put_probe(&self, suffix: &str, bytes: &[u8]) -> Result<String> {
        let key = format!("{}/{suffix}", self.prefix);
        self.store
            .put(
                &ObjectPath::from(key.as_str()),
                PutPayload::from(bytes.to_vec()),
            )
            .await
            .with_context(|| format!("write RustFS fixture probe {key}"))?;
        Ok(key)
    }

    /// Reads exact bytes from this fixture's bucket.
    pub async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        self.store
            .get(&ObjectPath::from(key))
            .await
            .with_context(|| format!("read RustFS fixture object {key}"))?
            .bytes()
            .await
            .with_context(|| format!("collect RustFS fixture object {key}"))
            .map(|bytes| bytes.to_vec())
    }

    /// Deletes every object under the fixture prefix, then its unique bucket.
    ///
    /// The RustFS container itself is also fixture-owned, so dropping the
    /// parent is a final cleanup fallback if a test aborts before this method.
    pub async fn cleanup_namespace(&self) -> Result<()> {
        let prefix = ObjectPath::from(self.prefix.as_str());
        let objects = self
            .store
            .list(Some(&prefix))
            .map_ok(|meta| meta.location)
            .try_collect::<Vec<_>>()
            .await
            .context("enumerate RustFS fixture checkpoint prefix")?;
        for object in objects {
            self.store
                .delete(&object)
                .await
                .with_context(|| format!("delete RustFS fixture object {object}"))?;
        }
        let response = send_bucket_request(
            Method::DELETE,
            &self.endpoint,
            &self.bucket,
            &self.access_key,
            &self.secret_key,
        )
        .await?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("delete RustFS fixture bucket failed with {status}: {body}")
    }

    pub(super) fn orchestrator_env(&self) -> Vec<(String, String)> {
        vec![
            ("MOA_OBJECT_STORE_BACKEND".to_string(), "s3".to_string()),
            (
                "MOA_OBJECT_STORE_REGION".to_string(),
                RUSTFS_REGION.to_string(),
            ),
            (
                "MOA_OBJECT_STORE_ENDPOINT".to_string(),
                self.endpoint.clone(),
            ),
            (
                "MOA_OBJECT_STORE_ACCESS_KEY_ID".to_string(),
                self.access_key.clone(),
            ),
            (
                "MOA_OBJECT_STORE_SECRET_ACCESS_KEY".to_string(),
                self.secret_key.clone(),
            ),
            (
                "MOA_OBJECT_STORE_ALLOW_HTTP".to_string(),
                "true".to_string(),
            ),
            (
                "MOA_OBJECT_STORE_VIRTUAL_HOSTED_STYLE".to_string(),
                "false".to_string(),
            ),
            (
                "MOA_SANDBOX_CHECKPOINT_ENABLED".to_string(),
                "true".to_string(),
            ),
            (
                "MOA_SANDBOX_CHECKPOINT_BUCKET".to_string(),
                self.bucket.clone(),
            ),
            (
                "MOA_SANDBOX_CHECKPOINT_PREFIX".to_string(),
                self.prefix.clone(),
            ),
        ]
    }
}

async fn start_rustfs_container(
    access_key: &str,
    secret_key: &str,
) -> Result<(ContainerAsync<GenericImage>, u16)> {
    let mut failures = Vec::new();
    for attempt in 1..=3 {
        // Docker on GitHub-hosted runners can ignore PublishAllPorts for this
        // image even when the container port is explicitly exposed. An exact
        // host binding avoids that daemon-specific path. The retry loop also
        // closes the small release-to-create race around the reserved port.
        let port_listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("reserve RustFS fixture host port")?;
        let host_port = port_listener
            .local_addr()
            .context("read reserved RustFS fixture host port")?
            .port();
        drop(port_listener);
        let container = match GenericImage::new(RUSTFS_IMAGE, RUSTFS_TAG)
            .with_exposed_port(RUSTFS_PORT.tcp())
            .with_wait_for(WaitFor::seconds(2))
            .with_env_var("RUSTFS_ACCESS_KEY", access_key)
            .with_env_var("RUSTFS_SECRET_KEY", secret_key)
            .with_env_var("RUSTFS_REGION", RUSTFS_REGION)
            .with_env_var("RUSTFS_ADDRESS", "0.0.0.0:9000")
            .with_env_var("RUSTFS_CONSOLE_ENABLE", "false")
            .with_cmd(["/data"])
            .with_mapped_port(host_port, RUSTFS_PORT.tcp())
            .start()
            .await
        {
            Ok(container) => container,
            Err(error) => {
                failures.push(format!("attempt {attempt} failed to start: {error}"));
                continue;
            }
        };
        match fixture_host_port_ipv4(
            &container,
            "sandbox workspace RustFS API",
            RUSTFS_PORT.tcp(),
        )
        .await
        {
            Ok(host_port) => return Ok((container, host_port)),
            Err(error) => {
                let stdout = container
                    .stdout_to_vec()
                    .await
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|log_error| format!("unavailable: {log_error}"));
                let stderr = container
                    .stderr_to_vec()
                    .await
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|log_error| format!("unavailable: {log_error}"));
                failures.push(format!(
                    "attempt {attempt} exposed incomplete ports: {error:#}; stdout={stdout:?}; stderr={stderr:?}"
                ));
                tracing::warn!(
                    attempt,
                    container_id = %container.id(),
                    %error,
                    "restarting RustFS fixture after incomplete Docker port publication"
                );
                if let Err(remove_error) = container.rm().await {
                    tracing::warn!(
                        attempt,
                        %remove_error,
                        "failed to remove incomplete RustFS fixture container"
                    );
                }
            }
        }
    }
    bail!(
        "start RustFS testcontainer with API port failed after 3 attempts: {}",
        failures.join("; ")
    )
}

async fn create_bucket_with_retry(
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match send_bucket_request(Method::PUT, endpoint, bucket, access_key, secret_key).await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for RustFS bucket API");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for RustFS testcontainer");
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                bail!("create RustFS fixture bucket failed with {status}: {body}");
            }
            Err(error) => return Err(error).context("RustFS testcontainer did not become ready"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn send_bucket_request(
    method: Method,
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<reqwest::Response> {
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let url =
        reqwest::Url::parse(&format!("{endpoint}/{bucket}")).context("parse RustFS bucket URL")?;
    let host = match url.port() {
        Some(port) => format!(
            "{}:{port}",
            url.host_str().context("RustFS bucket URL has no host")?
        ),
        None => url
            .host_str()
            .context("RustFS bucket URL has no host")?
            .to_string(),
    };
    let now = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{EMPTY_SHA256}\nx-amz-date:{amz_date}\n");
    let canonical_request = format!(
        "{}\n/{}\n\n{}\n{}\n{}",
        method.as_str(),
        bucket,
        canonical_headers,
        signed_headers,
        EMPTY_SHA256
    );
    let scope = format!("{date}/{RUSTFS_REGION}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let date_key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes())?;
    let region_key = hmac_sha256(&date_key, RUSTFS_REGION.as_bytes())?;
    let service_key = hmac_sha256(&region_key, b"s3")?;
    let signing_key = hmac_sha256(&service_key, b"aws4_request")?;
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    reqwest::Client::new()
        .request(method, url)
        .header("host", host)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", amz_date)
        .header("authorization", authorization)
        .send()
        .await
        .context("execute signed RustFS bucket request")
}

fn hmac_sha256(key: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(key)
        .map_err(|_| anyhow!("invalid HMAC-SHA256 key"))?;
    mac.update(bytes);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires local Docker; uses only a disposable RustFS testcontainer"]
    async fn unique_authenticated_bucket_round_trips_exact_bytes_docker() -> Result<()> {
        // Pins: the recovery fixture provisions an authenticated, isolated
        // checkpoint namespace instead of silently relying on compose state.
        let fixture = RustFsFixture::start().await?;
        let expected = b"restart-stable-checkpoint-bytes";
        let key = fixture.put_probe("round-trip", expected).await?;
        assert_eq!(fixture.get_bytes(&key).await?, expected);
        fixture.cleanup_namespace().await?;
        Ok(())
    }
}
