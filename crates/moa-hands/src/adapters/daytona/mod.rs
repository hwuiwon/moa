//! Daytona-backed hand provider for cloud container execution.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::{
    error::MoaError, error::Result, error::ToolFailureClass, error::classify_tool_error,
    traits::HandProvider, types::hands::DeadlineEnforcement, types::hands::EgressMode,
    types::hands::HandHandle, types::hands::HandProviderCapabilities, types::hands::HandSpec,
    types::hands::HandStatus, types::hands::ResourceSupport, types::hands::SandboxFile,
    types::hands::SandboxProfile, types::hands::SandboxTier, types::hands::SandboxTierCapabilities,
    types::hands::validate_sandbox_file_path, types::tools::ToolOutput,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

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
use crate::tools::{bash, file_outline, file_read, grep};

const DAYTONA_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_DAYTONA_API_URL: &str = "https://app.daytona.io/api";
const DEFAULT_DAYTONA_TOOLBOX_URL: &str = "https://proxy.app.daytona.io/toolbox";
const DEFAULT_DAYTONA_IMAGE: &str = "daytonaio/workspace:latest";
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const DESTROY_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// Optional Daytona organization id, resolved from the environment once.
static DAYTONA_ORGANIZATION_ID: LazyLock<Option<String>> =
    LazyLock::new(|| std::env::var("DAYTONA_ORGANIZATION_ID").ok());

/// Daytona cloud hand provider.
#[derive(Clone)]
pub struct DaytonaHandProvider {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    default_image: String,
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
        let api_key = hands
            .daytona_api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MoaError::MissingEnvironmentVariable("MOA_CLOUD_HANDS_DAYTONA_API_KEY".to_string())
            })?
            .to_string();
        let mut provider = Self::with_urls(
            api_key,
            hands
                .daytona_api_url
                .as_deref()
                .unwrap_or(DEFAULT_DAYTONA_API_URL),
            DEFAULT_DAYTONA_TOOLBOX_URL,
        )?;
        if let Some(image) = &hands.daytona_default_image {
            provider.default_image = image.clone();
        }
        Ok(provider)
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
        })
    }

    async fn create_workspace(&self, spec: &HandSpec) -> Result<String> {
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| self.default_image.clone());
        let auto_stop_minutes = daytona_auto_stop_minutes(spec.effective_profile.profile())?;
        let response = self
            .client
            .post(format!("{}/sandbox", self.api_url))
            .json(&json!({
                "image": image,
                "env": spec.env,
                "autoStopInterval": auto_stop_minutes,
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create Daytona sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "Daytona").await?;
        extract_workspace_id(&value)
    }

    async fn execute_command(
        &self,
        workspace_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<ToolOutput> {
        let timeout_secs = timeout.map(|timeout| timeout.as_secs());
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
        let value = expect_success_json(response, "Daytona").await?;
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
            "Daytona",
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
        let existing = match self.read_file(workspace_id, path).await {
            Ok(output) => ExistingFileContent::Text(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => ExistingFileContent::Missing,
            Err(error) => return Err(error),
        };
        let duration = self
            .upload_file(workspace_id, path, content.as_bytes())
            .await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        workspace_id: &str,
        path: &str,
        content: &[u8],
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/{}/files/upload", self.toolbox_url, workspace_id),
            &[("path", path)],
            "Daytona",
        )?;
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(content.to_vec())
                .file_name("upload.txt")
                .mime_str("application/octet-stream")
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
        Ok(started_at.elapsed())
    }

    async fn chmod_file(&self, workspace_id: &str, path: &str) -> Result<()> {
        let command = format!("chmod 755 {}", shell_quote(path));
        let output = self
            .execute_command(workspace_id, &command, None, Some(Duration::from_secs(30)))
            .await?;
        if output.is_error {
            return Err(MoaError::ProviderError(format!(
                "failed to mark Daytona file executable `{path}`: {}",
                output.to_text()
            )));
        }
        Ok(())
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
        let duration = self
            .upload_file(workspace_id, path, planned.updated_content.as_bytes())
            .await?;
        Ok(build_text_edit_output(
            path,
            existing_content.as_deref().unwrap_or_default(),
            &planned.updated_content,
            duration,
        ))
    }

    async fn search_files(&self, workspace_id: &str, pattern: &str) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/{}/files/search", self.toolbox_url, workspace_id),
            &[("path", "/"), ("pattern", pattern)],
            "Daytona",
        )?;
        let response = self.client.get(url).send().await.map_err(|error| {
            MoaError::ProviderError(format!("failed to search Daytona files: {error}"))
        })?;
        let value = expect_success_json(response, "Daytona").await?;
        Ok(ToolOutput::json(
            serde_json::to_string_pretty(&value)?,
            value,
            started_at.elapsed(),
        ))
    }

    /// Routes one already-parsed tool invocation to its Daytona toolbox call.
    async fn dispatch_tool(
        &self,
        workspace_id: &str,
        tool: &str,
        input: &str,
        payload: &Value,
    ) -> Result<ToolOutput> {
        match supported_capability_for_tool(tool, DAYTONA_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                // Parsed through the shared validated input rather than read
                // out of the raw payload: the remote toolbox honours whatever
                // `timeout` it is handed, so an unvalidated read here would
                // reinstate the unbounded timeout on the Daytona route alone.
                let params = bash::BashToolInput::parse(input)?;
                self.execute_command(
                    workspace_id,
                    &params.cmd,
                    None,
                    params.timeout_secs.map(|timeout| timeout.duration()),
                )
                .await
            }
            Some(SandboxToolCapability::Grep) => {
                let command = grep::remote_shell_command(input, "/")?;
                self.execute_command(workspace_id, &command, None, None)
                    .await
            }
            Some(SandboxToolCapability::FileOutline) => {
                let path = required_string_field(payload, "path")?;
                let content = self.read_file(workspace_id, path).await?.to_text();
                file_outline::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::FileRead) => {
                let path = required_string_field(payload, "path")?;
                let content = self.read_file(workspace_id, path).await?.to_text();
                file_read::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::StrReplace) => {
                self.str_replace_file(workspace_id, required_string_field(payload, "path")?, input)
                    .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                self.write_file(
                    workspace_id,
                    required_string_field(payload, "path")?,
                    required_string_field(payload, "content")?,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                self.search_files(workspace_id, required_string_field(payload, "pattern")?)
                    .await
            }
            None => Err(unsupported_tool("Daytona", tool)),
        }
    }
}

/// Revision of the Daytona provider's capability declaration.
pub const DAYTONA_CAPABILITIES_REVISION: &str = "daytona-hands-v1";

/// What Daytona can enforce for a container sandbox.
///
/// The sandbox-create payload MOA sends carries exactly one policy-bearing
/// field, `autoStopInterval`, which Daytona enforces as an idle timeout in
/// whole minutes. Per-sandbox CPU, memory, and disk bounds and any network
/// posture other than the account default are not fields on this request, so a
/// profile that asks for them is refused before the sandbox is created rather
/// than being sent as JSON Daytona ignores. The hard maximum lifetime has no
/// Daytona-side owner, so the durable reaper owns it.
pub static DAYTONA_CAPABILITIES: LazyLock<HandProviderCapabilities> =
    LazyLock::new(|| HandProviderCapabilities {
        revision: DAYTONA_CAPABILITIES_REVISION.to_string(),
        tiers: vec![
            daytona_tier_capabilities(SandboxTier::Container),
            daytona_tier_capabilities(SandboxTier::None),
        ],
    });

/// Capabilities for one Daytona-served tier.
fn daytona_tier_capabilities(tier: SandboxTier) -> SandboxTierCapabilities {
    SandboxTierCapabilities {
        tier,
        cpu: ResourceSupport::unbounded_only(),
        memory: ResourceSupport::unbounded_only(),
        ephemeral_disk: ResourceSupport::unbounded_only(),
        egress_modes: vec![EgressMode::Unrestricted],
        idle_enforcement: DeadlineEnforcement::Provider,
        max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
    }
}

/// Seconds in the whole-minute granularity Daytona's `autoStopInterval` uses.
const DAYTONA_AUTO_STOP_GRANULARITY_SECS: u64 = 60;

/// Translates the idle timeout into Daytona's `autoStopInterval`, refusing
/// anything the field cannot express.
///
/// `autoStopInterval` counts whole minutes, and `0` disables auto-stop
/// entirely. Rounding a 90-second policy to either 1 or 2 minutes would enforce
/// a deadline nobody asked for, so a value that is not a whole number of
/// minutes is refused instead.
fn daytona_auto_stop_minutes(profile: &SandboxProfile) -> Result<u64> {
    reject_unsupported_daytona_dimension("CPU", profile.cpu.bounded_millicores().is_some())?;
    reject_unsupported_daytona_dimension("memory", profile.memory.bounded_mebibytes().is_some())?;
    reject_unsupported_daytona_dimension(
        "ephemeral disk",
        profile.ephemeral_disk.bounded_mebibytes().is_some(),
    )?;
    if profile.egress.mode() != EgressMode::Unrestricted {
        return Err(MoaError::Unsupported(format!(
            "Daytona sandboxes cannot enforce {} egress",
            profile.egress.mode().as_str()
        )));
    }
    let Some(seconds) = profile.idle_timeout.bounded_seconds() else {
        // 0 is Daytona's documented "never auto-stop", which is exactly what an
        // explicitly unbounded idle timeout means.
        return Ok(0);
    };
    if seconds.get() % DAYTONA_AUTO_STOP_GRANULARITY_SECS != 0 {
        return Err(MoaError::Unsupported(format!(
            "Daytona auto-stop is expressed in whole minutes; requested {seconds}s"
        )));
    }
    Ok(seconds.get() / DAYTONA_AUTO_STOP_GRANULARITY_SECS)
}

/// Refuses a bounded resource dimension Daytona's create request cannot carry.
fn reject_unsupported_daytona_dimension(dimension: &str, bounded: bool) -> Result<()> {
    if bounded {
        return Err(MoaError::Unsupported(format!(
            "Daytona sandbox creation carries no {dimension} bound and cannot enforce one"
        )));
    }
    Ok(())
}

#[async_trait]
impl HandProvider for DaytonaHandProvider {
    fn provider_name(&self) -> &str {
        "daytona"
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        DAYTONA_CAPABILITIES.clone()
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
        // Attempt the tool directly rather than resuming on every call. The
        // sandbox is only probed and resumed after a failure, and only when it is
        // genuinely not running (so the tool never started); this keeps the happy
        // path free of a status()+resume() round trip while still recovering a
        // sandbox that auto-stopped between calls without risking a double run.
        match self
            .dispatch_tool(workspace_id, tool, input, &payload)
            .await
        {
            Ok(output) => Ok(output),
            Err(error) => match self.status(handle).await {
                Ok(HandStatus::Stopped | HandStatus::Paused) => {
                    self.resume(handle).await?;
                    self.dispatch_tool(workspace_id, tool, input, &payload)
                        .await
                }
                _ => Err(error),
            },
        }
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        let workspace_id = handle.daytona_id()?;
        self.resume(handle).await?;
        for file in files {
            validate_sandbox_file_path(&file.path)?;
            self.upload_file(workspace_id, &file.path, &file.content)
                .await?;
            if file.executable {
                self.chmod_file(workspace_id, &file.path).await?;
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
        self::classify_error(error, status, consecutive_timeouts)
    }

    async fn health_check(&self, handle: &HandHandle) -> Result<bool> {
        Ok(matches!(
            self.status(handle).await?,
            HandStatus::Running | HandStatus::Paused | HandStatus::Provisioning
        ))
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
        let value = expect_success_json(response, "Daytona").await?;
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
                    retry_after: None,
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
    if let Some(org_id) = DAYTONA_ORGANIZATION_ID.as_ref() {
        headers.insert(
            "X-Daytona-Organization-ID",
            HeaderValue::from_str(org_id).map_err(|error| {
                MoaError::ValidationError(format!(
                    "invalid Daytona organization header value: {error}"
                ))
            })?,
        );
    }
    Ok(headers)
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Classifies one Daytona execution error for retry and re-provision decisions.
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
            reason: format!(
                "Daytona sandbox is no longer healthy ({})",
                status_label(status)
            ),
        };
    }

    if matches!(error, MoaError::HttpStatus { status: 404, .. }) {
        return ToolFailureClass::ReProvision {
            reason: "Daytona sandbox no longer exists".to_string(),
        };
    }

    if let MoaError::HttpStatus {
        status: 502..=504, ..
    } = error
    {
        return ToolFailureClass::Retryable {
            reason: format!("Daytona sandbox gateway is temporarily unavailable: {error}"),
            backoff_hint: Duration::from_secs(1),
        };
    }

    classify_tool_error(error, consecutive_timeouts)
}

fn status_label(status: Option<HandStatus>) -> &'static str {
    match status {
        Some(HandStatus::Provisioning) => "provisioning",
        Some(HandStatus::Running) => "running",
        Some(HandStatus::Paused) => "paused",
        Some(HandStatus::Stopped) => "stopped",
        Some(HandStatus::Destroyed) => "destroyed",
        Some(HandStatus::Failed) => "failed",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moa_core::{traits::HandProvider, types::hands::SandboxProfile, types::hands::SandboxTier};
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
            .provision(crate::core::profile::test_support::hand_spec(
                SandboxTier::Container,
                SandboxProfile::unrestricted(),
            ))
            .await
            .unwrap();

        let output = provider
            .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
            .await
            .unwrap();
        assert_eq!(output.process_stdout(), Some("hello\n"));

        provider.destroy(&handle).await.unwrap();

        let seen = seen.lock().await.join("\n");
        assert!(seen.contains("POST /api/sandbox "));
        assert!(seen.contains("POST /toolbox/sbx-123/process/execute "));
        assert!(seen.contains("DELETE /api/sandbox/sbx-123 "));
    }
}
