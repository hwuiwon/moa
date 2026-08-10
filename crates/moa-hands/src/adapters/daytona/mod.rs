//! Daytona-backed hand provider for cloud container execution.

pub mod storage;
mod volume;
mod workspace;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::CloudHandProviderKind;
use moa_core::{
    canonical_json::canonical_json_bytes,
    error::MoaError,
    error::Result,
    error::ToolFailureClass,
    error::classify_tool_error,
    traits::HandProvider,
    traits::SandboxStorageProvider,
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
    types::identifiers::{HandProvisioningOperationId, WorkspaceCheckpointId},
    types::sandbox_workspace::{
        ProviderAccountStorageInventory, ProviderInventoryResource, ProviderInventoryResourceKind,
        ProviderStorageRef, TenantStoragePurgeRequest, WorkspaceAttachRequest,
        WorkspaceCheckpointPublication, WorkspaceCheckpointPublishRequest,
        WorkspacePostCommitState, WorkspaceReconcileRequest, WorkspaceRestoreRequest,
        WorkspaceRevisionRef, WorkspaceStorageDeleteRequest, WorkspaceStorageOperationResult,
        WorkspaceStoragePrepareRequest,
    },
    types::tools::ToolOutput,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::time::{Instant, sleep, timeout};

use crate::adapters::http_util::{
    build_url, expect_success, expect_success_json, http_error, required_string_field,
};
use crate::adapters::trusted_command::{resolve_provider_file_path, resolve_trusted_skill_command};
use crate::core::provider_credentials::{
    ProviderCredentialSource, ProviderEndpoint, ProviderHttpAttempt,
};
use crate::core::sandbox_workspace::checkpoint::revision::{
    next_workspace_revision, required_current_revision,
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
const WORKSPACE_ID_LABEL: &str = "moa_workspace_id";
const TENANT_OWNER_LABEL: &str = "moa_tenant_owner_sha256";
const DAYTONA_TRUSTED_ROOT: &str = "/opt/moa/trusted";
const DAYTONA_PREPARE_MUTABLE_ROOT: &str = "if test -d /workspace && test -w /workspace; then :; elif install -d -m 700 /workspace 2>/dev/null && test -w /workspace; then :; else sudo -n install -d -m 700 -o \"$(id -u)\" -g \"$(id -g)\" /workspace; fi";
const DAYTONA_RESET_TRUSTED_ROOT: &str = "if install -d -m 700 /opt/moa/trusted 2>/dev/null; then :; else sudo -n install -d -m 700 -o \"$(id -u)\" -g \"$(id -g)\" /opt/moa/trusted; fi && find /opt/moa/trusted -mindepth 1 -delete";
const WRITER_EPOCH_LABEL: &str = "moa_writer_epoch";
const INSTANCE_GENERATION_LABEL: &str = "moa_instance_generation";
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
            "workspaceBinding": spec.workspace,
            "filesystem": spec.filesystem,
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

/// Daytona cloud hand provider.
#[derive(Clone)]
pub struct DaytonaHandProvider {
    credentials: Arc<dyn ProviderCredentialSource>,
    storage: Option<Arc<storage::DaytonaStorageDependencies>>,
}

impl DaytonaHandProvider {
    /// Creates a provider backed by rotating persisted-account credentials.
    #[must_use]
    pub fn new(credentials: Arc<dyn ProviderCredentialSource>) -> Self {
        Self {
            credentials,
            storage: None,
        }
    }

    /// Creates a Daytona provider with every durable persistent-workspace owner.
    pub fn new_with_storage(
        credentials: Arc<dyn ProviderCredentialSource>,
        storage: storage::DaytonaStorageDependencies,
    ) -> Result<Self> {
        storage.validate()?;
        Ok(Self {
            credentials,
            storage: Some(Arc::new(storage)),
        })
    }

    async fn attempt(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
        endpoint: ProviderEndpoint,
    ) -> Result<ProviderHttpAttempt> {
        self.credentials
            .resolve_attempt(
                provider_account_id,
                provider_account_generation,
                CloudHandProviderKind::Daytona,
                endpoint,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await
    }

    async fn create_workspace(
        &self,
        attempt: &ProviderHttpAttempt,
        spec: &HandSpec,
        image: &str,
        auto_stop_minutes: u64,
        identity: &DaytonaProvisioningIdentity,
        storage: Option<&ProviderStorageRef>,
    ) -> Result<String> {
        let spec_fingerprint = identity.spec_fingerprint.as_deref().ok_or_else(|| {
            MoaError::ProviderError(
                "Daytona provisioning identity is missing its spec fingerprint".to_string(),
            )
        })?;
        let mut body = json!({
            "name": identity.sandbox_name,
            "image": image,
            "env": spec.env,
            "autoStopInterval": auto_stop_minutes,
            "labels": {
                (PROVISIONING_OPERATION_LABEL): identity.operation_id.as_str(),
                (PROVISIONING_SPEC_LABEL): spec_fingerprint,
                (WORKSPACE_ID_LABEL): spec.workspace.workspace_id.to_string(),
                (TENANT_OWNER_LABEL): opaque_tenant_owner(&spec.workspace),
                (WRITER_EPOCH_LABEL): spec.workspace.writer_epoch.to_string(),
                (INSTANCE_GENERATION_LABEL): spec.workspace.instance_generation.to_string(),
            },
        });
        if let Some(storage) = storage {
            let locator = storage.workspace_locator.as_deref().ok_or_else(|| {
                MoaError::ValidationError(
                    "Daytona tenant volume is missing its opaque workspace subpath".to_string(),
                )
            })?;
            storage::validate_workspace_subpath(locator)?;
            let mount_path = spec.filesystem.mutable_root.to_str().ok_or_else(|| {
                MoaError::ValidationError(
                    "Daytona mutable root must be a valid UTF-8 sandbox path".to_string(),
                )
            })?;
            validate_daytona_mount_path(mount_path)?;
            let body = body.as_object_mut().ok_or_else(|| {
                MoaError::ProviderError("Daytona sandbox request body is not an object".to_string())
            })?;
            body.insert(
                "volumes".to_string(),
                serde_json::to_value([volume::DaytonaSandboxVolumeMount {
                    volume_id: &storage.resource_id,
                    mount_path,
                    subpath: locator,
                }])?,
            );
        }
        let response = attempt
            .client()
            .post(format!("{}/api/sandbox", attempt.origin()))
            .bearer_auth(attempt.credential())
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create Daytona sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "Daytona").await?;
        verify_created_workspace_identity(&value, identity)?;
        extract_workspace_id(&value)
    }

    async fn provisioning_storage(&self, spec: &HandSpec) -> Result<Option<ProviderStorageRef>> {
        let Some(dependencies) = self.storage.as_deref() else {
            return Ok(None);
        };
        let operation = moa_core::types::sandbox_workspace::WorkspaceStorageOperation {
            operation_id: moa_core::types::identifiers::WorkspaceOperationId(
                spec.provisioning_operation_id.0,
            ),
            kind: moa_core::types::sandbox_workspace::WorkspaceOperationKind::Attach,
            binding: spec.workspace.clone(),
            deadline: spec.budget.deadline.unwrap_or_else(chrono::Utc::now),
            request_hash: spec.effective_profile.profile_hash().to_string(),
        };
        self.mutable_storage_for_operation(dependencies, &operation)
            .await
            .map(Some)
    }

    async fn mutable_storage_for_operation(
        &self,
        dependencies: &storage::DaytonaStorageDependencies,
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
    ) -> Result<ProviderStorageRef> {
        let account = dependencies
            .config
            .account(operation.binding.provider_account_id)
            .ok_or_else(|| {
                MoaError::ConfigError(
                    "Daytona workspace account has no configured storage security class"
                        .to_string(),
                )
            })?;
        let generation =
            i64::try_from(operation.binding.provider_account_generation).map_err(|_| {
                MoaError::ValidationError(
                    "Daytona provider-account generation overflows bigint".to_string(),
                )
            })?;
        let resource = dependencies
            .storage_resources
            .live_tenant_volume(
                operation.binding.tenant_id,
                operation.binding.provider_account_id,
                generation,
                &account.security_class,
            )
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "Daytona hand provisioning requires a pre-admitted tenant volume".to_string(),
                )
            })?;
        if !matches!(
            resource.state,
            crate::core::sandbox_workspace::storage_resources::StorageResourceState::Ready
                | crate::core::sandbox_workspace::storage_resources::StorageResourceState::Attached
        ) {
            return Err(MoaError::StorageError(
                "Daytona tenant volume is not ready for a writable mount".to_string(),
            ));
        }
        let volume_id = resource.provider_reference.ok_or_else(|| {
            MoaError::StorageError(
                "Daytona ready tenant volume has no verified provider id".to_string(),
            )
        })?;
        storage::mutable_storage_reference(operation, volume_id)
    }

    async fn resolve_workspace(
        &self,
        attempt: &ProviderHttpAttempt,
        identity: &DaytonaProvisioningIdentity,
    ) -> Result<Option<String>> {
        let workspace_ids = self.provisioned_workspace_ids(attempt, identity).await?;
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
        attempt: &ProviderHttpAttempt,
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
            let url = build_url(
                &format!("{}/api/sandbox", attempt.origin()),
                &query,
                "Daytona",
            )?;
            let response = attempt
                .client()
                .get(url)
                .bearer_auth(attempt.credential())
                .send()
                .await
                .map_err(|error| {
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
                let normalized_state = state.to_ascii_lowercase();
                if matches!(
                    normalized_state.as_str(),
                    "destroyed" | "deleted" | "archived"
                ) {
                    continue;
                }
                if !NON_DESTROYED_SANDBOX_STATES.contains(&normalized_state.as_str()) {
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
        attempt: &ProviderHttpAttempt,
        identity: &DaytonaProvisioningIdentity,
        expected_workspace_id: Option<&str>,
    ) -> Result<Option<String>> {
        let started_at = Instant::now();
        for resolution_attempt in 0..PROVISION_RESOLVE_ATTEMPTS {
            let Some(remaining) = PROVISION_RESOLVE_TIMEOUT.checked_sub(started_at.elapsed())
            else {
                return Ok(None);
            };
            let workspace_id = timeout(remaining, self.resolve_workspace(attempt, identity))
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
            if resolution_attempt + 1 == PROVISION_RESOLVE_ATTEMPTS {
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
        attempt: &ProviderHttpAttempt,
        workspace_lookup: &str,
        started_at: Instant,
    ) -> Result<()> {
        loop {
            let remaining = remaining_destroy_time(started_at, workspace_lookup)?;
            let response = timeout(
                remaining,
                attempt
                    .client()
                    .get(format!(
                        "{}/api/sandbox/{workspace_lookup}",
                        attempt.origin()
                    ))
                    .bearer_auth(attempt.credential())
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
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        started_at: Instant,
    ) -> Result<Option<String>> {
        let remaining = remaining_destroy_time(started_at, workspace_id)?;
        let response = timeout(
            remaining,
            attempt
                .client()
                .get(format!("{}/api/sandbox/{workspace_id}", attempt.origin()))
                .bearer_auth(attempt.credential())
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
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<ToolOutput> {
        let timeout_secs = timeout.map(|timeout| timeout.as_secs());
        let started_at = Instant::now();
        let response = attempt
            .client()
            .post(format!(
                "{}/toolbox/{}/process/execute",
                attempt.origin(),
                workspace_id
            ))
            .bearer_auth(attempt.credential())
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

    async fn prepare_mutable_root(&self, handle: &HandHandle) -> Result<()> {
        self.resume(handle).await?;
        let workspace_id = handle.daytona_id()?;
        let (provider_account_id, provider_account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(
                provider_account_id,
                provider_account_generation,
                ProviderEndpoint::Toolbox,
            )
            .await?;
        let output = self
            .execute_command(
                &attempt,
                workspace_id,
                DAYTONA_PREPARE_MUTABLE_ROOT,
                None,
                Some(DEFAULT_COMMAND_TIMEOUT),
            )
            .await?;
        if output.is_error {
            return Err(MoaError::ProviderError(
                "Daytona could not prepare the mutable sandbox root".to_string(),
            ));
        }
        Ok(())
    }

    async fn read_file(
        &self,
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        path: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!(
                "{}/toolbox/{}/files/download",
                attempt.origin(),
                workspace_id
            ),
            &[("path", path)],
            "Daytona",
        )?;
        let response = attempt
            .client()
            .get(url)
            .bearer_auth(attempt.credential())
            .send()
            .await
            .map_err(|error| {
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
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        path: &str,
        remote_path: &str,
        content: &str,
    ) -> Result<ToolOutput> {
        let existing = match self.read_file(attempt, workspace_id, remote_path).await {
            Ok(output) => ExistingFileContent::Text(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => ExistingFileContent::Missing,
            Err(error) => return Err(error),
        };
        let duration = self
            .upload_file(attempt, workspace_id, remote_path, content.as_bytes())
            .await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        path: &str,
        content: &[u8],
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/toolbox/{}/files/upload", attempt.origin(), workspace_id),
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
        let response = attempt
            .client()
            .post(url)
            .bearer_auth(attempt.credential())
            .multipart(form)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to write Daytona file: {error}"))
            })?;
        expect_success(response).await?;
        Ok(started_at.elapsed())
    }

    async fn chmod_file(
        &self,
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        path: &str,
    ) -> Result<()> {
        let command = format!("chmod 755 {}", shell_quote(path));
        let output = self
            .execute_command(
                attempt,
                workspace_id,
                &command,
                None,
                Some(Duration::from_secs(30)),
            )
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
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        path: &str,
        remote_path: &str,
        input: &str,
    ) -> Result<ToolOutput> {
        let existing_content = match self.read_file(attempt, workspace_id, remote_path).await {
            Ok(output) => Some(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => None,
            Err(error) => return Err(error),
        };
        let planned = plan_str_replace(input, existing_content.as_deref(), path, 4)?;
        let duration = self
            .upload_file(
                attempt,
                workspace_id,
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

    async fn search_files(
        &self,
        attempt: &ProviderHttpAttempt,
        workspace_id: &str,
        pattern: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/toolbox/{}/files/search", attempt.origin(), workspace_id),
            &[("path", "/"), ("pattern", pattern)],
            "Daytona",
        )?;
        let response = attempt
            .client()
            .get(url)
            .bearer_auth(attempt.credential())
            .send()
            .await
            .map_err(|error| {
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
        attempt: &ProviderHttpAttempt,
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
                let trusted = resolve_trusted_skill_command(&params.cmd, DAYTONA_TRUSTED_ROOT)?;
                let command = trusted
                    .as_ref()
                    .map_or_else(|| params.cmd.clone(), |command| command.shell_token());
                let mut output = self
                    .execute_command(
                        attempt,
                        workspace_id,
                        &command,
                        None,
                        params.timeout_secs.map(|timeout| timeout.duration()),
                    )
                    .await?;
                if let Some(command) = trusted {
                    command.redact_output(&mut output);
                }
                Ok(output)
            }
            Some(SandboxToolCapability::Grep) => {
                let command = grep::remote_shell_command(input, "/")?;
                self.execute_command(attempt, workspace_id, &command, None, None)
                    .await
            }
            Some(SandboxToolCapability::FileOutline) => {
                let path = required_string_field(payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, "/workspace", DAYTONA_TRUSTED_ROOT)?;
                let content = self
                    .read_file(attempt, workspace_id, &remote_path)
                    .await?
                    .to_text();
                file_outline::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::FileRead) => {
                let path = required_string_field(payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, "/workspace", DAYTONA_TRUSTED_ROOT)?;
                let content = self
                    .read_file(attempt, workspace_id, &remote_path)
                    .await?
                    .to_text();
                file_read::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::StrReplace) => {
                let path = required_string_field(payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, "/workspace", DAYTONA_TRUSTED_ROOT)?;
                self.str_replace_file(attempt, workspace_id, path, &remote_path, input)
                    .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                let path = required_string_field(payload, "path")?;
                let remote_path =
                    resolve_provider_file_path(path, "/workspace", DAYTONA_TRUSTED_ROOT)?;
                self.write_file(
                    attempt,
                    workspace_id,
                    path,
                    &remote_path,
                    required_string_field(payload, "content")?,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                self.search_files(
                    attempt,
                    workspace_id,
                    required_string_field(payload, "pattern")?,
                )
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
        if spec.filesystem.mutable_root != std::path::Path::new("/workspace") {
            return Err(MoaError::ValidationError(
                "Daytona mutable root must be /workspace so volume mounts and checkpoints cannot include trusted or runtime state"
                    .to_string(),
            ));
        }
        let account_id = spec.workspace.provider_account_id;
        let account_generation = spec.workspace.provider_account_generation;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Api)
            .await?;
        let image = spec.image.clone().unwrap_or_else(|| {
            attempt
                .default_runtime()
                .unwrap_or(DEFAULT_DAYTONA_IMAGE)
                .to_string()
        });
        let auto_stop_minutes = daytona_auto_stop_minutes(spec.effective_profile.profile())?;
        let identity = DaytonaProvisioningIdentity::for_spec(&spec, &image, auto_stop_minutes)?;
        let workspace_storage = self.provisioning_storage(&spec).await?;
        let workspace_id = if let Some(workspace_id) =
            self.resolve_workspace(&attempt, &identity).await?
        {
            workspace_id
        } else {
            match self
                .create_workspace(
                    &attempt,
                    &spec,
                    &image,
                    auto_stop_minutes,
                    &identity,
                    workspace_storage.as_ref(),
                )
                .await
            {
                Ok(created_workspace_id) => self
                    .resolve_workspace_with_retries(
                        &attempt,
                        &identity,
                        Some(&created_workspace_id),
                    )
                    .await?
                    .ok_or_else(|| provision_resolution_timeout_error(&identity))?,
                Err(create_error) => match self
                    .resolve_workspace_with_retries(&attempt, &identity, None)
                    .await
                {
                    Ok(Some(workspace_id)) => workspace_id,
                    Ok(None) => return Err(create_error),
                    Err(resolve_error) => {
                        return Err(MoaError::ProviderError(format!(
                            "Daytona sandbox creation failed ({create_error}); resolving the durable operation also failed ({resolve_error})"
                        )));
                    }
                },
            }
        };
        let handle = HandHandle::daytona(workspace_id, account_id, account_generation);
        self.prepare_mutable_root(&handle).await?;
        Ok(handle)
    }

    async fn provisioned_hands(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        let attempt = self
            .attempt(
                provider_account_id,
                provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        let identity = DaytonaProvisioningIdentity::for_operation(operation_id);
        Ok(self
            .provisioned_workspace_ids(&attempt, &identity)
            .await?
            .into_iter()
            .map(|workspace_id| {
                HandHandle::daytona(
                    workspace_id,
                    provider_account_id,
                    provider_account_generation,
                )
            })
            .collect())
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let workspace_id = handle.daytona_id()?;
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Toolbox)
            .await?;
        let payload: Value = serde_json::from_str(input)?;
        // Attempt the tool directly rather than resuming on every call. The
        // sandbox is only probed and resumed after a failure, and only when it is
        // genuinely not running (so the tool never started); this keeps the happy
        // path free of a status()+resume() round trip while still recovering a
        // sandbox that auto-stopped between calls without risking a double run.
        match self
            .dispatch_tool(&attempt, workspace_id, tool, input, &payload)
            .await
        {
            Ok(output) => Ok(output),
            Err(error) => match self.status(handle).await {
                Ok(HandStatus::Stopped | HandStatus::Paused) => {
                    self.resume(handle).await?;
                    let attempt = self
                        .attempt(account_id, account_generation, ProviderEndpoint::Toolbox)
                        .await?;
                    self.dispatch_tool(&attempt, workspace_id, tool, input, &payload)
                        .await
                }
                _ => Err(error),
            },
        }
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        let workspace_id = handle.daytona_id()?;
        self.resume(handle).await?;
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Toolbox)
            .await?;
        let reset = self
            .execute_command(
                &attempt,
                workspace_id,
                DAYTONA_RESET_TRUSTED_ROOT,
                None,
                Some(DEFAULT_COMMAND_TIMEOUT),
            )
            .await?;
        if reset.is_error {
            return Err(MoaError::ProviderError(
                "Daytona could not reset the trusted sandbox root".to_string(),
            ));
        }
        for file in files {
            validate_sandbox_file_path(&file.path)?;
            let trusted_path = format!("{DAYTONA_TRUSTED_ROOT}/{}", file.path);
            self.upload_file(&attempt, workspace_id, &trusted_path, &file.content)
                .await?;
            if file.executable {
                self.chmod_file(&attempt, workspace_id, &trusted_path)
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
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Api)
            .await?;
        let response = attempt
            .client()
            .get(format!("{}/api/sandbox/{workspace_id}", attempt.origin()))
            .bearer_auth(attempt.credential())
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
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Api)
            .await?;
        let response = attempt
            .client()
            .post(format!(
                "{}/api/sandbox/{workspace_id}/stop",
                attempt.origin()
            ))
            .bearer_auth(attempt.credential())
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
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Api)
            .await?;
        let response = attempt
            .client()
            .post(format!(
                "{}/api/sandbox/{workspace_id}/start",
                attempt.origin()
            ))
            .bearer_auth(attempt.credential())
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
        let (account_id, account_generation) = cloud_account(handle, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Api)
            .await?;
        let started_at = Instant::now();
        let Some(workspace_lookup) = self
            .workspace_deletion_lookup(&attempt, workspace_id, started_at)
            .await?
        else {
            return Ok(());
        };
        loop {
            let remaining = remaining_destroy_time(started_at, workspace_id)?;
            let response = timeout(
                remaining,
                attempt
                    .client()
                    .delete(format!("{}/api/sandbox/{workspace_id}", attempt.origin()))
                    .bearer_auth(attempt.credential())
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
                    .wait_until_workspace_absent(&attempt, &workspace_lookup, started_at)
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

fn cloud_account(
    handle: &HandHandle,
    provider: &str,
) -> Result<(moa_core::types::identifiers::ProviderAccountId, u64)> {
    handle.provider_account().ok_or_else(|| {
        MoaError::ValidationError(format!(
            "non-{provider} hand handle passed to {provider} provider"
        ))
    })
}

fn opaque_tenant_owner(binding: &moa_core::types::sandbox_workspace::WorkspaceBinding) -> String {
    let mut digest = Sha256::new();
    digest.update(b"moa/daytona/tenant-owner/v1\0");
    digest.update(binding.tenant_id.0.as_bytes());
    digest.update(binding.provider_account_id.0.as_bytes());
    digest.update(binding.provider_account_generation.to_be_bytes());
    hex::encode(digest.finalize())
}

fn tenant_volume_name(
    tenant_id: moa_core::types::identifiers::TenantId,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
    security_class: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"moa/daytona/tenant-volume/v1\0");
    digest.update(tenant_id.0.as_bytes());
    digest.update(provider_account_id.0.as_bytes());
    digest.update(security_class.as_bytes());
    format!("moa-tv-{}", &hex::encode(digest.finalize())[..40])
}

fn enforce_volume_headroom(observed: usize, ceiling: usize, headroom: usize) -> Result<()> {
    let admitted = observed
        .checked_add(headroom)
        .and_then(|used| used.checked_add(1))
        .is_some_and(|next| next <= ceiling);
    if !admitted {
        return Err(MoaError::ProviderError(
            "Daytona tenant-volume admission exhausted configured organization headroom"
                .to_string(),
        ));
    }
    Ok(())
}

fn verify_request_resources(
    operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
    hand: Option<&HandHandle>,
    storage: Option<&ProviderStorageRef>,
) -> Result<()> {
    if operation.request_hash.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "Daytona operation does not match its workspace account fence".to_string(),
        ));
    }
    if let Some((account, generation)) = hand.and_then(HandHandle::provider_account)
        && (account != operation.binding.provider_account_id
            || generation != operation.binding.provider_account_generation)
    {
        return Err(MoaError::ValidationError(
            "Daytona hand does not match its workspace account fence".to_string(),
        ));
    }
    if let Some(storage) = storage {
        use moa_core::types::sandbox_workspace::ProviderStorageKind;

        let locator_matches = match storage.kind {
            ProviderStorageKind::MutableFilesystem => {
                storage.workspace_locator.as_deref()
                    == Some(storage::workspace_subpath(operation).as_str())
                    || (operation.kind
                        == moa_core::types::sandbox_workspace::WorkspaceOperationKind::Delete
                        && storage.workspace_locator.is_none())
            }
            ProviderStorageKind::PortableCheckpoint => storage.workspace_locator.is_none(),
        };
        if storage.provider_account_id != operation.binding.provider_account_id
            || storage.provider_account_generation != operation.binding.provider_account_generation
            || !locator_matches
        {
            return Err(MoaError::ValidationError(
                "Daytona storage does not match its workspace binding".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_daytona_mount_path(path: &str) -> Result<()> {
    const FORBIDDEN: &[&str] = &[
        "/proc", "/sys", "/dev", "/boot", "/etc", "/bin", "/sbin", "/lib", "/lib64",
    ];
    if !path.starts_with('/')
        || path == "/"
        || path.contains("//")
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
        || FORBIDDEN
            .iter()
            .any(|root| path == *root || path.starts_with(&format!("{root}/")))
    {
        return Err(MoaError::ValidationError(
            "Daytona volume mount path is not a safe dedicated data root".to_string(),
        ));
    }
    Ok(())
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
