//! Local hand provider with direct host execution and optional Docker sandboxes.

mod workspace;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError,
    error::Result,
    error::ToolFailureClass,
    error::classify_tool_error,
    traits::{HandProvider, SandboxStorageProvider},
    types::hands::CpuLimit,
    types::hands::DeadlineEnforcement,
    types::hands::DiskLimit,
    types::hands::EgressMode,
    types::hands::EgressPolicy,
    types::hands::HandHandle,
    types::hands::HandProviderCapabilities,
    types::hands::HandSpec,
    types::hands::HandStatus,
    types::hands::MemoryLimit,
    types::hands::ResourceSupport,
    types::hands::SandboxFile,
    types::hands::SandboxProfile,
    types::hands::SandboxTier,
    types::hands::SandboxTierCapabilities,
    types::hands::validate_sandbox_file_path,
    types::identifiers::{HandProvisioningOperationId, WorkspaceCheckpointId},
    types::resource::ResourceBudget,
    types::sandbox_workspace::{
        ProviderAccountStorageInventory, ProviderInventoryOwner, ProviderInventoryResource,
        ProviderInventoryResourceKind, ProviderStorageKind, ProviderStorageRef,
        TenantStoragePurgeRequest, WorkspaceAttachRequest, WorkspaceCheckpointPublication,
        WorkspaceCheckpointPublishRequest, WorkspaceConfirmedDisposition, WorkspaceOperationKind,
        WorkspaceOperationOutcome, WorkspacePostCommitState, WorkspaceReconcileRequest,
        WorkspaceRestoreRequest, WorkspaceRevisionRef, WorkspaceStorageDeleteRequest,
        WorkspaceStorageOperationResult, WorkspaceStoragePrepareRequest,
    },
    types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use opentelemetry::trace::Status;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::adapters::trusted_command::{
    normalized_trusted_skill_path, resolve_trusted_skill_command, rewrite_bash_input,
};
use crate::core::leases::LeaseHandle;
use crate::core::sandbox_workspace::capacity::PostgresWorkspaceCapacityRepository;
use crate::core::sandbox_workspace::checkpoint::archive::build_checkpoint_archive;
use crate::core::sandbox_workspace::checkpoint::revision::{
    next_workspace_revision, required_current_revision,
};
use crate::core::sandbox_workspace::checkpoint::store::{
    CheckpointObjectStore, CheckpointStoreContext,
};
use crate::tools::sandbox_descriptor::{SandboxToolCapability, supported_capability_for_tool};
use crate::tools::{bash, file_outline, file_read, file_search, file_write, grep, str_replace};

const LOCAL_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_DOCKER_IMAGE: &str = "alpine:3.20";
const DEFAULT_TOOL_TIMEOUT: Duration = crate::tools::bash::DEFAULT_BASH_TIMEOUT;
const DOCKER_DETECTION_TIMEOUT: Duration = Duration::from_secs(2);
const DOCKER_TMPFS_OPTIONS: &str = "rw,nosuid,nodev,size=64m";
const HAND_SANDBOX_PREFIX: &str = "hand-";
const HAND_INTENT_DIRECTORY: &str = ".moa-hand-intents";
const HAND_TRUSTED_DIRECTORY: &str = ".moa-hand-trusted";
const HAND_INTENT_MARKER_SUFFIX: &str = ".json";
const DOCKER_NAME_PREFIX: &str = "moa-hand-";
const DOCKER_OPERATION_LABEL: &str = "com.moa.provisioning-operation-id";
const DOCKER_TRUSTED_ROOT: &str = "/opt/moa/trusted";
const DOCKER_SPEC_LABEL: &str = "com.moa.provisioning-spec-sha256";
const TOOL_ERROR_OUTPUT_STATUS: &str = "tool returned error output";
const TOOL_EXECUTION_FAILED_STATUS: &str = "tool execution failed";

/// Optional Docker seccomp profile path, resolved from the environment once.
static DOCKER_SECCOMP_PROFILE: LazyLock<Option<String>> =
    LazyLock::new(|| std::env::var("MOA_DOCKER_SECCOMP_PROFILE").ok());

/// Revision of the local provider's capability declaration.
///
/// Bump this whenever what the local provider can enforce changes. It is
/// hash-significant, so a bump makes every sandbox provisioned under the old
/// declaration unreusable rather than silently reinterpreted.
pub const LOCAL_CAPABILITIES_REVISION: &str = "local-hands-v1";

/// What the local provider can enforce, per tier.
///
/// The two tiers differ sharply and are declared separately. A bare host
/// process has no CPU, memory, disk, or network enforcement at all, so it
/// admits only explicit `Unbounded` dimensions and `Unrestricted` egress —
/// stating that plainly is what keeps a bounded cloud policy from silently
/// degrading into an unrestricted host process. A Docker container maps CPU,
/// memory, and the deny-all/unrestricted network postures, which are the ones
/// `docker run` actually enforces, and refuses ephemeral-disk bounds and egress
/// allowlists, which it does not: `--storage-opt size=` is unsupported on the
/// default overlay2 driver, and `docker run` has no per-destination filter.
/// Neither tier has any deadline of its own, so both name the durable reaper.
pub static LOCAL_HAND_CAPABILITIES: LazyLock<HandProviderCapabilities> =
    LazyLock::new(|| HandProviderCapabilities {
        revision: LOCAL_CAPABILITIES_REVISION.to_string(),
        tiers: vec![
            host_tier_capabilities(SandboxTier::Local),
            host_tier_capabilities(SandboxTier::None),
            SandboxTierCapabilities {
                tier: SandboxTier::Container,
                cpu: docker_cpu_support(),
                memory: docker_memory_support(),
                ephemeral_disk: ResourceSupport {
                    allows_unbounded: true,
                    bounded: None,
                },
                egress_modes: vec![EgressMode::DenyAll, EgressMode::Unrestricted],
                idle_enforcement: DeadlineEnforcement::DurableReaper,
                max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
            },
        ],
    });

/// Capabilities for a tier executed directly on the host.
fn host_tier_capabilities(tier: SandboxTier) -> SandboxTierCapabilities {
    SandboxTierCapabilities {
        tier,
        cpu: ResourceSupport::unbounded_only(),
        memory: ResourceSupport::unbounded_only(),
        ephemeral_disk: ResourceSupport::unbounded_only(),
        egress_modes: vec![EgressMode::Unrestricted],
        idle_enforcement: DeadlineEnforcement::DurableReaper,
        max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
    }
}

/// Builds a bounded resource range from compile-time constants.
///
/// The bounds below are literals chosen to satisfy `bounded_range`'s nonzero
/// and min-at-or-below-max contract, so a failure here would be a bug in this
/// file rather than a runtime condition. Falling back to "cannot bound this
/// dimension" keeps that bug fail-closed: the provider would refuse bounded
/// requests instead of accepting ones it might mistranslate.
fn docker_range(min: u32, max: u32, granularity: u32) -> ResourceSupport {
    match ResourceSupport::bounded_range(min, max, granularity) {
        Ok(support) => support,
        Err(error) => {
            tracing::error!(
                error = %error,
                "local Docker capability range is misconfigured; refusing bounded requests"
            );
            ResourceSupport {
                allows_unbounded: true,
                bounded: None,
            }
        }
    }
}

/// Docker's `--cpus` range, in millicores.
fn docker_cpu_support() -> ResourceSupport {
    docker_range(10, 64_000, 10)
}

/// Docker's `--memory` range, in mebibytes. Docker's own floor is 6 MiB.
fn docker_memory_support() -> ResourceSupport {
    docker_range(16, 1_048_576, 1)
}

#[derive(Debug, Clone)]
struct LocalSandbox {
    execution_root: PathBuf,
    trusted_root: PathBuf,
    /// Absolute instant this sandbox's maximum lifetime expires.
    ///
    /// `None` means the resolved profile declared an explicitly unbounded
    /// maximum lifetime, which is a statement, not a missing value.
    hard_deadline: Option<DateTime<Utc>>,
    inventory_identity: Option<LocalInventoryIdentity>,
}

#[derive(Debug, Clone)]
struct DockerSandbox {
    sandbox_dir: PathBuf,
    trusted_root: PathBuf,
    mutable_root: String,
    /// Absolute instant this container's maximum lifetime expires.
    hard_deadline: Option<DateTime<Utc>>,
    inventory_identity: Option<LocalInventoryIdentity>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LocalInventoryIdentity {
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
    provider_account_generation: u64,
    owner: ProviderInventoryOwner,
}

impl LocalInventoryIdentity {
    fn from_spec(spec: &HandSpec) -> Self {
        Self {
            provider_account_id: spec.workspace.provider_account_id,
            provider_account_generation: spec.workspace.provider_account_generation,
            owner: ProviderInventoryOwner {
                tenant_id: spec.workspace.tenant_id,
                workspace_id: spec.workspace.workspace_id,
                provisioning_operation_id: Some(spec.provisioning_operation_id),
                writer_epoch: Some(spec.workspace.writer_epoch),
                instance_generation: Some(spec.workspace.instance_generation),
            },
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalProvisioningMarker {
    operation_id: HandProvisioningOperationId,
    spec_fingerprint: String,
    sandbox_tier: SandboxTier,
    mutable_root: PathBuf,
    hard_deadline: Option<DateTime<Utc>>,
    inventory_identity: LocalInventoryIdentity,
}

#[derive(serde::Serialize)]
struct LocalCreationSpecIdentity<'a> {
    sandbox_tier: SandboxTier,
    image: &'a Option<String>,
    env: Vec<(&'a str, &'a str)>,
    filesystem: &'a moa_core::types::sandbox_workspace::SandboxFilesystemLayout,
    effective_profile_hash: &'a str,
    workspace: &'a moa_core::types::sandbox_workspace::WorkspaceBinding,
}

#[derive(Debug)]
struct DockerInspection {
    container_id: String,
    operation_id: String,
    spec_fingerprint: String,
}

/// Returns the absolute instant a profile's maximum lifetime expires.
///
/// Recorded as wall-clock rather than a monotonic instant so it survives the
/// durable lease round trip: a sandbox recovered by another replica has to
/// honour the deadline the original provisioning established, not restart it.
/// Recovers a persisted sandbox deadline from durable lease metadata.
///
/// An absent field means the sandbox was provisioned unbounded. A present but
/// unparseable field is treated as already expired rather than unbounded: a
/// deadline nobody can read is not a licence to run forever.
fn hard_deadline_from_metadata(metadata: &serde_json::Value) -> Option<DateTime<Utc>> {
    let value = metadata.get("hard_deadline")?;
    if value.is_null() {
        return None;
    }
    match serde_json::from_value::<DateTime<Utc>>(value.clone()) {
        Ok(deadline) => Some(deadline),
        Err(error) => {
            tracing::warn!(
                %error,
                "unreadable hand lease deadline; treating the sandbox as expired"
            );
            Some(DateTime::<Utc>::UNIX_EPOCH)
        }
    }
}

fn inventory_identity_from_metadata(
    metadata: &serde_json::Value,
) -> Result<LocalInventoryIdentity> {
    serde_json::from_value(metadata.get("inventory_identity").cloned().ok_or_else(|| {
        MoaError::ProviderError(
            "local hand lease is missing verified inventory identity".to_string(),
        )
    })?)
    .map_err(|error| {
        MoaError::ProviderError(format!(
            "local hand lease has malformed inventory identity: {error}"
        ))
    })
}

fn hard_deadline_for(profile: &SandboxProfile) -> Option<DateTime<Utc>> {
    profile
        .max_lifetime
        .bounded_seconds()
        .and_then(|seconds| i64::try_from(seconds.get()).ok())
        .and_then(|seconds| Utc::now().checked_add_signed(chrono::TimeDelta::seconds(seconds)))
}

/// Local zero-setup hand provider used by interactive clients and test harnesses.
#[derive(Clone)]
pub struct LocalHandProvider {
    work_dir: Arc<PathBuf>,
    docker_reconciliation_enabled: bool,
    docker_available: bool,
    command_timeout: Duration,
    local_sandboxes: Arc<RwLock<HashMap<PathBuf, LocalSandbox>>>,
    docker_sandboxes: Arc<RwLock<HashMap<String, DockerSandbox>>>,
    checkpoint_store: Option<Arc<CheckpointObjectStore>>,
    checkpoint_capacity: Option<Arc<PostgresWorkspaceCapacityRepository>>,
}

impl LocalHandProvider {
    /// Creates a new local hand provider rooted at a sandbox work directory.
    pub async fn new(work_dir: impl AsRef<Path>) -> Result<Self> {
        Self::new_with_docker_detection(work_dir, true).await
    }

    /// Creates a new local hand provider with optional Docker detection.
    pub async fn new_with_docker_detection(
        work_dir: impl AsRef<Path>,
        detect_docker_availability: bool,
    ) -> Result<Self> {
        let work_dir = work_dir.as_ref().to_path_buf();
        fs::create_dir_all(&work_dir).await?;

        Ok(Self {
            work_dir: Arc::new(work_dir),
            docker_reconciliation_enabled: detect_docker_availability,
            docker_available: if detect_docker_availability {
                detect_docker().await
            } else {
                false
            },
            command_timeout: DEFAULT_TOOL_TIMEOUT,
            local_sandboxes: Arc::new(RwLock::new(HashMap::new())),
            docker_sandboxes: Arc::new(RwLock::new(HashMap::new())),
            checkpoint_store: None,
            checkpoint_capacity: None,
        })
    }

    /// Returns whether Docker was detected on the current machine.
    pub fn docker_available(&self) -> bool {
        self.docker_available
    }

    /// Overrides the default per-tool timeout.
    #[must_use]
    pub fn with_command_timeout(mut self, command_timeout: Duration) -> Self {
        self.command_timeout = command_timeout;
        self
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

    fn sandbox_dir(&self, operation_id: HandProvisioningOperationId) -> PathBuf {
        self.work_dir
            .join(format!("{HAND_SANDBOX_PREFIX}{operation_id}"))
    }

    fn trusted_dir(&self, operation_id: HandProvisioningOperationId) -> PathBuf {
        self.work_dir
            .join(HAND_TRUSTED_DIRECTORY)
            .join(operation_id.to_string())
    }

    fn intent_marker_path(&self, operation_id: HandProvisioningOperationId) -> PathBuf {
        self.work_dir
            .join(HAND_INTENT_DIRECTORY)
            .join(format!("{operation_id}{HAND_INTENT_MARKER_SUFFIX}"))
    }

    fn docker_name(operation_id: HandProvisioningOperationId) -> String {
        format!("{DOCKER_NAME_PREFIX}{operation_id}")
    }

    fn creation_fingerprint(spec: &HandSpec) -> Result<String> {
        let mut env = spec
            .env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        env.sort_unstable();
        let identity = LocalCreationSpecIdentity {
            sandbox_tier: spec.sandbox_tier,
            image: &spec.image,
            env,
            filesystem: &spec.filesystem,
            effective_profile_hash: spec.effective_profile.profile_hash(),
            workspace: &spec.workspace,
        };
        let digest = Sha256::digest(serde_json::to_vec(&identity)?);
        Ok(hex::encode(digest))
    }

    async fn publish_intent_marker(&self, marker: &LocalProvisioningMarker) -> Result<bool> {
        let marker_path = self.intent_marker_path(marker.operation_id);
        let parent = marker_path.parent().ok_or_else(|| {
            MoaError::ProviderError("local hand intent marker has no parent directory".to_string())
        })?;
        fs::create_dir_all(parent).await?;
        let temporary_path = marker_path.with_extension(format!(
            "{HAND_INTENT_MARKER_SUFFIX}.{}.tmp",
            Uuid::now_v7()
        ));
        fs::write(&temporary_path, serde_json::to_vec(marker)?).await?;
        let published = match fs::hard_link(&temporary_path, &marker_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => {
                let _ = fs::remove_file(&temporary_path).await;
                return Err(error.into());
            }
        };
        if let Err(error) = fs::remove_file(&temporary_path).await {
            tracing::warn!(
                %error,
                path = %temporary_path.display(),
                "failed to remove temporary local hand intent marker"
            );
        }
        Ok(published)
    }

    async fn read_intent_marker(
        &self,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Option<LocalProvisioningMarker>> {
        let marker_path = self.intent_marker_path(operation_id);
        match fs::read(&marker_path).await {
            Ok(bytes) => {
                let marker: LocalProvisioningMarker = serde_json::from_slice(&bytes)?;
                if marker.operation_id != operation_id {
                    return Err(MoaError::ProviderError(format!(
                        "local hand intent marker {} belongs to operation {} instead of {operation_id}",
                        marker_path.display(),
                        marker.operation_id
                    )));
                }
                Ok(Some(marker))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn prepare_sandbox_dir(
        &self,
        spec: &HandSpec,
    ) -> Result<(PathBuf, LocalProvisioningMarker)> {
        let sandbox_dir = self.sandbox_dir(spec.provisioning_operation_id);
        let expected_fingerprint = Self::creation_fingerprint(spec)?;
        let marker = match self
            .read_intent_marker(spec.provisioning_operation_id)
            .await?
        {
            Some(marker) => {
                if marker.spec_fingerprint != expected_fingerprint {
                    return Err(MoaError::ProviderError(format!(
                        "hand provisioning operation {} was reused with a different creation spec",
                        spec.provisioning_operation_id
                    )));
                }
                marker
            }
            None => {
                if fs::try_exists(&sandbox_dir).await? {
                    return Err(MoaError::ProviderError(format!(
                        "deterministic sandbox path {} exists without its provisioning intent marker",
                        sandbox_dir.display()
                    )));
                }
                let marker = LocalProvisioningMarker {
                    operation_id: spec.provisioning_operation_id,
                    spec_fingerprint: expected_fingerprint,
                    sandbox_tier: spec.sandbox_tier,
                    mutable_root: spec.filesystem.mutable_root.clone(),
                    hard_deadline: hard_deadline_for(spec.effective_profile.profile()),
                    inventory_identity: LocalInventoryIdentity::from_spec(spec),
                };
                if self.publish_intent_marker(&marker).await? {
                    marker
                } else {
                    let winner = self
                        .read_intent_marker(spec.provisioning_operation_id)
                        .await?
                        .ok_or_else(|| {
                            MoaError::ProviderError(format!(
                                "hand provisioning intent {} disappeared during concurrent publication",
                                spec.provisioning_operation_id
                            ))
                        })?;
                    if winner.spec_fingerprint != marker.spec_fingerprint {
                        return Err(MoaError::ProviderError(format!(
                            "hand provisioning operation {} was concurrently reused with a different creation spec",
                            spec.provisioning_operation_id
                        )));
                    }
                    winner
                }
            }
        };

        match fs::create_dir(&sandbox_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        fs::set_permissions(&sandbox_dir, std::fs::Permissions::from_mode(0o770)).await?;
        let trusted_dir = self.trusted_dir(spec.provisioning_operation_id);
        fs::create_dir_all(&trusted_dir).await?;
        #[cfg(unix)]
        fs::set_permissions(&trusted_dir, std::fs::Permissions::from_mode(0o700)).await?;
        Ok((sandbox_dir, marker))
    }

    async fn register_docker_sandbox(
        &self,
        container_id: String,
        sandbox_dir: &Path,
        workspace_mount: &Path,
        hard_deadline: Option<DateTime<Utc>>,
        inventory_identity: &LocalInventoryIdentity,
    ) -> Result<HandHandle> {
        self.docker_sandboxes.write().await.insert(
            container_id.clone(),
            DockerSandbox {
                sandbox_dir: sandbox_dir.to_path_buf(),
                trusted_root: self.trusted_dir(
                    operation_id_from_sandbox_dir(sandbox_dir).ok_or_else(|| {
                        MoaError::ProviderError(
                            "Docker sandbox directory has no provisioning operation identity"
                                .to_string(),
                        )
                    })?,
                ),
                mutable_root: workspace_mount.to_string_lossy().into_owned(),
                hard_deadline,
                inventory_identity: Some(inventory_identity.clone()),
            },
        );
        Ok(HandHandle::docker(container_id))
    }

    async fn docker_containers_for_operation(
        &self,
        operation_id: HandProvisioningOperationId,
    ) -> Result<BTreeSet<String>> {
        if !self.docker_reconciliation_enabled {
            return Err(MoaError::ProviderError(format!(
                "cannot reconcile Docker hand provisioning operation {operation_id} because Docker reconciliation is disabled"
            )));
        }
        let expected_name = Self::docker_name(operation_id);
        let mut container_ids = BTreeSet::new();
        if let Some(inspection) = inspect_docker_container(&expected_name).await? {
            validate_docker_operation(&inspection, operation_id)?;
            container_ids.insert(inspection.container_id);
        }
        for container_id in list_docker_containers(operation_id).await? {
            let inspection = inspect_docker_container(&container_id)
                .await?
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "Docker listed hand container {container_id} but it disappeared before inspection"
                    ))
                })?;
            validate_docker_operation(&inspection, operation_id)?;
            container_ids.insert(inspection.container_id);
        }
        Ok(container_ids)
    }

    async fn resolve_docker_sandbox(
        &self,
        container_name: &str,
        spec: &HandSpec,
        marker: &LocalProvisioningMarker,
        sandbox_dir: &Path,
        workspace_mount: &Path,
    ) -> Result<Option<HandHandle>> {
        let Some(inspection) = inspect_docker_container(container_name).await? else {
            return Ok(None);
        };
        validate_docker_operation(&inspection, spec.provisioning_operation_id)?;
        validate_docker_spec(&inspection, &marker.spec_fingerprint)?;
        Ok(Some(
            self.register_docker_sandbox(
                inspection.container_id,
                sandbox_dir,
                workspace_mount,
                marker.hard_deadline,
                &marker.inventory_identity,
            )
            .await?,
        ))
    }

    async fn provision_docker(
        &self,
        spec: &HandSpec,
        sandbox_dir: &Path,
        marker: &LocalProvisioningMarker,
    ) -> Result<HandHandle> {
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_IMAGE.to_string());
        let workspace_mount = spec.filesystem.mutable_root.clone();
        let mount = format!("{}:{}", sandbox_dir.display(), workspace_mount.display());
        let container_name = Self::docker_name(spec.provisioning_operation_id);
        if let Some(handle) = self
            .resolve_docker_sandbox(&container_name, spec, marker, sandbox_dir, &workspace_mount)
            .await?
        {
            return Ok(handle);
        }
        let user = docker_user_spec().await;
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "--label".to_string(),
            format!(
                "{DOCKER_OPERATION_LABEL}={}",
                spec.provisioning_operation_id
            ),
            "--label".to_string(),
            format!("{DOCKER_SPEC_LABEL}={}", marker.spec_fingerprint),
            "--user".to_string(),
            user,
            "--read-only".to_string(),
            "--workdir".to_string(),
            workspace_mount.display().to_string(),
            "--tmpfs".to_string(),
            format!("/tmp:{DOCKER_TMPFS_OPTIONS}"),
            "--tmpfs".to_string(),
            format!("/run:{DOCKER_TMPFS_OPTIONS}"),
            "--tmpfs".to_string(),
            "/opt/moa/trusted:rw,nosuid,nodev,size=64m,mode=0755".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges:true".to_string(),
            "--pids-limit".to_string(),
            "256".to_string(),
            "-v".to_string(),
            mount,
        ];
        args.extend(docker_profile_args(spec.effective_profile.profile())?);
        if let Some(profile) = DOCKER_SECCOMP_PROFILE.as_ref() {
            args.push("--security-opt".to_string());
            args.push(format!("seccomp={profile}"));
        }
        args.extend([
            image,
            "sh".to_string(),
            "-lc".to_string(),
            "trap : TERM INT; while sleep 3600; do :; done".to_string(),
        ]);
        let output = Command::new("docker").args(&args).output().await?;
        if !output.status.success() {
            if let Some(handle) = self
                .resolve_docker_sandbox(
                    &container_name,
                    spec,
                    marker,
                    sandbox_dir,
                    &workspace_mount,
                )
                .await?
            {
                return Ok(handle);
            }
            return Err(MoaError::ProviderError(format!(
                "failed to start docker sandbox: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        self.register_docker_sandbox(
            container_id,
            sandbox_dir,
            &workspace_mount,
            marker.hard_deadline,
            &marker.inventory_identity,
        )
        .await
    }

    async fn provision_local(
        &self,
        spec: &HandSpec,
        sandbox_dir: PathBuf,
        marker: &LocalProvisioningMarker,
    ) -> Result<HandHandle> {
        reject_unenforceable_host_profile(spec.effective_profile.profile())?;
        let execution_root = sandbox_dir.clone();
        self.local_sandboxes.write().await.insert(
            sandbox_dir.clone(),
            LocalSandbox {
                execution_root,
                trusted_root: self.trusted_dir(spec.provisioning_operation_id),
                hard_deadline: marker.hard_deadline,
                inventory_identity: Some(marker.inventory_identity.clone()),
            },
        );
        Ok(HandHandle::local(sandbox_dir))
    }

    async fn resolve_local_sandbox(&self, sandbox_dir: &Path) -> LocalSandbox {
        self.local_sandboxes
            .read()
            .await
            .get(sandbox_dir)
            .cloned()
            .unwrap_or_else(|| LocalSandbox {
                execution_root: sandbox_dir.to_path_buf(),
                trusted_root: operation_id_from_sandbox_dir(sandbox_dir)
                    .map(|operation_id| self.trusted_dir(operation_id))
                    .unwrap_or_else(|| self.work_dir.join(HAND_TRUSTED_DIRECTORY).join("unknown")),
                hard_deadline: None,
                inventory_identity: None,
            })
    }

    async fn execute_local_tool(
        &self,
        sandbox_dir: &Path,
        tool: &str,
        input: &str,
        hard_cancel_token: Option<&CancellationToken>,
        budget: ResourceBudget,
    ) -> Result<ToolOutput> {
        let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
        let now = Utc::now();
        match supported_capability_for_tool(tool, LOCAL_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                let params = bash::BashToolInput::parse(input)?;
                let trusted_root = sandbox.trusted_root.to_str().ok_or_else(|| {
                    MoaError::ProviderError(
                        "local trusted sandbox root is not valid UTF-8".to_string(),
                    )
                })?;
                let trusted = resolve_trusted_skill_command(&params.cmd, trusted_root)?;
                let rewritten;
                let execution_input = if let Some(command) = trusted.as_ref() {
                    rewritten = rewrite_bash_input(input, command)?;
                    rewritten.as_str()
                } else {
                    input
                };
                let mut output = bash::execute_local(
                    &sandbox.execution_root,
                    execution_input,
                    self.command_timeout,
                    bash::remaining_lifetime(sandbox.hard_deadline, now),
                    budget.time_remaining(now),
                    hard_cancel_token,
                )
                .await?;
                if let Some(command) = trusted {
                    command.redact_output(&mut output);
                }
                Ok(output)
            }
            Some(SandboxToolCapability::Grep) => {
                grep::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileOutline) => {
                file_outline::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileRead) => {
                let root = if file_read_targets_trusted_skill(input)? {
                    &sandbox.trusted_root
                } else {
                    &sandbox.execution_root
                };
                file_read::execute(root, input).await
            }
            Some(SandboxToolCapability::StrReplace) => {
                str_replace::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileWrite) => {
                file_write::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileSearch) => {
                file_search::execute(&sandbox.execution_root, input).await
            }
            None => Err(MoaError::ToolError(format!(
                "unsupported local hand tool: {tool}"
            ))),
        }
    }

    async fn execute_docker_tool(
        &self,
        container_id: &str,
        tool: &str,
        input: &str,
        hard_cancel_token: Option<&CancellationToken>,
        budget: ResourceBudget,
    ) -> Result<ToolOutput> {
        let sandbox = self
            .docker_sandboxes
            .read()
            .await
            .get(container_id)
            .cloned()
            .ok_or_else(|| {
                MoaError::ProviderError(format!("unknown docker sandbox handle: {container_id}"))
            })?;

        match supported_capability_for_tool(tool, LOCAL_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                let params = bash::BashToolInput::parse(input)?;
                let trusted = resolve_trusted_skill_command(&params.cmd, DOCKER_TRUSTED_ROOT)?;
                let rewritten;
                let execution_input = if let Some(command) = trusted.as_ref() {
                    rewritten = rewrite_bash_input(input, command)?;
                    rewritten.as_str()
                } else {
                    input
                };
                let mut output = bash::execute_docker(
                    container_id,
                    &sandbox.mutable_root,
                    execution_input,
                    self.command_timeout,
                    bash::remaining_lifetime(sandbox.hard_deadline, Utc::now()),
                    budget.time_remaining(Utc::now()),
                    hard_cancel_token,
                )
                .await?;
                if let Some(command) = trusted {
                    command.redact_output(&mut output);
                }
                Ok(output)
            }
            Some(SandboxToolCapability::Grep) => grep::execute(&sandbox.sandbox_dir, input).await,
            Some(SandboxToolCapability::FileOutline) => {
                file_outline::execute_docker(
                    container_id,
                    &sandbox.mutable_root,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::FileRead) => {
                if file_read_targets_trusted_skill(input)? {
                    file_read::execute(&sandbox.trusted_root, input).await
                } else {
                    file_read::execute_docker_bind_mount(
                        &sandbox.sandbox_dir,
                        &sandbox.mutable_root,
                        input,
                    )
                    .await
                }
            }
            Some(SandboxToolCapability::StrReplace) => {
                str_replace::execute_docker(
                    container_id,
                    &sandbox.mutable_root,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                file_write::execute_docker_bind_mount(
                    &sandbox.sandbox_dir,
                    &sandbox.mutable_root,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                file_search::execute_docker(
                    container_id,
                    &sandbox.mutable_root,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            None => Err(MoaError::ToolError(format!(
                "tool {tool} not supported in Docker mode"
            ))),
        }
    }

    async fn destroy_local_sandbox(&self, sandbox_dir: &Path) -> Result<()> {
        let operation_id = operation_id_from_sandbox_dir(sandbox_dir);
        if let Some(operation_id) = operation_id
            && self
                .read_intent_marker(operation_id)
                .await?
                .is_some_and(|marker| marker.sandbox_tier == SandboxTier::Container)
        {
            let remaining = self.docker_containers_for_operation(operation_id).await?;
            if !remaining.is_empty() {
                return Err(MoaError::ProviderError(format!(
                    "refusing to remove local state for hand provisioning operation {operation_id} while {} Docker container(s) remain",
                    remaining.len()
                )));
            }
        }
        match fs::remove_dir_all(sandbox_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(operation_id) = operation_id {
            match fs::remove_dir_all(self.trusted_dir(operation_id)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let marker_path = self.intent_marker_path(operation_id);
            match fs::remove_file(marker_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    async fn install_files_at_root(&self, root: &Path, files: &[SandboxFile]) -> Result<()> {
        for file in files {
            let target = sandbox_install_path(root, &file.path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&target, &file.content).await?;
            if file.executable {
                set_executable(&target).await?;
            }
        }
        Ok(())
    }

    async fn replace_trusted_files_at_root(
        &self,
        root: &Path,
        files: &[SandboxFile],
    ) -> Result<()> {
        match fs::remove_dir_all(root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(root).await?;
        #[cfg(unix)]
        fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
        self.install_files_at_root(root, files).await
    }

    async fn replace_docker_trusted_files(
        &self,
        container_id: &str,
        sandbox: &DockerSandbox,
        files: &[SandboxFile],
    ) -> Result<()> {
        self.replace_trusted_files_at_root(&sandbox.trusted_root, files)
            .await?;
        let clear = Command::new("docker")
            .args([
                "exec",
                "--user",
                "0",
                container_id,
                "sh",
                "-c",
                "find /opt/moa/trusted -mindepth 1 -delete",
            ])
            .output()
            .await?;
        if !clear.status.success() {
            return Err(MoaError::ProviderError(format!(
                "failed to clear Docker trusted root: {}",
                String::from_utf8_lossy(&clear.stderr).trim()
            )));
        }
        let source = format!("{}/.", sandbox.trusted_root.display());
        let destination = format!("{container_id}:/opt/moa/trusted");
        let copy = Command::new("docker")
            .args(["cp", &source, &destination])
            .output()
            .await?;
        if !copy.status.success() {
            return Err(MoaError::ProviderError(format!(
                "failed to install Docker trusted files: {}",
                String::from_utf8_lossy(&copy.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Builds the durable lease payload needed to reconnect this local or Docker handle.
    pub async fn lease_handle(
        &self,
        provisioning_operation_id: HandProvisioningOperationId,
        handle: &HandHandle,
    ) -> Result<LeaseHandle> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
                Ok(LeaseHandle::with_metadata(
                    provisioning_operation_id,
                    handle.clone(),
                    serde_json::json!({
                        "kind": "local",
                        "execution_root": sandbox.execution_root,
                        "trusted_root": sandbox.trusted_root,
                        "hard_deadline": sandbox.hard_deadline,
                        "inventory_identity": sandbox.inventory_identity,
                    }),
                ))
            }
            HandHandle::Docker { container_id } => {
                let sandbox = self
                    .docker_sandboxes
                    .read()
                    .await
                    .get(container_id)
                    .cloned()
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "unknown docker sandbox handle: {container_id}"
                        ))
                    })?;
                Ok(LeaseHandle::with_metadata(
                    provisioning_operation_id,
                    handle.clone(),
                    serde_json::json!({
                        "kind": "docker",
                        "sandbox_dir": sandbox.sandbox_dir,
                        "trusted_root": sandbox.trusted_root,
                        "mutable_root": sandbox.mutable_root,
                        "hard_deadline": sandbox.hard_deadline,
                        "inventory_identity": sandbox.inventory_identity,
                    }),
                ))
            }
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
    }

    /// Rehydrates local provider caches from a durable lease payload.
    pub async fn adopt_lease_handle(&self, lease_handle: &LeaseHandle) -> Result<HandHandle> {
        match &lease_handle.handle {
            HandHandle::Local { sandbox_dir } => {
                let metadata = lease_handle.provider_metadata.as_ref().ok_or_else(|| {
                    MoaError::ProviderError(
                        "local hand lease is missing sandbox metadata".to_string(),
                    )
                })?;
                let execution_root = metadata
                    .get("execution_root")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| sandbox_dir.clone());
                self.local_sandboxes.write().await.insert(
                    sandbox_dir.clone(),
                    LocalSandbox {
                        execution_root,
                        trusted_root: metadata
                            .get("trusted_root")
                            .and_then(serde_json::Value::as_str)
                            .map(PathBuf::from)
                            .ok_or_else(|| {
                                MoaError::ProviderError(
                                    "local hand lease is missing trusted_root".to_string(),
                                )
                            })?,
                        hard_deadline: hard_deadline_from_metadata(metadata),
                        inventory_identity: Some(inventory_identity_from_metadata(metadata)?),
                    },
                );
                Ok(lease_handle.handle.clone())
            }
            HandHandle::Docker { container_id } => {
                let metadata = lease_handle.provider_metadata.as_ref().ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "docker hand lease {container_id} is missing sandbox metadata"
                    ))
                })?;
                let sandbox_dir = metadata
                    .get("sandbox_dir")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "docker hand lease {container_id} is missing sandbox_dir"
                        ))
                    })?;
                let mutable_root = metadata
                    .get("mutable_root")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "docker hand lease {container_id} is missing mutable_root"
                        ))
                    })?
                    .to_string();
                let trusted_root = metadata
                    .get("trusted_root")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "docker hand lease {container_id} is missing trusted_root"
                        ))
                    })?;
                self.docker_sandboxes.write().await.insert(
                    container_id.clone(),
                    DockerSandbox {
                        sandbox_dir,
                        trusted_root,
                        mutable_root,
                        hard_deadline: hard_deadline_from_metadata(metadata),
                        inventory_identity: Some(inventory_identity_from_metadata(metadata)?),
                    },
                );
                Ok(lease_handle.handle.clone())
            }
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
    }

    /// Executes a tool with cooperative cancellation support.
    pub async fn execute_with_cancel(
        &self,
        handle: &HandHandle,
        tool: &str,
        input: &str,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<ToolOutput> {
        self.execute_bounded(
            handle,
            tool,
            input,
            hard_cancel_token,
            ResourceBudget::UNBOUNDED,
        )
        .await
    }

    /// Executes a tool under both cooperative cancellation and a run budget.
    ///
    /// The token and the budget are not redundant. The token says *stop now*
    /// and only arrives when something else decides to cancel; the budget says
    /// *how much time is left*, which lets the command be started with the
    /// right timeout instead of being interrupted after the fact — and lets an
    /// already-expired run be refused before a process is spawned at all.
    pub async fn execute_bounded(
        &self,
        handle: &HandHandle,
        tool: &str,
        input: &str,
        hard_cancel_token: Option<&CancellationToken>,
        budget: ResourceBudget,
    ) -> Result<ToolOutput> {
        let tier = match handle {
            HandHandle::Local { .. } => "local",
            HandHandle::Docker { .. } => "container",
            HandHandle::Daytona { .. } => "container",
            HandHandle::E2B { .. } => "microvm",
        };
        // The backing execution target varies by handle: a Docker-backed handle
        // must report "docker" here, not the router-level "local" provider name,
        // or provisioning/execution spans for containerized runs become
        // indistinguishable from host-local runs in traces.
        let provider = hand_execute_provider_label(handle);
        let span_name = format!("hand.execute {provider}/{tool}");
        let hand_span = match current_turn_root_span() {
            Some(parent) => {
                tracing::info_span!(parent: &parent, "hand_execute", otel.name = %span_name)
            }
            None => tracing::info_span!("hand_execute", otel.name = %span_name),
        };
        hand_span.set_attribute("moa.hand.provider", provider);
        hand_span.set_attribute("moa.hand.tier", tier);

        let instrument_hand_span = hand_span.clone();
        async move {
            let started_at = Instant::now();
            let result = match handle {
                HandHandle::Local { sandbox_dir } => {
                    self.execute_local_tool(sandbox_dir, tool, input, hard_cancel_token, budget)
                        .await
                }
                HandHandle::Docker { container_id } => {
                    self.execute_docker_tool(container_id, tool, input, hard_cancel_token, budget)
                        .await
                }
                _ => Err(MoaError::Unsupported(
                    "non-local hand handle passed to LocalHandProvider".to_string(),
                )),
            };
            hand_span.set_attribute(
                "moa.tool.duration_ms",
                started_at.elapsed().as_millis() as i64,
            );

            match &result {
                Ok(output) if output.is_error => {
                    hand_span.set_status(Status::error(TOOL_ERROR_OUTPUT_STATUS));
                    if let Some(exit_code) = output.process_exit_code() {
                        hand_span.set_attribute("moa.tool.exit_code", exit_code as i64);
                    }
                }
                Ok(output) => {
                    if let Some(exit_code) = output.process_exit_code() {
                        hand_span.set_attribute("moa.tool.exit_code", exit_code as i64);
                    }
                }
                Err(MoaError::Cancelled) => {}
                Err(_) => {
                    hand_span.set_status(Status::error(TOOL_EXECUTION_FAILED_STATUS));
                }
            }

            result
        }
        .instrument(instrument_hand_span)
        .await
    }
}

/// Returns the bounded-cardinality provider label for a hand handle's backing target.
///
/// The [`LocalHandProvider`] only ever executes [`HandHandle::Local`] and
/// [`HandHandle::Docker`] handles (other variants hit the `Unsupported` arm
/// above), but the label is computed defensively for every variant so span
/// attributes never fall back to a misleading default.
fn hand_execute_provider_label(handle: &HandHandle) -> &'static str {
    match handle {
        HandHandle::Local { .. } => "local",
        HandHandle::Docker { .. } => "docker",
        HandHandle::Daytona { .. } => "daytona",
        HandHandle::E2B { .. } => "e2b",
    }
}

/// Translates the enforceable dimensions of a profile into `docker run` flags,
/// and refuses the ones Docker cannot actually enforce.
///
/// The refusals are the point. `--storage-opt size=` is silently ignored on the
/// default overlay2 storage driver, and `docker run` has no per-destination
/// egress filter at all, so accepting either would produce a container that
/// reports a bounded disk or a restricted allowlist while enforcing neither.
fn docker_profile_args(profile: &SandboxProfile) -> Result<Vec<String>> {
    let mut args = Vec::new();
    if let CpuLimit::Bounded { millicores } = profile.cpu {
        args.push("--cpus".to_string());
        args.push(format!("{:.3}", f64::from(millicores.get()) / 1000.0));
    }
    if let MemoryLimit::Bounded { mebibytes } = profile.memory {
        args.push("--memory".to_string());
        args.push(format!("{mebibytes}m"));
    }
    if let DiskLimit::Bounded { mebibytes } = profile.ephemeral_disk {
        return Err(MoaError::Unsupported(format!(
            "local Docker sandboxes cannot enforce a {mebibytes} MiB ephemeral disk limit"
        )));
    }
    match &profile.egress {
        EgressPolicy::DenyAll => {
            args.push("--network".to_string());
            args.push("none".to_string());
        }
        EgressPolicy::Unrestricted => {
            args.push("--network".to_string());
            args.push("bridge".to_string());
        }
        EgressPolicy::AllowList { .. } => {
            return Err(MoaError::Unsupported(
                "local Docker sandboxes cannot enforce a per-destination egress allowlist"
                    .to_string(),
            ));
        }
    }
    Ok(args)
}

/// Refuses any bounded dimension a bare host process cannot enforce.
fn reject_unenforceable_host_profile(profile: &SandboxProfile) -> Result<()> {
    if profile.cpu.bounded_millicores().is_some() {
        return Err(MoaError::Unsupported(
            "local host sandboxes cannot enforce a bounded CPU limit".to_string(),
        ));
    }
    if profile.memory.bounded_mebibytes().is_some() {
        return Err(MoaError::Unsupported(
            "local host sandboxes cannot enforce a bounded memory limit".to_string(),
        ));
    }
    if profile.ephemeral_disk.bounded_mebibytes().is_some() {
        return Err(MoaError::Unsupported(
            "local host sandboxes cannot enforce a bounded ephemeral disk limit".to_string(),
        ));
    }
    if profile.egress.mode() != EgressMode::Unrestricted {
        return Err(MoaError::Unsupported(format!(
            "local host sandboxes share the host network and cannot enforce {} egress",
            profile.egress.mode().as_str()
        )));
    }
    Ok(())
}

#[async_trait]
impl HandProvider for LocalHandProvider {
    fn provider_name(&self) -> &str {
        "local"
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        LOCAL_HAND_CAPABILITIES.clone()
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        match spec.sandbox_tier {
            SandboxTier::None | SandboxTier::Local => {
                reject_unenforceable_host_profile(spec.effective_profile.profile())?;
                let (sandbox_dir, marker) = self.prepare_sandbox_dir(&spec).await?;
                self.provision_local(&spec, sandbox_dir, &marker).await
            }
            SandboxTier::Container if self.docker_available => {
                let (sandbox_dir, marker) = self.prepare_sandbox_dir(&spec).await?;
                self.provision_docker(&spec, &sandbox_dir, &marker).await
            }
            SandboxTier::Container => Err(MoaError::ProviderError(
                "container sandbox requested but Docker is unavailable".to_string(),
            )),
            SandboxTier::MicroVM => Err(MoaError::Unsupported(
                "microvm sandboxes are not supported by the local hand provider".to_string(),
            )),
        }
    }

    async fn provisioned_hands(
        &self,
        _provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        _provider_account_generation: u64,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        let sandbox_dir = self.sandbox_dir(operation_id);
        let marker = self.read_intent_marker(operation_id).await?;
        let mut handles = Vec::new();

        // Docker is provider-visible state. It must remain discoverable even
        // after a partial cleanup removed the local marker and bind directory.
        let container_ids = if self.docker_reconciliation_enabled
            || marker
                .as_ref()
                .is_some_and(|marker| marker.sandbox_tier == SandboxTier::Container)
        {
            self.docker_containers_for_operation(operation_id).await?
        } else {
            BTreeSet::new()
        };

        for container_id in container_ids {
            if let Some(marker) = marker.as_ref()
                && marker.sandbox_tier == SandboxTier::Container
            {
                let workspace_mount = marker.mutable_root.clone();
                handles.push(
                    self.register_docker_sandbox(
                        container_id,
                        &sandbox_dir,
                        &workspace_mount,
                        marker.hard_deadline,
                        &marker.inventory_identity,
                    )
                    .await?,
                );
            } else {
                handles.push(HandHandle::docker(container_id));
            }
        }

        if let Some(marker) = marker.as_ref()
            && marker.sandbox_tier != SandboxTier::Container
        {
            self.local_sandboxes.write().await.insert(
                sandbox_dir.clone(),
                LocalSandbox {
                    execution_root: sandbox_dir.clone(),
                    trusted_root: self.trusted_dir(operation_id),
                    hard_deadline: marker.hard_deadline,
                    inventory_identity: Some(marker.inventory_identity.clone()),
                },
            );
        }
        if marker.is_some() || fs::try_exists(&sandbox_dir).await? {
            handles.push(HandHandle::local(sandbox_dir));
        }
        Ok(handles)
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        self.execute_with_cancel(handle, tool, input, None).await
    }

    async fn execute_within(
        &self,
        handle: &HandHandle,
        tool: &str,
        input: &str,
        budget: ResourceBudget,
    ) -> Result<ToolOutput> {
        self.execute_bounded(handle, tool, input, None, budget)
            .await
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
                self.replace_trusted_files_at_root(&sandbox.trusted_root, files)
                    .await
            }
            HandHandle::Docker { container_id } => {
                let sandbox = self
                    .docker_sandboxes
                    .read()
                    .await
                    .get(container_id)
                    .cloned()
                    .ok_or_else(|| {
                        MoaError::ProviderError(format!(
                            "unknown docker sandbox handle: {container_id}"
                        ))
                    })?;
                self.replace_docker_trusted_files(container_id, &sandbox, files)
                    .await
            }
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
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
        match handle {
            HandHandle::Local { sandbox_dir } => {
                if fs::try_exists(sandbox_dir).await? {
                    Ok(HandStatus::Running)
                } else {
                    Ok(HandStatus::Destroyed)
                }
            }
            HandHandle::Docker { container_id } => docker_status(container_id).await,
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
    }

    /// Refuses compute suspension: neither local backend can actually release compute.
    ///
    /// `docker pause` is a cgroup freeze — it stops CPU scheduling but keeps the
    /// container's memory and disk allocated, so it releases nothing on any
    /// billing model. `docker stop` would release them, but containers are
    /// created with `--rm`, which makes a stop a destroy. A local sandbox
    /// directory has no compute to release at all.
    async fn suspend(&self, _handle: &HandHandle) -> Result<()> {
        Err(MoaError::Unsupported(
            "local sandboxes cannot release compute: `docker pause` is a cgroup freeze that keeps \
             memory and disk allocated, and `--rm` containers are removed by `docker stop`"
                .to_string(),
        ))
    }

    async fn resume(&self, handle: &HandHandle) -> Result<()> {
        match handle {
            HandHandle::Docker { container_id } => {
                match docker_status(container_id).await? {
                    HandStatus::Paused => {
                        let output = Command::new("docker")
                            .args(["unpause", container_id])
                            .output()
                            .await?;
                        if !output.status.success() {
                            return Err(MoaError::ProviderError(format!(
                                "failed to resume docker sandbox: {}",
                                String::from_utf8_lossy(&output.stderr).trim()
                            )));
                        }
                    }
                    HandStatus::Stopped => {
                        let output = Command::new("docker")
                            .args(["start", container_id])
                            .output()
                            .await?;
                        if !output.status.success() {
                            return Err(MoaError::ProviderError(format!(
                                "failed to start docker sandbox: {}",
                                String::from_utf8_lossy(&output.stderr).trim()
                            )));
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            HandHandle::Local { .. } => Ok(()),
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                self.local_sandboxes.write().await.remove(sandbox_dir);
                self.destroy_local_sandbox(sandbox_dir).await
            }
            HandHandle::Docker { container_id } => {
                let sandbox = self.docker_sandboxes.write().await.remove(container_id);
                let output = Command::new("docker")
                    .args(["rm", "-f", container_id])
                    .output()
                    .await?;
                if !output.status.success()
                    && !String::from_utf8_lossy(&output.stderr).contains("No such container")
                {
                    return Err(MoaError::ProviderError(format!(
                        "failed to destroy docker sandbox: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
                if let Some(sandbox) = sandbox {
                    self.destroy_local_sandbox(&sandbox.sandbox_dir).await?;
                }
                Ok(())
            }
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
    }
}

fn confirmed_storage_result(
    storage: Option<ProviderStorageRef>,
) -> WorkspaceStorageOperationResult {
    WorkspaceStorageOperationResult {
        outcome: WorkspaceOperationOutcome::Confirmed,
        confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
        storage,
        checkpoint_publication: None,
        post_commit_state: None,
    }
}

fn file_read_targets_trusted_skill(input: &str) -> Result<bool> {
    let payload: serde_json::Value = serde_json::from_str(input)?;
    let path = payload
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MoaError::ValidationError("file_read requires a path".to_string()))?;
    if let Some(path) = normalized_trusted_skill_path(path) {
        validate_sandbox_file_path(&path)?;
        return Ok(true);
    }
    Ok(false)
}

fn local_mutable_storage_id(
    binding: &moa_core::types::sandbox_workspace::WorkspaceBinding,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"moa/local-mutable-workspace/v1");
    digest.update(binding.tenant_id.0.as_bytes());
    digest.update(binding.workspace_id.0.as_bytes());
    digest.update(binding.provider_account_id.0.as_bytes());
    digest.update(binding.provider_account_generation.to_be_bytes());
    hex::encode(digest.finalize())
}

async fn promote_into_empty_compute_root(staging: &Path, root: &Path) -> Result<()> {
    let mut entries = fs::read_dir(root).await?;
    if entries.next_entry().await?.is_some() {
        let _ = fs::remove_dir_all(staging).await;
        return Err(MoaError::ValidationError(
            "checkpoint restore requires a fresh empty compute data root".to_string(),
        ));
    }
    fs::remove_dir(root).await?;
    if let Err(error) = fs::rename(staging, root).await {
        let _ = fs::create_dir(root).await;
        return Err(error.into());
    }
    Ok(())
}

fn sandbox_install_path(root: &Path, relative_path: &str) -> Result<PathBuf> {
    validate_sandbox_file_path(relative_path)?;
    let mut target = root.to_path_buf();
    for segment in relative_path.split('/') {
        target.push(segment);
    }
    Ok(target)
}

#[cfg(unix)]
async fn set_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path).await?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

async fn detect_docker() -> bool {
    let started_at = Instant::now();
    let mut command = Command::new("docker");
    command.args(["info"]).kill_on_drop(true);
    let available = match tokio::time::timeout(DOCKER_DETECTION_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output.status.success(),
        Ok(Err(_)) => false,
        Err(_) => false,
    };
    tracing::debug!(
        docker_available = available,
        elapsed_ms = started_at.elapsed().as_millis(),
        "checked docker availability for local hand provider"
    );
    available
}

async fn docker_user_spec() -> String {
    let uid = command_output_trimmed("id", &["-u"])
        .await
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|uid| *uid != 0)
        .unwrap_or(1000);
    let gid = command_output_trimmed("id", &["-g"])
        .await
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1000);
    format!("{uid}:{gid}")
}

async fn command_output_trimmed(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn operation_id_from_sandbox_dir(sandbox_dir: &Path) -> Option<HandProvisioningOperationId> {
    let name = sandbox_dir.file_name()?.to_str()?;
    let value = name.strip_prefix(HAND_SANDBOX_PREFIX)?;
    Uuid::parse_str(value).ok().map(HandProvisioningOperationId)
}

async fn inspect_docker_container(reference: &str) -> Result<Option<DockerInspection>> {
    let output = Command::new("docker")
        .args(["container", "inspect", "--format", "{{json .}}", reference])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such object") || stderr.contains("No such container") {
            return Ok(None);
        }
        return Err(MoaError::ProviderError(format!(
            "failed to inspect Docker hand container {reference}: {}",
            stderr.trim()
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let labels = value.get("Config").and_then(|config| config.get("Labels"));
    Ok(Some(DockerInspection {
        container_id: value
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(reference)
            .to_string(),
        operation_id: labels
            .and_then(|labels| labels.get(DOCKER_OPERATION_LABEL))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        spec_fingerprint: labels
            .and_then(|labels| labels.get(DOCKER_SPEC_LABEL))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}

fn validate_docker_operation(
    inspection: &DockerInspection,
    expected_operation_id: HandProvisioningOperationId,
) -> Result<()> {
    if inspection.operation_id != expected_operation_id.to_string() {
        return Err(MoaError::ProviderError(format!(
            "deterministic Docker hand container {} carries operation label {:?}, expected {expected_operation_id}",
            inspection.container_id, inspection.operation_id
        )));
    }
    Ok(())
}

fn validate_docker_spec(
    inspection: &DockerInspection,
    expected_spec_fingerprint: &str,
) -> Result<()> {
    if inspection.spec_fingerprint != expected_spec_fingerprint {
        return Err(MoaError::ProviderError(format!(
            "Docker hand container {} carries a different creation spec fingerprint",
            inspection.container_id
        )));
    }
    Ok(())
}

async fn list_docker_containers(operation_id: HandProvisioningOperationId) -> Result<Vec<String>> {
    let filter = format!("label={DOCKER_OPERATION_LABEL}={operation_id}");
    let output = Command::new("docker")
        .args([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--no-trunc",
            "--filter",
            &filter,
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(MoaError::ProviderError(format!(
            "failed to enumerate Docker hands for operation {operation_id}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

async fn docker_status(container_id: &str) -> Result<HandStatus> {
    let output = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", container_id])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such object") || stderr.contains("No such container") {
            return Ok(HandStatus::Destroyed);
        }
        return Err(MoaError::ProviderError(format!(
            "failed to inspect docker sandbox status: {}",
            stderr.trim()
        )));
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "running" => Ok(HandStatus::Running),
        "paused" => Ok(HandStatus::Paused),
        "exited" | "created" => Ok(HandStatus::Stopped),
        "dead" | "removing" => Ok(HandStatus::Destroyed),
        other => Err(MoaError::ProviderError(format!(
            "unknown docker sandbox status: {other}"
        ))),
    }
}

/// Classifies one local or Docker-backed hand execution error.
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
            reason: "local sandbox is no longer healthy".to_string(),
        };
    }

    match error {
        MoaError::ProviderError(message) | MoaError::ToolError(message) => {
            let message_lower = message.to_ascii_lowercase();
            if message_lower.contains("unknown docker sandbox handle")
                || message_lower.contains("no such container")
                || message_lower.contains("no such object")
                || message_lower.contains("connection refused")
                || message_lower.contains("cannot connect to the docker daemon")
            {
                return ToolFailureClass::ReProvision {
                    reason: "Docker sandbox is no longer reachable".to_string(),
                };
            }
            classify_tool_error(error, consecutive_timeouts)
        }
        _ => classify_tool_error(error, consecutive_timeouts),
    }
}
