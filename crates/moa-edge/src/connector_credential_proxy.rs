//! Narrow proxy for connector credential plaintext.
//!
//! Unlike [`crate::proxy::OrchestratorProxy`], this proxy cannot target Restate
//! or accept a caller-selected path. It sends one bounded opaque body to one
//! fixed private orchestrator endpoint and accepts only an empty `204`
//! response, keeping credential material out of edge-visible response bodies.

use std::time::Duration;

use moa_core::traits::Identity;
use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::ConnectorConnectionId;
use moa_wire::connectors::{
    CONNECTOR_CONNECTION_ID_HEADER, CONNECTOR_CREDENTIAL_INGRESS_PATH,
    CONNECTOR_CREDENTIAL_SLOT_HEADER,
};
use reqwest::{Client, StatusCode, Url};
use thiserror::Error;

use crate::headers;

/// Maximum opaque request bytes accepted by both the public and private ingress.
pub const MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES: usize = 65_536;

/// Failure to construct the private credential proxy.
#[derive(Debug, Error)]
pub enum ConnectorCredentialProxyBuildError {
    /// The configured value is not an origin-only HTTP(S) URL.
    #[error("connector credential upstream must be an origin-only HTTP(S) URL")]
    InvalidUpstream,
    /// The hardened HTTP client could not be constructed.
    #[error("build connector credential HTTP client")]
    Client(#[source] reqwest::Error),
}

/// Sanitized private-ingress failure.
///
/// This error deliberately never stores an upstream response body or request
/// bytes, so logging it cannot reflect credential material.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectorCredentialProxyError {
    /// The credential envelope was empty.
    #[error("private credential request body is required")]
    InvalidRequest,
    /// The credential envelope exceeded the fixed pre-deserialization cap.
    #[error("private credential request exceeds the size limit")]
    RequestTooLarge,
    /// The private listener could not be reached or did not complete in time.
    #[error("private credential ingress unavailable")]
    Transport,
    /// The private listener rejected the request with a public-safe status.
    #[error("private credential ingress rejected the request with status {status}")]
    Rejected {
        /// Sanitized upstream status; no response body is retained.
        status: StatusCode,
    },
    /// The private listener returned a response outside the empty-204 contract.
    #[error("private credential ingress returned an invalid response contract")]
    InvalidResponse,
}

/// Exact-path HTTP client for connector credential writes.
pub struct ConnectorCredentialProxy {
    http: Client,
    write_url: Url,
}

impl ConnectorCredentialProxy {
    /// Builds a proxy whose upstream is an origin, never a caller-selected URL.
    pub fn new(
        upstream_origin: impl AsRef<str>,
    ) -> Result<Self, ConnectorCredentialProxyBuildError> {
        let mut origin = Url::parse(upstream_origin.as_ref())
            .map_err(|_| ConnectorCredentialProxyBuildError::InvalidUpstream)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !matches!(origin.path(), "" | "/")
        {
            return Err(ConnectorCredentialProxyBuildError::InvalidUpstream);
        }
        origin.set_path(CONNECTOR_CREDENTIAL_INGRESS_PATH);

        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(60))
            // A redirect could move plaintext and trusted headers outside the
            // private listener's network boundary.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ConnectorCredentialProxyBuildError::Client)?;

        Ok(Self {
            http,
            write_url: origin,
        })
    }

    /// Forwards one opaque credential body to the fixed private ingress path.
    ///
    /// No inbound HTTP headers are accepted by this API. All forwarded headers
    /// are constants or values derived from the authenticated [`Identity`] and
    /// typed route selectors.
    pub async fn forward(
        &self,
        identity: &Identity,
        connection_id: ConnectorConnectionId,
        slot_name: &CredentialSlotName,
        body: axum::body::Bytes,
    ) -> Result<(), ConnectorCredentialProxyError> {
        if body.is_empty() {
            return Err(ConnectorCredentialProxyError::InvalidRequest);
        }
        if body.len() > MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES {
            return Err(ConnectorCredentialProxyError::RequestTooLarge);
        }
        let mut request = self
            .http
            .post(self.write_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(headers::H_IDENTITY_TYPE, identity.identity_type.as_str())
            .header(headers::H_IDENTITY_ID, identity.id.to_string())
            .header(headers::H_TENANT_ID, identity.tenant_id.to_string())
            .header(CONNECTOR_CONNECTION_ID_HEADER, connection_id.to_string())
            .header(CONNECTOR_CREDENTIAL_SLOT_HEADER, slot_name.as_str());
        if let Some(api_key_id) = identity.api_key_id {
            request = request.header(headers::H_API_KEY_ID, api_key_id.to_string());
        }
        if let Some(user_id) = identity.acting_on_behalf_of {
            request = request.header(headers::H_ACTING_ON_BEHALF_OF, user_id.to_string());
        }
        request = moa_observability::propagation::with_reqwest_trace_headers(request);

        let response = request
            .body(body)
            .send()
            .await
            .map_err(|_| ConnectorCredentialProxyError::Transport)?;
        let status = response.status();
        if status != StatusCode::NO_CONTENT {
            if status.is_success() || !is_public_rejection_status(status) {
                return Err(ConnectorCredentialProxyError::InvalidResponse);
            }
            return Err(ConnectorCredentialProxyError::Rejected { status });
        }

        if let Some(length) = response.headers().get(reqwest::header::CONTENT_LENGTH) {
            let valid_empty_length = length
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                == Some(0);
            if !valid_empty_length {
                return Err(ConnectorCredentialProxyError::InvalidResponse);
            }
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| ConnectorCredentialProxyError::Transport)?;
        if !body.is_empty() {
            return Err(ConnectorCredentialProxyError::InvalidResponse);
        }
        Ok(())
    }
}

fn is_public_rejection_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::CONFLICT
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::TOO_MANY_REQUESTS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use moa_core::traits::IdentityType;
    use moa_core::types::identifiers::TenantId;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    const FIXTURE_SECRET: &str = "fixture_secret_private_proxy_only";

    #[tokio::test]
    async fn credential_proxy_uses_only_exact_path_and_derived_trust_headers() {
        // Pins: credential bytes can reach only the fixed private path, with no
        // caller header API capable of smuggling a second X-Moa identity.
        let (origin, request_rx, server) =
            capture_one_request(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec())
                .await;
        let identity = test_identity();
        let connection_id = ConnectorConnectionId(Uuid::from_u128(44));
        let proxy = ConnectorCredentialProxy::new(origin)
            .expect("private credential proxy should build for loopback origin");
        proxy
            .forward(
                &identity,
                connection_id,
                &CredentialSlotName::PRIMARY,
                Bytes::from(format!(r#"{{"material":"{FIXTURE_SECRET}"}}"#)),
            )
            .await
            .expect("exact private credential write should succeed");

        let request = request_rx
            .await
            .expect("capture server should return the received request");
        let request_lower = request.to_ascii_lowercase();
        assert!(
            request.starts_with("POST /internal/v1/connectors/credentials/write HTTP/1.1\r\n"),
            "credential request must target only the exact private path; observed request line: {}",
            request.lines().next().unwrap_or("<missing>")
        );
        for expected in [
            format!("x-moa-identity-type: {}", identity.identity_type.as_str()),
            format!("x-moa-identity-id: {}", identity.id),
            format!("x-moa-tenant-id: {}", identity.tenant_id),
            format!(
                "x-moa-api-key-id: {}",
                identity.api_key_id.expect("fixture api key")
            ),
            format!(
                "x-moa-acting-on-behalf-of: {}",
                identity.acting_on_behalf_of.expect("fixture delegation")
            ),
            format!("x-moa-connector-connection-id: {connection_id}"),
            "x-moa-connector-credential-slot: primary".to_string(),
            "content-type: application/json".to_string(),
        ] {
            assert!(
                request_lower.contains(&expected.to_ascii_lowercase()),
                "missing exact derived header `{expected}` in captured request"
            );
        }
        assert!(request.contains(FIXTURE_SECRET));
        server
            .await
            .expect("capture server task should complete after one request");
    }

    #[tokio::test]
    async fn credential_proxy_discards_rejected_body_and_exposes_only_status() {
        // Pins: an upstream cannot reflect submitted credential bytes through a
        // public error or through this error's Debug/Display representation.
        let response = format!(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            FIXTURE_SECRET.len(),
            FIXTURE_SECRET
        )
        .into_bytes();
        let (origin, _request_rx, server) = capture_one_request(response).await;
        let proxy = ConnectorCredentialProxy::new(origin)
            .expect("private credential proxy should build for loopback origin");

        let error = proxy
            .forward(
                &test_identity(),
                ConnectorConnectionId(Uuid::from_u128(45)),
                &CredentialSlotName::PRIMARY,
                Bytes::from_static(FIXTURE_SECRET.as_bytes()),
            )
            .await
            .expect_err("conflict should be returned as a sanitized rejection");
        assert_eq!(
            error,
            ConnectorCredentialProxyError::Rejected {
                status: StatusCode::CONFLICT,
            }
        );
        assert!(!error.to_string().contains(FIXTURE_SECRET));
        assert!(!format!("{error:?}").contains(FIXTURE_SECRET));
        server
            .await
            .expect("capture server task should complete after one request");
    }

    #[tokio::test]
    async fn credential_proxy_rejects_nonempty_or_wrong_success_contract() {
        // Pins: the private listener cannot return any success payload that
        // might reflect plaintext, ciphertext, or a credential reference.
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 204 No Content\r\nContent-Length: 1\r\n\r\nx".to_vec(),
        ] {
            let (origin, _request_rx, server) = capture_one_request(response).await;
            let proxy = ConnectorCredentialProxy::new(origin)
                .expect("private credential proxy should build for loopback origin");
            let error = proxy
                .forward(
                    &test_identity(),
                    ConnectorConnectionId(Uuid::from_u128(46)),
                    &CredentialSlotName::PRIMARY,
                    Bytes::from_static(FIXTURE_SECRET.as_bytes()),
                )
                .await
                .expect_err("success outside the empty-204 contract must fail closed");
            assert_eq!(error, ConnectorCredentialProxyError::InvalidResponse);
            server
                .await
                .expect("capture server task should complete after one request");
        }
    }

    #[test]
    fn credential_proxy_rejects_non_origin_upstreams() {
        // Pins: config cannot preselect a path, query, fragment, userinfo, or
        // non-HTTP scheme that could change where plaintext is delivered.
        for upstream in [
            "file:///tmp/socket",
            "http://user:password@example.test",
            "http://example.test/base",
            "http://example.test?target=other",
            "http://example.test#fragment",
        ] {
            assert!(
                matches!(
                    ConnectorCredentialProxy::new(upstream),
                    Err(ConnectorCredentialProxyBuildError::InvalidUpstream)
                ),
                "unsafe private upstream `{upstream}` must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn credential_proxy_enforces_request_bound_before_connecting() {
        // Pins: the proxy itself owns the bound, so a future non-HTTP caller
        // cannot bypass the edge handler and send an unbounded secret body.
        let proxy = ConnectorCredentialProxy::new("http://127.0.0.1:9")
            .expect("loopback discard origin should be syntactically valid");
        let oversized = Bytes::from(vec![b'x'; MAX_CONNECTOR_CREDENTIAL_REQUEST_BYTES + 1]);

        assert_eq!(
            proxy
                .forward(
                    &test_identity(),
                    ConnectorConnectionId(Uuid::from_u128(47)),
                    &CredentialSlotName::PRIMARY,
                    oversized,
                )
                .await
                .expect_err("oversized request must fail before transport"),
            ConnectorCredentialProxyError::RequestTooLarge
        );
        assert_eq!(
            proxy
                .forward(
                    &test_identity(),
                    ConnectorConnectionId(Uuid::from_u128(47)),
                    &CredentialSlotName::PRIMARY,
                    Bytes::new(),
                )
                .await
                .expect_err("empty request must fail before transport"),
            ConnectorCredentialProxyError::InvalidRequest
        );
    }

    async fn capture_one_request(
        response: Vec<u8>,
    ) -> (
        String,
        oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("capture listener should bind");
        let address = listener
            .local_addr()
            .expect("capture listener should expose its address");
        let (request_tx, request_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("capture listener should accept one connection");
            let mut captured = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("capture listener should read request bytes");
                if read == 0 {
                    break;
                }
                captured.extend_from_slice(&buffer[..read]);
                if complete_http_request(&captured) {
                    break;
                }
            }
            let request = String::from_utf8(captured)
                .expect("fixture request should contain only UTF-8 bytes");
            request_tx
                .send(request)
                .expect("request receiver should remain alive");
            stream
                .write_all(&response)
                .await
                .expect("capture listener should write scripted response");
        });
        (format!("http://{address}"), request_rx, server)
    }

    fn complete_http_request(bytes: &[u8]) -> bool {
        let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers_end = headers_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..headers_end]);
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
        content_length.is_some_and(|length| bytes.len() >= headers_end + length)
    }

    fn test_identity() -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(11),
            tenant_id: TenantId::from(Uuid::from_u128(22)),
            api_key_id: Some(Uuid::from_u128(33)),
            acting_on_behalf_of: Some(Uuid::from_u128(34)),
        }
    }
}
