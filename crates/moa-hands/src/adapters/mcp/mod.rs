//! Model Context Protocol client support for HTTP and SSE transports.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::{StreamExt, pin_mut};
use moa_core::{McpServerConfig, MoaError, Result, ToolContent, ToolOutput};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(60);

/// One tool discovered from a connected MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpDiscoveredTool {
    /// Stable MCP tool name.
    pub name: String,
    /// Human-readable tool description.
    pub description: String,
    /// JSON schema for the tool input.
    pub input_schema: Value,
}

/// Async MCP client bound to a single configured server.
pub struct MCPClient {
    server_name: String,
    transport: RemoteClient,
    next_id: AtomicU64,
}

impl MCPClient {
    /// Connects to an MCP server and performs the initialize handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        let client = Self {
            server_name: config.name.clone(),
            transport: RemoteClient::new(config)?,
            next_id: AtomicU64::new(1),
        };
        client.initialize().await?;
        Ok(client)
    }

    /// Returns the configured MCP server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Lists all currently exposed tools from the server.
    pub async fn list_tools(&self) -> Result<Vec<McpDiscoveredTool>> {
        let response = self
            .request("tools/list", json!({}), HeaderMap::new())
            .await?;
        let parsed: ToolsListResponse = serde_json::from_value(response)?;
        Ok(parsed
            .tools
            .into_iter()
            .map(|tool| McpDiscoveredTool {
                name: tool.name,
                description: tool.description.unwrap_or_default(),
                input_schema: tool.input_schema,
            })
            .collect())
    }

    /// Calls one MCP tool with optional extra transport headers.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        extra_headers: HashMap<String, String>,
    ) -> Result<ToolOutput> {
        let headers = header_map_from_pairs(extra_headers)?;
        let response = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments,
                }),
                headers,
            )
            .await?;
        Ok(flatten_call_result(response))
    }

    async fn initialize(&self) -> Result<()> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "moa",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
                HeaderMap::new(),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.transport.notify(message).await
    }

    async fn request(&self, method: &str, params: Value, headers: HeaderMap) -> Result<Value> {
        let message_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": message_id,
            "method": method,
            "params": params,
        });
        let response = self.transport.request(request, headers).await?;
        parse_jsonrpc_result(response)
    }
}

// ---------------------------------------------------------------------------
// HTTP / SSE remote client
// ---------------------------------------------------------------------------

struct RemoteClient {
    client: reqwest::Client,
    url: String,
}

impl RemoteClient {
    fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config.url.clone().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MCP server {} requires a url for remote transport",
                config.name
            ))
        })?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(DEFAULT_MCP_TIMEOUT)
                .build()
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to build MCP http client: {error}"))
                })?,
            url,
        })
    }

    async fn notify(&self, message: Value) -> Result<()> {
        let response = self
            .client
            .post(&self.url)
            .json(&message)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to notify MCP server: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(MoaError::HttpStatus {
                status: response.status().as_u16(),
                retry_after: None,
                message: response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read MCP notify error".to_string()),
            });
        }
        Ok(())
    }

    async fn request(&self, message: Value, headers: HeaderMap) -> Result<Value> {
        let request = self.client.post(&self.url).headers(headers).json(&message);
        let response = request.send().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to call MCP server: {error}"))
        })?;
        if !response.status().is_success() {
            return Err(MoaError::HttpStatus {
                status: response.status().as_u16(),
                retry_after: None,
                message: response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read MCP error body".to_string()),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.contains("text/event-stream") {
            return read_sse_response(response).await;
        }

        response
            .json::<Value>()
            .await
            .map_err(|error| MoaError::StreamError(format!("invalid MCP JSON response: {error}")))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ToolsListResponse {
    tools: Vec<ToolsListEntry>,
}

#[derive(Debug, Deserialize)]
struct ToolsListEntry {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

async fn read_sse_response(response: reqwest::Response) -> Result<Value> {
    // `eventsource-stream` defaults an absent `event:` field to "message", so a
    // bare `data:`-only event matches the same JSON-RPC response the manual
    // parser accepted.
    let events = response.bytes_stream().eventsource();
    pin_mut!(events);
    while let Some(event) = events.next().await {
        let event = event.map_err(|error| {
            MoaError::StreamError(format!("failed reading MCP SSE body: {error}"))
        })?;
        if event.event == "message" && !event.data.is_empty() {
            return serde_json::from_str(&event.data).map_err(|error| {
                MoaError::StreamError(format!("invalid MCP SSE payload: {error}"))
            });
        }
    }

    Err(MoaError::StreamError(
        "MCP SSE stream ended without a JSON-RPC response".to_string(),
    ))
}

fn parse_jsonrpc_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        return Err(MoaError::ToolError(format!(
            "MCP server returned error: {error}"
        )));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| MoaError::StreamError("missing MCP result payload".to_string()))
}

fn flatten_call_result(result: Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut content_blocks = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                content_blocks.push(ToolContent::Text {
                    text: text.to_string(),
                });
                continue;
            }
            content_blocks.push(ToolContent::Json { data: item.clone() });
        }
    } else if result != Value::Null {
        content_blocks.push(ToolContent::Json {
            data: result.clone(),
        });
    }

    ToolOutput {
        content: content_blocks,
        is_error,
        structured: result.get("structuredContent").cloned(),
        duration: Duration::default(),
        truncated: false,
        original_output_tokens: None,
        artifact: None,
    }
}

fn header_map_from_pairs(headers: HashMap<String, String>) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
            MoaError::ValidationError(format!("invalid MCP header name {key}: {error}"))
        })?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            MoaError::ValidationError(format!("invalid MCP header value for {key}: {error}"))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests;
