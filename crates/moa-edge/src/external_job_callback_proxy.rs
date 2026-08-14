//! Fixed-origin proxy for authenticated asynchronous-provider callbacks.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use reqwest::{Client, Url};
use thiserror::Error;
use uuid::Uuid;

/// Public callback route registered by the edge.
pub const EXTERNAL_JOB_CALLBACK_PUBLIC_ROUTE: &str = "/v1/execution/external-jobs/{external_job_uid}/generations/{job_generation}/callbacks/{provider_event_id}";

const INTERNAL_CALLBACK_PREFIX: &str = "/internal/v1/execution/external-jobs";

/// Maximum raw callback body accepted before forwarding.
pub const MAX_EXTERNAL_JOB_CALLBACK_BODY_BYTES: usize = 256 * 1024;
/// Maximum number of callback headers accepted at either boundary.
pub const MAX_EXTERNAL_JOB_CALLBACK_HEADERS: usize = 64;
/// Maximum aggregate callback-header bytes accepted at either boundary.
pub const MAX_EXTERNAL_JOB_CALLBACK_HEADER_BYTES: usize = 32 * 1024;
/// Maximum provider event identity length.
pub const MAX_EXTERNAL_JOB_PROVIDER_EVENT_ID_BYTES: usize = 512;

/// Immutable callback selector carried outside the untrusted body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalJobCallbackSelector {
    /// Stable MOA external-job identity.
    pub external_job_uid: Uuid,
    /// Exact provider-job generation.
    pub job_generation: u64,
    /// Provider event identity used for durable deduplication.
    pub provider_event_id: String,
}

/// Failure to construct the fixed private callback proxy.
#[derive(Debug, Error)]
pub enum ExternalJobCallbackProxyBuildError {
    /// The configured value was not an origin-only HTTP(S) URL.
    #[error("external-job callback upstream must be an origin-only HTTP(S) URL")]
    InvalidUpstream,
    /// The hardened HTTP client could not be constructed.
    #[error("build external-job callback HTTP client")]
    Client(#[source] reqwest::Error),
}

/// Sanitized callback proxy failure that never retains headers or body bytes.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExternalJobCallbackProxyError {
    /// Route selectors, headers, or body shape were invalid.
    #[error("invalid external-job callback request")]
    InvalidRequest,
    /// Raw callback evidence exceeded a fixed pre-forwarding limit.
    #[error("external-job callback request exceeds the size limit")]
    RequestTooLarge,
    /// The private listener could not be reached within its bounded timeout.
    #[error("private external-job callback ingress unavailable")]
    Transport,
    /// The private listener returned a public-safe rejection status.
    #[error("private external-job callback ingress rejected the request with status {status}")]
    Rejected {
        /// Sanitized status; no upstream response body is retained.
        status: StatusCode,
    },
    /// The private listener violated the empty-response contract.
    #[error("private external-job callback ingress returned an invalid response contract")]
    InvalidResponse,
}

/// Exact-path client for the private non-Restate callback listener.
pub struct ExternalJobCallbackProxy {
    http: Client,
    origin: Url,
}

impl ExternalJobCallbackProxy {
    /// Builds a proxy whose upstream is an origin, never a caller-selected URL.
    pub fn new(
        upstream_origin: impl AsRef<str>,
    ) -> Result<Self, ExternalJobCallbackProxyBuildError> {
        let origin = Url::parse(upstream_origin.as_ref())
            .map_err(|_| ExternalJobCallbackProxyBuildError::InvalidUpstream)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !matches!(origin.path(), "" | "/")
        {
            return Err(ExternalJobCallbackProxyBuildError::InvalidUpstream);
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ExternalJobCallbackProxyBuildError::Client)?;
        Ok(Self { http, origin })
    }

    /// Forwards bounded raw callback evidence to one fixed private path.
    pub async fn forward(
        &self,
        selector: &ExternalJobCallbackSelector,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<(), ExternalJobCallbackProxyError> {
        validate_selector(selector)?;
        validate_headers(headers)?;
        if body.is_empty() {
            return Err(ExternalJobCallbackProxyError::InvalidRequest);
        }
        if body.len() > MAX_EXTERNAL_JOB_CALLBACK_BODY_BYTES {
            return Err(ExternalJobCallbackProxyError::RequestTooLarge);
        }
        let url = callback_url(&self.origin, selector)?;
        let mut request = self.http.post(url);
        for (name, value) in headers {
            if should_forward_header(name.as_str()) {
                request = request.header(name.clone(), value.clone());
            }
        }
        request = moa_observability::propagation::with_reqwest_trace_headers(request).body(body);
        let response = request
            .send()
            .await
            .map_err(|_| ExternalJobCallbackProxyError::Transport)?;
        let status = response.status();
        if status != StatusCode::NO_CONTENT {
            if is_public_rejection_status(status) {
                return Err(ExternalJobCallbackProxyError::Rejected { status });
            }
            return Err(ExternalJobCallbackProxyError::InvalidResponse);
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .is_some_and(|length| length.as_bytes() != b"0")
        {
            return Err(ExternalJobCallbackProxyError::InvalidResponse);
        }
        let response_body = response
            .bytes()
            .await
            .map_err(|_| ExternalJobCallbackProxyError::Transport)?;
        if !response_body.is_empty() {
            return Err(ExternalJobCallbackProxyError::InvalidResponse);
        }
        Ok(())
    }
}

fn callback_url(
    origin: &Url,
    selector: &ExternalJobCallbackSelector,
) -> Result<Url, ExternalJobCallbackProxyError> {
    let mut url = origin.clone();
    url.path_segments_mut()
        .map_err(|_| ExternalJobCallbackProxyError::InvalidRequest)?
        .pop_if_empty()
        .extend([
            "internal",
            "v1",
            "execution",
            "external-jobs",
            &selector.external_job_uid.to_string(),
            "generations",
            &selector.job_generation.to_string(),
            "callbacks",
            &selector.provider_event_id,
        ]);
    debug_assert!(url.path().starts_with(INTERNAL_CALLBACK_PREFIX));
    Ok(url)
}

fn validate_selector(
    selector: &ExternalJobCallbackSelector,
) -> Result<(), ExternalJobCallbackProxyError> {
    if selector.external_job_uid.is_nil()
        || selector.job_generation == 0
        || selector.provider_event_id.trim().is_empty()
        || selector.provider_event_id.len() > MAX_EXTERNAL_JOB_PROVIDER_EVENT_ID_BYTES
        || selector.provider_event_id.chars().any(char::is_control)
    {
        return Err(ExternalJobCallbackProxyError::InvalidRequest);
    }
    Ok(())
}

fn validate_headers(headers: &HeaderMap) -> Result<(), ExternalJobCallbackProxyError> {
    if headers.len() > MAX_EXTERNAL_JOB_CALLBACK_HEADERS {
        return Err(ExternalJobCallbackProxyError::RequestTooLarge);
    }
    let mut bytes = 0usize;
    for name in headers.keys() {
        let values = headers.get_all(name);
        let mut values = values.iter();
        let value = values
            .next()
            .ok_or(ExternalJobCallbackProxyError::InvalidRequest)?;
        if values.next().is_some() {
            return Err(ExternalJobCallbackProxyError::InvalidRequest);
        }
        bytes = bytes
            .checked_add(name.as_str().len())
            .and_then(|sum| sum.checked_add(value.as_bytes().len()))
            .ok_or(ExternalJobCallbackProxyError::RequestTooLarge)?;
        if bytes > MAX_EXTERNAL_JOB_CALLBACK_HEADER_BYTES {
            return Err(ExternalJobCallbackProxyError::RequestTooLarge);
        }
    }
    Ok(())
}

fn should_forward_header(name: &str) -> bool {
    !name.eq_ignore_ascii_case("content-length")
        && !crate::proxy::is_hop_by_hop_header(name)
        && !crate::headers::is_moa_header(name)
        && !crate::proxy::is_trace_context_header(name)
}

fn is_public_rejection_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::NOT_FOUND
            | StatusCode::PAYLOAD_TOO_LARGE
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::SERVICE_UNAVAILABLE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn callback_proxy_preserves_signature_bytes_on_only_the_fixed_private_path() {
        // Pins: provider authentication bytes are forwarded unchanged, while
        // caller-selected MOA and hop-by-hop headers cannot cross the boundary.
        let (origin, request_rx, server) = capture_one_request().await;
        let proxy = ExternalJobCallbackProxy::new(origin).expect("build callback proxy");
        let selector = fixture_selector();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-provider-signature",
            "sha256=fixture".parse().expect("header"),
        );
        headers.insert("authorization", "Bearer fixture".parse().expect("header"));
        headers.insert("x-moa-tenant-id", "attacker".parse().expect("header"));
        headers.insert("connection", "close".parse().expect("header"));
        proxy
            .forward(&selector, &headers, Bytes::from_static(b"{\"ok\":true}"))
            .await
            .expect("callback proxy should accept empty 204");

        let request = request_rx.await.expect("captured request");
        assert!(request.starts_with(&format!(
            "POST /internal/v1/execution/external-jobs/{}/generations/7/callbacks/event%2F11 HTTP/1.1\r\n",
            selector.external_job_uid
        )));
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("x-provider-signature: sha256=fixture"));
        assert!(lower.contains("authorization: bearer fixture"));
        assert!(!lower.contains("x-moa-tenant-id"));
        server.await.expect("capture server");
    }

    #[tokio::test]
    async fn callback_proxy_rejects_limits_before_transport_and_never_echoes_body() {
        // Pins: oversized raw evidence is rejected without connecting, and its
        // bytes never enter the stable proxy error.
        let proxy = ExternalJobCallbackProxy::new("http://127.0.0.1:9")
            .expect("build syntactic callback proxy");
        let secret = "provider-signature-secret";
        let error = proxy
            .forward(
                &fixture_selector(),
                &HeaderMap::new(),
                Bytes::from(vec![b'x'; MAX_EXTERNAL_JOB_CALLBACK_BODY_BYTES + 1]),
            )
            .await
            .expect_err("oversized callback must fail locally");
        assert_eq!(error, ExternalJobCallbackProxyError::RequestTooLarge);
        assert!(!error.to_string().contains(secret));
    }

    fn fixture_selector() -> ExternalJobCallbackSelector {
        ExternalJobCallbackSelector {
            external_job_uid: Uuid::from_u128(1),
            job_generation: 7,
            provider_event_id: "event/11".to_string(),
        }
    }

    async fn capture_one_request() -> (
        String,
        oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let (request_tx, request_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut bytes = vec![0; 16 * 1024];
            let count = stream.read(&mut bytes).await.expect("read");
            request_tx
                .send(String::from_utf8_lossy(&bytes[..count]).into_owned())
                .ok();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write");
        });
        (format!("http://{addr}"), request_rx, server)
    }
}
