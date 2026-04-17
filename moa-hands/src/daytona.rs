//! Daytona-backed hand provider for cloud container execution.

use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    HandHandle, HandProvider, HandSpec, HandStatus, MoaConfig, MoaError, Result, SandboxTier,
    ToolOutput,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

use crate::tools::str_replace::plan_str_replace;

const DEFAULT_DAYTONA_API_URL: &str = "https://app.daytona.io/api";
const DEFAULT_DAYTONA_TOOLBOX_URL: &str = "https://proxy.app.daytona.io/toolbox";
const DEFAULT_DAYTONA_IMAGE: &str = "daytonaio/workspace:latest";
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const DESTROY_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// Daytona cloud hand provider.
#[derive(Clone)]
pub struct DaytonaHandProvider {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    default_image: String,
    idle_timeout: Duration,
}

impl DaytonaHandProvider {
    /// Creates a new Daytona provider from an API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_urls(
            api_key,
            DEFAULT_DAYTONA_API_URL,
            DEFAULT_DAYTONA_TOOLBOX_URL,
        )
    }

    /// Creates a Daytona provider from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let hands = config
            .cloud
            .hands
            .as_ref()
            .ok_or_else(|| MoaError::ConfigError("missing [cloud.hands] config".to_string()))?;
        let api_key_env = hands
            .daytona_api_key_env
            .as_deref()
            .ok_or_else(|| MoaError::ConfigError("missing Daytona API key env".to_string()))?;
        let api_key = std::env::var(api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.to_string()))?;
        let mut provider = Self::with_urls(
            api_key,
            hands
                .daytona_api_url
                .as_deref()
                .unwrap_or(DEFAULT_DAYTONA_API_URL),
            derive_toolbox_url(
                hands
                    .daytona_api_url
                    .as_deref()
                    .unwrap_or(DEFAULT_DAYTONA_API_URL),
            ),
        )?;
        if let Some(image) = &hands.daytona_default_image {
            provider.default_image = image.clone();
        }
        Ok(provider)
    }

    /// Overrides the default idle timeout sent during provisioning.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// Overrides the default image used when the hand spec does not set one.
    #[must_use]
    pub fn with_default_image(mut self, default_image: impl Into<String>) -> Self {
        self.default_image = default_image.into();
        self
    }

    /// Creates a provider with explicit API and toolbox URLs.
    pub fn with_urls(
        api_key: impl Into<String>,
        api_url: impl Into<String>,
        toolbox_url: impl Into<String>,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_COMMAND_TIMEOUT)
            .default_headers(default_headers(&api_key)?)
            .build()
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to build Daytona client: {error}"))
            })?;
        Ok(Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            toolbox_url: toolbox_url.into().trim_end_matches('/').to_string(),
            default_image: DEFAULT_DAYTONA_IMAGE.to_string(),
            idle_timeout: DEFAULT_COMMAND_TIMEOUT,
        })
    }

    async fn create_workspace(&self, spec: &HandSpec) -> Result<String> {
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| self.default_image.clone());
        let response = self
            .client
            .post(format!("{}/sandbox", self.api_url))
            .json(&json!({
                "image": image,
                "env": spec.env,
                "autoStopInterval": (spec.idle_timeout.as_secs() / 60).max((self.idle_timeout.as_secs() / 60).max(1)),
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create Daytona sandbox: {error}"))
            })?;
        let value = expect_success_json(response).await?;
        extract_workspace_id(&value)
    }

    async fn execute_command(
        &self,
        workspace_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let response = self
            .client
            .post(format!(
                "{}/{}/process/execute",
                self.toolbox_url, workspace_id
            ))
            .json(&json!({
                "command": command,
                "cwd": cwd,
                "timeout": timeout_secs.unwrap_or(DEFAULT_COMMAND_TIMEOUT.as_secs()),
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to execute Daytona command: {error}"))
            })?;
        let value = expect_success_json(response).await?;
        Ok(ToolOutput::from_process(
            value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            String::new(),
            value
                .get("exitCode")
                .or_else(|| value.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or_default() as i32,
            started_at.elapsed(),
        ))
    }

    async fn read_file(&self, workspace_id: &str, path: &str) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/{}/files/download", self.toolbox_url, workspace_id),
            &[("path", path)],
        )?;
        let response = self.client.get(url).send().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to read Daytona file: {error}"))
        })?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        Ok(ToolOutput::text(
            response.text().await.map_err(|error| {
                MoaError::ProviderError(format!("failed to decode Daytona file response: {error}"))
            })?,
            started_at.elapsed(),
        ))
    }

    async fn write_file(
        &self,
        workspace_id: &str,
        path: &str,
        content: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/{}/files/upload", self.toolbox_url, workspace_id),
            &[("path", path)],
        )?;
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(content.as_bytes().to_vec())
                .file_name("upload.txt")
                .mime_str("text/plain; charset=utf-8")
                .map_err(|error| {
                    MoaError::ValidationError(format!("invalid Daytona upload MIME type: {error}"))
                })?,
        );
        let response = self
            .client
            .post(url)
            .multipart(form)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to write Daytona file: {error}"))
            })?;
        expect_success(response).await?;
        Ok(ToolOutput::text(
            format!("wrote {path}"),
            started_at.elapsed(),
        ))
    }

    async fn str_replace_file(
        &self,
        workspace_id: &str,
        path: &str,
        input: &str,
    ) -> Result<ToolOutput> {
        let existing_content = match self.read_file(workspace_id, path).await {
            Ok(output) => Some(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => None,
            Err(error) => return Err(error),
        };
        let planned = plan_str_replace(input, existing_content.as_deref(), path, 4)?;
        let write_output = self
            .write_file(workspace_id, path, &planned.updated_content)
            .await?;
        Ok(ToolOutput::text(planned.message, write_output.duration))
    }

    async fn search_files(&self, workspace_id: &str, pattern: &str) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/{}/files/search", self.toolbox_url, workspace_id),
            &[("path", "/"), ("pattern", pattern)],
        )?;
        let response = self.client.get(url).send().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to search Daytona files: {error}"))
        })?;
        let value = expect_success_json(response).await?;
        Ok(ToolOutput::json(
            serde_json::to_string_pretty(&value)?,
            value,
            started_at.elapsed(),
        ))
    }
}

#[async_trait]
impl HandProvider for DaytonaHandProvider {
    fn provider_name(&self) -> &str {
        "daytona"
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        if matches!(spec.sandbox_tier, SandboxTier::MicroVM) {
            return Err(MoaError::Unsupported(
                "use the E2B provider for microvm sandboxes".to_string(),
            ));
        }
        let workspace_id = self.create_workspace(&spec).await?;
        Ok(HandHandle::daytona(workspace_id))
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let workspace_id = handle.daytona_id()?;
        let payload: Value = serde_json::from_str(input)?;
        self.resume(handle).await?;
        match tool {
            "bash" => {
                self.execute_command(
                    workspace_id,
                    required_string_field(&payload, "cmd")?,
                    None,
                    payload.get("timeout_secs").and_then(Value::as_u64),
                )
                .await
            }
            "file_read" => {
                self.read_file(workspace_id, required_string_field(&payload, "path")?)
                    .await
            }
            "str_replace" => {
                self.str_replace_file(
                    workspace_id,
                    required_string_field(&payload, "path")?,
                    input,
                )
                .await
            }
            "file_write" => {
                self.write_file(
                    workspace_id,
                    required_string_field(&payload, "path")?,
                    required_string_field(&payload, "content")?,
                )
                .await
            }
            "file_search" => {
                self.search_files(workspace_id, required_string_field(&payload, "pattern")?)
                    .await
            }
            other => Err(MoaError::ToolError(format!(
                "unsupported Daytona tool: {other}"
            ))),
        }
    }

    async fn status(&self, handle: &HandHandle) -> Result<HandStatus> {
        let workspace_id = handle.daytona_id()?;
        let response = self
            .client
            .get(format!("{}/sandbox/{workspace_id}", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to inspect Daytona sandbox: {error}"))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(HandStatus::Destroyed);
        }
        let value = expect_success_json(response).await?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("started")
            .to_ascii_lowercase();
        Ok(match state.as_str() {
            "creating" | "pending" | "starting" => HandStatus::Provisioning,
            "started" | "running" => HandStatus::Running,
            "stopped" => HandStatus::Stopped,
            "archived" | "deleted" => HandStatus::Destroyed,
            "error" | "failed" => HandStatus::Failed,
            _ => HandStatus::Running,
        })
    }

    async fn pause(&self, handle: &HandHandle) -> Result<()> {
        let workspace_id = handle.daytona_id()?;
        let response = self
            .client
            .post(format!("{}/sandbox/{workspace_id}/stop", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to stop Daytona sandbox: {error}"))
            })?;
        expect_success(response).await?;
        Ok(())
    }

    async fn resume(&self, handle: &HandHandle) -> Result<()> {
        let workspace_id = handle.daytona_id()?;
        let status = self.status(handle).await?;
        if matches!(status, HandStatus::Running | HandStatus::Provisioning) {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/sandbox/{workspace_id}/start", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to start Daytona sandbox: {error}"))
            })?;
        expect_success(response).await?;
        Ok(())
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        let workspace_id = handle.daytona_id()?;
        let started_at = Instant::now();
        loop {
            let response = self
                .client
                .delete(format!("{}/sandbox/{workspace_id}", self.api_url))
                .send()
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to delete Daytona sandbox: {error}"))
                })?;
            if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND
            {
                return Ok(());
            }
            if response.status() == reqwest::StatusCode::CONFLICT {
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "failed to read response body".to_string());
                if message.contains("state change in progress")
                    && started_at.elapsed() < DESTROY_RETRY_TIMEOUT
                {
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }
                return Err(MoaError::HttpStatus {
                    status: reqwest::StatusCode::CONFLICT.as_u16(),
                    message,
                });
            }
            return Err(http_error(response).await);
        }
    }
}

fn default_headers(api_key: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|error| {
            MoaError::ValidationError(format!("invalid Daytona API key header: {error}"))
        })?,
    );
    if let Ok(org_id) = std::env::var("DAYTONA_ORGANIZATION_ID") {
        headers.insert(
            "X-Daytona-Organization-ID",
            HeaderValue::from_str(&org_id).map_err(|error| {
                MoaError::ValidationError(format!(
                    "invalid Daytona organization header value: {error}"
                ))
            })?,
        );
    }
    Ok(headers)
}

async fn expect_success_json(response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| MoaError::ProviderError(format!("invalid Daytona JSON response: {error}")))
}

async fn expect_success(response: reqwest::Response) -> Result<()> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    Ok(())
}

async fn http_error(response: reqwest::Response) -> MoaError {
    let status = response.status().as_u16();
    let message = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read response body".to_string());
    MoaError::HttpStatus { status, message }
}

fn derive_toolbox_url(api_url: &str) -> &str {
    let _ = api_url;
    DEFAULT_DAYTONA_TOOLBOX_URL
}

fn extract_workspace_id(value: &Value) -> Result<String> {
    value
        .get("id")
        .or_else(|| value.get("sandboxId"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            MoaError::ProviderError("Daytona create sandbox response missing id".to_string())
        })
}

fn required_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| MoaError::ValidationError(format!("missing string field `{field}`")))
}

fn build_url(base: &str, params: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).map_err(|error| {
        MoaError::ValidationError(format!("invalid Daytona URL {base}: {error}"))
    })?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use moa_core::{HandProvider, HandResources, HandSpec, SandboxTier};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::DaytonaHandProvider;

    #[tokio::test]
    async fn provisions_executes_and_destroys_workspace() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let seen_server = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen_server.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 8192];
                    let bytes = socket.read(&mut buffer).await.unwrap();
                    let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                    let first_line = request.lines().next().unwrap_or_default().to_string();
                    seen.lock().await.push(first_line.clone());
                    let (status, body) = if first_line.starts_with("POST /api/sandbox ") {
                        (
                            "200 OK",
                            r#"{"id":"sbx-123","state":"started"}"#.to_string(),
                        )
                    } else if first_line.starts_with("GET /api/sandbox/sbx-123 ") {
                        (
                            "200 OK",
                            r#"{"id":"sbx-123","state":"stopped"}"#.to_string(),
                        )
                    } else if first_line.starts_with("POST /api/sandbox/sbx-123/start ") {
                        ("200 OK", r#"{"ok":true}"#.to_string())
                    } else if first_line.starts_with("POST /toolbox/sbx-123/process/execute ") {
                        ("200 OK", r#"{"exitCode":0,"result":"hello\n"}"#.to_string())
                    } else if first_line.starts_with("DELETE /api/sandbox/sbx-123 ") {
                        ("200 OK", r#"{"ok":true}"#.to_string())
                    } else {
                        ("404 Not Found", r#"{"error":"unexpected"}"#.to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });

        let provider = DaytonaHandProvider::with_urls(
            "test-key",
            format!("http://{addr}/api"),
            format!("http://{addr}/toolbox"),
        )
        .unwrap();
        let handle = provider
            .provision(HandSpec {
                sandbox_tier: SandboxTier::Container,
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
        assert_eq!(output.process_stdout().as_deref(), Some("hello\n"));

        provider.destroy(&handle).await.unwrap();

        let seen = seen.lock().await.join("\n");
        assert!(seen.contains("POST /api/sandbox "));
        assert!(seen.contains("POST /toolbox/sbx-123/process/execute "));
        assert!(seen.contains("DELETE /api/sandbox/sbx-123 "));
    }
}
