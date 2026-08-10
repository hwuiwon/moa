//! Authenticated provider observation of checkpoint-bucket versioning state.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_config::{MoaConfig, ObjectStoreBackend};
use moa_core::error::{MoaError, Result};
use object_store::ObjectStore;
use object_store::aws::{AmazonS3Builder, AwsAuthorizer, AwsCredentialProvider, S3ConditionalPut};
use object_store::gcp::{GcpCredentialProvider, GoogleCloudStorageBuilder};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest_object_store::redirect::Policy;
use reqwest_object_store::{Client, StatusCode, Url};
use serde::Deserialize;

use super::store::ObservedCheckpointBucketVersioning;

const GCS_JSON_API_ORIGIN: &str = "https://storage.googleapis.com";
const MAX_VERSIONING_RESPONSE_BYTES: usize = 64 * 1024;

/// One authenticated provider observation and its monotonic freshness clock.
#[derive(Debug, Clone)]
pub struct CheckpointBucketVersioningObservation {
    state: ObservedCheckpointBucketVersioning,
    observed_at: DateTime<Utc>,
    observed_instant: Instant,
}

impl CheckpointBucketVersioningObservation {
    /// Returns the provider-observed state.
    #[must_use]
    pub const fn state(&self) -> ObservedCheckpointBucketVersioning {
        self.state
    }

    /// Returns the wall-clock observation time for diagnostics.
    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    /// Returns the elapsed monotonic age of this observation.
    #[must_use]
    pub fn age(&self) -> Duration {
        self.observed_instant.elapsed()
    }
}

#[derive(Debug, Default)]
pub(crate) struct CheckpointBucketVersioningGate {
    verified_at: RwLock<Option<Instant>>,
    maximum_age: Duration,
}

impl CheckpointBucketVersioningGate {
    fn new(maximum_age: Duration) -> Self {
        Self {
            verified_at: RwLock::new(None),
            maximum_age,
        }
    }

    pub(crate) fn record(&self, observation: &CheckpointBucketVersioningObservation) {
        let verified_at = if observation.state == ObservedCheckpointBucketVersioning::Unversioned
            && observation.age() <= self.maximum_age
        {
            Some(observation.observed_instant)
        } else {
            None
        };
        if let Ok(mut state) = self.verified_at.write() {
            *state = verified_at;
        }
    }

    pub(crate) fn invalidate(&self) {
        if let Ok(mut state) = self.verified_at.write() {
            *state = None;
        }
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.verified_at
            .read()
            .ok()
            .and_then(|state| *state)
            .is_some_and(|observed| observed.elapsed() <= self.maximum_age)
    }

    pub(crate) fn preverified(maximum_age: Duration) -> Self {
        Self {
            verified_at: RwLock::new(Some(Instant::now())),
            maximum_age,
        }
    }
}

#[derive(Clone)]
enum ObservationBackend {
    S3 {
        endpoint: Url,
        region: String,
        credentials: AwsCredentialProvider,
    },
    Gcs {
        endpoint: Url,
        credentials: GcpCredentialProvider,
    },
}

/// Cloneable authenticated bucket-versioning observer used by startup and refresh jobs.
#[derive(Clone)]
pub struct CheckpointBucketVersioningObserver {
    backend: ObservationBackend,
    client: Client,
    timeout: Duration,
    maximum_age: Duration,
    gate: Arc<CheckpointBucketVersioningGate>,
    observation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl CheckpointBucketVersioningObserver {
    /// Performs one bounded authenticated provider observation.
    ///
    /// Credential lookup and HTTP I/O share one deadline. A timeout, transport
    /// error, non-success status, oversized response, redirect, or ambiguous
    /// response invalidates the shared store gate immediately.
    pub async fn observe(&self) -> Result<CheckpointBucketVersioningObservation> {
        let _guard = self.observation_lock.lock().await;
        let result = tokio::time::timeout(self.timeout, self.observe_inner()).await;
        let state = match result {
            Err(_) => {
                self.gate.invalidate();
                return Err(MoaError::ProviderTimeout(
                    "checkpoint bucket versioning observation exceeded its deadline".to_string(),
                ));
            }
            Ok(Err(error)) => {
                self.gate.invalidate();
                return Err(error);
            }
            Ok(Ok(state)) => state,
        };
        let observation = CheckpointBucketVersioningObservation {
            state,
            observed_at: Utc::now(),
            observed_instant: Instant::now(),
        };
        self.gate.record(&observation);
        if state == ObservedCheckpointBucketVersioning::Unknown {
            return Err(MoaError::ProviderError(
                "checkpoint bucket versioning response was not authoritative".to_string(),
            ));
        }
        Ok(observation)
    }

    /// Returns the configured request deadline.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the maximum acceptable observation age.
    #[must_use]
    pub const fn maximum_age(&self) -> Duration {
        self.maximum_age
    }

    /// Returns whether the latest observation is unversioned and still fresh.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.gate.is_verified()
    }

    /// Returns a refresh interval that renews the observation halfway through
    /// its freshness window.
    ///
    /// Runtime supervision must call [`Self::observe_unversioned`] at this
    /// interval, mark readiness false on any error, and treat unexpected loop
    /// exit as fatal. The observer serializes cloned callers itself.
    #[must_use]
    pub fn recommended_refresh_interval(&self) -> Duration {
        self.maximum_age
            .checked_div(2)
            .filter(|interval| !interval.is_zero())
            .unwrap_or(Duration::from_millis(1))
    }

    /// Observes and requires a fresh provider-reported unversioned state.
    pub async fn observe_unversioned(&self) -> Result<CheckpointBucketVersioningObservation> {
        let observation = self.observe().await?;
        if observation.state != ObservedCheckpointBucketVersioning::Unversioned {
            self.gate.invalidate();
            return Err(MoaError::ConfigError(format!(
                "checkpoint bucket provider reported {:?} versioning; unversioned is required",
                observation.state
            )));
        }
        Ok(observation)
    }

    /// Invalidates the shared readiness/store gate when supervision stops.
    pub fn invalidate(&self) {
        self.gate.invalidate();
    }

    async fn observe_inner(&self) -> Result<ObservedCheckpointBucketVersioning> {
        match &self.backend {
            ObservationBackend::S3 {
                endpoint,
                region,
                credentials,
            } => {
                let credential = credentials.get_credential().await.map_err(|error| {
                    MoaError::ProviderTransport(format!(
                        "resolve checkpoint S3 credential: {error}"
                    ))
                })?;
                let mut request = self
                    .client
                    .get(endpoint.clone())
                    .build()
                    .map_err(map_transport_error)?;
                AwsAuthorizer::new(credential.as_ref(), "s3", region).authorize(&mut request, None);
                let bytes = execute_bounded(&self.client, request).await?;
                Ok(parse_s3_versioning(&bytes))
            }
            ObservationBackend::Gcs {
                endpoint,
                credentials,
            } => {
                let credential = credentials.get_credential().await.map_err(|error| {
                    MoaError::ProviderTransport(format!(
                        "resolve checkpoint GCS credential: {error}"
                    ))
                })?;
                if credential.bearer.trim().is_empty() {
                    return Err(MoaError::ProviderError(
                        "checkpoint GCS versioning observation requires an authenticated credential"
                            .to_string(),
                    ));
                }
                let request = self
                    .client
                    .get(endpoint.clone())
                    .bearer_auth(&credential.bearer)
                    .build()
                    .map_err(map_transport_error)?;
                let bytes = execute_bounded(&self.client, request).await?;
                Ok(parse_gcs_versioning(&bytes))
            }
        }
    }
}

pub(crate) fn build_checkpoint_store_and_observer(
    config: &MoaConfig,
) -> Result<(
    Arc<dyn ObjectStore>,
    CheckpointBucketVersioningObserver,
    Arc<CheckpointBucketVersioningGate>,
)> {
    let shared = &config.object_store;
    let checkpoint = &config.sandbox_checkpoints;
    let bucket = checkpoint.storage.bucket.trim();
    let timeout = Duration::from_secs(checkpoint.versioning_observation.timeout_seconds);
    let maximum_age = Duration::from_secs(checkpoint.versioning_observation.maximum_age_seconds);
    let gate = Arc::new(CheckpointBucketVersioningGate::new(maximum_age));
    let client = Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|error| {
            MoaError::ConfigError(format!(
                "failed to build checkpoint versioning observer client: {error}"
            ))
        })?;

    let (store, backend): (Arc<dyn ObjectStore>, ObservationBackend) = match shared.backend {
        ObjectStoreBackend::S3 => {
            let region = non_empty(&shared.region).unwrap_or("us-east-1").to_string();
            let mut builder = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_allow_http(shared.allow_http)
                .with_virtual_hosted_style_request(shared.virtual_hosted_style)
                .with_conditional_put(S3ConditionalPut::ETagMatch)
                .with_region(&region);
            if shared.endpoint.is_some() {
                builder = builder.with_endpoint(s3_configured_endpoint(shared, bucket)?);
            }
            if let Some(access_key_id) = non_empty(&shared.access_key_id) {
                builder = builder.with_access_key_id(access_key_id);
            }
            if let Some(secret_access_key) = non_empty(&shared.secret_access_key) {
                builder = builder.with_secret_access_key(secret_access_key);
            }
            let store = builder.build().map_err(|error| {
                MoaError::ConfigError(format!(
                    "failed to build checkpoint S3 object store: {error}"
                ))
            })?;
            let credentials = Arc::clone(store.credentials());
            let endpoint = s3_bucket_versioning_url(shared, bucket, &region)?;
            (
                Arc::new(store),
                ObservationBackend::S3 {
                    endpoint,
                    region,
                    credentials,
                },
            )
        }
        ObjectStoreBackend::Gcs => {
            let mut builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);
            if let Some(path) = non_empty(&shared.gcp_service_account_path) {
                builder = builder.with_service_account_path(path);
            }
            if let Some(key) = non_empty(&shared.gcp_service_account_key) {
                builder = builder.with_service_account_key(key);
            }
            if let Some(path) = non_empty(&shared.gcp_application_credentials_path) {
                builder = builder.with_application_credentials(path);
            }
            let store = builder.build().map_err(|error| {
                MoaError::ConfigError(format!(
                    "failed to build checkpoint GCS object store: {error}"
                ))
            })?;
            let credentials = Arc::clone(store.credentials());
            (
                Arc::new(store),
                ObservationBackend::Gcs {
                    endpoint: gcs_bucket_versioning_url(GCS_JSON_API_ORIGIN, bucket)?,
                    credentials,
                },
            )
        }
    };

    Ok((
        store,
        CheckpointBucketVersioningObserver {
            backend,
            client,
            timeout,
            maximum_age,
            gate: Arc::clone(&gate),
            observation_lock: Arc::new(tokio::sync::Mutex::new(())),
        },
        gate,
    ))
}

fn s3_bucket_versioning_url(
    config: &moa_config::ObjectStoreConfig,
    bucket: &str,
    region: &str,
) -> Result<Url> {
    let endpoint = match (&config.endpoint, config.virtual_hosted_style) {
        (Some(_), true) => s3_configured_endpoint(config, bucket)?,
        (Some(endpoint), false) => format!("{}/{}", endpoint.trim_end_matches('/'), bucket),
        (None, true) => format!("https://{bucket}.s3.{region}.amazonaws.com"),
        (None, false) => format!("https://s3.{region}.amazonaws.com/{bucket}"),
    };
    let mut url = strict_provider_url(&endpoint, config.allow_http)?;
    url.set_query(Some("versioning"));
    Ok(url)
}

fn s3_configured_endpoint(config: &moa_config::ObjectStoreConfig, bucket: &str) -> Result<String> {
    let raw = config
        .endpoint
        .as_deref()
        .ok_or_else(|| MoaError::ConfigError("checkpoint S3 endpoint is missing".to_string()))?;
    if !config.virtual_hosted_style {
        return Ok(raw.to_string());
    }
    let mut url = strict_provider_url(raw, config.allow_http)?;
    if url.path() != "/" {
        return Err(MoaError::ConfigError(
            "virtual-hosted checkpoint S3 endpoint must not contain a path".to_string(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| MoaError::ConfigError("checkpoint S3 endpoint has no host".to_string()))?;
    if !host.starts_with(&format!("{bucket}.")) {
        let bucket_host = format!("{bucket}.{host}");
        url.set_host(Some(&bucket_host)).map_err(|_| {
            MoaError::ConfigError("checkpoint S3 virtual-host endpoint is invalid".to_string())
        })?;
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn gcs_bucket_versioning_url(origin: &str, bucket: &str) -> Result<Url> {
    let mut url = strict_provider_url(origin, false)?;
    url.path_segments_mut()
        .map_err(|_| {
            MoaError::ConfigError("checkpoint GCS metadata origin cannot be a base URL".to_string())
        })?
        .extend(["storage", "v1", "b", bucket]);
    url.set_query(Some("fields=versioning"));
    Ok(url)
}

fn strict_provider_url(raw: &str, allow_http: bool) -> Result<Url> {
    let url = Url::parse(raw).map_err(|error| {
        MoaError::ConfigError(format!(
            "invalid checkpoint bucket versioning endpoint: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "https" | "http")
        || (url.scheme() == "http" && !allow_http)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(MoaError::ConfigError(
            "checkpoint bucket versioning endpoint must be an exact HTTP(S) origin/path without credentials, query, or fragment"
                .to_string(),
        ));
    }
    Ok(url)
}

async fn execute_bounded(
    client: &Client,
    request: reqwest_object_store::Request,
) -> Result<Vec<u8>> {
    let response = client.execute(request).await.map_err(map_transport_error)?;
    if response.status().is_redirection() {
        return Err(MoaError::ProviderError(
            "checkpoint bucket versioning endpoint attempted a redirect".to_string(),
        ));
    }
    if response.status() != StatusCode::OK {
        return Err(MoaError::HttpStatus {
            status: response.status().as_u16(),
            retry_after: None,
            message: "checkpoint bucket versioning observation failed".to_string(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VERSIONING_RESPONSE_BYTES as u64)
    {
        return Err(MoaError::ProviderError(
            "checkpoint bucket versioning response exceeded its size bound".to_string(),
        ));
    }
    let bytes = response.bytes().await.map_err(map_transport_error)?;
    if bytes.len() > MAX_VERSIONING_RESPONSE_BYTES {
        return Err(MoaError::ProviderError(
            "checkpoint bucket versioning response exceeded its size bound".to_string(),
        ));
    }
    Ok(bytes.to_vec())
}

fn parse_s3_versioning(bytes: &[u8]) -> ObservedCheckpointBucketVersioning {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut root_seen = false;
    let mut root_closed = false;
    let mut depth = 0_usize;
    let mut in_status = false;
    let mut status = None::<String>;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                let name = tag.local_name();
                if root_closed {
                    return ObservedCheckpointBucketVersioning::Unknown;
                }
                if depth == 0 {
                    if name.as_ref() != b"VersioningConfiguration" {
                        return ObservedCheckpointBucketVersioning::Unknown;
                    }
                    root_seen = true;
                } else if depth == 1 && name.as_ref() == b"Status" {
                    if in_status || status.is_some() {
                        return ObservedCheckpointBucketVersioning::Unknown;
                    }
                    in_status = true;
                } else if name.as_ref() == b"Status" {
                    return ObservedCheckpointBucketVersioning::Unknown;
                }
                depth = depth.saturating_add(1);
            }
            Ok(Event::Empty(tag)) => {
                let name = tag.local_name();
                if root_closed || name.as_ref() == b"Status" {
                    return ObservedCheckpointBucketVersioning::Unknown;
                }
                if depth == 0 {
                    if root_seen || name.as_ref() != b"VersioningConfiguration" {
                        return ObservedCheckpointBucketVersioning::Unknown;
                    }
                    root_seen = true;
                    root_closed = true;
                }
            }
            Ok(Event::Text(text)) if in_status => match text.unescape() {
                Ok(value) => status = Some(value.trim().to_string()),
                Err(_) => return ObservedCheckpointBucketVersioning::Unknown,
            },
            Ok(Event::Text(text)) => {
                let bytes: &[u8] = text.as_ref();
                if bytes.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return ObservedCheckpointBucketVersioning::Unknown;
                }
            }
            Ok(Event::End(tag)) => {
                if depth == 0 {
                    return ObservedCheckpointBucketVersioning::Unknown;
                }
                let name = tag.local_name();
                if depth == 2 && name.as_ref() == b"Status" {
                    if status.as_deref().is_none_or(str::is_empty) {
                        return ObservedCheckpointBucketVersioning::Unknown;
                    }
                    in_status = false;
                } else if depth == 1 {
                    if name.as_ref() != b"VersioningConfiguration" {
                        return ObservedCheckpointBucketVersioning::Unknown;
                    }
                    root_closed = true;
                }
                depth -= 1;
            }
            Ok(Event::Eof) => break,
            Err(_) => return ObservedCheckpointBucketVersioning::Unknown,
            _ => {}
        }
    }
    if !root_seen || !root_closed || depth != 0 || in_status {
        return ObservedCheckpointBucketVersioning::Unknown;
    }
    match status.as_deref() {
        None => ObservedCheckpointBucketVersioning::Unversioned,
        Some("Enabled") => ObservedCheckpointBucketVersioning::Enabled,
        Some("Suspended") => ObservedCheckpointBucketVersioning::Suspended,
        Some(_) => ObservedCheckpointBucketVersioning::Unknown,
    }
}

fn parse_gcs_versioning(bytes: &[u8]) -> ObservedCheckpointBucketVersioning {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BucketMetadata {
        versioning: Option<VersioningMetadata>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct VersioningMetadata {
        enabled: Option<bool>,
    }

    let Ok(value) = serde_json::from_slice::<BucketMetadata>(bytes) else {
        return ObservedCheckpointBucketVersioning::Unknown;
    };
    match value.versioning {
        None => ObservedCheckpointBucketVersioning::Unversioned,
        Some(versioning) => match versioning.enabled {
            Some(true) => ObservedCheckpointBucketVersioning::Enabled,
            Some(false) | None => ObservedCheckpointBucketVersioning::Unversioned,
        },
    }
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn map_transport_error(error: reqwest_object_store::Error) -> MoaError {
    if error.is_timeout() {
        return MoaError::ProviderTimeout(
            "checkpoint bucket versioning observation exceeded its HTTP deadline".to_string(),
        );
    }
    MoaError::ProviderTransport(format!(
        "checkpoint bucket versioning observation transport failed: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::StaticCredentialProvider;
    use object_store::aws::AwsCredential;
    use object_store::gcp::GcpCredential;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(response: &'static str) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback versioning fixture");
        let address = listener
            .local_addr()
            .expect("read loopback versioning fixture address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept versioning fixture request");
            let mut request = vec![0_u8; 16 * 1024];
            let read = stream
                .read(&mut request)
                .await
                .expect("read versioning fixture request");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write versioning fixture response");
            String::from_utf8(request[..read].to_vec())
                .expect("HTTP request should contain valid UTF-8 headers")
        });
        (
            Url::parse(&format!("http://{address}"))
                .expect("construct loopback versioning fixture URL"),
            task,
        )
    }

    fn test_client(timeout: Duration) -> Client {
        Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .expect("build loopback versioning client")
    }

    fn s3_observer(endpoint: Url, timeout: Duration) -> CheckpointBucketVersioningObserver {
        let gate = Arc::new(CheckpointBucketVersioningGate::new(Duration::from_secs(60)));
        CheckpointBucketVersioningObserver {
            backend: ObservationBackend::S3 {
                endpoint,
                region: "us-east-1".to_string(),
                credentials: Arc::new(StaticCredentialProvider::new(AwsCredential {
                    key_id: "fixture-access".to_string(),
                    secret_key: "fixture-secret".to_string(),
                    token: None,
                })),
            },
            client: test_client(timeout),
            timeout,
            maximum_age: Duration::from_secs(60),
            gate,
            observation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn gcs_observer(endpoint: Url) -> CheckpointBucketVersioningObserver {
        let gate = Arc::new(CheckpointBucketVersioningGate::new(Duration::from_secs(60)));
        CheckpointBucketVersioningObserver {
            backend: ObservationBackend::Gcs {
                endpoint,
                credentials: Arc::new(StaticCredentialProvider::new(GcpCredential {
                    bearer: "fixture-bearer".to_string(),
                })),
            },
            client: test_client(Duration::from_secs(1)),
            timeout: Duration::from_secs(1),
            maximum_age: Duration::from_secs(60),
            gate,
            observation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    #[test]
    fn s3_response_distinguishes_unversioned_enabled_suspended_and_unknown() {
        // Pins: enabled and suspended S3 buckets are never collapsed into the
        // only cleanup-safe state, including ambiguous response shapes.
        let cases = [
            (
                br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unversioned,
            ),
            (
                br#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Enabled,
            ),
            (
                br#"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Suspended,
            ),
            (
                br#"<VersioningConfiguration><Status>FutureState</Status></VersioningConfiguration>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"<VersioningConfiguration><Status>Enabled</Status><Status>Suspended</Status></VersioningConfiguration>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"<VersioningConfiguration><Status/></VersioningConfiguration>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"<VersioningConfiguration/><VersioningConfiguration/>"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"<VersioningConfiguration><Status>Enabled"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(parse_s3_versioning(body), expected);
        }
    }

    #[test]
    fn gcs_response_distinguishes_unversioned_enabled_and_unknown() {
        // Pins: missing/false GCS versioning is authoritative unversioned, while
        // malformed metadata cannot be treated as a configuration assertion.
        let cases = [
            (
                br#"{}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unversioned,
            ),
            (
                br#"{"versioning":{"enabled":false}}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unversioned,
            ),
            (
                br#"{"versioning":{"enabled":true}}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Enabled,
            ),
            (
                br#"{"versioning":{"enabled":"false"}}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"{"versioning":{"enabled":false,"enabled":true}}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"{"versioning":{"enabled":false},"versioning":{"enabled":true}}"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
            (
                br#"[]"#.as_slice(),
                ObservedCheckpointBucketVersioning::Unknown,
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(parse_gcs_versioning(body), expected);
        }
    }

    #[test]
    fn provider_endpoints_refuse_query_credentials_and_plaintext_cloud() {
        // Pins: redirects are disabled at the client and endpoint construction
        // cannot smuggle credentials, a query override, or public plaintext.
        for endpoint in [
            "https://user:secret@objects.example.com",
            "https://objects.example.com?versioning=false",
            "https://objects.example.com#other",
            "http://objects.example.com",
        ] {
            assert!(
                strict_provider_url(endpoint, false).is_err(),
                "endpoint should fail closed: {endpoint}"
            );
        }
    }

    #[test]
    fn virtual_hosted_s3_endpoint_is_shared_by_store_and_observer() {
        // Pins: a custom virtual-host endpoint receives the bucket host exactly
        // once, so object I/O and versioning observation cannot diverge.
        let config = moa_config::ObjectStoreConfig {
            endpoint: Some("https://objects.example.com".to_string()),
            virtual_hosted_style: true,
            ..moa_config::ObjectStoreConfig::default()
        };
        assert_eq!(
            s3_configured_endpoint(&config, "checkpoint-bucket")
                .expect("normalize virtual-hosted S3 endpoint"),
            "https://checkpoint-bucket.objects.example.com"
        );
        let already_scoped = moa_config::ObjectStoreConfig {
            endpoint: Some("https://checkpoint-bucket.objects.example.com".to_string()),
            ..config
        };
        assert_eq!(
            s3_configured_endpoint(&already_scoped, "checkpoint-bucket")
                .expect("preserve already-scoped virtual host"),
            "https://checkpoint-bucket.objects.example.com"
        );
    }

    #[tokio::test]
    async fn s3_observer_signs_exact_request_and_opens_freshness_gate() {
        // Pins: the preflight uses the exact S3 credential provider, signed
        // bucket-versioning request, and only a provider response opens the gate.
        let response = "HTTP/1.1 200 OK\r\ncontent-length: 26\r\nconnection: close\r\n\r\n<VersioningConfiguration/>";
        let (mut endpoint, server) = serve_once(response).await;
        endpoint.set_path("/checkpoint-bucket");
        endpoint.set_query(Some("versioning"));
        let observer = s3_observer(endpoint, Duration::from_secs(1));

        let observation = observer
            .observe_unversioned()
            .await
            .expect("authenticated S3 unversioned observation should pass");
        let request = server.await.expect("join S3 versioning fixture");

        assert_eq!(
            observation.state(),
            ObservedCheckpointBucketVersioning::Unversioned
        );
        assert!(observer.is_ready());
        assert!(request.starts_with("GET /checkpoint-bucket?versioning HTTP/1.1\r\n"));
        assert!(request.lines().any(|line| {
            line.starts_with("authorization: AWS4-HMAC-SHA256 Credential=fixture-access/")
        }));
    }

    #[tokio::test]
    async fn gcs_observer_uses_exact_bearer_and_rejects_enabled_bucket() {
        // Pins: GCS metadata is authenticated with the same rotating token
        // provider and an enabled bucket never opens readiness/store access.
        let body = r#"{"versioning":{"enabled":true}}"#;
        let response = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (mut endpoint, server) = serve_once(response).await;
        endpoint.set_path("/storage/v1/b/checkpoint-bucket");
        endpoint.set_query(Some("fields=versioning"));
        let observer = gcs_observer(endpoint);

        let error = observer
            .observe_unversioned()
            .await
            .expect_err("enabled GCS bucket must fail preflight");
        let request = server.await.expect("join GCS versioning fixture");

        assert!(matches!(error, MoaError::ConfigError(_)));
        assert!(!observer.is_ready());
        assert!(
            request
                .starts_with("GET /storage/v1/b/checkpoint-bucket?fields=versioning HTTP/1.1\r\n")
        );
        assert!(
            request
                .lines()
                .any(|line| line == "authorization: Bearer fixture-bearer")
        );
    }

    #[tokio::test]
    async fn redirect_and_timeout_invalidate_prior_versioning_readiness() {
        // Pins: redirects never forward credentials and an ambiguous timeout
        // immediately closes a gate that a previous observation opened.
        let redirect = "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:9/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let (endpoint, server) = serve_once(redirect).await;
        let observer = s3_observer(endpoint, Duration::from_secs(1));
        observer
            .gate
            .record(&CheckpointBucketVersioningObservation {
                state: ObservedCheckpointBucketVersioning::Unversioned,
                observed_at: Utc::now(),
                observed_instant: Instant::now(),
            });
        assert!(observer.is_ready());
        let error = observer
            .observe()
            .await
            .expect_err("redirect must fail without following");
        let _request = server.await.expect("join redirect fixture");
        assert!(matches!(error, MoaError::ProviderError(_)));
        assert!(!observer.is_ready());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind timeout fixture");
        let endpoint = Url::parse(&format!(
            "http://{}",
            listener.local_addr().expect("timeout fixture address")
        ))
        .expect("timeout fixture URL");
        let stalled = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept timeout request");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let observer = s3_observer(endpoint, Duration::from_millis(25));
        let error = observer
            .observe()
            .await
            .expect_err("stalled provider must time out");
        stalled.abort();
        assert!(matches!(error, MoaError::ProviderTimeout(_)));
        assert!(!observer.is_ready());
    }

    #[test]
    fn stale_observation_closes_gate_without_wall_clock_arithmetic() {
        // Pins: freshness uses monotonic elapsed time and cannot remain open
        // after the configured maximum age.
        let gate = CheckpointBucketVersioningGate::new(Duration::from_secs(1));
        gate.record(&CheckpointBucketVersioningObservation {
            state: ObservedCheckpointBucketVersioning::Unversioned,
            observed_at: Utc::now(),
            observed_instant: Instant::now() - Duration::from_secs(2),
        });
        assert!(!gate.is_verified());
    }
}
