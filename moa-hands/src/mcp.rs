//! Model Context Protocol client support for stdio and remote transports.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moa_core::{McpServerConfig, McpTransportConfig, MoaError, Result, ToolContent, ToolOutput};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

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
    transport: McpTransport,
    next_id: AtomicU64,
}

impl MCPClient {
    /// Connects to an MCP server and performs the initialize handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        let transport = match config.transport {
            McpTransportConfig::Stdio => {
                McpTransport::Stdio(StdioTransport::spawn(config)?)
            }
            McpTransportConfig::Http | McpTransportConfig::Sse => {
                McpTransport::Remote(RemoteClient::new(config)?)
            }
        };
        let client = Self {
            server_name: config.name.clone(),
            transport,
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
        match &self.transport {
            McpTransport::Stdio(transport) => transport.notify(message).await,
            McpTransport::Remote(transport) => transport.notify(message).await,
        }
    }

    async fn request(&self, method: &str, params: Value, headers: HeaderMap) -> Result<Value> {
        let message_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": message_id,
            "method": method,
            "params": params,
        });
        let response = match &self.transport {
            McpTransport::Stdio(transport) => transport.request(message_id, request).await?,
            McpTransport::Remote(transport) => transport.request(request, headers).await?,
        };
        parse_jsonrpc_result(response)
    }
}

enum McpTransport {
    Stdio(StdioTransport),
    Remote(RemoteClient),
}

// ---------------------------------------------------------------------------
// Stdio transport — demuxed, concurrent-safe
// ---------------------------------------------------------------------------

/// Shared map of in-flight requests: id -> oneshot sender for the response.
type PendingMap = Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>;

struct StdioTransport {
    /// Mutex only held during the actual `write_all` call — released before
    /// waiting for a response.
    writer: Mutex<ChildStdin>,
    pending: std::sync::Arc<PendingMap>,
    /// Background reader task handle (kept so it is aborted on drop).
    _reader_task: JoinHandle<()>,
    /// Held to keep the child process alive.
    _child: Child,
}

impl StdioTransport {
    fn spawn(config: &McpServerConfig) -> Result<Self> {
        let command = config.command.as_deref().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MCP server {} requires a command for stdio transport",
                config.name
            ))
        })?;
        let mut child = Command::new(command)
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            MoaError::ProviderError(format!(
                "failed to capture stdin for MCP server {}",
                config.name
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            MoaError::ProviderError(format!(
                "failed to capture stdout for MCP server {}",
                config.name
            ))
        })?;

        let pending: std::sync::Arc<PendingMap> =
            std::sync::Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();

        let reader_task = tokio::spawn(async move {
            run_reader(BufReader::new(stdout), pending_reader).await;
        });

        Ok(Self {
            writer: Mutex::new(stdin),
            pending,
            _reader_task: reader_task,
            _child: child,
        })
    }

    /// Write a notification (fire-and-forget, no response expected).
    async fn notify(&self, message: Value) -> Result<()> {
        let mut stdin = self.writer.lock().await;
        write_framed_message(&mut stdin, &message).await
        // lock released here — before any await that waits for a response
    }

    /// Send a request and wait for the matching response, fully concurrent.
    async fn request(&self, id: u64, message: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel::<Result<Value>>();

        // Register before writing so we never miss a fast response.
        self.pending.lock().await.insert(id, tx);

        // RAII guard: removes the pending entry if this future is dropped
        // (cancelled) before the oneshot fires.
        let pending_ref = self.pending.clone();
        let guard = PendingGuard { id, pending: Some(pending_ref) };

        // Write to stdin — mutex is held only for the duration of the write.
        {
            let mut stdin = self.writer.lock().await;
            if let Err(err) = write_framed_message(&mut stdin, &message).await {
                // Remove the pending entry we just inserted.
                drop(guard);
                // Clean up is handled by the guard's drop, but we already
                // consumed it — explicitly remove just in case guard didn't
                // fire yet (it hasn't been dropped yet; force it):
                self.pending.lock().await.remove(&id);
                return Err(err);
            }
        } // write mutex released here

        // Await the response via the oneshot — no lock held.
        let result = tokio::time::timeout(DEFAULT_MCP_TIMEOUT, rx).await;

        // Disarm the guard: the oneshot fired (or we're about to error out).
        guard.disarm();

        match result {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => Err(MoaError::StreamError(
                "MCP stdio reader task closed".to_string(),
            )),
            Err(_) => {
                // Timeout: clean up the now-stale pending entry.
                self.pending.lock().await.remove(&id);
                Err(MoaError::StreamError(format!(
                    "MCP stdio request timed out after {}s",
                    DEFAULT_MCP_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

/// RAII guard that removes a pending entry from the map when dropped (cancel
/// safety). Call `disarm()` before intentional completion to skip removal.
struct PendingGuard {
    id: u64,
    pending: Option<std::sync::Arc<PendingMap>>,
}

impl PendingGuard {
    fn disarm(mut self) {
        self.pending = None;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            // Best-effort synchronous removal; if the lock is contended we
            // spin briefly. In practice the map is never held for long.
            if let Ok(mut map) = pending.try_lock() {
                map.remove(&self.id);
            } else {
                // Fall back: spawn a tiny task to do the cleanup.
                let id = self.id;
                tokio::spawn(async move {
                    pending.lock().await.remove(&id);
                });
            }
        }
    }
}

/// Background task: reads framed JSON-RPC messages from stdout and routes
/// each response to the appropriate oneshot sender by id.
async fn run_reader(
    mut stdout: BufReader<ChildStdout>,
    pending: std::sync::Arc<PendingMap>,
) {
    loop {
        match read_framed_message(&mut stdout).await {
            Ok(value) => {
                // Responses carry an "id"; notifications do not.
                let id = match value.get("id").and_then(Value::as_u64) {
                    Some(id) => id,
                    None => continue, // server-side notification — ignore
                };
                let sender = pending.lock().await.remove(&id);
                if let Some(tx) = sender {
                    let _ = tx.send(Ok(value));
                }
            }
            Err(err) => {
                // EOF or framing error — drain all pending requests with an
                // error so callers don't wait until timeout.
                let mut map = pending.lock().await;
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(MoaError::StreamError(format!(
                        "MCP stdio process exited: {err}"
                    ))));
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP / SSE remote client (unchanged)
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

async fn write_framed_message(stdin: &mut ChildStdin, message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(&payload).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_framed_message(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).await?;
        if bytes == 0 {
            return Err(MoaError::StreamError(
                "MCP stdio stream closed while waiting for response".to_string(),
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().map_err(|error| {
                MoaError::StreamError(format!("invalid MCP Content-Length header: {error}"))
            })?);
        }
    }

    let content_length = content_length
        .ok_or_else(|| MoaError::StreamError("missing MCP Content-Length header".to_string()))?;
    let mut payload = vec![0_u8; content_length];
    stdout.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map_err(|error| MoaError::StreamError(format!("invalid MCP stdio payload: {error}")))
}

async fn read_sse_response(response: reqwest::Response) -> Result<Value> {
    let body = response
        .text()
        .await
        .map_err(|error| MoaError::StreamError(format!("failed reading MCP SSE body: {error}")))?;

    let mut current_event = String::new();
    let mut current_data = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            if !current_data.is_empty() && (current_event.is_empty() || current_event == "message")
            {
                return serde_json::from_str(&current_data.join("\n")).map_err(|error| {
                    MoaError::StreamError(format!("invalid MCP SSE payload: {error}"))
                });
            }
            current_event.clear();
            current_data.clear();
            continue;
        }

        if let Some(comment) = line.strip_prefix(':') {
            let _ = comment;
            continue;
        }

        if let Some(event) = line.strip_prefix("event:") {
            current_event = event.trim().to_string();
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            current_data.push(data.trim_start().to_string());
        }
    }

    if !current_data.is_empty() && (current_event.is_empty() || current_event == "message") {
        return serde_json::from_str(&current_data.join("\n"))
            .map_err(|error| MoaError::StreamError(format!("invalid MCP SSE payload: {error}")));
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
mod tests {
    use std::collections::HashMap;

    use moa_core::{McpServerConfig, McpTransportConfig};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{MCPClient, flatten_call_result};

    #[tokio::test]
    async fn flatten_tool_result_aggregates_text_items() {
        let output = flatten_call_result(json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" }
            ]
        }));
        assert_eq!(output.to_text(), "hello\n\nworld");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn stdio_client_lists_and_calls_tools() {
        let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mock_mcp_stdio_server.py");
        let client = MCPClient::connect(&McpServerConfig {
            name: "mock".to_string(),
            transport: McpTransportConfig::Stdio,
            command: Some("python3".to_string()),
            args: vec![server.display().to_string()],
            ..McpServerConfig::default()
        })
        .await
        .unwrap();

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let output = client
            .call_tool("echo", json!({ "text": "hello" }), HashMap::new())
            .await
            .unwrap();
        assert_eq!(output.to_text(), "hello");
    }

    #[tokio::test]
    async fn http_client_sends_headers_and_parses_jsonrpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for request_index in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0_u8; 4096];
                let bytes = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                if request_index == 2 {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer token")
                    );
                }
                let body = if request_index == 0 {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                } else if request_index == 1 {
                    r"{}"
                } else {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}]}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = MCPClient::connect(&McpServerConfig {
            name: "remote".to_string(),
            transport: McpTransportConfig::Http,
            url: Some(format!("http://{addr}")),
            ..McpServerConfig::default()
        })
        .await
        .unwrap();

        let output = client
            .call_tool(
                "ping",
                json!({}),
                HashMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
            )
            .await
            .unwrap();
        assert_eq!(output.to_text(), "pong");
    }
}
