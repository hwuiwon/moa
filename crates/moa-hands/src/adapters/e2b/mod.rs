//! E2B-backed hand provider for microVM execution.

mod client;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    config::MoaConfig, error::MoaError, error::Result, error::ToolFailureClass,
    traits::HandProvider, types::hands::HandHandle, types::hands::HandSpec,
    types::hands::HandStatus, types::hands::SandboxFile, types::hands::SandboxTier,
    types::hands::validate_sandbox_file_path, types::tools::ToolOutput,
};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::adapters::http_util::{
    build_url, expect_success, expect_success_json, http_error, required_string_field,
};
use crate::tools::edit_output::{
    ExistingFileContent, build_file_write_output, build_text_edit_output,
};
use crate::tools::sandbox_descriptor::{
    SandboxToolCapability, supported_capability_for_tool, unsupported_tool,
};
use crate::tools::str_replace::plan_str_replace;
use crate::tools::{file_outline, file_read, grep};

use client::{
    default_headers, encode_connect_request, envd_headers, parse_e2b_connect_stream, shell_escape,
};

const E2B_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_E2B_API_URL: &str = "https://api.e2b.dev";
const DEFAULT_E2B_DOMAIN: &str = "e2b.app";
const DEFAULT_E2B_TEMPLATE: &str = "base";
const DEFAULT_ENVD_PORT: u16 = 49983;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_PROTOCOL_VERSION: &str = "1";
#[derive(Debug, Clone)]
pub(super) struct ConnectedSandbox {
    pub(super) sandbox_domain: String,
    pub(super) envd_access_token: String,
    pub(super) _envd_version: String,
}

/// E2B cloud hand provider for microVM-backed execution.
pub struct E2BHandProvider {
    client: reqwest::Client,
    api_url: String,
    sandbox_domain: String,
    default_template: String,
    allow_internet_access: bool,
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
        let api_key = hands
            .e2b_api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MoaError::MissingEnvironmentVariable("MOA_CLOUD_HANDS_E2B_API_KEY".to_string())
            })?
            .to_string();
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
            allow_internet_access: false,
            sandbox_base_url_override: None,
            sandboxes: RwLock::new(HashMap::new()),
        })
    }

    /// Explicitly configures E2B sandbox internet access.
    #[must_use]
    pub fn with_allow_internet_access(mut self, allow_internet_access: bool) -> Self {
        self.allow_internet_access = allow_internet_access;
        self
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
                "allow_internet_access": self.allow_internet_access,
                "autoPause": true,
                "autoResume": { "enabled": true },
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "E2B").await?;
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
        let value = expect_success_json(response, "E2B").await?;
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
            "E2B",
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
        let duration = self
            .upload_file(sandbox_id, sandbox, path, content.as_bytes())
            .await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &[u8],
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", self.envd_url(sandbox_id, sandbox)),
            &[("path", path)],
            "E2B",
        )?;
        let response = self
            .client
            .post(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(content.to_vec())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to write E2B file: {error}"))
            })?;
        let _ = expect_success_json(response, "E2B").await?;
        Ok(started_at.elapsed())
    }

    async fn chmod_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<()> {
        let output = self
            .execute_bash(
                sandbox_id,
                sandbox,
                &format!("chmod 755 {}", shell_escape(path)),
            )
            .await?;
        if output.is_error {
            return Err(MoaError::ProviderError(format!(
                "failed to mark E2B file executable `{path}`: {}",
                output.to_text()
            )));
        }
        Ok(())
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
            .upload_file(
                sandbox_id,
                sandbox,
                path,
                planned.updated_content.as_bytes(),
            )
            .await?;
        Ok(build_text_edit_output(
            path,
            existing_content.as_deref().unwrap_or_default(),
            &planned.updated_content,
            duration,
        ))
    }
}

/// Extracts the sandbox id from an E2B hand handle.
fn sandbox_id(handle: &HandHandle) -> Result<&str> {
    match handle {
        HandHandle::E2B { sandbox_id } => Ok(sandbox_id.as_str()),
        _ => Err(MoaError::Unsupported(
            "non-E2B hand handle passed to E2BHandProvider".to_string(),
        )),
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
        let sandbox_id = sandbox_id(handle)?;
        let sandbox = self.connected_sandbox(sandbox_id).await?;
        let payload: Value = serde_json::from_str(input)?;
        match supported_capability_for_tool(tool, E2B_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                self.execute_bash(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "cmd")?,
                )
                .await
            }
            Some(SandboxToolCapability::Grep) => {
                let command = grep::remote_shell_command(input, "/")?;
                self.execute_bash(sandbox_id, &sandbox, &command).await
            }
            Some(SandboxToolCapability::FileOutline) => {
                let path = required_string_field(&payload, "path")?;
                let content = self.read_file(sandbox_id, &sandbox, path).await?.to_text();
                file_outline::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::FileRead) => {
                let path = required_string_field(&payload, "path")?;
                let content = self.read_file(sandbox_id, &sandbox, path).await?.to_text();
                file_read::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::StrReplace) => {
                self.str_replace_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                self.write_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    required_string_field(&payload, "content")?,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                let pattern = shell_escape(required_string_field(&payload, "pattern")?);
                self.execute_bash(
                    sandbox_id,
                    &sandbox,
                    &format!("find / -name {pattern} -print 2>/dev/null || true"),
                )
                .await
            }
            None => Err(unsupported_tool("E2B", tool)),
        }
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let sandbox = self.connected_sandbox(sandbox_id).await?;
        for file in files {
            validate_sandbox_file_path(&file.path)?;
            self.upload_file(sandbox_id, &sandbox, &file.path, &file.content)
                .await?;
            if file.executable {
                self.chmod_file(sandbox_id, &sandbox, &file.path).await?;
            }
        }
        Ok(())
    }

    async fn classify_error(
        &self,
        handle: &HandHandle,
        error: &MoaError,
        consecutive_timeouts: u32,
    ) -> ToolFailureClass {
        let status = self.status(handle).await.ok();
        client::classify_error(error, status, consecutive_timeouts)
    }

    async fn health_check(&self, handle: &HandHandle) -> Result<bool> {
        Ok(matches!(
            self.status(handle).await?,
            HandStatus::Running | HandStatus::Paused | HandStatus::Provisioning
        ))
    }

    async fn status(&self, handle: &HandHandle) -> Result<HandStatus> {
        let sandbox_id = sandbox_id(handle)?;
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
        let value = expect_success_json(response, "E2B").await?;
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
        let sandbox_id = sandbox_id(handle)?;
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
        let sandbox_id = sandbox_id(handle)?;
        let _ = self.connect_sandbox(sandbox_id).await?;
        Ok(())
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
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
