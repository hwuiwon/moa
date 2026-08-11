//! Model Context Protocol client support for stateless Streamable HTTP.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::Instant;

use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, pin_mut};
use moa_config::McpServerConfig;
use moa_core::{
    error::MoaError, error::Result, types::tools::ToolContent, types::tools::ToolOutput,
};
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds unique-cursor pagination so a malicious server cannot keep discovery alive forever.
const MAX_MCP_TOOL_LIST_PAGES: usize = 100;
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MCP_METHOD_HEADER: &str = "mcp-method";
const MCP_NAME_HEADER: &str = "mcp-name";
const CLIENT_PROTOCOL_VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_INFO_META: &str = "io.modelcontextprotocol/clientInfo";
const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";

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
    protocol_version: Option<String>,
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

    /// Returns the fixed protocol revision used for this discovery, when known.
    #[must_use]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// Returns whether this exact server was configured to trust tool annotations.
    #[must_use]
    pub fn trusts_tool_annotations(&self) -> bool {
        self.trust_tool_annotations
    }

    /// Returns whether configured annotation trust permits automatic retries.
    pub(crate) fn allows_idempotent_retry(&self) -> bool {
        self.trust_tool_annotations && self.idempotent_hint == Some(true)
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
            protocol_version: None,
            trust_tool_annotations: false,
        }
    }
}

/// Async MCP client bound to a single configured server.
pub struct MCPClient {
    server_name: String,
    transport: RemoteClient,
    next_id: AtomicU64,
    trust_tool_annotations: bool,
    tool_headers: tokio::sync::RwLock<HashMap<String, Vec<ToolHeaderProjection>>>,
    catalog_freshness: RwLock<CatalogFreshness>,
}

impl MCPClient {
    /// Connects to a modern, stateless MCP server and verifies its advertised capabilities.
    pub async fn connect(
        config: &McpServerConfig,
        headers: HashMap<String, String>,
    ) -> Result<Self> {
        let client = Self {
            server_name: config.name.clone(),
            transport: RemoteClient::new(config, header_map_from_pairs(headers)?)?,
            next_id: AtomicU64::new(1),
            trust_tool_annotations: config.trust_tool_annotations,
            tool_headers: tokio::sync::RwLock::new(HashMap::new()),
            catalog_freshness: RwLock::new(CatalogFreshness::default()),
        };
        client.discover().await?;
        Ok(client)
    }

    /// Returns the configured MCP server name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns the modern protocol revision used on every request.
    #[must_use]
    pub fn protocol_version(&self) -> &'static str {
        MCP_PROTOCOL_VERSION
    }

    /// Returns the earliest expiry advertised by discovery and tool listing.
    #[must_use]
    pub(crate) fn catalog_fresh_until(&self) -> Option<Instant> {
        self.catalog_freshness
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effective_deadline()
    }

    /// Lists all currently exposed tools from the server.
    pub async fn list_tools(&self) -> Result<Vec<McpDiscoveredToolRegistration>> {
        let mut registrations = Vec::new();
        let mut projections = HashMap::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut page_count = 0_usize;
        let mut tools_fresh_until = None;

        loop {
            page_count += 1;
            let mut params = serde_json::Map::new();
            if let Some(cursor) = &cursor {
                params.insert("cursor".to_string(), Value::String(cursor.clone()));
            }
            let response = self
                .request("tools/list", Value::Object(params), None, HeaderMap::new())
                .await?;
            let parsed: ToolsListResponse = serde_json::from_value(response)?;
            ensure_complete_result(&parsed.result_type, "tools/list")?;
            validate_cache_fields(parsed.ttl_ms, &parsed.cache_scope, "tools/list")?;
            let page_fresh_until = cache_deadline(parsed.ttl_ms);
            tools_fresh_until = Some(
                tools_fresh_until.map_or(page_fresh_until, |current: Instant| {
                    current.min(page_fresh_until)
                }),
            );

            for tool in parsed.tools {
                let headers = match tool_header_projections(&tool.input_schema) {
                    Ok(headers) => headers,
                    Err(error) => {
                        tracing::warn!(
                            mcp_server = %self.server_name,
                            tool = %tool.name,
                            %error,
                            "excluding MCP tool with invalid x-mcp-header annotations"
                        );
                        continue;
                    }
                };
                projections.insert(tool.name.clone(), headers);
                registrations.push(McpDiscoveredToolRegistration {
                    tool: McpDiscoveredTool {
                        name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        input_schema: tool.input_schema,
                    },
                    idempotent_hint: tool
                        .annotations
                        .and_then(|annotations| annotations.idempotent_hint),
                    protocol_version: Some(MCP_PROTOCOL_VERSION.to_string()),
                    trust_tool_annotations: self.trust_tool_annotations,
                });
            }

            let Some(next_cursor) = parsed.next_cursor else {
                break;
            };
            if page_count >= MAX_MCP_TOOL_LIST_PAGES {
                return Err(MoaError::StreamError(format!(
                    "MCP tools/list exceeded the {MAX_MCP_TOOL_LIST_PAGES}-page safety limit"
                )));
            }
            if next_cursor.is_empty() || !seen_cursors.insert(next_cursor.clone()) {
                return Err(MoaError::StreamError(
                    "MCP tools/list returned an empty or repeated nextCursor".to_string(),
                ));
            }
            cursor = Some(next_cursor);
        }

        *self.tool_headers.write().await = projections;
        self.catalog_freshness
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tools_fresh_until = tools_fresh_until;
        Ok(registrations)
    }

    /// Calls one MCP tool, stopping the local wait when `cancel_token` is cancelled.
    ///
    /// On cancellation the request future is dropped, closing any response stream.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        tool_invocation_id: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<ToolOutput> {
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
        let headers = self
            .tool_call_headers(name, params.get("arguments"))
            .await?;
        let response = self
            .request("tools/call", Value::Object(params), cancel_token, headers)
            .await?;
        ensure_result_is_complete(&response, "tools/call")?;
        Ok(flatten_call_result(response))
    }

    async fn discover(&self) -> Result<()> {
        let response = self
            .request("server/discover", json!({}), None, HeaderMap::new())
            .await?;
        let discovered: DiscoverResponse = serde_json::from_value(response)?;
        ensure_complete_result(&discovered.result_type, "server/discover")?;
        validate_cache_fields(
            discovered.ttl_ms,
            &discovered.cache_scope,
            "server/discover",
        )?;
        self.catalog_freshness
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .discovery_fresh_until = Some(cache_deadline(discovered.ttl_ms));
        if !discovered
            .supported_versions
            .iter()
            .any(|version| version == MCP_PROTOCOL_VERSION)
        {
            return Err(MoaError::StreamError(format!(
                "MCP server does not support required protocol version {MCP_PROTOCOL_VERSION}; supported versions: {}",
                discovered.supported_versions.join(", ")
            )));
        }
        if !discovered
            .capabilities
            .get("tools")
            .is_some_and(Value::is_object)
        {
            return Err(MoaError::StreamError(
                "MCP server/discover did not advertise the tools capability".to_string(),
            ));
        }
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        mut params: Value,
        cancel_token: Option<&CancellationToken>,
        headers: HeaderMap,
    ) -> Result<Value> {
        insert_request_metadata(&mut params)?;
        let message_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": message_id,
            "method": method,
            "params": params,
        });
        let response = if let Some(cancel_token) = cancel_token {
            tokio::select! {
                response = self.transport.request(request, method, headers, message_id) => response?,
                _ = cancel_token.cancelled() => {
                    tracing::debug!(
                        mcp_server = %self.server_name,
                        method,
                        request_id = message_id,
                        local_outcome = "cancelled",
                        remote_outcome = "unknown",
                        "MCP request was cancelled locally; remote side effects may still complete"
                    );
                    return Err(MoaError::Cancelled);
                },
            }
        } else {
            self.transport
                .request(request, method, headers, message_id)
                .await?
        };
        parse_jsonrpc_result(response, message_id)
    }

    async fn tool_call_headers(&self, name: &str, arguments: Option<&Value>) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(MCP_NAME_HEADER),
            encoded_header_value(name)?,
        );
        let projections = self.tool_headers.read().await;
        let Some(tool_projections) = projections.get(name) else {
            return Ok(headers);
        };
        let arguments = arguments.unwrap_or(&Value::Null);
        for projection in tool_projections {
            let Some(value) = value_at_path(arguments, &projection.path) else {
                continue;
            };
            if value.is_null() {
                continue;
            }
            let value = projected_value(value, projection.value_type)?;
            let header_name =
                HeaderName::from_bytes(format!("mcp-param-{}", projection.header_name).as_bytes())
                    .map_err(|error| {
                        MoaError::ValidationError(format!(
                            "invalid projected MCP header {}: {error}",
                            projection.header_name
                        ))
                    })?;
            headers.insert(header_name, encoded_header_value(&value)?);
        }
        Ok(headers)
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
    fn new(config: &McpServerConfig, headers: HeaderMap) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(DEFAULT_MCP_TIMEOUT)
                .default_headers(headers)
                .build()
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to build MCP http client: {error}"))
                })?,
            url: config.url.clone(),
        })
    }

    async fn request(
        &self,
        message: Value,
        method: &str,
        headers: HeaderMap,
        message_id: u64,
    ) -> Result<Value> {
        let request = self
            .client
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION)
            .header(MCP_METHOD_HEADER, method)
            .headers(headers)
            .json(&message);
        let response = request.send().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to call MCP server: {error}"))
        })?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.contains("text/event-stream") {
            let body = read_sse_response(response, message_id).await?;
            if status.is_success() {
                return Ok(body);
            }
            return parse_jsonrpc_result(body, message_id).and_then(|_| {
                Err(MoaError::HttpStatus {
                    status: status.as_u16(),
                    retry_after: None,
                    message: "MCP server request failed".to_string(),
                })
            });
        }
        if !content_type.contains("application/json") {
            return Err(MoaError::StreamError(format!(
                "MCP server returned unsupported content type {content_type:?}"
            )));
        }
        let body = response.json::<Value>().await.map_err(|error| {
            MoaError::StreamError(format!("invalid MCP JSON response: {error}"))
        })?;
        if !status.is_success() {
            return parse_jsonrpc_result(body, message_id).and_then(|_| {
                Err(MoaError::HttpStatus {
                    status: status.as_u16(),
                    retry_after: None,
                    message: "MCP server request failed".to_string(),
                })
            });
        }
        Ok(body)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ToolsListResponse {
    #[serde(rename = "resultType")]
    result_type: String,
    tools: Vec<ToolsListEntry>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
    #[serde(rename = "ttlMs")]
    ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    cache_scope: String,
}

#[derive(Debug, Deserialize)]
struct DiscoverResponse {
    #[serde(rename = "resultType")]
    result_type: String,
    #[serde(rename = "supportedVersions")]
    supported_versions: Vec<String>,
    capabilities: Value,
    #[serde(rename = "ttlMs")]
    ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    cache_scope: String,
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

#[derive(Debug, Default)]
struct CatalogFreshness {
    discovery_fresh_until: Option<Instant>,
    tools_fresh_until: Option<Instant>,
}

impl CatalogFreshness {
    fn effective_deadline(&self) -> Option<Instant> {
        match (self.discovery_fresh_until, self.tools_fresh_until) {
            (Some(discovery), Some(tools)) => Some(discovery.min(tools)),
            _ => None,
        }
    }
}

fn cache_deadline(ttl_ms: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(ttl_ms))
        .unwrap_or_else(Instant::now)
}

async fn read_sse_response(response: reqwest::Response, expected_id: u64) -> Result<Value> {
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
            let message: Value = serde_json::from_str(&event.data).map_err(|error| {
                MoaError::StreamError(format!("invalid MCP SSE payload: {error}"))
            })?;
            if message.get("method").is_some() && message.get("id").is_none() {
                continue;
            }
            if message.get("method").is_some() {
                return Err(MoaError::StreamError(
                    "MCP server sent a prohibited server-initiated request on an HTTP response stream"
                        .to_string(),
                ));
            }
            if message.get("id") == Some(&json!(expected_id)) {
                return Ok(message);
            }
            return Err(MoaError::StreamError(format!(
                "MCP SSE response carried an unexpected JSON-RPC id; expected {expected_id}"
            )));
        }
    }

    Err(MoaError::StreamError(
        "MCP SSE stream ended without a JSON-RPC response".to_string(),
    ))
}

fn parse_jsonrpc_result(response: Value, expected_id: u64) -> Result<Value> {
    if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(MoaError::StreamError(
            "MCP response is missing jsonrpc=2.0".to_string(),
        ));
    }
    if response.get("id") != Some(&json!(expected_id)) {
        return Err(MoaError::StreamError(format!(
            "MCP response id did not match request id {expected_id}"
        )));
    }
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

fn insert_request_metadata(params: &mut Value) -> Result<()> {
    let params = params.as_object_mut().ok_or_else(|| {
        MoaError::ValidationError("MCP request params must be a JSON object".to_string())
    })?;
    let meta = params.entry("_meta").or_insert_with(|| json!({}));
    let meta = meta.as_object_mut().ok_or_else(|| {
        MoaError::ValidationError("MCP request _meta must be a JSON object".to_string())
    })?;
    meta.insert(
        CLIENT_PROTOCOL_VERSION_META.to_string(),
        Value::String(MCP_PROTOCOL_VERSION.to_string()),
    );
    meta.insert(
        CLIENT_INFO_META.to_string(),
        json!({"name": "moa", "version": env!("CARGO_PKG_VERSION")}),
    );
    meta.insert(CLIENT_CAPABILITIES_META.to_string(), json!({}));
    Ok(())
}

fn ensure_result_is_complete(result: &Value, method: &str) -> Result<()> {
    let result_type = result
        .get("resultType")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::StreamError(format!("MCP {method} result omitted required resultType"))
        })?;
    ensure_complete_result(result_type, method)
}

fn ensure_complete_result(result_type: &str, method: &str) -> Result<()> {
    match result_type {
        "complete" => Ok(()),
        "input_required" => Err(MoaError::ToolError(format!(
            "MCP {method} requires client input that MOA did not advertise support for"
        ))),
        other => Err(MoaError::StreamError(format!(
            "MCP {method} returned unknown resultType {other:?}"
        ))),
    }
}

fn validate_cache_fields(ttl_ms: u64, cache_scope: &str, method: &str) -> Result<()> {
    let _ = ttl_ms;
    if matches!(cache_scope, "public" | "private") {
        Ok(())
    } else {
        Err(MoaError::StreamError(format!(
            "MCP {method} returned invalid cacheScope {cache_scope:?}"
        )))
    }
}

#[derive(Debug, Clone, Copy)]
enum HeaderValueType {
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Clone)]
struct ToolHeaderProjection {
    header_name: String,
    path: Vec<String>,
    value_type: HeaderValueType,
}

fn tool_header_projections(
    schema: &Value,
) -> std::result::Result<Vec<ToolHeaderProjection>, String> {
    let schema_object = schema
        .as_object()
        .ok_or_else(|| "inputSchema must be a JSON object".to_string())?;
    if schema_object.get("type").and_then(Value::as_str) != Some("object") {
        return Err("inputSchema must declare type=object".to_string());
    }
    let mut projections = Vec::new();
    let mut names = HashSet::new();
    scan_header_annotations(schema, &[], true, &mut names, &mut projections)?;
    Ok(projections)
}

fn scan_header_annotations(
    schema: &Value,
    path: &[String],
    statically_reachable: bool,
    names: &mut HashSet<String>,
    projections: &mut Vec<ToolHeaderProjection>,
) -> std::result::Result<(), String> {
    if let Some(items) = schema.as_array() {
        for item in items {
            scan_header_annotations(item, path, false, names, projections)?;
        }
        return Ok(());
    }
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(annotation) = object.get("x-mcp-header") {
        let header_name = annotation
            .as_str()
            .ok_or_else(|| "x-mcp-header must be a string".to_string())?;
        if path.is_empty() || !statically_reachable {
            return Err("x-mcp-header is not on a statically reachable property".to_string());
        }
        if header_name.is_empty() || !header_name.bytes().all(is_tchar) {
            return Err(format!("x-mcp-header {header_name:?} is not an HTTP token"));
        }
        if !names.insert(header_name.to_ascii_lowercase()) {
            return Err(format!("duplicate x-mcp-header name {header_name:?}"));
        }
        let value_type = match object.get("type").and_then(Value::as_str) {
            Some("string") => HeaderValueType::String,
            Some("integer") => HeaderValueType::Integer,
            Some("boolean") => HeaderValueType::Boolean,
            _ => {
                return Err(format!(
                    "x-mcp-header {header_name:?} must annotate a string, integer, or boolean"
                ));
            }
        };
        projections.push(ToolHeaderProjection {
            header_name: header_name.to_string(),
            path: path.to_vec(),
            value_type,
        });
    }

    for (key, value) in object {
        if key == "properties" {
            let Some(properties) = value.as_object() else {
                continue;
            };
            for (property, property_schema) in properties {
                let mut property_path = path.to_vec();
                property_path.push(property.clone());
                scan_header_annotations(
                    property_schema,
                    &property_path,
                    statically_reachable,
                    names,
                    projections,
                )?;
            }
        } else if key != "x-mcp-header" {
            scan_header_annotations(value, path, false, names, projections)?;
        }
    }
    Ok(())
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn value_at_path<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn projected_value(value: &Value, value_type: HeaderValueType) -> Result<String> {
    match value_type {
        HeaderValueType::String => value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
            MoaError::ValidationError("x-mcp-header argument must be a string".to_string())
        }),
        HeaderValueType::Boolean => {
            value
                .as_bool()
                .map(|value| value.to_string())
                .ok_or_else(|| {
                    MoaError::ValidationError("x-mcp-header argument must be a boolean".to_string())
                })
        }
        HeaderValueType::Integer => {
            const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;
            let integer = value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
                .ok_or_else(|| {
                    MoaError::ValidationError(
                        "x-mcp-header argument must be an integer".to_string(),
                    )
                })?;
            if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&integer) {
                return Err(MoaError::ValidationError(
                    "x-mcp-header integer exceeds the JavaScript safe integer range".to_string(),
                ));
            }
            Ok(integer.to_string())
        }
    }
}

fn encoded_header_value(value: &str) -> Result<HeaderValue> {
    let sentinel = value.starts_with("=?base64?") && value.ends_with("?=");
    let plain_ascii = value == value.trim()
        && value
            .bytes()
            .all(|byte| matches!(byte, 0x20..=0x7e) || byte == b'\t')
        && !sentinel;
    let encoded = if plain_ascii {
        value.to_string()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
        )
    };
    HeaderValue::from_str(&encoded).map_err(|error| {
        MoaError::ValidationError(format!("invalid MCP request header value: {error}"))
    })
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
        let mut value = HeaderValue::from_str(&value).map_err(|error| {
            MoaError::ValidationError(format!("invalid MCP header value for {key}: {error}"))
        })?;
        value.set_sensitive(true);
        map.insert(name, value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests;
