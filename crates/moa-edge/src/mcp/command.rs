//! Typed forwarding from MCP tools to allowlisted Restate service handlers.

use axum::http::HeaderMap;
use moa_core::traits::Identity;
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::ingress::{IngressScope, call_path};
use crate::proxy::OrchestratorProxy;

/// A compile-time Restate service handler path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ServicePath(&'static str);

impl ServicePath {
    /// Construct an allowlisted service path from a source constant.
    pub(crate) const fn new(path: &'static str) -> Self {
        Self(path)
    }
}

/// Shared typed command client used by MCP mutation and status tools.
pub(crate) struct McpCommandClient<'a> {
    proxy: &'a OrchestratorProxy,
    identity: &'a Identity,
    headers: &'a HeaderMap,
}

impl<'a> McpCommandClient<'a> {
    /// Bind a command client to one authenticated HTTP request.
    pub(crate) fn new(
        proxy: &'a OrchestratorProxy,
        identity: &'a Identity,
        headers: &'a HeaderMap,
    ) -> Self {
        Self {
            proxy,
            identity,
            headers,
        }
    }

    /// Invoke one allowlisted Restate service handler with a typed request and response.
    pub(crate) async fn call<Request, Response>(
        &self,
        path: ServicePath,
        request: &Request,
    ) -> Result<Response, McpCommandError>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let body = serde_json::to_vec(request).map_err(McpCommandError::SerializeRequest)?;
        let ingress_path = call_path(&IngressScope::Unscoped, path.0);
        let response = self
            .proxy
            .forward(
                self.identity,
                Method::POST,
                &ingress_path,
                body,
                self.headers,
            )
            .await
            .map_err(McpCommandError::Upstream)?;
        decode_response(response).await
    }

    /// Invoke an allowlisted Restate handler that accepts no request body.
    pub(crate) async fn call_empty<Response>(
        &self,
        path: ServicePath,
    ) -> Result<Response, McpCommandError>
    where
        Response: DeserializeOwned,
    {
        let ingress_path = call_path(&IngressScope::Unscoped, path.0);
        let response = self
            .proxy
            .forward(
                self.identity,
                Method::POST,
                &ingress_path,
                Vec::new(),
                self.headers,
            )
            .await
            .map_err(McpCommandError::Upstream)?;
        decode_response(response).await
    }
}

async fn decode_response<Response>(response: reqwest::Response) -> Result<Response, McpCommandError>
where
    Response: DeserializeOwned,
{
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| McpCommandError::Upstream(error.into()))?;
    if !status.is_success() {
        return Err(McpCommandError::Rejected {
            status: status.as_u16(),
            message: bounded_message(&bytes),
        });
    }
    let bytes = if bytes.is_empty() {
        b"null".as_slice()
    } else {
        bytes.as_ref()
    };
    serde_json::from_slice(bytes).map_err(McpCommandError::DeserializeResponse)
}

/// Failure returned while forwarding a typed MCP command.
#[derive(Debug, Error)]
pub(crate) enum McpCommandError {
    /// The adapter could not serialize a shared wire request.
    #[error("failed to encode command request")]
    SerializeRequest(#[source] serde_json::Error),
    /// The internal Restate ingress could not be reached.
    #[error("orchestrator unavailable")]
    Upstream(#[source] anyhow::Error),
    /// The owning service rejected the command.
    #[error("service rejected command with status {status}: {message}")]
    Rejected {
        /// Internal HTTP status returned by Restate ingress.
        status: u16,
        /// Bounded caller-visible service error.
        message: String,
    },
    /// The owning service returned a response that violated its shared wire contract.
    #[error("service returned an invalid response")]
    DeserializeResponse(#[source] serde_json::Error),
}

fn bounded_message(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 4 * 1024;
    let truncated = &bytes[..bytes.len().min(MAX_BYTES)];
    let message = String::from_utf8_lossy(truncated).trim().to_string();
    if message.is_empty() {
        "request failed".to_string()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_message;

    #[test]
    fn command_error_body_is_bounded_offline() {
        // Pins: an internal service cannot amplify an MCP result with an unbounded error body.
        let body = vec![b'x'; 8 * 1024];
        assert_eq!(bounded_message(&body).len(), 4 * 1024);
    }
}
