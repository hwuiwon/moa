//! Forwarding HTTP proxy with identity-header injection.

use crate::headers;
use anyhow::{Context, bail};
use moa_core::traits::{Identity, IdentityType};
use reqwest::Client;
use std::time::Duration;

/// HTTP proxy to the internal orchestrator Restate handler port.
pub struct OrchestratorProxy {
    http: Client,
    upstream_base: String,
}

impl OrchestratorProxy {
    /// Build a proxy for an orchestrator base URL.
    pub fn new(upstream_base: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(5))
                .pool_max_idle_per_host(32)
                .pool_idle_timeout(Duration::from_secs(90))
                .build()?,
            upstream_base: upstream_base.into().trim_end_matches('/').to_string(),
        })
    }

    /// Forward a request to the orchestrator with sanitized headers and injected identity.
    pub async fn forward(
        &self,
        identity: &Identity,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        request_headers: &axum::http::HeaderMap,
    ) -> Result<reqwest::Response, anyhow::Error> {
        validate_upstream_path(path)?;
        let url = format!("{}{}", self.upstream_base, path);
        let mut request = self.http.request(method, url);

        for (name, value) in request_headers {
            let name_str = name.as_str();
            if headers::is_moa_header(name_str)
                || name_str.eq_ignore_ascii_case("authorization")
                || is_hop_by_hop_header(name_str)
            {
                continue;
            }
            request = request.header(name.clone(), value.clone());
        }

        request = request
            .header(
                headers::H_IDENTITY_TYPE,
                identity_type_str(identity.identity_type),
            )
            .header(headers::H_IDENTITY_ID, identity.id.to_string())
            .header(headers::H_TENANT_ID, identity.tenant_id.to_string());
        if let Some(api_key_id) = identity.api_key_id {
            request = request.header(headers::H_API_KEY_ID, api_key_id.to_string());
        }
        if let Some(user_id) = identity.acting_on_behalf_of {
            request = request.header(headers::H_ACTING_ON_BEHALF_OF, user_id.to_string());
        }
        if !body.is_empty() {
            request = request.body(body);
        }

        Ok(request.send().await?)
    }

    /// Forward a token-verified public contact request without MOA identity headers.
    pub async fn forward_public(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        request_headers: &axum::http::HeaderMap,
    ) -> Result<reqwest::Response, anyhow::Error> {
        validate_upstream_path(path)?;
        let url = format!("{}{}", self.upstream_base, path);
        let mut request = self.http.request(method, url);

        for (name, value) in request_headers {
            let name_str = name.as_str();
            if headers::is_moa_header(name_str)
                || name_str.eq_ignore_ascii_case("authorization")
                || is_hop_by_hop_header(name_str)
            {
                continue;
            }
            request = request.header(name.clone(), value.clone());
        }
        if !body.is_empty() {
            request = request.body(body);
        }

        Ok(request.send().await?)
    }
}

fn validate_upstream_path(path: &str) -> Result<(), anyhow::Error> {
    if path.is_empty() {
        bail!("upstream path is empty");
    }
    if path.starts_with("//") {
        bail!("upstream path must not be an absolute URL");
    }
    if !path.starts_with('/') {
        bail!("upstream path must begin with /");
    }
    if path.contains('\\') {
        bail!("upstream path must not contain backslashes");
    }

    let decoded = percent_decode(path)?;
    if decoded.starts_with("//") {
        bail!("upstream path must not be an absolute URL");
    }
    if decoded.contains('\\') {
        bail!("upstream path must not contain backslashes");
    }

    let decoded_path = decoded
        .split(['?', '#'])
        .next()
        .filter(|value| !value.is_empty())
        .context("upstream path is empty")?;
    if decoded_path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        bail!("upstream path must not contain dot segments");
    }
    if !decoded_path.starts_with("/restate/") {
        bail!("upstream path must target /restate/");
    }

    Ok(())
}

fn percent_decode(path: &str) -> Result<String, anyhow::Error> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some(first) = bytes.get(index + 1).copied().and_then(hex_value) else {
            bail!("upstream path contains invalid percent encoding");
        };
        let Some(second) = bytes.get(index + 2).copied().and_then(hex_value) else {
            bail!("upstream path contains invalid percent encoding");
        };
        decoded.push((first << 4) | second);
        index += 3;
    }
    String::from_utf8(decoded).context("upstream path is not valid UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn identity_type_str(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::Operator => "operator",
        IdentityType::Contact => "contact",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}

fn is_hop_by_hop_header(name: &str) -> bool {
    const HOP_BY_HOP: [&str; 9] = [
        "host",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ];
    HOP_BY_HOP
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use moa_core::types::identifiers::TenantId;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::timeout;
    use uuid::Uuid;

    #[tokio::test]
    async fn edge_proxy_security_rejects_dot_segments() {
        // Pins: traversal-shaped paths are rejected before reqwest can normalize the URL and
        // before the proxy emits trusted X-Moa identity headers to an upstream connection.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener
            .local_addr()
            .expect("test listener should expose an address");
        let (request_tx, request_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("test server should accept");
            let mut buffer = vec![0; 4096];
            let count = stream
                .read(&mut buffer)
                .await
                .expect("test server should read request");
            let request = String::from_utf8_lossy(&buffer[..count]).into_owned();
            let _ = request_tx.send(request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("test server should write response");
        });

        let proxy =
            OrchestratorProxy::new(format!("http://{addr}")).expect("proxy client should be built");
        let error = proxy
            .forward(
                &test_identity(),
                reqwest::Method::POST,
                "/v1/../../restate/call/SessionStore/append_event",
                Vec::new(),
                &HeaderMap::new(),
            )
            .await
            .expect_err("literal dot-segment path should be rejected before forwarding");
        assert!(
            error.to_string().contains("dot segments"),
            "unexpected validation error: {error}"
        );

        if let Ok(Ok(request)) = timeout(Duration::from_millis(50), request_rx).await {
            assert!(
                !request.to_ascii_lowercase().contains("x-moa-"),
                "rejected request must not emit identity headers: {request}"
            );
            panic!("rejected request unexpectedly reached upstream: {request}");
        }
        server.abort();

        for path in [
            "/restate/call/Session/../append_event",
            "/restate/call/Session/%2e%2e/append_event",
            "/restate/call/Session/%2E/append_event",
            "/restate/call/Session%2f..%2fappend_event",
            "/restate/call\\Session/append_event",
            "/restate/call/%5cSession/append_event",
            "http://127.0.0.1/restate/call/SessionStore/append_event",
            "//127.0.0.1/restate/call/SessionStore/append_event",
            "",
            "restate/call/SessionStore/append_event",
            "/v1/%2e%2e/%2e%2e/restate/call/SessionStore/append_event",
        ] {
            assert!(
                validate_upstream_path(path).is_err(),
                "{path} must be rejected as an unsafe upstream path"
            );
        }
    }

    #[test]
    fn edge_proxy_security_allows_service_paths() {
        // Pins: legitimate translated Restate request-response paths remain valid proxy targets.
        for path in [
            "/restate/call/SessionStore/append_event",
            "/restate/call/Session/11111111-1111-1111-1111-111111111111/progress",
            "/restate/scope/tenant-22222222-2222-2222-2222-222222222222/call/Contacts/send_message",
        ] {
            validate_upstream_path(path)
                .unwrap_or_else(|error| panic!("{path} should be allowed: {error}"));
        }
    }

    fn test_identity() -> Identity {
        Identity {
            identity_type: IdentityType::Service,
            id: Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                .expect("identity id should parse"),
            tenant_id: TenantId::from(
                Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                    .expect("tenant id should parse"),
            ),
            api_key_id: Some(
                Uuid::parse_str("33333333-3333-3333-3333-333333333333")
                    .expect("api key id should parse"),
            ),
            acting_on_behalf_of: None,
        }
    }
}
