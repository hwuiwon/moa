//! E2B-backed hand provider for microVM execution.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_core::{
    HandHandle, HandProvider, HandSpec, HandStatus, MoaConfig, MoaError, Result, SandboxTier,
    ToolFailureClass, ToolOutput, classify_tool_error,
};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::tools::edit_output::{
    ExistingFileContent, build_file_write_output, build_text_edit_output,
};
use crate::tools::str_replace::plan_str_replace;

const DEFAULT_E2B_API_URL: &str = "https://api.e2b.dev";
const DEFAULT_E2B_DOMAIN: &str = "e2b.app";
const DEFAULT_E2B_TEMPLATE: &str = "base";
const DEFAULT_ENVD_PORT: u16 = 49983;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_PROTOCOL_VERSION: &str = "1";
const CONNECT_COMPRESSED_FLAG: u8 = 0b0000_0001;
const CONNECT_END_STREAM_FLAG: u8 = 0b0000_0010;

#[derive(Debug, Clone)]
struct ConnectedSandbox {
    sandbox_domain: String,
    envd_access_token: String,
    _envd_version: String,
}

/// E2B cloud hand provider for microVM-backed execution.
pub struct E2BHandProvider {
    client: reqwest::Client,
    api_url: String,
    sandbox_domain: String,
    default_template: String,
    sandbox_base_url_override: Option<String>,
    sandboxes: RwLock<HashMap<String, ConnectedSandbox>>,
}

impl E2BHandProvider {
    /// Creates a new E2B provider from an API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_api_url(
            api_key,
            DEFAULT_E2B_API_URL,
            DEFAULT_E2B_DOMAIN,
            DEFAULT_E2B_TEMPLATE,
        )
    }

    /// Creates an E2B provider from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let hands = config
            .cloud
            .hands
            .as_ref()
            .ok_or_else(|| MoaError::ConfigError("missing [cloud.hands] config".to_string()))?;
        let api_key_env = hands
            .e2b_api_key_env
            .as_deref()
            .ok_or_else(|| MoaError::ConfigError("missing E2B API key env".to_string()))?;
        let api_key = std::env::var(api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.to_string()))?;
        Self::with_api_url(
            api_key,
            hands.e2b_api_url.as_deref().unwrap_or(DEFAULT_E2B_API_URL),
            hands.e2b_domain.as_deref().unwrap_or(DEFAULT_E2B_DOMAIN),
            hands
                .e2b_template
                .as_deref()
                .unwrap_or(DEFAULT_E2B_TEMPLATE),
        )
    }

    /// Creates a provider with explicit API URL, domain, and template overrides.
    pub fn with_api_url(
        api_key: impl Into<String>,
        api_url: impl Into<String>,
        sandbox_domain: impl Into<String>,
        default_template: impl Into<String>,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_COMMAND_TIMEOUT)
            .default_headers(default_headers(&api_key)?)
            .build()
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to build E2B client: {error}"))
            })?;
        Ok(Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            sandbox_domain: sandbox_domain.into(),
            default_template: default_template.into(),
            sandbox_base_url_override: None,
            sandboxes: RwLock::new(HashMap::new()),
        })
    }

    /// Overrides the computed envd sandbox base URL. Intended for tests and local proxies.
    pub fn with_sandbox_base_url(mut self, sandbox_base_url: impl Into<String>) -> Self {
        self.sandbox_base_url_override =
            Some(sandbox_base_url.into().trim_end_matches('/').to_string());
        self
    }

    async fn create_sandbox(&self, spec: &HandSpec) -> Result<String> {
        let response = self
            .client
            .post(format!("{}/sandboxes", self.api_url))
            .json(&json!({
                "templateID": spec.image.clone().unwrap_or_else(|| self.default_template.clone()),
                "envVars": spec.env,
                "timeout": spec.idle_timeout.as_secs().max(60),
                "secure": true,
                "allow_internet_access": true,
                "autoPause": true,
                "autoResume": { "enabled": true },
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response).await?;
        let sandbox_id = value
            .get("sandboxID")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MoaError::ProviderError("E2B create sandbox response missing sandboxID".to_string())
            })?
            .to_string();
        self.sandboxes.write().await.insert(
            sandbox_id.clone(),
            ConnectedSandbox {
                sandbox_domain: value
                    .get("domain")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.sandbox_domain)
                    .to_string(),
                envd_access_token: required_string_field(&value, "envdAccessToken")?.to_string(),
                _envd_version: required_string_field(&value, "envdVersion")?.to_string(),
            },
        );
        Ok(sandbox_id)
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> Result<ConnectedSandbox> {
        let response = self
            .client
            .post(format!("{}/sandboxes/{sandbox_id}/connect", self.api_url))
            .json(&json!({
                "timeout": DEFAULT_COMMAND_TIMEOUT.as_secs(),
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to connect E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response).await?;
        let sandbox = ConnectedSandbox {
            sandbox_domain: value
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or(&self.sandbox_domain)
                .to_string(),
            envd_access_token: required_string_field(&value, "envdAccessToken")?.to_string(),
            _envd_version: required_string_field(&value, "envdVersion")?.to_string(),
        };
        self.sandboxes
            .write()
            .await
            .insert(sandbox_id.to_string(), sandbox.clone());
        Ok(sandbox)
    }

    async fn connected_sandbox(&self, sandbox_id: &str) -> Result<ConnectedSandbox> {
        if let Some(sandbox) = self.sandboxes.read().await.get(sandbox_id).cloned() {
            return Ok(sandbox);
        }
        self.connect_sandbox(sandbox_id).await
    }

    fn envd_url(&self, sandbox_id: &str, sandbox: &ConnectedSandbox) -> String {
        if let Some(base_url) = &self.sandbox_base_url_override {
            return base_url.clone();
        }
        format!(
            "https://{}-{}.{}",
            DEFAULT_ENVD_PORT, sandbox_id, sandbox.sandbox_domain
        )
    }

    async fn execute_bash(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        cmd: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = format!(
            "{}/process.Process/Start",
            self.envd_url(sandbox_id, sandbox)
        );
        let response = self
            .client
            .post(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .header(CONTENT_TYPE, "application/connect+json")
            .header("Connect-Protocol-Version", CONNECT_PROTOCOL_VERSION)
            .body(encode_connect_request(&json!({
                "process": {
                    "cmd": "/bin/bash",
                    "args": ["-l", "-c", cmd],
                    "envs": {},
                },
                "stdin": false,
            }))?)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to start E2B command: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        let body = response.bytes().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to read E2B command body: {error}"))
        })?;
        parse_e2b_connect_stream(&body, started_at.elapsed())
    }

    async fn read_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", self.envd_url(sandbox_id, sandbox)),
            &[("path", path)],
        )?;
        let response = self
            .client
            .get(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to read E2B file: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        Ok(ToolOutput::text(
            response.text().await.map_err(|error| {
                MoaError::ProviderError(format!("failed to decode E2B file response: {error}"))
            })?,
            started_at.elapsed(),
        ))
    }

    async fn write_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &str,
    ) -> Result<ToolOutput> {
        let existing = match self.read_file(sandbox_id, sandbox, path).await {
            Ok(output) => ExistingFileContent::Text(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => ExistingFileContent::Missing,
            Err(error) => return Err(error),
        };
        let duration = self.upload_file(sandbox_id, sandbox, path, content).await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &str,
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", self.envd_url(sandbox_id, sandbox)),
            &[("path", path)],
        )?;
        let response = self
            .client
            .post(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(content.to_string())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to write E2B file: {error}"))
            })?;
        let _ = expect_success_json(response).await?;
        Ok(started_at.elapsed())
    }

    async fn str_replace_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        input: &str,
    ) -> Result<ToolOutput> {
        let existing_content = match self.read_file(sandbox_id, sandbox, path).await {
            Ok(output) => Some(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => None,
            Err(error) => return Err(error),
        };
        let planned = plan_str_replace(input, existing_content.as_deref(), path, 4)?;
        let duration = self
            .upload_file(sandbox_id, sandbox, path, &planned.updated_content)
            .await?;
        Ok(build_text_edit_output(
            path,
            existing_content.as_deref().unwrap_or_default(),
            &planned.updated_content,
            duration,
        ))
    }
}

#[async_trait]
impl HandProvider for E2BHandProvider {
    fn provider_name(&self) -> &str {
        "e2b"
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        if !matches!(spec.sandbox_tier, SandboxTier::MicroVM) {
            return Err(MoaError::Unsupported(
                "E2B provider is reserved for microvm sandboxes".to_string(),
            ));
        }
        let sandbox_id = self.create_sandbox(&spec).await?;
        Ok(HandHandle::e2b(sandbox_id))
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let sandbox_id = match handle {
            HandHandle::E2B { sandbox_id } => sandbox_id.as_str(),
            _ => {
                return Err(MoaError::Unsupported(
                    "non-E2B hand handle passed to E2BHandProvider".to_string(),
                ));
            }
        };
        let sandbox = self.connected_sandbox(sandbox_id).await?;
        let payload: Value = serde_json::from_str(input)?;
        match tool {
            "bash" => {
                self.execute_bash(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "cmd")?,
                )
                .await
            }
            "file_read" => {
                self.read_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                )
                .await
            }
            "str_replace" => {
                self.str_replace_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    input,
                )
                .await
            }
            "file_write" => {
                self.write_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    required_string_field(&payload, "content")?,
                )
                .await
            }
            "file_search" => {
                let pattern = shell_escape(required_string_field(&payload, "pattern")?);
                self.execute_bash(
                    sandbox_id,
                    &sandbox,
                    &format!("find / -name {pattern} -print 2>/dev/null || true"),
                )
                .await
            }
            other => Err(MoaError::ToolError(format!(
                "unsupported E2B tool: {other}"
            ))),
        }
    }

    async fn classify_error(
        &self,
        handle: &HandHandle,
        error: &MoaError,
        consecutive_timeouts: u32,
    ) -> ToolFailureClass {
        let status = self.status(handle).await.ok();
        self::classify_error(error, status, consecutive_timeouts)
    }

    async fn health_check(&self, handle: &HandHandle) -> Result<bool> {
        Ok(matches!(
            self.status(handle).await?,
            HandStatus::Running | HandStatus::Paused | HandStatus::Provisioning
        ))
    }

    async fn status(&self, handle: &HandHandle) -> Result<HandStatus> {
        let sandbox_id = match handle {
            HandHandle::E2B { sandbox_id } => sandbox_id.as_str(),
            _ => {
                return Err(MoaError::Unsupported(
                    "non-E2B hand handle passed to E2BHandProvider".to_string(),
                ));
            }
        };
        let response = self
            .client
            .get(format!("{}/sandboxes/{sandbox_id}", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to inspect E2B sandbox: {error}"))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(HandStatus::Destroyed);
        }
        let value = expect_success_json(response).await?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_ascii_lowercase();
        Ok(match state.as_str() {
            "running" | "started" => HandStatus::Running,
            "paused" | "stopped" => HandStatus::Stopped,
            "provisioning" | "starting" => HandStatus::Provisioning,
            "ended" | "deleted" => HandStatus::Destroyed,
            "error" => HandStatus::Failed,
            _ => HandStatus::Running,
        })
    }

    async fn pause(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = match handle {
            HandHandle::E2B { sandbox_id } => sandbox_id.as_str(),
            _ => {
                return Err(MoaError::Unsupported(
                    "non-E2B hand handle passed to E2BHandProvider".to_string(),
                ));
            }
        };
        let response = self
            .client
            .post(format!("{}/sandboxes/{sandbox_id}/pause", self.api_url))
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to pause E2B sandbox: {error}"))
            })?;
        expect_success(response).await?;
        Ok(())
    }

    async fn resume(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = match handle {
            HandHandle::E2B { sandbox_id } => sandbox_id.as_str(),
            _ => {
                return Err(MoaError::Unsupported(
                    "non-E2B hand handle passed to E2BHandProvider".to_string(),
                ));
            }
        };
        let _ = self.connect_sandbox(sandbox_id).await?;
        Ok(())
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = match handle {
            HandHandle::E2B { sandbox_id } => sandbox_id.as_str(),
            _ => {
                return Err(MoaError::Unsupported(
                    "non-E2B hand handle passed to E2BHandProvider".to_string(),
                ));
            }
        };
        let response = self
            .client
            .delete(format!("{}/sandboxes/{sandbox_id}", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to destroy E2B sandbox: {error}"))
            })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            self.sandboxes.write().await.remove(sandbox_id);
            return Ok(());
        }
        Err(http_error(response).await)
    }
}

fn default_headers(api_key: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-API-KEY",
        HeaderValue::from_str(api_key).map_err(|error| {
            MoaError::ValidationError(format!("invalid E2B API key header: {error}"))
        })?,
    );
    Ok(headers)
}

fn envd_headers(sandbox_id: &str, sandbox: &ConnectedSandbox) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "E2b-Sandbox-Id",
        HeaderValue::from_str(sandbox_id).map_err(|error| {
            MoaError::ValidationError(format!("invalid sandbox header: {error}"))
        })?,
    );
    headers.insert(
        "E2b-Sandbox-Port",
        HeaderValue::from_str(&DEFAULT_ENVD_PORT.to_string()).map_err(|error| {
            MoaError::ValidationError(format!("invalid sandbox port header: {error}"))
        })?,
    );
    headers.insert(
        "X-Access-Token",
        HeaderValue::from_str(&sandbox.envd_access_token).map_err(|error| {
            MoaError::ValidationError(format!("invalid E2B access token header: {error}"))
        })?,
    );
    Ok(headers)
}

fn build_url(base: &str, params: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|error| MoaError::ValidationError(format!("invalid E2B URL {base}: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

async fn expect_success_json(response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| MoaError::ProviderError(format!("invalid E2B JSON response: {error}")))
}

async fn expect_success(response: reqwest::Response) -> Result<()> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    Ok(())
}

async fn http_error(response: reqwest::Response) -> MoaError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    let message = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read response body".to_string());
    MoaError::HttpStatus {
        status,
        retry_after,
        message,
    }
}

fn encode_connect_request(value: &Value) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value)?;
    let length = u32::try_from(json.len())
        .map_err(|_| MoaError::ValidationError("E2B connect request too large".to_string()))?;
    let mut envelope = Vec::with_capacity(json.len() + 5);
    envelope.push(0);
    envelope.extend_from_slice(&length.to_be_bytes());
    envelope.extend_from_slice(&json);
    Ok(envelope)
}

fn parse_e2b_connect_stream(body: &[u8], duration: Duration) -> Result<ToolOutput> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0;
    let mut cursor = 0;
    while cursor + 5 <= body.len() {
        let flags = body[cursor];
        let length = u32::from_be_bytes([
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
            body[cursor + 4],
        ]) as usize;
        cursor += 5;
        if cursor + length > body.len() {
            return Err(MoaError::StreamError(
                "incomplete E2B connect envelope".to_string(),
            ));
        }
        let payload = &body[cursor..cursor + length];
        cursor += length;

        if (flags & CONNECT_COMPRESSED_FLAG) != 0 {
            return Err(MoaError::StreamError(
                "compressed E2B command envelopes are unsupported".to_string(),
            ));
        }
        if (flags & CONNECT_END_STREAM_FLAG) != 0 {
            if payload != b"{}" && !payload.is_empty() {
                let value: Value = serde_json::from_slice(payload).map_err(|error| {
                    MoaError::StreamError(format!("invalid E2B end-stream event: {error}"))
                })?;
                if let Some(message) = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                {
                    return Err(MoaError::ProviderError(format!(
                        "E2B command stream error: {message}"
                    )));
                }
            }
            continue;
        }

        let value: Value = serde_json::from_slice(payload).map_err(|error| {
            MoaError::StreamError(format!("invalid E2B command event: {error}"))
        })?;
        let Some(event) = value.get("event").and_then(Value::as_object) else {
            continue;
        };
        if let Some(data) = event.get("data").and_then(Value::as_object) {
            if let Some(text) = data.get("stdout").and_then(Value::as_str) {
                stdout.push_str(&decode_stream_chunk(text));
            }
            if let Some(text) = data.get("stderr").and_then(Value::as_str) {
                stderr.push_str(&decode_stream_chunk(text));
            }
            continue;
        }
        if let Some(end) = event.get("end").and_then(Value::as_object) {
            exit_code = extract_exit_code(end);
            if let Some(error) = end.get("error").and_then(Value::as_str) {
                stderr.push_str(error);
            }
        }
    }

    if cursor != body.len() {
        return Err(MoaError::StreamError(
            "trailing bytes in E2B connect stream".to_string(),
        ));
    }

    Ok(ToolOutput::from_process(
        stdout, stderr, exit_code, duration,
    ))
}

/// Classifies one E2B execution error for retry and re-provision decisions.
pub fn classify_error(
    error: &MoaError,
    status: Option<HandStatus>,
    consecutive_timeouts: u32,
) -> ToolFailureClass {
    if matches!(
        status,
        Some(HandStatus::Stopped | HandStatus::Destroyed | HandStatus::Failed)
    ) {
        return ToolFailureClass::ReProvision {
            reason: "E2B sandbox is no longer healthy".to_string(),
        };
    }

    match error {
        MoaError::HttpStatus { status: 404, .. } => ToolFailureClass::ReProvision {
            reason: "E2B sandbox no longer exists".to_string(),
        },
        MoaError::ProviderError(message)
        | MoaError::StreamError(message)
        | MoaError::ToolError(message) => {
            let message_lower = message.to_ascii_lowercase();
            if message_lower.contains("timeoutexception")
                && (message_lower.contains("unavailable") || message_lower.contains("unknown"))
            {
                return ToolFailureClass::ReProvision {
                    reason: "E2B sandbox became unavailable".to_string(),
                };
            }
            if message_lower.contains("deadline_exceeded") {
                return ToolFailureClass::Retryable {
                    reason: message.clone(),
                    backoff_hint: Duration::ZERO,
                };
            }
            classify_tool_error(error, consecutive_timeouts)
        }
        _ => classify_tool_error(error, consecutive_timeouts),
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn decode_stream_chunk(value: &str) -> String {
    BASE64
        .decode(value)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| value.to_string())
}

fn extract_exit_code(end: &serde_json::Map<String, Value>) -> i32 {
    if let Some(exit_code) = end
        .get("exitCode")
        .or_else(|| end.get("exit_code"))
        .and_then(Value::as_i64)
    {
        return exit_code as i32;
    }
    if let Some(status) = end.get("status").and_then(Value::as_str) {
        if status == "exit status 0" {
            return 0;
        }
        if let Some(code) = status
            .strip_prefix("exit status ")
            .and_then(|raw| raw.parse::<i32>().ok())
        {
            return code;
        }
    }
    if end.get("error").and_then(Value::as_str).is_some() {
        return 1;
    }
    0
}

fn required_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| MoaError::ValidationError(format!("missing string field `{field}`")))
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::{HandProvider, HandResources, HandSpec, SandboxTier};
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::E2BHandProvider;

    #[tokio::test]
    async fn provisions_executes_and_destroys_sandbox() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 8192];
                    let bytes = socket.read(&mut buffer).await.unwrap();
                    let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                    let first_line = request.lines().next().unwrap_or_default();
                    let (status, content_type, body) = if first_line.starts_with("POST /sandboxes ")
                        || first_line.starts_with("POST /sandboxes/sbx-123/connect ")
                    {
                        (
                            "200 OK",
                            "application/json",
                            r#"{"sandboxID":"sbx-123","domain":"example.e2b.test","envdAccessToken":"envd-token","envdVersion":"0.1.1"}"#.to_string(),
                        )
                    } else if first_line.starts_with("POST /process.Process/Start ") {
                        (
                            "200 OK",
                            "application/connect+json",
                            encode_test_envelopes(&[
                                serde_json::json!({"event":{"start":{"pid": 12}}}),
                                serde_json::json!({"event":{"data":{"stdout":"aGVsbG8K"}}}),
                                serde_json::json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
                                serde_json::json!({}),
                            ]),
                        )
                    } else if first_line.starts_with("DELETE /sandboxes/sbx-123 ") {
                        ("204 No Content", "application/json", String::new())
                    } else if first_line.starts_with("GET /sandboxes/sbx-123 ") {
                        (
                            "200 OK",
                            "application/json",
                            r#"{"state":"paused"}"#.to_string(),
                        )
                    } else {
                        (
                            "404 Not Found",
                            "application/json",
                            r#"{"error":"unexpected"}"#.to_string(),
                        )
                    };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                        body.len(),
                    );
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                });
            }
        });

        let provider = E2BHandProvider::with_api_url(
            "test-key",
            format!("http://{addr}"),
            "example.e2b.test",
            "base",
        )
        .unwrap()
        .with_sandbox_base_url(format!("http://{addr}"));
        let handle = provider
            .provision(HandSpec {
                sandbox_tier: SandboxTier::MicroVM,
                image: None,
                resources: HandResources::default(),
                env: std::collections::HashMap::new(),
                workspace_mount: None,
                idle_timeout: Duration::from_secs(300),
                max_lifetime: Duration::from_secs(300),
            })
            .await
            .unwrap();

        let output = provider
            .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
            .await
            .unwrap();
        assert_eq!(output.process_stdout(), Some("hello\n"));

        provider.destroy(&handle).await.unwrap();
    }

    fn encode_test_envelopes(messages: &[Value]) -> String {
        let mut bytes = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let payload = serde_json::to_vec(message).unwrap();
            let flags = if index + 1 == messages.len() {
                super::CONNECT_END_STREAM_FLAG
            } else {
                0
            };
            bytes.push(flags);
            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&payload);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
