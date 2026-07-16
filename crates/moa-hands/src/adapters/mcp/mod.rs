//! Model Context Protocol client support for HTTP and SSE transports.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, pin_mut};
use moa_core::{
    config::McpServerConfig, error::MoaError, error::Result, types::tools::ToolContent,
    types::tools::ToolOutput,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
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

/// A discovered MCP tool plus the server evidence needed for safe registration.
#[derive(Debug, Clone, PartialEq)]
pub struct McpDiscoveredToolRegistration {
    tool: McpDiscoveredTool,
    idempotent_hint: Option<bool>,
    negotiated_protocol_version: Option<String>,
    trust_tool_annotations: bool,
}

impl McpDiscoveredToolRegistration {
    /// Returns the discovered tool definition.
    #[must_use]
    pub fn tool(&self) -> &McpDiscoveredTool {
        &self.tool
    }

    /// Returns the raw standard `idempotentHint`, when the server supplied one.
    #[must_use]
    pub fn idempotent_hint(&self) -> Option<bool> {
        self.idempotent_hint
    }

    /// Returns the protocol revision negotiated with the server, when known.
    #[must_use]
    pub fn negotiated_protocol_version(&self) -> Option<&str> {
        self.negotiated_protocol_version.as_deref()
    }

    /// Returns whether this exact server was configured to trust tool annotations.
    #[must_use]
    pub fn trusts_tool_annotations(&self) -> bool {
        self.trust_tool_annotations
    }

    /// Returns whether all negotiated trust conditions permit automatic retries.
    pub(crate) fn allows_idempotent_retry(&self) -> bool {
        self.trust_tool_annotations
            && self.idempotent_hint == Some(true)
            && self
                .negotiated_protocol_version
                .as_deref()
                .is_some_and(protocol_supports_tool_annotations)
    }

    /// Consumes the registration evidence and returns its tool definition.
    pub(crate) fn into_tool(self) -> McpDiscoveredTool {
        self.tool
    }
}

impl From<McpDiscoveredTool> for McpDiscoveredToolRegistration {
    fn from(tool: McpDiscoveredTool) -> Self {
        Self {
            tool,
            idempotent_hint: None,
            negotiated_protocol_version: None,
            trust_tool_annotations: false,
        }
    }
}

/// Async MCP client bound to a single configured server.
pub struct MCPClient {
    server_name: String,
    transport: RemoteClient,
    next_id: AtomicU64,
    negotiated_protocol_version: String,
    trust_tool_annotations: bool,
}

impl MCPClient {
    /// Connects to an MCP server and performs the initialize handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        let mut client = Self {
            server_name: config.name.clone(),
            transport: RemoteClient::new(config)?,
            next_id: AtomicU64::new(1),
            negotiated_protocol_version: String::new(),
            trust_tool_annotations: config.trust_tool_annotations,
        };
        client.negotiated_protocol_version = client.initialize().await?;
        Ok(client)
    }

    /// Returns the configured MCP server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns the protocol revision selected by the server during initialization.
    #[must_use]
    pub fn negotiated_protocol_version(&self) -> &str {
        &self.negotiated_protocol_version
    }

    /// Lists all currently exposed tools from the server.
    pub async fn list_tools(&self) -> Result<Vec<McpDiscoveredToolRegistration>> {
        let response = self
            .request("tools/list", json!({}), HeaderMap::new())
            .await?;
        let parsed: ToolsListResponse = serde_json::from_value(response)?;
        Ok(parsed
            .tools
            .into_iter()
            .map(|tool| McpDiscoveredToolRegistration {
                tool: McpDiscoveredTool {
                    name: tool.name,
                    description: tool.description.unwrap_or_default(),
                    input_schema: tool.input_schema,
                },
                idempotent_hint: tool
                    .annotations
                    .and_then(|annotations| annotations.idempotent_hint),
                negotiated_protocol_version: Some(self.negotiated_protocol_version.clone()),
                trust_tool_annotations: self.trust_tool_annotations,
            })
            .collect())
    }

    /// Calls one MCP tool with optional extra transport headers.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        tool_invocation_id: Option<&str>,
        extra_headers: HashMap<String, String>,
    ) -> Result<ToolOutput> {
        let headers = header_map_from_pairs(extra_headers)?;
        let mut params = serde_json::Map::from_iter([
            ("name".to_string(), Value::String(name.to_string())),
            ("arguments".to_string(), arguments),
        ]);
        if let Some(tool_invocation_id) = tool_invocation_id {
            params.insert(
                "_meta".to_string(),
                json!({"moa/toolInvocationId": tool_invocation_id}),
            );
        }
        let response = self
            .request("tools/call", Value::Object(params), headers)
            .await?;
        Ok(flatten_call_result(response))
    }

    async fn initialize(&self) -> Result<String> {
        let response = self
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
        let initialized: InitializeResponse = serde_json::from_value(response)?;
        if initialized.protocol_version.trim().is_empty() {
            return Err(MoaError::StreamError(
                "MCP initialize result contained an empty protocolVersion".to_string(),
            ));
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(initialized.protocol_version)
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
struct InitializeResponse {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
}

#[derive(Debug, Deserialize)]
struct ToolsListEntry {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
    annotations: Option<ToolAnnotations>,
}

#[derive(Debug, Deserialize)]
struct ToolAnnotations {
    #[serde(rename = "idempotentHint")]
    idempotent_hint: Option<bool>,
}

fn protocol_supports_tool_annotations(protocol_version: &str) -> bool {
    parse_protocol_date(protocol_version).is_some_and(|version| {
        NaiveDate::from_ymd_opt(2025, 3, 26).is_some_and(|minimum| version >= minimum)
    })
}

fn parse_protocol_date(protocol_version: &str) -> Option<NaiveDate> {
    let bytes = protocol_version.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    NaiveDate::parse_from_str(protocol_version, "%Y-%m-%d").ok()
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
