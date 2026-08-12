//! E2B-backed hand provider for microVM execution.

mod client;
mod storage;
mod workspace;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use moa_config::CloudHandProviderKind;
use moa_core::types::identifiers::WorkspaceCheckpointId;
use moa_core::types::sandbox_workspace::{
    DurabilityClass, ProviderAccountStorageInventory, ProviderInventoryOwner,
    ProviderInventoryResource, ProviderInventoryResourceKind, ProviderStorageKind,
    ProviderStorageRef, TenantStoragePurgeRequest, WorkspaceAttachRequest,
    WorkspaceCheckpointPublication, WorkspaceCheckpointPublishRequest,
    WorkspaceConfirmedDisposition, WorkspaceOperationKind, WorkspaceOperationOutcome,
    WorkspacePostCommitState, WorkspaceReconcileRequest, WorkspaceRestoreRequest,
    WorkspaceRevisionRef, WorkspaceStorageDeleteRequest, WorkspaceStorageOperation,
    WorkspaceStorageOperationResult, WorkspaceStoragePrepareRequest,
};
use moa_core::{
    canonical_json::canonical_json_bytes,
    error::MoaError,
    error::Result,
    error::ToolFailureClass,
    traits::{HandProvider, SandboxStorageProvider},
    types::hands::DeadlineEnforcement,
    types::hands::EgressMode,
    types::hands::HandHandle,
    types::hands::HandProviderCapabilities,
    types::hands::HandSpec,
    types::hands::HandStatus,
    types::hands::ResourceSupport,
    types::hands::SandboxFile,
    types::hands::SandboxProfile,
    types::hands::SandboxTier,
    types::hands::SandboxTierCapabilities,
    types::hands::validate_sandbox_file_path,
    types::identifiers::HandProvisioningOperationId,
    types::tools::ToolOutput,
};
use reqwest::header::CONTENT_TYPE;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::adapters::http_util::{
    build_url, expect_success_json, http_error, required_string_field,
};
use crate::adapters::trusted_command::{resolve_provider_file_path, resolve_trusted_skill_command};
use crate::tools::edit_output::{
    ExistingFileContent, build_file_write_output, build_text_edit_output,
};
use crate::tools::sandbox_descriptor::{
    SandboxToolCapability, supported_capability_for_tool, unsupported_tool,
};
use crate::tools::str_replace::plan_str_replace;
use crate::tools::{bash, file_outline, file_read, grep};

use crate::core::provider_credentials::{
    ProviderCredentialSource, ProviderEndpoint, ProviderHttpAttempt, ProviderSandboxAttempt,
};
use crate::core::sandbox_workspace::capacity::PostgresWorkspaceCapacityRepository;
use crate::core::sandbox_workspace::checkpoint::revision::{
    next_workspace_revision, required_current_revision,
};
use crate::core::sandbox_workspace::checkpoint::store::CheckpointObjectStore;
use client::{encode_connect_request, envd_headers, parse_e2b_connect_stream, shell_escape};

const E2B_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_E2B_DOMAIN: &str = "e2b.app";
const DEFAULT_E2B_TEMPLATE: &str = "base";
const DEFAULT_ENVD_PORT: u16 = 49983;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_PROTOCOL_VERSION: &str = "1";
const E2B_PROVISIONING_OPERATION_METADATA_KEY: &str = "moa_provisioning_operation_id";
const E2B_PROVISIONING_SPEC_METADATA_KEY: &str = "moa_provisioning_spec_sha256";
const E2B_BINDING_METADATA_KEY: &str = "moa_workspace_binding_sha256";
const E2B_TENANT_METADATA_KEY: &str = "moa_tenant_id";
const E2B_WORKSPACE_METADATA_KEY: &str = "moa_workspace_id";
const E2B_PROVIDER_ACCOUNT_METADATA_KEY: &str = "moa_provider_account_id";
const E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY: &str = "moa_provider_account_generation";
const E2B_WRITER_EPOCH_METADATA_KEY: &str = "moa_writer_epoch";
const E2B_INSTANCE_GENERATION_METADATA_KEY: &str = "moa_instance_generation";
const E2B_SANDBOX_LIST_LIMIT: usize = 100;
const E2B_TRUSTED_ROOT: &str = "/opt/moa/trusted";
const E2B_RESET_TRUSTED_ROOT: &str = "if install -d -m 700 /opt/moa/trusted 2>/dev/null; then :; else sudo -n install -d -m 700 -o \"$(id -u)\" -g \"$(id -g)\" /opt/moa/trusted; fi && find /opt/moa/trusted -mindepth 1 -delete";

#[derive(Clone)]
pub(super) struct ConnectedSandbox {
    pub(super) sandbox_domain: String,
    pub(super) envd_access_token: SecretString,
    pub(super) _envd_version: String,
}

impl std::fmt::Debug for ConnectedSandbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectedSandbox")
            .field("sandbox_domain", &"[REDACTED]")
            .field("envd_access_token", &"[REDACTED]")
            .field("envd_version", &self._envd_version)
            .finish()
    }
}

/// E2B cloud hand provider for microVM-backed execution.
pub struct E2BHandProvider {
    credentials: Arc<dyn ProviderCredentialSource>,
    sandbox_base_url_override: Option<String>,
    checkpoint_store: Option<Arc<CheckpointObjectStore>>,
    checkpoint_capacity: Option<Arc<PostgresWorkspaceCapacityRepository>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvisionedE2BSandbox {
    sandbox_id: String,
    spec_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum E2BSandboxState {
    Running,
    Paused,
    Provisioning,
}

#[derive(Debug)]
struct InspectedE2BSandbox {
    state: E2BSandboxState,
    connected: Option<ConnectedSandbox>,
}

impl E2BHandProvider {
    /// Creates a provider backed by rotating persisted-account credentials.
    #[must_use]
    pub fn new(credentials: Arc<dyn ProviderCredentialSource>) -> Self {
        Self {
            credentials,
            sandbox_base_url_override: None,
            checkpoint_store: None,
            checkpoint_capacity: None,
        }
    }

    /// Installs durable encrypted portable-checkpoint storage.
    #[must_use]
    pub fn with_checkpoint_store(mut self, checkpoint_store: Arc<CheckpointObjectStore>) -> Self {
        self.checkpoint_store = Some(checkpoint_store);
        self
    }

    /// Installs provider-neutral pre-publication checkpoint admission.
    #[must_use]
    pub fn with_checkpoint_capacity(
        mut self,
        capacity: Arc<PostgresWorkspaceCapacityRepository>,
    ) -> Self {
        self.checkpoint_capacity = Some(capacity);
        self
    }

    async fn api_attempt(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderHttpAttempt> {
        self.credentials
            .resolve_attempt(
                provider_account_id,
                provider_account_generation,
                CloudHandProviderKind::E2b,
                ProviderEndpoint::Api,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await
    }

    /// Overrides the computed envd sandbox base URL. Intended for tests and local proxies.
    #[cfg(test)]
    pub fn with_sandbox_base_url(mut self, sandbox_base_url: impl Into<String>) -> Self {
        self.sandbox_base_url_override =
            Some(sandbox_base_url.into().trim_end_matches('/').to_string());
        self
    }

    async fn create_sandbox(
        &self,
        attempt: &ProviderHttpAttempt,
        spec: &HandSpec,
        translated: E2BProfileFields,
        spec_fingerprint: &str,
    ) -> Result<String> {
        let binding_fingerprint = workspace_binding_fingerprint(&spec.workspace)?;
        let metadata = HashMap::from([
            (
                E2B_PROVISIONING_OPERATION_METADATA_KEY,
                spec.provisioning_operation_id.to_string(),
            ),
            (
                E2B_PROVISIONING_SPEC_METADATA_KEY,
                spec_fingerprint.to_string(),
            ),
            (E2B_BINDING_METADATA_KEY, binding_fingerprint),
            (
                E2B_TENANT_METADATA_KEY,
                spec.workspace.tenant_id.to_string(),
            ),
            (
                E2B_WORKSPACE_METADATA_KEY,
                spec.workspace.workspace_id.to_string(),
            ),
            (
                E2B_PROVIDER_ACCOUNT_METADATA_KEY,
                spec.workspace.provider_account_id.to_string(),
            ),
            (
                E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY,
                spec.workspace.provider_account_generation.to_string(),
            ),
            (
                E2B_WRITER_EPOCH_METADATA_KEY,
                spec.workspace.writer_epoch.to_string(),
            ),
            (
                E2B_INSTANCE_GENERATION_METADATA_KEY,
                spec.workspace.instance_generation.to_string(),
            ),
        ]);
        let response = attempt
            .client()
            .post(format!("{}/sandboxes", attempt.origin()))
            .header("X-API-KEY", attempt.credential())
            .json(&json!({
                "templateID": spec.image.clone().unwrap_or_else(|| attempt.default_runtime().unwrap_or(DEFAULT_E2B_TEMPLATE).to_string()),
                "envVars": spec.env,
                "timeout": translated.timeout_secs,
                "secure": true,
                "allow_internet_access": translated.allow_internet_access,
                "autoPause": false,
                "autoResume": { "enabled": false },
                "metadata": metadata,
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
        Ok(sandbox_id)
    }

    fn provisioning_spec_fingerprint(
        &self,
        spec: &HandSpec,
        translated: E2BProfileFields,
        default_template: &str,
    ) -> Result<String> {
        let creation_contract = json!({
            "templateID": spec.image.as_deref().unwrap_or(default_template),
            "envVars": spec.env,
            "timeout": translated.timeout_secs,
            "secure": true,
            "allow_internet_access": translated.allow_internet_access,
            "autoPause": false,
            "autoResume": { "enabled": false },
            "workspaceBinding": spec.workspace,
            "filesystem": spec.filesystem,
        });
        let canonical = canonical_json_bytes(&creation_contract).map_err(|error| {
            MoaError::ProviderError(format!(
                "failed to canonicalize E2B provisioning spec: {error}"
            ))
        })?;
        Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
    }

    async fn provisioned_sandboxes(
        &self,
        attempt: &ProviderHttpAttempt,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<ProvisionedE2BSandbox>> {
        let metadata_query = format!("{E2B_PROVISIONING_OPERATION_METADATA_KEY}={operation_id}");
        let page_limit = E2B_SANDBOX_LIST_LIMIT.to_string();
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut seen_sandbox_ids = HashSet::new();
        let mut sandboxes = Vec::new();

        loop {
            let mut url = build_url(
                &format!("{}/v2/sandboxes", attempt.origin()),
                &[
                    ("metadata", metadata_query.as_str()),
                    ("limit", page_limit.as_str()),
                    ("state", "running,paused"),
                ],
                "E2B",
            )?;
            if let Some(token) = next_token.as_deref() {
                url.query_pairs_mut().append_pair("nextToken", token);
            }
            let response = attempt
                .client()
                .get(url)
                .header("X-API-KEY", attempt.credential())
                .send()
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!(
                        "failed to list E2B provisioning operation sandboxes: {error}"
                    ))
                })?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            let response_next_token = response
                .headers()
                .get("x-next-token")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::trim)
                        .map(str::to_string)
                        .map_err(|error| {
                            MoaError::ProviderError(format!(
                                "invalid E2B X-Next-Token response header: {error}"
                            ))
                        })
                })
                .transpose()?
                .filter(|token| !token.is_empty());
            let value = expect_success_json(response, "E2B").await?;
            let page = value.as_array().ok_or_else(|| {
                MoaError::ProviderError(
                    "E2B list sandboxes response must be a JSON array".to_string(),
                )
            })?;

            for item in page {
                let sandbox = decode_provisioned_sandbox(item, operation_id)?;
                if seen_sandbox_ids.insert(sandbox.sandbox_id.clone()) {
                    sandboxes.push(sandbox);
                }
            }

            match response_next_token {
                Some(token) => {
                    if !seen_tokens.insert(token.clone()) {
                        return Err(MoaError::ProviderError(
                            "E2B sandbox pagination repeated a continuation token".to_string(),
                        ));
                    }
                    next_token = Some(token);
                }
                None if page.len() == E2B_SANDBOX_LIST_LIMIT => {
                    return Err(MoaError::ProviderError(format!(
                        "E2B returned a full {E2B_SANDBOX_LIST_LIMIT}-sandbox page without an X-Next-Token; refusing to truncate provisioning recovery"
                    )));
                }
                None => break,
            }
        }

        sandboxes.sort_unstable_by(|left, right| left.sandbox_id.cmp(&right.sandbox_id));
        Ok(sandboxes)
    }

    async fn connect_running_sandbox(
        &self,
        attempt: &ProviderHttpAttempt,
        sandbox_id: &str,
    ) -> Result<ConnectedSandbox> {
        let response = attempt
            .client()
            .post(format!(
                "{}/sandboxes/{sandbox_id}/connect",
                attempt.origin()
            ))
            .header("X-API-KEY", attempt.credential())
            .json(&json!({
                "timeout": DEFAULT_COMMAND_TIMEOUT.as_secs(),
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to connect E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "E2B").await?;
        if value
            .get("sandboxID")
            .and_then(Value::as_str)
            .is_some_and(|observed| observed != sandbox_id)
        {
            return Err(MoaError::ProviderError(
                "E2B connect returned a different sandbox identity".to_string(),
            ));
        }
        let sandbox = ConnectedSandbox {
            sandbox_domain: value
                .get("domain")
                .and_then(Value::as_str)
                .or_else(|| attempt.sandbox_domain())
                .unwrap_or(DEFAULT_E2B_DOMAIN)
                .to_string(),
            envd_access_token: SecretString::from(
                required_string_field(&value, "envdAccessToken")?.to_string(),
            ),
            _envd_version: required_string_field(&value, "envdVersion")?.to_string(),
        };
        Ok(sandbox)
    }

    async fn inspect_sandbox(
        &self,
        attempt: &ProviderHttpAttempt,
        sandbox_id: &str,
        expected_account: (moa_core::types::identifiers::ProviderAccountId, u64),
        expected_binding: Option<&moa_core::types::sandbox_workspace::WorkspaceBinding>,
    ) -> Result<Option<InspectedE2BSandbox>> {
        let response = attempt
            .client()
            .get(format!("{}/sandboxes/{sandbox_id}", attempt.origin()))
            .header("X-API-KEY", attempt.credential())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to inspect E2B sandbox: {error}"))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value = expect_success_json(response, "E2B").await?;
        if value
            .get("sandboxID")
            .and_then(Value::as_str)
            .is_some_and(|observed| observed != sandbox_id)
        {
            return Err(MoaError::ProviderError(
                "E2B inspection returned a different sandbox identity".to_string(),
            ));
        }
        let metadata = value
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                MoaError::ProviderError(
                    "E2B sandbox is missing MOA workspace ownership metadata".to_string(),
                )
            })?;
        verify_workspace_metadata(
            metadata,
            expected_account.0,
            expected_account.1,
            expected_binding,
        )?;

        let state = match required_string_field(&value, "state")?
            .to_ascii_lowercase()
            .as_str()
        {
            "running" | "started" => E2BSandboxState::Running,
            "paused" | "stopped" => E2BSandboxState::Paused,
            "provisioning" | "starting" => E2BSandboxState::Provisioning,
            other => {
                return Err(MoaError::ProviderError(format!(
                    "E2B sandbox is in unsupported state `{other}`"
                )));
            }
        };
        let connected = value
            .get("envdAccessToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(|token| ConnectedSandbox {
                sandbox_domain: value
                    .get("domain")
                    .and_then(Value::as_str)
                    .filter(|domain| !domain.is_empty())
                    .or_else(|| attempt.sandbox_domain())
                    .unwrap_or(DEFAULT_E2B_DOMAIN)
                    .to_string(),
                envd_access_token: SecretString::from(token.to_string()),
                _envd_version: value
                    .get("envdVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        Ok(Some(InspectedE2BSandbox { state, connected }))
    }

    async fn running_sandbox(
        &self,
        attempt: &ProviderHttpAttempt,
        sandbox_id: &str,
        expected_account: (moa_core::types::identifiers::ProviderAccountId, u64),
        expected_binding: Option<&moa_core::types::sandbox_workspace::WorkspaceBinding>,
    ) -> Result<ConnectedSandbox> {
        let inspection = self
            .inspect_sandbox(attempt, sandbox_id, expected_account, expected_binding)
            .await?
            .ok_or_else(|| MoaError::HttpStatus {
                status: 404,
                retry_after: None,
                message: "E2B sandbox is absent".to_string(),
            })?;
        if inspection.state != E2BSandboxState::Running {
            return Err(MoaError::ProviderError(
                "E2B sandbox is not running; fresh-compute recovery is required".to_string(),
            ));
        }
        match inspection.connected {
            Some(connected) => Ok(connected),
            None => self.connect_running_sandbox(attempt, sandbox_id).await,
        }
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
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        cmd: &str,
        timeout: Duration,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = format!("{}/process.Process/Start", attempt.origin());
        tokio::time::timeout(timeout, async {
            let response = attempt
                .client()
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
        })
        .await
        .map_err(|_| {
            MoaError::ToolError(format!(
                "E2B command timed out after {}s",
                timeout.as_secs()
            ))
        })?
    }

    async fn run_checked_command(
        &self,
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        operation: &'static str,
        command: &str,
    ) -> Result<()> {
        let output = self
            .execute_bash(
                attempt,
                sandbox_id,
                sandbox,
                command,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        if output.is_error {
            return Err(MoaError::ProviderError(format!(
                "E2B workspace filesystem operation {operation} failed with exit code {:?}",
                output.process_exit_code()
            )));
        }
        Ok(())
    }

    async fn read_file(
        &self,
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", attempt.origin()),
            &[("path", path)],
            "E2B",
        )?;
        let response = attempt
            .client()
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
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        remote_path: &str,
        content: &str,
    ) -> Result<ToolOutput> {
        let existing = match self
            .read_file(attempt, sandbox_id, sandbox, remote_path)
            .await
        {
            Ok(output) => ExistingFileContent::Text(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => ExistingFileContent::Missing,
            Err(error) => return Err(error),
        };
        let duration = self
            .upload_file(
                attempt,
                sandbox_id,
                sandbox,
                remote_path,
                content.as_bytes(),
            )
            .await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &[u8],
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", attempt.origin()),
            &[("path", path)],
            "E2B",
        )?;
        let response = attempt
            .client()
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
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<()> {
        let output = self
            .execute_bash(
                attempt,
                sandbox_id,
                sandbox,
                &format!("chmod 755 {}", shell_escape(path)),
                DEFAULT_COMMAND_TIMEOUT,
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
        attempt: &ProviderSandboxAttempt,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        remote_path: &str,
        input: &str,
    ) -> Result<ToolOutput> {
        let existing_content = match self
            .read_file(attempt, sandbox_id, sandbox, remote_path)
            .await
        {
            Ok(output) => Some(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => None,
            Err(error) => return Err(error),
        };
        let planned = plan_str_replace(input, existing_content.as_deref(), path, 4)?;
        let duration = self
            .upload_file(
                attempt,
                sandbox_id,
                sandbox,
                remote_path,
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
        HandHandle::E2B { sandbox_id, .. } => Ok(sandbox_id.as_str()),
        _ => Err(MoaError::Unsupported(
            "non-E2B hand handle passed to E2BHandProvider".to_string(),
        )),
    }
}

fn cloud_account(
    handle: &HandHandle,
) -> Result<(moa_core::types::identifiers::ProviderAccountId, u64)> {
    handle.provider_account().ok_or_else(|| {
        MoaError::Unsupported("non-E2B hand handle passed to E2BHandProvider".to_string())
    })
}

fn workspace_binding_fingerprint(
    binding: &moa_core::types::sandbox_workspace::WorkspaceBinding,
) -> Result<String> {
    let canonical = canonical_json_bytes(binding).map_err(|error| {
        MoaError::ProviderError(format!(
            "failed to canonicalize E2B workspace binding: {error}"
        ))
    })?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

fn required_metadata<'a>(
    metadata: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "E2B sandbox is missing required MOA metadata `{key}`"
            ))
        })
}

fn verify_workspace_metadata(
    metadata: &serde_json::Map<String, Value>,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
    provider_account_generation: u64,
    expected_binding: Option<&moa_core::types::sandbox_workspace::WorkspaceBinding>,
) -> Result<()> {
    if required_metadata(metadata, E2B_PROVIDER_ACCOUNT_METADATA_KEY)?
        != provider_account_id.to_string()
        || required_metadata(metadata, E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY)?
            != provider_account_generation.to_string()
    {
        return Err(MoaError::ProviderError(
            "E2B sandbox provider-account metadata does not match its persisted handle".to_string(),
        ));
    }
    let binding_hash = required_metadata(metadata, E2B_BINDING_METADATA_KEY)?;
    let tenant = required_metadata(metadata, E2B_TENANT_METADATA_KEY)?;
    let workspace = required_metadata(metadata, E2B_WORKSPACE_METADATA_KEY)?;
    let writer_epoch = required_metadata(metadata, E2B_WRITER_EPOCH_METADATA_KEY)?;
    let instance_generation = required_metadata(metadata, E2B_INSTANCE_GENERATION_METADATA_KEY)?;
    if binding_hash.len() != 64
        || !binding_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || tenant.parse::<uuid::Uuid>().is_err()
        || workspace.parse::<uuid::Uuid>().is_err()
        || writer_epoch.parse::<u64>().is_err()
        || instance_generation.parse::<u64>().is_err()
    {
        return Err(MoaError::ProviderError(
            "E2B sandbox carries malformed MOA workspace metadata".to_string(),
        ));
    }
    if let Some(binding) = expected_binding
        && (binding_hash != workspace_binding_fingerprint(binding)?
            || tenant != binding.tenant_id.to_string()
            || workspace != binding.workspace_id.to_string()
            || writer_epoch != binding.writer_epoch.to_string()
            || instance_generation != binding.instance_generation.to_string())
    {
        return Err(MoaError::ProviderError(
            "E2B sandbox workspace metadata does not match the durable binding".to_string(),
        ));
    }
    Ok(())
}

fn decode_provisioned_sandbox(
    value: &Value,
    operation_id: HandProvisioningOperationId,
) -> Result<ProvisionedE2BSandbox> {
    let expected_operation_id = operation_id.to_string();
    let sandbox_id = required_string_field(value, "sandboxID")?.to_string();
    let metadata = value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "E2B sandbox `{sandbox_id}` matched operation `{operation_id}` without metadata"
            ))
        })?;
    let actual_operation_id = metadata
        .get(E2B_PROVISIONING_OPERATION_METADATA_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "E2B sandbox `{sandbox_id}` omitted `{E2B_PROVISIONING_OPERATION_METADATA_KEY}`"
            ))
        })?;
    if actual_operation_id != expected_operation_id.as_str() {
        return Err(MoaError::ProviderError(format!(
            "E2B sandbox `{sandbox_id}` returned for operation `{operation_id}` carries operation `{actual_operation_id}`"
        )));
    }
    let spec_fingerprint = metadata
        .get(E2B_PROVISIONING_SPEC_METADATA_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ProvisionedE2BSandbox {
        sandbox_id,
        spec_fingerprint,
    })
}

/// Revision of the E2B provider's capability declaration.
pub const E2B_CAPABILITIES_REVISION: &str = "e2b-hands-v1";

/// What E2B can enforce for a microVM sandbox.
///
/// Only two of the six dimensions map onto documented sandbox-create fields:
/// `timeout`, which E2B enforces as the sandbox's maximum lifetime, and
/// `allow_internet_access`, which is a whole-sandbox on/off switch. CPU,
/// memory, and disk are fixed by the template at build time and are not
/// settable per sandbox, so a bounded request for any of them is refused rather
/// than serialized into a field E2B would ignore. There is no per-destination
/// filter, so an egress allowlist is refused too, and no idle field, so the
/// durable reaper owns the idle deadline.
pub static E2B_CAPABILITIES: LazyLock<HandProviderCapabilities> =
    LazyLock::new(|| HandProviderCapabilities {
        revision: E2B_CAPABILITIES_REVISION.to_string(),
        tiers: vec![SandboxTierCapabilities {
            tier: SandboxTier::MicroVM,
            cpu: ResourceSupport::unbounded_only(),
            memory: ResourceSupport::unbounded_only(),
            ephemeral_disk: ResourceSupport::unbounded_only(),
            egress_modes: vec![EgressMode::DenyAll, EgressMode::Unrestricted],
            idle_enforcement: DeadlineEnforcement::DurableReaper,
            max_lifetime_enforcement: DeadlineEnforcement::Provider,
        }],
    });

/// E2B's smallest accepted sandbox timeout, in seconds.
const E2B_MIN_TIMEOUT_SECS: u64 = 60;

/// The sandbox-create fields E2B actually honors, translated from a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct E2BProfileFields {
    timeout_secs: u64,
    allow_internet_access: bool,
}

impl E2BProfileFields {
    /// Translates the enforceable dimensions and refuses the rest.
    fn translate(profile: &SandboxProfile) -> Result<Self> {
        reject_unsupported_e2b_resource("CPU", profile.cpu.bounded_millicores().is_some())?;
        reject_unsupported_e2b_resource("memory", profile.memory.bounded_mebibytes().is_some())?;
        reject_unsupported_e2b_resource(
            "ephemeral disk",
            profile.ephemeral_disk.bounded_mebibytes().is_some(),
        )?;
        let allow_internet_access = match profile.egress.mode() {
            EgressMode::DenyAll => false,
            EgressMode::Unrestricted => true,
            EgressMode::AllowList => {
                return Err(MoaError::Unsupported(
                    "E2B sandboxes cannot enforce a per-destination egress allowlist".to_string(),
                ));
            }
        };
        // An unbounded maximum lifetime still has to become a number here,
        // because E2B has no "no timeout" value. Refusing is the honest answer:
        // a sandbox MOA believes is unbounded must not silently acquire E2B's
        // own deadline.
        let seconds = profile.max_lifetime.bounded_seconds().ok_or_else(|| {
            MoaError::Unsupported(
                "E2B sandboxes always carry a maximum lifetime and cannot serve an unbounded one"
                    .to_string(),
            )
        })?;
        if seconds.get() < E2B_MIN_TIMEOUT_SECS {
            return Err(MoaError::Unsupported(format!(
                "E2B sandboxes require a maximum lifetime of at least {E2B_MIN_TIMEOUT_SECS}s, \
                 requested {seconds}s"
            )));
        }
        Ok(Self {
            timeout_secs: seconds.get(),
            allow_internet_access,
        })
    }
}

/// Refuses a bounded resource dimension E2B fixes at template build time.
fn reject_unsupported_e2b_resource(dimension: &str, bounded: bool) -> Result<()> {
    if bounded {
        return Err(MoaError::Unsupported(format!(
            "E2B fixes {dimension} in the sandbox template and cannot honor a per-sandbox bound"
        )));
    }
    Ok(())
}

#[async_trait]
impl HandProvider for E2BHandProvider {
    fn provider_name(&self) -> &str {
        "e2b"
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        E2B_CAPABILITIES.clone()
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        if !matches!(spec.sandbox_tier, SandboxTier::MicroVM) {
            return Err(MoaError::Unsupported(
                "E2B provider is reserved for microvm sandboxes".to_string(),
            ));
        }
        if spec.filesystem.mutable_root != std::path::Path::new(storage::E2B_DATA_ROOT) {
            return Err(MoaError::ValidationError(format!(
                "E2B mutable root must be {} so checkpoint export cannot include trusted or runtime state",
                storage::E2B_DATA_ROOT
            )));
        }
        let account_id = spec.workspace.provider_account_id;
        let account_generation = spec.workspace.provider_account_generation;
        let attempt = self.api_attempt(account_id, account_generation).await?;
        let translated = E2BProfileFields::translate(spec.effective_profile.profile())?;
        let default_template = attempt.default_runtime().unwrap_or(DEFAULT_E2B_TEMPLATE);
        let expected_fingerprint =
            self.provisioning_spec_fingerprint(&spec, translated, default_template)?;
        let existing = self
            .provisioned_sandboxes(&attempt, spec.provisioning_operation_id)
            .await?;
        if existing.len() > 1 {
            return Err(MoaError::ProviderError(format!(
                "E2B provisioning operation `{}` resolved {} sandboxes; durable reaper cleanup is required",
                spec.provisioning_operation_id,
                existing.len()
            )));
        }
        if let Some(existing) = existing.first() {
            if existing.spec_fingerprint.as_deref() != Some(expected_fingerprint.as_str()) {
                return Err(MoaError::ProviderError(format!(
                    "E2B provisioning operation `{}` was reused with a different creation spec",
                    spec.provisioning_operation_id
                )));
            }
            return Ok(HandHandle::e2b(
                existing.sandbox_id.clone(),
                account_id,
                account_generation,
            ));
        }
        let sandbox_id = self
            .create_sandbox(&attempt, &spec, translated, &expected_fingerprint)
            .await?;
        Ok(HandHandle::e2b(sandbox_id, account_id, account_generation))
    }

    async fn provisioned_hands(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        let attempt = self
            .api_attempt(provider_account_id, provider_account_generation)
            .await?;
        Ok(self
            .provisioned_sandboxes(&attempt, operation_id)
            .await?
            .into_iter()
            .map(|sandbox| {
                HandHandle::e2b(
                    sandbox.sandbox_id,
                    provider_account_id,
                    provider_account_generation,
                )
            })
            .collect())
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let sandbox_id = sandbox_id(handle)?;
        let (account_id, account_generation) = cloud_account(handle)?;
        let api_attempt = self.api_attempt(account_id, account_generation).await?;
        let sandbox = self
            .running_sandbox(
                &api_attempt,
                sandbox_id,
                (account_id, account_generation),
                None,
            )
            .await?;
        let envd_origin = self.envd_url(sandbox_id, &sandbox);
        let sandbox_attempt = self
            .credentials
            .admit_sandbox_attempt(
                account_id,
                account_generation,
                CloudHandProviderKind::E2b,
                &envd_origin,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        let payload: Value = serde_json::from_str(input)?;
        match supported_capability_for_tool(tool, E2B_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                let params = bash::BashToolInput::parse(input)?;
                // This adapter implements only the unbounded `execute`, so no run
                // deadline reaches it: both the sandbox lifetime and the run
                // budget are absent here. Opting E2B into `execute_bounded` is
                // what would carry them.
                let timeout = params.timeout(DEFAULT_COMMAND_TIMEOUT, None, None);
                let trusted = resolve_trusted_skill_command(&params.cmd, E2B_TRUSTED_ROOT)?;
                let command = trusted
                    .as_ref()
                    .map_or_else(|| params.cmd.clone(), |command| command.shell_token());
                let mut output = self
                    .execute_bash(&sandbox_attempt, sandbox_id, &sandbox, &command, timeout)
                    .await?;
                if let Some(command) = trusted {
                    command.redact_output(&mut output);
                }
                Ok(output)
            }
            Some(SandboxToolCapability::Grep) => {
                let command = grep::remote_shell_command(input, "/")?;
                self.execute_bash(
                    &sandbox_attempt,
                    sandbox_id,
                    &sandbox,
                    &command,
                    DEFAULT_COMMAND_TIMEOUT,
                )
                .await
            }
            Some(SandboxToolCapability::FileOutline) => {
                let path = required_string_field(&payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, storage::E2B_DATA_ROOT, E2B_TRUSTED_ROOT)?;
                let content = self
                    .read_file(&sandbox_attempt, sandbox_id, &sandbox, &remote_path)
                    .await?
                    .to_text();
                file_outline::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::FileRead) => {
                let path = required_string_field(&payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, storage::E2B_DATA_ROOT, E2B_TRUSTED_ROOT)?;
                let content = self
                    .read_file(&sandbox_attempt, sandbox_id, &sandbox, &remote_path)
                    .await?
                    .to_text();
                file_read::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::StrReplace) => {
                let path = required_string_field(&payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, storage::E2B_DATA_ROOT, E2B_TRUSTED_ROOT)?;
                self.str_replace_file(
                    &sandbox_attempt,
                    sandbox_id,
                    &sandbox,
                    path,
                    &remote_path,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                let path = required_string_field(&payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, storage::E2B_DATA_ROOT, E2B_TRUSTED_ROOT)?;
                self.write_file(
                    &sandbox_attempt,
                    sandbox_id,
                    &sandbox,
                    path,
                    &remote_path,
                    required_string_field(&payload, "content")?,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                let pattern = shell_escape(required_string_field(&payload, "pattern")?);
                self.execute_bash(
                    &sandbox_attempt,
                    sandbox_id,
                    &sandbox,
                    &format!("find / -name {pattern} -print 2>/dev/null || true"),
                    DEFAULT_COMMAND_TIMEOUT,
                )
                .await
            }
            None => Err(unsupported_tool("E2B", tool)),
        }
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let (account_id, account_generation) = cloud_account(handle)?;
        let api_attempt = self.api_attempt(account_id, account_generation).await?;
        let sandbox = self
            .running_sandbox(
                &api_attempt,
                sandbox_id,
                (account_id, account_generation),
                None,
            )
            .await?;
        let envd_origin = self.envd_url(sandbox_id, &sandbox);
        let sandbox_attempt = self
            .credentials
            .admit_sandbox_attempt(
                account_id,
                account_generation,
                CloudHandProviderKind::E2b,
                &envd_origin,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        self.run_checked_command(
            &sandbox_attempt,
            sandbox_id,
            &sandbox,
            "reset_trusted_root",
            E2B_RESET_TRUSTED_ROOT,
        )
        .await?;
        for file in files {
            validate_sandbox_file_path(&file.path)?;
            let trusted_path = format!("{E2B_TRUSTED_ROOT}/{}", file.path);
            self.upload_file(
                &sandbox_attempt,
                sandbox_id,
                &sandbox,
                &trusted_path,
                &file.content,
            )
            .await?;
            if file.executable {
                self.chmod_file(&sandbox_attempt, sandbox_id, &sandbox, &trusted_path)
                    .await?;
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
            HandStatus::Running | HandStatus::Provisioning
        ))
    }

    async fn status(&self, handle: &HandHandle) -> Result<HandStatus> {
        let sandbox_id = sandbox_id(handle)?;
        let (account_id, account_generation) = cloud_account(handle)?;
        let attempt = self.api_attempt(account_id, account_generation).await?;
        Ok(
            match self
                .inspect_sandbox(&attempt, sandbox_id, (account_id, account_generation), None)
                .await?
            {
                None => HandStatus::Destroyed,
                Some(inspection) => match inspection.state {
                    E2BSandboxState::Running => HandStatus::Running,
                    E2BSandboxState::Paused => HandStatus::Stopped,
                    E2BSandboxState::Provisioning => HandStatus::Provisioning,
                },
            },
        )
    }

    /// Refuses compute suspension: MOA deliberately opts E2B out of auto-pause.
    ///
    /// Sandbox creation sets `autoPause: false` and `autoResume.enabled: false`
    /// so a sandbox's durable state lives in MOA-owned portable checkpoints
    /// rather than in a provider-side paused snapshot MOA cannot fence or
    /// account for. Pausing here would reintroduce exactly that hidden state, so
    /// the continuation boundary uses the checkpoint path instead.
    async fn suspend(&self, _handle: &HandHandle) -> Result<()> {
        Err(MoaError::Unsupported(
            "E2B pause is deliberately disabled (autoPause/autoResume off); MOA carries sandbox \
             state in portable checkpoints instead"
                .to_string(),
        ))
    }

    async fn resume(&self, _handle: &HandHandle) -> Result<()> {
        Err(MoaError::Unsupported(
            "E2B resume restores process memory; MOA recovers workspaces into fresh compute"
                .to_string(),
        ))
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let (account_id, account_generation) = cloud_account(handle)?;
        let attempt = self.api_attempt(account_id, account_generation).await?;
        if self
            .inspect_sandbox(&attempt, sandbox_id, (account_id, account_generation), None)
            .await?
            .is_none()
        {
            return Ok(());
        }
        let response = attempt
            .client()
            .delete(format!("{}/sandboxes/{sandbox_id}", attempt.origin()))
            .header("X-API-KEY", attempt.credential())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to destroy E2B sandbox: {error}"))
            })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(http_error(response).await)
    }
}

fn confirmed_storage_result(
    storage: Option<ProviderStorageRef>,
    disposition: WorkspaceConfirmedDisposition,
) -> WorkspaceStorageOperationResult {
    WorkspaceStorageOperationResult {
        outcome: WorkspaceOperationOutcome::Confirmed,
        confirmed_disposition: Some(disposition),
        storage,
        checkpoint_publication: None,
        post_commit_state: None,
    }
}

#[cfg(test)]
mod operation_intent_tests {
    use moa_core::types::identifiers::HandProvisioningOperationId;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        E2B_BINDING_METADATA_KEY, E2B_INSTANCE_GENERATION_METADATA_KEY,
        E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY, E2B_PROVIDER_ACCOUNT_METADATA_KEY,
        E2B_PROVISIONING_OPERATION_METADATA_KEY, E2B_PROVISIONING_SPEC_METADATA_KEY,
        E2B_TENANT_METADATA_KEY, E2B_WORKSPACE_METADATA_KEY, E2B_WRITER_EPOCH_METADATA_KEY,
        decode_provisioned_sandbox, verify_workspace_metadata, workspace_binding_fingerprint,
    };

    #[test]
    fn decodes_operation_identity_and_spec_fingerprint() {
        // Pins: recovery accepts only resources carrying the exact durable
        // operation identity returned by E2B's metadata-filtered list API.
        let operation_id = HandProvisioningOperationId(Uuid::from_u128(0xe2b));
        let value = json!({
            "sandboxID": "sandbox-2",
            "metadata": {
                (E2B_PROVISIONING_OPERATION_METADATA_KEY): operation_id.to_string(),
                (E2B_PROVISIONING_SPEC_METADATA_KEY): "sha256:spec",
            },
        });

        let decoded = decode_provisioned_sandbox(&value, operation_id)
            .expect("matching operation metadata should decode");

        assert_eq!(decoded.sandbox_id, "sandbox-2");
        assert_eq!(decoded.spec_fingerprint.as_deref(), Some("sha256:spec"));
    }

    #[test]
    fn rejects_a_list_result_with_a_different_operation_identity() {
        // Pins: a provider-side filtering error cannot make the reaper destroy
        // a sandbox belonging to a different durable operation.
        let operation_id = HandProvisioningOperationId(Uuid::from_u128(0xe2b));
        let other_operation_id = HandProvisioningOperationId(Uuid::from_u128(0xbad));
        let value = json!({
            "sandboxID": "sandbox-other",
            "metadata": {
                (E2B_PROVISIONING_OPERATION_METADATA_KEY): other_operation_id.to_string(),
            },
        });

        let error = decode_provisioned_sandbox(&value, operation_id)
            .expect_err("mismatched operation metadata must fail closed");

        assert!(error.to_string().contains(&other_operation_id.to_string()));
    }

    #[test]
    fn rejects_workspace_metadata_from_a_different_writer_generation() {
        // Pins: exact-handle GET inspection cannot authorize an E2B sandbox
        // carrying a stale writer/instance binding.
        let binding = crate::core::profile::test_support::hand_spec(
            moa_core::types::hands::SandboxTier::MicroVM,
            moa_core::types::hands::SandboxProfile::unrestricted(),
        )
        .workspace;
        let mut stale = binding.clone();
        stale.writer_epoch += 1;
        let metadata = serde_json::json!({
            (E2B_BINDING_METADATA_KEY): workspace_binding_fingerprint(&stale).expect("fingerprint"),
            (E2B_TENANT_METADATA_KEY): stale.tenant_id.to_string(),
            (E2B_WORKSPACE_METADATA_KEY): stale.workspace_id.to_string(),
            (E2B_PROVIDER_ACCOUNT_METADATA_KEY): stale.provider_account_id.to_string(),
            (E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY): stale.provider_account_generation.to_string(),
            (E2B_WRITER_EPOCH_METADATA_KEY): stale.writer_epoch.to_string(),
            (E2B_INSTANCE_GENERATION_METADATA_KEY): stale.instance_generation.to_string(),
        });

        let error = verify_workspace_metadata(
            metadata.as_object().expect("metadata object"),
            binding.provider_account_id,
            binding.provider_account_generation,
            Some(&binding),
        )
        .expect_err("stale writer generation must be fenced");

        assert!(error.to_string().contains("durable binding"));
    }
}
