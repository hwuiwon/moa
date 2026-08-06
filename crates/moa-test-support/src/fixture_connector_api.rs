//! Programmable in-process HTTP API for custom connector runtime tests.
//!
//! The fixture listens on an ephemeral IPv4 loopback port and writes HTTP/1.1
//! responses directly. Direct response framing lets callers exercise truncated
//! bodies and connection loss without relying on a production HTTP stack. The
//! request recorder retains structured metadata, redacts configured credential
//! headers before storage, and never logs request or response contents.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// One captured header value, with credential-bearing headers redacted at ingestion.
#[derive(Clone, PartialEq, Eq)]
pub enum FixtureCapturedHeaderValue {
    /// A non-sensitive header value retained for exact request assertions.
    Visible(String),
    /// A value deliberately discarded because its header name is sensitive.
    Redacted,
}

impl fmt::Debug for FixtureCapturedHeaderValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Visible(value) => formatter.debug_tuple("Visible").field(value).finish(),
            Self::Redacted => formatter.write_str("Redacted"),
        }
    }
}

/// Structured metadata for one request received by the fixture.
#[derive(Clone, PartialEq)]
pub struct FixtureConnectorRequest {
    /// One-based arrival order assigned under the fixture's observation lock.
    pub arrival_order: u64,
    /// One-based order of the logical upstream effect represented by this request.
    pub logical_effect_order: u64,
    /// Whether an earlier transport already applied the same idempotency-keyed effect.
    pub is_replay: bool,
    /// Exact HTTP method from the request line.
    pub method: String,
    /// Exact request target, including any encoded query string.
    pub target: String,
    /// Lowercase header names and their ordered, redacted-or-visible values.
    pub headers: BTreeMap<String, Vec<FixtureCapturedHeaderValue>>,
    /// Number of request-body bytes received.
    pub body_bytes: usize,
    /// Lowercase SHA-256 digest of the complete request body.
    pub body_sha256: String,
    /// Parsed JSON request body, when the complete body is valid JSON.
    pub json_body: Option<Value>,
}

impl fmt::Debug for FixtureConnectorRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FixtureConnectorRequest")
            .field("arrival_order", &self.arrival_order)
            .field("logical_effect_order", &self.logical_effect_order)
            .field("is_replay", &self.is_replay)
            .field("method", &self.method)
            .field("target", &self.target)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body_bytes)
            .field("body_sha256", &self.body_sha256)
            .field("json_body", &self.json_body.as_ref().map(|_| "<json>"))
            .finish()
    }
}

/// One logical upstream connector effect after idempotency-key deduplication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureConnectorEffect {
    /// One-based order among unique logical effects.
    pub arrival_order: u64,
    /// Caller-supplied upstream idempotency key, when present.
    pub idempotency_key: Option<String>,
    /// Exact HTTP method of the first transport applying this effect.
    pub method: String,
    /// Exact request target of the first transport applying this effect.
    pub target: String,
    /// Lowercase SHA-256 digest of the first request body applying this effect.
    pub body_sha256: String,
}

/// Body framing used by one scripted fixture response.
#[derive(Clone, Debug, PartialEq)]
pub enum FixtureConnectorBody {
    /// A JSON value serialized immediately before the response is written.
    Json(Value),
    /// Exact response bytes.
    Bytes(Vec<u8>),
    /// A repeated byte streamed with HTTP chunked transfer encoding.
    RepeatedChunked {
        /// Byte written into every payload position.
        byte: u8,
        /// Total number of payload bytes advertised by the script.
        total_bytes: usize,
        /// Maximum payload bytes in each HTTP chunk.
        chunk_bytes: usize,
    },
}

/// Deliberate connection termination point for a scripted response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureConnectorClose {
    /// Write the complete response and valid body terminator.
    #[default]
    Complete,
    /// Close without writing response headers.
    BeforeHeaders,
    /// Write headers and then close without any body bytes.
    AfterHeaders,
    /// Close after at most this many payload bytes without completing the body.
    AfterBodyBytes(usize),
}

/// One response in the fixture's ordered, final-item-repeating script.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureConnectorResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: FixtureConnectorBody,
    delay_before_headers: Duration,
    delay_between_chunks: Duration,
    close: FixtureConnectorClose,
}

impl FixtureConnectorResponse {
    /// Creates a successful `application/json` response.
    #[must_use]
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: FixtureConnectorBody::Json(body),
            delay_before_headers: Duration::ZERO,
            delay_between_chunks: Duration::ZERO,
            close: FixtureConnectorClose::Complete,
        }
    }

    /// Sets the HTTP response status.
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// Replaces the response content type.
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        self.headers
            .push(("content-type".to_string(), content_type.into()));
        self
    }

    /// Adds a response header. Script validation rejects unsafe framing names and CR/LF.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Delays response headers for the supplied duration.
    #[must_use]
    pub fn with_delay_before_headers(mut self, delay: Duration) -> Self {
        self.delay_before_headers = delay;
        self
    }

    /// Delays each chunk after the first for streamed-response timeout tests.
    #[must_use]
    pub fn with_delay_between_chunks(mut self, delay: Duration) -> Self {
        self.delay_between_chunks = delay;
        self
    }

    /// Selects a deliberate connection-close point.
    #[must_use]
    pub fn with_connection_close(mut self, close: FixtureConnectorClose) -> Self {
        self.close = close;
        self
    }

    /// Creates one redirect response without following or validating its location.
    #[must_use]
    pub fn redirect(status: u16, location: impl Into<String>) -> Self {
        Self::json(Value::Null)
            .with_status(status)
            .with_header("location", location)
    }

    /// Creates an exact byte response with the supplied content type.
    #[must_use]
    pub fn bytes(content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_string(), content_type.into())],
            body: FixtureConnectorBody::Bytes(body),
            delay_before_headers: Duration::ZERO,
            delay_between_chunks: Duration::ZERO,
            close: FixtureConnectorClose::Complete,
        }
    }

    /// Creates a chunked body large enough to exercise streamed response caps.
    #[must_use]
    pub fn chunked_oversized(total_bytes: usize, chunk_bytes: usize) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: FixtureConnectorBody::RepeatedChunked {
                byte: b' ',
                total_bytes,
                chunk_bytes,
            },
            delay_before_headers: Duration::ZERO,
            delay_between_chunks: Duration::ZERO,
            close: FixtureConnectorClose::Complete,
        }
    }
}

/// Configuration used to start a programmable connector API fixture.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureConnectorScript {
    /// Ordered responses; the final response repeats after the script is exhausted.
    pub responses: Vec<FixtureConnectorResponse>,
    /// Additional case-insensitive request header names whose values are discarded.
    pub sensitive_header_names: Vec<String>,
    /// Maximum request body accepted by the fixture itself.
    pub max_request_body_bytes: usize,
}

impl FixtureConnectorScript {
    /// Creates a response script with default credential-header redaction.
    #[must_use]
    pub fn new(responses: Vec<FixtureConnectorResponse>) -> Self {
        Self {
            responses,
            sensitive_header_names: Vec::new(),
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
        }
    }

    /// Adds a request header name whose values must never enter captured metadata.
    #[must_use]
    pub fn with_sensitive_header(mut self, name: impl Into<String>) -> Self {
        self.sensitive_header_names.push(name.into());
        self
    }

    /// Sets the fixture-side request-body limit.
    #[must_use]
    pub fn with_max_request_body_bytes(mut self, limit: usize) -> Self {
        self.max_request_body_bytes = limit;
        self
    }
}

/// Concurrent-safe controller for observations and response programming.
#[derive(Clone)]
pub struct FixtureConnectorController {
    state: Arc<FixtureConnectorState>,
}

impl FixtureConnectorController {
    /// Returns captured requests in deterministic arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<FixtureConnectorRequest> {
        lock_unpoisoned(&self.state.mutable).requests.clone()
    }

    /// Returns unique logical effects after upstream idempotency-key deduplication.
    #[must_use]
    pub fn effects(&self) -> Vec<FixtureConnectorEffect> {
        lock_unpoisoned(&self.state.mutable).effects.clone()
    }

    /// Returns the exact number of HTTP transport arrivals.
    #[must_use]
    pub fn request_count(&self) -> usize {
        lock_unpoisoned(&self.state.mutable).requests.len()
    }

    /// Returns the exact number of logical upstream effects.
    #[must_use]
    pub fn effect_count(&self) -> usize {
        lock_unpoisoned(&self.state.mutable).effects.len()
    }

    /// Waits for at least `count` requests and returns the full ordered observation set.
    pub async fn wait_for_requests(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureConnectorRequest>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.state.request_notify.notified();
            let requests = self.requests();
            if requests.len() >= count {
                return Ok(requests);
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .with_context(|| {
                    format!(
                        "fixture connector received {} of {count} requests within {timeout:?}",
                        requests.len()
                    )
                })?;
        }
    }

    /// Replaces the response program and restarts its cursor at the first response.
    pub fn replace_responses(&self, responses: Vec<FixtureConnectorResponse>) -> Result<()> {
        validate_responses(&responses)?;
        let mut mutable = lock_unpoisoned(&self.state.mutable);
        mutable.responses = responses;
        mutable.response_cursor = 0;
        Ok(())
    }

    /// Clears observations and restarts the current response script.
    pub fn reset(&self) {
        let mut mutable = lock_unpoisoned(&self.state.mutable);
        mutable.requests.clear();
        mutable.effects.clear();
        mutable.effect_by_idempotency_key.clear();
        mutable.next_arrival_order = 0;
        mutable.next_effect_order = 0;
        mutable.response_cursor = 0;
    }
}

/// One running programmable connector API and its graceful-shutdown handle.
pub struct FixtureConnectorApi {
    controller: FixtureConnectorController,
    origin: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FixtureConnectorApi {
    /// Starts a connector API on one ephemeral IPv4 loopback port.
    pub async fn start(script: FixtureConnectorScript) -> Result<Self> {
        let state = Arc::new(FixtureConnectorState::new(script)?);
        let controller = FixtureConnectorController {
            state: Arc::clone(&state),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind fixture connector API listener")?;
        let address = listener
            .local_addr()
            .context("read fixture connector API listener address")?;
        let origin = format!("http://{address}");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(serve(listener, state, shutdown_rx));
        Ok(Self {
            controller,
            origin,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Returns the fixed loopback origin for connector connection configuration.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Returns the observation and programming controller.
    pub fn controller(&self) -> &FixtureConnectorController {
        &self.controller
    }

    /// Stops the listener and aborts in-flight response tasks.
    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for FixtureConnectorApi {
    fn drop(&mut self) {
        self.stop();
    }
}

struct FixtureConnectorState {
    mutable: StdMutex<FixtureConnectorMutable>,
    sensitive_header_names: BTreeSet<String>,
    max_request_body_bytes: usize,
    request_notify: Notify,
}

impl FixtureConnectorState {
    fn new(script: FixtureConnectorScript) -> Result<Self> {
        validate_responses(&script.responses)?;
        if script.max_request_body_bytes == 0 {
            bail!("fixture connector request-body limit must be greater than zero");
        }
        let mut sensitive_header_names = default_sensitive_headers();
        for name in script.sensitive_header_names {
            let normalized = normalize_header_name(&name)?;
            sensitive_header_names.insert(normalized);
        }
        Ok(Self {
            mutable: StdMutex::new(FixtureConnectorMutable {
                responses: script.responses,
                response_cursor: 0,
                requests: Vec::new(),
                effects: Vec::new(),
                effect_by_idempotency_key: HashMap::new(),
                next_arrival_order: 0,
                next_effect_order: 0,
            }),
            sensitive_header_names,
            max_request_body_bytes: script.max_request_body_bytes,
            request_notify: Notify::new(),
        })
    }

    fn record_and_select(&self, parsed: ParsedRequest) -> FixtureConnectorResponse {
        let mut mutable = lock_unpoisoned(&self.mutable);
        mutable.next_arrival_order += 1;
        let arrival_order = mutable.next_arrival_order;
        let idempotency_key = parsed
            .headers
            .iter()
            .find_map(|(name, value)| (name == "idempotency-key").then(|| value.clone()));
        let body_sha256 = sha256_hex(&parsed.body);
        let existing_effect_order = idempotency_key
            .as_ref()
            .and_then(|key| mutable.effect_by_idempotency_key.get(key).copied());
        let (logical_effect_order, is_replay) = if let Some(effect_order) = existing_effect_order {
            (effect_order, true)
        } else {
            mutable.next_effect_order += 1;
            let effect_order = mutable.next_effect_order;
            mutable.effects.push(FixtureConnectorEffect {
                arrival_order: effect_order,
                idempotency_key: idempotency_key.clone(),
                method: parsed.method.clone(),
                target: parsed.target.clone(),
                body_sha256: body_sha256.clone(),
            });
            if let Some(key) = &idempotency_key {
                mutable
                    .effect_by_idempotency_key
                    .insert(key.clone(), effect_order);
            }
            (effect_order, false)
        };
        let headers = parsed.headers.into_iter().fold(
            BTreeMap::<String, Vec<_>>::new(),
            |mut captured, (name, value)| {
                let value = if self.sensitive_header_names.contains(&name) {
                    FixtureCapturedHeaderValue::Redacted
                } else {
                    FixtureCapturedHeaderValue::Visible(value)
                };
                captured.entry(name).or_default().push(value);
                captured
            },
        );
        let json_body = serde_json::from_slice(&parsed.body).ok();
        mutable.requests.push(FixtureConnectorRequest {
            arrival_order,
            logical_effect_order,
            is_replay,
            method: parsed.method,
            target: parsed.target,
            headers,
            body_bytes: parsed.body.len(),
            body_sha256,
            json_body,
        });
        let index = mutable
            .response_cursor
            .min(mutable.responses.len().saturating_sub(1));
        let response = mutable.responses[index].clone();
        mutable.response_cursor = mutable.response_cursor.saturating_add(1);
        drop(mutable);
        self.request_notify.notify_waiters();
        response
    }
}

struct FixtureConnectorMutable {
    responses: Vec<FixtureConnectorResponse>,
    response_cursor: usize,
    requests: Vec<FixtureConnectorRequest>,
    effects: Vec<FixtureConnectorEffect>,
    effect_by_idempotency_key: HashMap<String, u64>,
    next_arrival_order: u64,
    next_effect_order: u64,
}

struct ParsedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn serve(
    listener: TcpListener,
    state: Arc<FixtureConnectorState>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let state = Arc::clone(&state);
                connections.spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
}

async fn handle_connection(mut stream: TcpStream, state: Arc<FixtureConnectorState>) -> Result<()> {
    let parsed = read_request(&mut stream, state.max_request_body_bytes).await?;
    let response = state.record_and_select(parsed);
    write_response(&mut stream, &response).await
}

async fn read_request(stream: &mut TcpStream, max_body_bytes: usize) -> Result<ParsedRequest> {
    let mut received = Vec::new();
    let head_end = loop {
        if let Some(index) = find_bytes(&received, b"\r\n\r\n") {
            break index + 4;
        }
        if received.len() >= MAX_REQUEST_HEAD_BYTES {
            bail!("fixture connector request headers exceed the fixture limit");
        }
        let mut buffer = [0_u8; 4096];
        let count = stream
            .read(&mut buffer)
            .await
            .context("read fixture connector request headers")?;
        if count == 0 {
            bail!("fixture connector connection closed before request headers");
        }
        received.extend_from_slice(&buffer[..count]);
    };
    let head = std::str::from_utf8(&received[..head_end - 4])
        .context("fixture connector request headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .context("fixture connector request line is missing")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .context("fixture connector request method is missing")?
        .to_string();
    let target = request_parts
        .next()
        .context("fixture connector request target is missing")?
        .to_string();
    let version = request_parts
        .next()
        .context("fixture connector request version is missing")?;
    if request_parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        bail!("fixture connector request line is invalid");
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .context("fixture connector request header is malformed")?;
        let name = normalize_header_name(name)?;
        let value = value.trim().to_string();
        if name == "content-length" {
            if content_length.is_some() {
                bail!("fixture connector request has duplicate content-length headers");
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .context("fixture connector content-length is invalid")?,
            );
        }
        headers.push((name, value));
    }
    let content_length = content_length.unwrap_or_default();
    if content_length > max_body_bytes {
        bail!("fixture connector request body exceeds the fixture limit");
    }
    let required = head_end
        .checked_add(content_length)
        .context("fixture connector request length overflow")?;
    while received.len() < required {
        let remaining = required - received.len();
        let mut buffer = vec![0_u8; remaining.min(8192)];
        let count = stream
            .read(&mut buffer)
            .await
            .context("read fixture connector request body")?;
        if count == 0 {
            bail!("fixture connector connection closed before request body");
        }
        received.extend_from_slice(&buffer[..count]);
    }
    Ok(ParsedRequest {
        method,
        target,
        headers,
        body: received[head_end..required].to_vec(),
    })
}

async fn write_response(stream: &mut TcpStream, response: &FixtureConnectorResponse) -> Result<()> {
    if !response.delay_before_headers.is_zero() {
        tokio::time::sleep(response.delay_before_headers).await;
    }
    if response.close == FixtureConnectorClose::BeforeHeaders {
        return Ok(());
    }
    let body = match &response.body {
        FixtureConnectorBody::Json(value) => {
            serde_json::to_vec(value).context("serialize fixture connector JSON response")?
        }
        FixtureConnectorBody::Bytes(bytes) => bytes.clone(),
        FixtureConnectorBody::RepeatedChunked { .. } => Vec::new(),
    };
    let chunked = matches!(response.body, FixtureConnectorBody::RepeatedChunked { .. });
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nconnection: close\r\n",
        response.status,
        reason_phrase(response.status)
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if chunked {
        head.push_str("transfer-encoding: chunked\r\n");
    } else {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .context("write fixture connector response headers")?;
    if response.close == FixtureConnectorClose::AfterHeaders {
        return Ok(());
    }
    match &response.body {
        FixtureConnectorBody::Json(_) | FixtureConnectorBody::Bytes(_) => {
            let limit = payload_limit(response.close, body.len());
            stream
                .write_all(&body[..limit])
                .await
                .context("write fixture connector response body")?;
        }
        FixtureConnectorBody::RepeatedChunked {
            byte,
            total_bytes,
            chunk_bytes,
        } => {
            write_chunked_body(stream, response, *byte, *total_bytes, *chunk_bytes).await?;
        }
    }
    stream
        .shutdown()
        .await
        .context("close fixture connector response")
}

async fn write_chunked_body(
    stream: &mut TcpStream,
    response: &FixtureConnectorResponse,
    byte: u8,
    total_bytes: usize,
    chunk_bytes: usize,
) -> Result<()> {
    let allowed = payload_limit(response.close, total_bytes);
    let mut written = 0;
    while written < allowed {
        if written > 0 && !response.delay_between_chunks.is_zero() {
            tokio::time::sleep(response.delay_between_chunks).await;
        }
        let count = (allowed - written).min(chunk_bytes);
        stream
            .write_all(format!("{count:x}\r\n").as_bytes())
            .await
            .context("write fixture connector chunk header")?;
        stream
            .write_all(&vec![byte; count])
            .await
            .context("write fixture connector chunk payload")?;
        stream
            .write_all(b"\r\n")
            .await
            .context("write fixture connector chunk delimiter")?;
        written += count;
    }
    if response.close == FixtureConnectorClose::Complete {
        stream
            .write_all(b"0\r\n\r\n")
            .await
            .context("write fixture connector chunk terminator")?;
    }
    Ok(())
}

fn payload_limit(close: FixtureConnectorClose, total: usize) -> usize {
    match close {
        FixtureConnectorClose::AfterBodyBytes(limit) => limit.min(total),
        FixtureConnectorClose::Complete
        | FixtureConnectorClose::BeforeHeaders
        | FixtureConnectorClose::AfterHeaders => total,
    }
}

fn validate_responses(responses: &[FixtureConnectorResponse]) -> Result<()> {
    if responses.is_empty() {
        bail!("fixture connector needs at least one response");
    }
    for response in responses {
        if !(200..=599).contains(&response.status) {
            bail!("fixture connector response status must be in 200..=599");
        }
        if let FixtureConnectorBody::RepeatedChunked {
            chunk_bytes,
            total_bytes: _,
            byte: _,
        } = &response.body
            && *chunk_bytes == 0
        {
            bail!("fixture connector response chunk size must be greater than zero");
        }
        let mut names = BTreeSet::new();
        for (name, value) in &response.headers {
            let name = normalize_header_name(name)?;
            if matches!(
                name.as_str(),
                "connection" | "content-length" | "transfer-encoding"
            ) {
                bail!("fixture connector response cannot override framing headers");
            }
            if !names.insert(name) {
                bail!("fixture connector response header names must be unique");
            }
            if value.contains(['\r', '\n']) {
                bail!("fixture connector response header value contains CR or LF");
            }
        }
    }
    Ok(())
}

fn normalize_header_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
    {
        bail!("fixture connector header name is invalid");
    }
    Ok(name.to_ascii_lowercase())
}

fn default_sensitive_headers() -> BTreeSet<String> {
    [
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Fixture Response",
    }
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: the fixture redacts credential headers before requests enter captured state.
    #[tokio::test]
    async fn request_capture_discards_sensitive_header_values() {
        let fixture = FixtureConnectorApi::start(
            FixtureConnectorScript::new(vec![FixtureConnectorResponse::json(
                serde_json::json!({"ok": true}),
            )])
            .with_sensitive_header("x-fixture-api-key"),
        )
        .await
        .expect("fixture connector should bind an ephemeral port");
        let response = reqwest::Client::new()
            .post(format!("{}/items?cursor=next", fixture.origin()))
            .header("authorization", "Bearer fixture-secret")
            .header("x-fixture-api-key", "custom-fixture-secret")
            .header("x-request-id", "stable-id")
            .json(&serde_json::json!({"name": "one"}))
            .send()
            .await
            .expect("fixture connector request should complete");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let requests = fixture.controller().requests();
        assert_eq!(requests.len(), 1);
        let captured = &requests[0];
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.target, "/items?cursor=next");
        assert_eq!(
            captured.headers.get("authorization"),
            Some(&vec![FixtureCapturedHeaderValue::Redacted])
        );
        assert_eq!(
            captured.headers.get("x-fixture-api-key"),
            Some(&vec![FixtureCapturedHeaderValue::Redacted])
        );
        assert_eq!(
            captured.headers.get("x-request-id"),
            Some(&vec![FixtureCapturedHeaderValue::Visible(
                "stable-id".to_string()
            )])
        );
        assert_eq!(captured.json_body, Some(serde_json::json!({"name": "one"})));
        assert!(!format!("{captured:?}").contains("fixture-secret"));
    }

    // Pins: scripted responses advance once per request and repeat the final item.
    #[tokio::test]
    async fn response_script_repeats_final_outcome() {
        let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
            FixtureConnectorResponse::json(serde_json::json!({"attempt": 1})).with_status(503),
            FixtureConnectorResponse::json(serde_json::json!({"attempt": 2})),
        ]))
        .await
        .expect("fixture connector should bind an ephemeral port");
        let client = reqwest::Client::new();
        let mut observed = Vec::new();
        for _ in 0..3 {
            let response = client
                .get(fixture.origin())
                .send()
                .await
                .expect("scripted fixture response should complete");
            observed.push((
                response.status().as_u16(),
                response
                    .json::<Value>()
                    .await
                    .expect("scripted fixture response should contain JSON"),
            ));
        }
        assert_eq!(
            observed,
            vec![
                (503, serde_json::json!({"attempt": 1})),
                (200, serde_json::json!({"attempt": 2})),
                (200, serde_json::json!({"attempt": 2})),
            ]
        );
        assert_eq!(
            fixture
                .controller()
                .requests()
                .into_iter()
                .map(|request| request.arrival_order)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    // Pins: repeated transport with one upstream idempotency key is one logical effect.
    #[tokio::test]
    async fn idempotency_key_retry_records_two_requests_but_one_logical_effect() {
        let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
            FixtureConnectorResponse::json(serde_json::json!({"attempt": 1})),
            FixtureConnectorResponse::json(serde_json::json!({"attempt": 2})),
        ]))
        .await
        .expect("fixture connector should bind an ephemeral port");
        let client = reqwest::Client::new();
        for _ in 0..2 {
            client
                .post(fixture.origin())
                .header("idempotency-key", "stable-effect")
                .json(&serde_json::json!({"record": "same"}))
                .send()
                .await
                .expect("idempotent fixture request should complete");
        }

        assert_eq!(fixture.controller().requests().len(), 2);
        assert_eq!(fixture.controller().effects().len(), 1);
        assert!(!fixture.controller().requests()[0].is_replay);
        assert!(fixture.controller().requests()[1].is_replay);
    }

    // Pins: a scripted mid-body close produces a transport failure, not valid JSON.
    #[tokio::test]
    async fn connection_close_truncates_advertised_body() {
        let fixture = FixtureConnectorApi::start(FixtureConnectorScript::new(vec![
            FixtureConnectorResponse::json(serde_json::json!({"complete": true}))
                .with_connection_close(FixtureConnectorClose::AfterBodyBytes(4)),
        ]))
        .await
        .expect("fixture connector should bind an ephemeral port");
        let response = reqwest::get(fixture.origin())
            .await
            .expect("fixture should write response headers before closing");
        let error = response
            .json::<Value>()
            .await
            .expect_err("truncated fixture body should fail response decoding");
        assert!(error.is_body() || error.is_decode());
    }
}
