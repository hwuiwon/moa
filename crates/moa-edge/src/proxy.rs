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
            http: Client::builder().timeout(Duration::from_secs(60)).build()?,
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
            let lowercase_name = name.as_str().to_ascii_lowercase();
            if headers::is_moa_header(&lowercase_name)
                || lowercase_name == "authorization"
                || is_hop_by_hop_header(&lowercase_name)
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
}

fn identity_type_str(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::User => "user",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}

fn is_hop_by_hop_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}
