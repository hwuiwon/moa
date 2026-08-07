//! Daytona-backed hand provider for cloud container execution.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::{
    canonical_json::canonical_json_bytes, error::MoaError, error::Result, error::ToolFailureClass,
    error::classify_tool_error, traits::HandProvider, types::hands::DeadlineEnforcement,
    types::hands::EgressMode, types::hands::HandHandle, types::hands::HandProviderCapabilities,
    types::hands::HandSpec, types::hands::HandStatus, types::hands::ResourceSupport,
    types::hands::SandboxFile, types::hands::SandboxProfile, types::hands::SandboxTier,
    types::hands::SandboxTierCapabilities, types::hands::validate_sandbox_file_path,
    types::identifiers::HandProvisioningOperationId, types::tools::ToolOutput,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, sleep, timeout};

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
const DESTROY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROVISION_RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const PROVISION_RESOLVE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROVISION_RESOLVE_ATTEMPTS: usize = 15;
const SANDBOX_LIST_PAGE_LIMIT: &str = "100";
const PROVISIONING_OPERATION_LABEL: &str = "moa_provisioning_operation_id";
const PROVISIONING_SPEC_LABEL: &str = "moa_provisioning_spec_sha256";
const NON_DESTROYED_SANDBOX_STATES: &[&str] = &[
    "creating",
    "restoring",
    "destroying",
    "started",
    "stopped",
    "starting",
    "stopping",
    "error",
    "build_failed",
    "pending_build",
    "building_snapshot",
    "unknown",
    "pulling_snapshot",
    "archived",
    "archiving",
    "resizing",
    "snapshotting",
    "forking",
    "pausing",
    "paused",
    "resuming",
];

struct DaytonaProvisioningIdentity {
    sandbox_name: String,
    operation_id: String,
    spec_fingerprint: Option<String>,
}

impl DaytonaProvisioningIdentity {
    fn for_operation(operation_id: HandProvisioningOperationId) -> Self {
        Self {
            sandbox_name: daytona_sandbox_name(operation_id),
            operation_id: operation_id.to_string(),
            spec_fingerprint: None,
        }
    }

    fn for_spec(spec: &HandSpec, image: &str, auto_stop_minutes: u64) -> Result<Self> {
        let fingerprint_payload = json!({
            "image": image,
            "env": spec.env,
            "autoStopInterval": auto_stop_minutes,
        });
        let canonical = canonical_json_bytes(&fingerprint_payload).map_err(|error| {
            MoaError::ProviderError(format!(
                "failed to fingerprint Daytona provisioning spec: {error}"
            ))
        })?;
        let fingerprint = format!("{:x}", Sha256::digest(canonical));
        Ok(Self {
            sandbox_name: daytona_sandbox_name(spec.provisioning_operation_id),
            operation_id: spec.provisioning_operation_id.to_string(),
            spec_fingerprint: Some(fingerprint),
        })
    }
}

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

    async fn create_workspace(
        &self,
        spec: &HandSpec,
        image: &str,
        auto_stop_minutes: u64,
        identity: &DaytonaProvisioningIdentity,
    ) -> Result<String> {
        let spec_fingerprint = identity.spec_fingerprint.as_deref().ok_or_else(|| {
            MoaError::ProviderError(
                "Daytona provisioning identity is missing its spec fingerprint".to_string(),
            )
        })?;
        let response = self
            .client
            .post(format!("{}/sandbox", self.api_url))
            .json(&json!({
                "name": identity.sandbox_name,
                "image": image,
                "env": spec.env,
                "autoStopInterval": auto_stop_minutes,
                "labels": {
                    (PROVISIONING_OPERATION_LABEL): identity.operation_id.as_str(),
                    (PROVISIONING_SPEC_LABEL): spec_fingerprint,
                },
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create Daytona sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "Daytona").await?;
        verify_created_workspace_identity(&value, identity)?;
        extract_workspace_id(&value)
    }

    async fn resolve_workspace(
        &self,
        identity: &DaytonaProvisioningIdentity,
    ) -> Result<Option<String>> {
        let workspace_ids = self.provisioned_workspace_ids(identity).await?;
        match workspace_ids.as_slice() {
            [] => Ok(None),
            [workspace_id] => Ok(Some(workspace_id.clone())),
            _ => Err(MoaError::ProviderError(format!(
                "Daytona returned {} sandboxes for durable provisioning operation `{}`; refusing to choose between duplicates",
                workspace_ids.len(),
                identity.operation_id
            ))),
        }
    }

    async fn provisioned_workspace_ids(
        &self,
        identity: &DaytonaProvisioningIdentity,
    ) -> Result<Vec<String>> {
        let label_filter = serde_json::to_string(&json!({
            (PROVISIONING_OPERATION_LABEL): identity.operation_id.as_str(),
        }))?;
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut workspace_ids = BTreeSet::new();

        loop {
            let mut query = vec![
                ("limit", SANDBOX_LIST_PAGE_LIMIT),
                ("labels", label_filter.as_str()),
                ("includeErroredDeleted", "true"),
                ("sort", "name"),
                ("order", "asc"),
            ];
            for &state in NON_DESTROYED_SANDBOX_STATES {
                query.push(("states", state));
            }
            if let Some(cursor) = cursor.as_deref() {
                query.push(("cursor", cursor));
            }
            let url = build_url(&format!("{}/sandbox", self.api_url), &query, "Daytona")?;
            let response = self.client.get(url).send().await.map_err(|error| {
                MoaError::ProviderError(format!("failed to list Daytona sandboxes: {error}"))
            })?;
            let page = expect_success_json(response, "Daytona").await?;
            let items = page.get("items").and_then(Value::as_array).ok_or_else(|| {
                MoaError::ProviderError(
                    "Daytona list sandboxes response is missing the `items` array".to_string(),
                )
            })?;
            for workspace in items {
                let state = workspace
                    .get("state")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MoaError::ProviderError(
                            "Daytona sandbox list item is missing its state".to_string(),
                        )
                    })?;
                if state == "destroyed" {
                    continue;
                }
                if !NON_DESTROYED_SANDBOX_STATES.contains(&state) {
                    return Err(MoaError::ProviderError(format!(
                        "Daytona sandbox list item has unsupported non-terminal state `{state}`"
                    )));
                }
                verify_resolved_workspace_identity(workspace, identity)?;
                workspace_ids.insert(extract_workspace_id(workspace)?);
            }

            cursor = match page.get("nextCursor") {
                Some(Value::Null) => None,
                Some(Value::String(next_cursor)) if !next_cursor.is_empty() => {
                    if !seen_cursors.insert(next_cursor.clone()) {
                        return Err(MoaError::ProviderError(format!(
                            "Daytona list sandboxes response repeated cursor `{next_cursor}`"
                        )));
                    }
                    Some(next_cursor.clone())
                }
                Some(Value::String(_)) => {
                    return Err(MoaError::ProviderError(
                        "Daytona list sandboxes response returned an empty next cursor".to_string(),
                    ));
                }
                Some(_) => {
                    return Err(MoaError::ProviderError(
                        "Daytona list sandboxes response has a non-string next cursor".to_string(),
                    ));
                }
                None => {
                    return Err(MoaError::ProviderError(
                        "Daytona list sandboxes response is missing `nextCursor`".to_string(),
                    ));
                }
            };
            if cursor.is_none() {
                break;
            }
        }

        Ok(workspace_ids.into_iter().collect())
    }

    async fn resolve_workspace_with_retries(
        &self,
        identity: &DaytonaProvisioningIdentity,
        expected_workspace_id: Option<&str>,
    ) -> Result<Option<String>> {
        let started_at = Instant::now();
        for attempt in 0..PROVISION_RESOLVE_ATTEMPTS {
            let Some(remaining) = PROVISION_RESOLVE_TIMEOUT.checked_sub(started_at.elapsed())
            else {
                return Ok(None);
            };
            let workspace_id = timeout(remaining, self.resolve_workspace(identity))
                .await
                .map_err(|_| provision_resolution_timeout_error(identity))??;
            if let Some(workspace_id) = workspace_id {
                if let Some(expected_workspace_id) = expected_workspace_id
                    && workspace_id != expected_workspace_id
                {
                    return Err(MoaError::ProviderError(format!(
                        "Daytona create response identified sandbox `{expected_workspace_id}`, but durable operation resolution found `{workspace_id}`"
                    )));
                }
                return Ok(Some(workspace_id));
            }
            if attempt + 1 == PROVISION_RESOLVE_ATTEMPTS {
                break;
            }
            let Some(remaining) = PROVISION_RESOLVE_TIMEOUT.checked_sub(started_at.elapsed())
            else {
                break;
            };
            sleep(PROVISION_RESOLVE_POLL_INTERVAL.min(remaining)).await;
        }
        Ok(None)
    }

    async fn wait_until_workspace_absent(
        &self,
        workspace_lookup: &str,
        started_at: Instant,
    ) -> Result<()> {
        loop {
            let remaining = remaining_destroy_time(started_at, workspace_lookup)?;
            let response = timeout(
                remaining,
                self.client
                    .get(format!("{}/sandbox/{workspace_lookup}", self.api_url))
                    .send(),
            )
            .await
            .map_err(|_| destroy_timeout_error(workspace_lookup))?
            .map_err(|error| {
                MoaError::ProviderError(format!(
                    "failed to verify Daytona sandbox deletion: {error}"
                ))
            })?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(());
            }
            if !response.status().is_success() {
                let remaining = remaining_destroy_time(started_at, workspace_lookup)?;
                let error = timeout(remaining, http_error(response))
                    .await
                    .map_err(|_| destroy_timeout_error(workspace_lookup))?;
                return Err(error);
            }
            let remaining = remaining_destroy_time(started_at, workspace_lookup)?;
            sleep(DESTROY_POLL_INTERVAL.min(remaining)).await;
        }
    }

    async fn workspace_deletion_lookup(
        &self,
        workspace_id: &str,
        started_at: Instant,
    ) -> Result<Option<String>> {
        let remaining = remaining_destroy_time(started_at, workspace_id)?;
        let response = timeout(
            remaining,
            self.client
                .get(format!("{}/sandbox/{workspace_id}", self.api_url))
                .send(),
        )
        .await
        .map_err(|_| destroy_timeout_error(workspace_id))?
        .map_err(|error| {
            MoaError::ProviderError(format!(
                "failed to inspect Daytona sandbox before deletion: {error}"
            ))
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let remaining = remaining_destroy_time(started_at, workspace_id)?;
        let value = timeout(remaining, expect_success_json(response, "Daytona"))
            .await
            .map_err(|_| destroy_timeout_error(workspace_id))??;
        Ok(Some(
            value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(workspace_id)
                .to_string(),
        ))
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
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| self.default_image.clone());
        let auto_stop_minutes = daytona_auto_stop_minutes(spec.effective_profile.profile())?;
        let identity = DaytonaProvisioningIdentity::for_spec(&spec, &image, auto_stop_minutes)?;
        if let Some(workspace_id) = self.resolve_workspace(&identity).await? {
            return Ok(HandHandle::daytona(workspace_id));
        }

        match self
            .create_workspace(&spec, &image, auto_stop_minutes, &identity)
            .await
        {
            Ok(created_workspace_id) => self
                .resolve_workspace_with_retries(&identity, Some(&created_workspace_id))
                .await?
                .map(HandHandle::daytona)
                .ok_or_else(|| provision_resolution_timeout_error(&identity)),
            Err(create_error) => match self.resolve_workspace_with_retries(&identity, None).await {
                Ok(Some(workspace_id)) => Ok(HandHandle::daytona(workspace_id)),
                Ok(None) => Err(create_error),
                Err(resolve_error) => Err(MoaError::ProviderError(format!(
                    "Daytona sandbox creation failed ({create_error}); resolving the durable operation also failed ({resolve_error})"
                ))),
            },
        }
    }

    async fn provisioned_hands(
        &self,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        let identity = DaytonaProvisioningIdentity::for_operation(operation_id);
        Ok(self
            .provisioned_workspace_ids(&identity)
            .await?
            .into_iter()
            .map(HandHandle::daytona)
            .collect())
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
        let Some(workspace_lookup) = self
            .workspace_deletion_lookup(workspace_id, started_at)
            .await?
        else {
            return Ok(());
        };
        loop {
            let remaining = remaining_destroy_time(started_at, workspace_id)?;
            let response = timeout(
                remaining,
                self.client
                    .delete(format!("{}/sandbox/{workspace_id}", self.api_url))
                    .send(),
            )
            .await
            .map_err(|_| destroy_timeout_error(workspace_id))?
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to delete Daytona sandbox: {error}"))
            })?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(());
            }
            if response.status().is_success() {
                return self
                    .wait_until_workspace_absent(&workspace_lookup, started_at)
                    .await;
            }
            if response.status() == reqwest::StatusCode::CONFLICT {
                let remaining = remaining_destroy_time(started_at, workspace_id)?;
                let message = timeout(remaining, response.text())
                    .await
                    .map_err(|_| destroy_timeout_error(workspace_id))?
                    .unwrap_or_else(|_| "failed to read response body".to_string());
                if message.contains("state change in progress")
                    && started_at.elapsed() < DESTROY_RETRY_TIMEOUT
                {
                    let remaining = remaining_destroy_time(started_at, workspace_id)?;
                    sleep(DESTROY_POLL_INTERVAL.min(remaining)).await;
                    continue;
                }
                return Err(MoaError::HttpStatus {
                    status: reqwest::StatusCode::CONFLICT.as_u16(),
                    retry_after: None,
                    message,
                });
            }
            let remaining = remaining_destroy_time(started_at, workspace_id)?;
            let error = timeout(remaining, http_error(response))
                .await
                .map_err(|_| destroy_timeout_error(workspace_id))?;
            return Err(error);
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
        .and_then(Value::as_str)
        .filter(|workspace_id| !workspace_id.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            MoaError::ProviderError(
                "Daytona sandbox response is missing its non-empty `id`".to_string(),
            )
        })
}

fn daytona_sandbox_name(operation_id: HandProvisioningOperationId) -> String {
    format!("moa-hand-{operation_id}")
}

fn verify_created_workspace_identity(
    value: &Value,
    identity: &DaytonaProvisioningIdentity,
) -> Result<()> {
    verify_workspace_name(value, identity)
}

fn verify_resolved_workspace_identity(
    value: &Value,
    identity: &DaytonaProvisioningIdentity,
) -> Result<()> {
    verify_workspace_name(value, identity)?;
    verify_workspace_labels(value, identity)
}

fn verify_workspace_name(value: &Value, identity: &DaytonaProvisioningIdentity) -> Result<()> {
    let actual_name = value.get("name").and_then(Value::as_str).ok_or_else(|| {
        MoaError::ProviderError("Daytona sandbox is missing its durable operation name".to_string())
    })?;
    if actual_name != identity.sandbox_name {
        return Err(MoaError::ProviderError(format!(
            "Daytona sandbox name `{actual_name}` does not match durable operation name `{}`",
            identity.sandbox_name
        )));
    }
    Ok(())
}

fn verify_workspace_labels(value: &Value, identity: &DaytonaProvisioningIdentity) -> Result<()> {
    let labels = value
        .get("labels")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            MoaError::ProviderError(
                "Daytona sandbox is missing creation-time identity labels".to_string(),
            )
        })?;
    verify_workspace_label(labels, PROVISIONING_OPERATION_LABEL, &identity.operation_id)?;
    match identity.spec_fingerprint.as_deref() {
        Some(spec_fingerprint) => {
            verify_workspace_label(labels, PROVISIONING_SPEC_LABEL, spec_fingerprint)?;
        }
        None => {
            required_workspace_label(labels, PROVISIONING_SPEC_LABEL)?;
        }
    }
    Ok(())
}

fn verify_workspace_label(
    labels: &serde_json::Map<String, Value>,
    key: &str,
    expected: &str,
) -> Result<()> {
    let actual = required_workspace_label(labels, key)?;
    if actual != expected {
        return Err(MoaError::ProviderError(format!(
            "Daytona sandbox label `{key}` does not match the durable provisioning identity"
        )));
    }
    Ok(())
}

fn required_workspace_label<'a>(
    labels: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str> {
    labels
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "Daytona sandbox is missing required non-empty label `{key}`"
            ))
        })
}

fn provision_resolution_timeout_error(identity: &DaytonaProvisioningIdentity) -> MoaError {
    MoaError::ProviderError(format!(
        "timed out resolving Daytona sandbox for durable provisioning operation `{}` with exact name and identity labels",
        identity.operation_id
    ))
}

fn remaining_destroy_time(started_at: Instant, workspace_id: &str) -> Result<Duration> {
    DESTROY_RETRY_TIMEOUT
        .checked_sub(started_at.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| destroy_timeout_error(workspace_id))
}

fn destroy_timeout_error(workspace_id: &str) -> MoaError {
    MoaError::ProviderError(format!(
        "timed out waiting for Daytona sandbox `{workspace_id}` deletion"
    ))
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use moa_core::{traits::HandProvider, types::hands::SandboxProfile, types::hands::SandboxTier};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        DEFAULT_DAYTONA_IMAGE, DaytonaHandProvider, DaytonaProvisioningIdentity,
        PROVISIONING_OPERATION_LABEL, PROVISIONING_SPEC_LABEL, daytona_auto_stop_minutes,
        daytona_sandbox_name,
    };

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let bytes = socket.read(&mut chunk).await.unwrap();
            if bytes == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..bytes]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or_default();
            if request.len() >= body_start + content_length {
                break;
            }
        }
        String::from_utf8_lossy(&request).to_string()
    }

    #[tokio::test]
    async fn provisions_executes_and_destroys_workspace() {
        let spec = crate::core::profile::test_support::hand_spec(
            SandboxTier::Container,
            SandboxProfile::unrestricted(),
        );
        let operation_id = spec.provisioning_operation_id;
        let sandbox_name = daytona_sandbox_name(operation_id);
        let auto_stop_minutes =
            daytona_auto_stop_minutes(spec.effective_profile.profile()).unwrap();
        let spec_fingerprint =
            DaytonaProvisioningIdentity::for_spec(&spec, DEFAULT_DAYTONA_IMAGE, auto_stop_minutes)
                .unwrap()
                .spec_fingerprint
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let seen_server = seen.clone();
        let created = Arc::new(AtomicBool::new(false));
        let created_server = created.clone();
        let deleted = Arc::new(AtomicBool::new(false));
        let deleted_server = deleted.clone();
        let sandbox_name_server = sandbox_name.clone();
        let spec_fingerprint_server = spec_fingerprint.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen_server.clone();
                let created = created_server.clone();
                let deleted = deleted_server.clone();
                let sandbox_name = sandbox_name_server.clone();
                let spec_fingerprint = spec_fingerprint_server.clone();
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    let first_line = request.lines().next().unwrap_or_default().to_string();
                    seen.lock().await.push(first_line.clone());
                    let (status, body) = if first_line.starts_with("POST /api/sandbox ") {
                        if request.contains(&format!("\"name\":\"{sandbox_name}\""))
                            && request.contains(PROVISIONING_OPERATION_LABEL)
                            && request.contains(&operation_id.to_string())
                            && request.contains(PROVISIONING_SPEC_LABEL)
                            && request.contains(&spec_fingerprint)
                        {
                            created.store(true, Ordering::SeqCst);
                            (
                                "200 OK",
                                format!(
                                    r#"{{"id":"sbx-123","name":"{sandbox_name}","state":"started"}}"#
                                ),
                            )
                        } else {
                            (
                                "400 Bad Request",
                                r#"{"error":"missing durable identity"}"#.to_string(),
                            )
                        }
                    } else if first_line.starts_with("GET /api/sandbox?") {
                        if created.load(Ordering::SeqCst) && !deleted.load(Ordering::SeqCst) {
                            (
                                "200 OK",
                                format!(
                                    r#"{{"items":[{{"id":"sbx-123","name":"{sandbox_name}","labels":{{"{PROVISIONING_OPERATION_LABEL}":"{operation_id}","{PROVISIONING_SPEC_LABEL}":"{spec_fingerprint}"}},"state":"paused"}}],"nextCursor":null}}"#
                                ),
                            )
                        } else {
                            ("200 OK", r#"{"items":[],"nextCursor":null}"#.to_string())
                        }
                    } else if first_line.starts_with(&format!("GET /api/sandbox/{sandbox_name} ")) {
                        if created.load(Ordering::SeqCst) && !deleted.load(Ordering::SeqCst) {
                            (
                                "200 OK",
                                format!(
                                    r#"{{"id":"sbx-123","name":"{sandbox_name}","labels":{{"{PROVISIONING_OPERATION_LABEL}":"{operation_id}","{PROVISIONING_SPEC_LABEL}":"{spec_fingerprint}"}},"state":"started"}}"#
                                ),
                            )
                        } else {
                            ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                        }
                    } else if first_line.starts_with("GET /api/sandbox/sbx-123 ") {
                        if deleted.load(Ordering::SeqCst) {
                            ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                        } else {
                            (
                                "200 OK",
                                format!(
                                    r#"{{"id":"sbx-123","name":"{sandbox_name}","state":"stopped"}}"#
                                ),
                            )
                        }
                    } else if first_line.starts_with("POST /api/sandbox/sbx-123/start ") {
                        ("200 OK", r#"{"ok":true}"#.to_string())
                    } else if first_line.starts_with("POST /toolbox/sbx-123/process/execute ") {
                        ("200 OK", r#"{"exitCode":0,"result":"hello\n"}"#.to_string())
                    } else if first_line.starts_with("DELETE /api/sandbox/sbx-123 ") {
                        deleted.store(true, Ordering::SeqCst);
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
        let handle = provider.provision(spec.clone()).await.unwrap();

        assert_eq!(provider.provision(spec).await.unwrap(), handle);

        assert_eq!(
            provider.provisioned_hands(operation_id).await.unwrap(),
            vec![handle.clone()]
        );

        let output = provider
            .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
            .await
            .unwrap();
        assert_eq!(output.process_stdout(), Some("hello\n"));

        provider.destroy(&handle).await.unwrap();

        let seen = seen.lock().await.join("\n");
        assert!(seen.contains("GET /api/sandbox?"));
        assert!(seen.contains(&format!("GET /api/sandbox/{sandbox_name} ")));
        assert!(seen.contains("POST /api/sandbox "));
        assert!(seen.contains("POST /toolbox/sbx-123/process/execute "));
        assert!(seen.contains("DELETE /api/sandbox/sbx-123 "));
        assert!(seen.contains("GET /api/sandbox/sbx-123 "));
        assert_eq!(
            seen.lines()
                .filter(|line| line.starts_with("POST /api/sandbox "))
                .count(),
            1
        );
    }
}
