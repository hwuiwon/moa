//! Forwarding HTTP proxy with identity-header injection.

use crate::headers;
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
