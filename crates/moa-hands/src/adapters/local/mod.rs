//! Local hand provider with direct host execution and optional Docker sandboxes.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, error::Result, error::ToolFailureClass, error::classify_tool_error,
    traits::HandProvider, types::hands::CpuLimit, types::hands::DeadlineEnforcement,
    types::hands::DiskLimit, types::hands::EgressMode, types::hands::EgressPolicy,
    types::hands::HandHandle, types::hands::HandProviderCapabilities, types::hands::HandSpec,
    types::hands::HandStatus, types::hands::MemoryLimit, types::hands::ResourceSupport,
    types::hands::SandboxFile, types::hands::SandboxProfile, types::hands::SandboxTier,
    types::hands::SandboxTierCapabilities, types::hands::validate_sandbox_file_path,
    types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use opentelemetry::trace::Status;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::core::leases::LeaseHandle;
use crate::tools::sandbox_descriptor::{SandboxToolCapability, supported_capability_for_tool};
use crate::tools::{bash, file_outline, file_read, file_search, file_write, grep, str_replace};

const LOCAL_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_DOCKER_IMAGE: &str = "alpine:3.20";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
const DOCKER_DETECTION_TIMEOUT: Duration = Duration::from_secs(2);
const DOCKER_TMPFS_OPTIONS: &str = "rw,nosuid,nodev,size=64m";
const DEFAULT_DOCKER_WORKSPACE: &str = "/workspace";
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
    extra_search_skips: Vec<String>,
    /// Absolute instant this sandbox's maximum lifetime expires.
    ///
    /// `None` means the resolved profile declared an explicitly unbounded
    /// maximum lifetime, which is a statement, not a missing value.
    hard_deadline: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct DockerSandbox {
    sandbox_dir: PathBuf,
    workspace_mount: String,
    extra_search_skips: Vec<String>,
    /// Absolute instant this container's maximum lifetime expires.
    hard_deadline: Option<DateTime<Utc>>,
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
    docker_available: bool,
    command_timeout: Duration,
    local_sandboxes: Arc<RwLock<HashMap<PathBuf, LocalSandbox>>>,
    docker_sandboxes: Arc<RwLock<HashMap<String, DockerSandbox>>>,
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
            docker_available: if detect_docker_availability {
                detect_docker().await
            } else {
                false
            },
            command_timeout: DEFAULT_TOOL_TIMEOUT,
            local_sandboxes: Arc::new(RwLock::new(HashMap::new())),
            docker_sandboxes: Arc::new(RwLock::new(HashMap::new())),
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

    async fn create_sandbox_dir(&self) -> Result<PathBuf> {
        let sandbox_dir = self.work_dir.join(format!("sandbox-{}", Uuid::now_v7()));
        fs::create_dir_all(&sandbox_dir).await?;
        #[cfg(unix)]
        fs::set_permissions(&sandbox_dir, std::fs::Permissions::from_mode(0o770)).await?;
        Ok(sandbox_dir)
    }

    async fn provision_docker(&self, spec: &HandSpec, sandbox_dir: &Path) -> Result<HandHandle> {
        let extra_search_skips = file_search::load_moaignore(sandbox_dir).await;
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_IMAGE.to_string());
        let workspace_mount = spec
            .workspace_mount
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DOCKER_WORKSPACE));
        let mount = format!("{}:{}", sandbox_dir.display(), workspace_mount.display());
        let user = docker_user_spec().await;
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--rm".to_string(),
            "--user".to_string(),
            user,
            "--read-only".to_string(),
            "--workdir".to_string(),
            workspace_mount.display().to_string(),
            "--tmpfs".to_string(),
            format!("/tmp:{DOCKER_TMPFS_OPTIONS}"),
            "--tmpfs".to_string(),
            format!("/run:{DOCKER_TMPFS_OPTIONS}"),
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
            return Err(MoaError::ProviderError(format!(
                "failed to start docker sandbox: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        self.docker_sandboxes.write().await.insert(
            container_id.clone(),
            DockerSandbox {
                sandbox_dir: sandbox_dir.to_path_buf(),
                workspace_mount: workspace_mount.to_string_lossy().into_owned(),
                extra_search_skips,
                hard_deadline: hard_deadline_for(spec.effective_profile.profile()),
            },
        );
        Ok(HandHandle::docker(container_id))
    }

    async fn provision_local(&self, spec: &HandSpec, sandbox_dir: PathBuf) -> Result<HandHandle> {
        reject_unenforceable_host_profile(spec.effective_profile.profile())?;
        let execution_root = spec
            .workspace_mount
            .clone()
            .unwrap_or_else(|| sandbox_dir.clone());
        let extra_search_skips = file_search::load_moaignore(&execution_root).await;
        self.local_sandboxes.write().await.insert(
            sandbox_dir.clone(),
            LocalSandbox {
                execution_root,
                extra_search_skips,
                hard_deadline: hard_deadline_for(spec.effective_profile.profile()),
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
                extra_search_skips: Vec::new(),
                hard_deadline: None,
            })
    }

    async fn execute_local_tool(
        &self,
        sandbox_dir: &Path,
        tool: &str,
        input: &str,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<ToolOutput> {
        let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
        match supported_capability_for_tool(tool, LOCAL_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                bash::execute_local(
                    &sandbox.execution_root,
                    input,
                    self.command_timeout,
                    bash::remaining_lifetime(sandbox.hard_deadline, Utc::now()),
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::Grep) => {
                grep::execute(&sandbox.execution_root, input, &sandbox.extra_search_skips).await
            }
            Some(SandboxToolCapability::FileOutline) => {
                file_outline::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileRead) => {
                file_read::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::StrReplace) => {
                str_replace::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileWrite) => {
                file_write::execute(&sandbox.execution_root, input).await
            }
            Some(SandboxToolCapability::FileSearch) => {
                file_search::execute(&sandbox.execution_root, input, &sandbox.extra_search_skips)
                    .await
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
                bash::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    bash::remaining_lifetime(sandbox.hard_deadline, Utc::now()),
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::Grep) => {
                grep::execute(&sandbox.sandbox_dir, input, &sandbox.extra_search_skips).await
            }
            Some(SandboxToolCapability::FileOutline) => {
                file_outline::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::FileRead) => {
                file_read::execute_docker_bind_mount(
                    &sandbox.sandbox_dir,
                    &sandbox.workspace_mount,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::StrReplace) => {
                str_replace::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                file_write::execute_docker_bind_mount(
                    &sandbox.sandbox_dir,
                    &sandbox.workspace_mount,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                file_search::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    &sandbox.extra_search_skips,
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
        match fs::remove_dir_all(sandbox_dir).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn cleanup_failed_sandbox_dir(&self, sandbox_dir: &Path, reason: &str) {
        if let Err(error) = self.destroy_local_sandbox(sandbox_dir).await {
            tracing::warn!(
                %error,
                %reason,
                sandbox_dir = %sandbox_dir.display(),
                "failed to clean up local sandbox directory after provisioning failure"
            );
        }
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

    /// Builds the durable lease payload needed to reconnect this local or Docker handle.
    pub async fn lease_handle(&self, handle: &HandHandle) -> Result<LeaseHandle> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
                Ok(LeaseHandle::with_metadata(
                    handle.clone(),
                    serde_json::json!({
                        "kind": "local",
                        "execution_root": sandbox.execution_root,
                        "extra_search_skips": sandbox.extra_search_skips,
                        "hard_deadline": sandbox.hard_deadline,
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
                    handle.clone(),
                    serde_json::json!({
                        "kind": "docker",
                        "sandbox_dir": sandbox.sandbox_dir,
                        "workspace_mount": sandbox.workspace_mount,
                        "extra_search_skips": sandbox.extra_search_skips,
                        "hard_deadline": sandbox.hard_deadline,
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
                let Some(metadata) = lease_handle.provider_metadata.as_ref() else {
                    return Ok(lease_handle.handle.clone());
                };
                let execution_root = metadata
                    .get("execution_root")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| sandbox_dir.clone());
                let extra_search_skips = metadata
                    .get("extra_search_skips")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| string_vec_from_json_array(values))
                    .unwrap_or_default();
                self.local_sandboxes.write().await.insert(
                    sandbox_dir.clone(),
                    LocalSandbox {
                        execution_root,
                        extra_search_skips,
                        hard_deadline: hard_deadline_from_metadata(metadata),
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
                let workspace_mount = metadata
                    .get("workspace_mount")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(DEFAULT_DOCKER_WORKSPACE)
                    .to_string();
                let extra_search_skips = metadata
                    .get("extra_search_skips")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| string_vec_from_json_array(values))
                    .unwrap_or_default();
                self.docker_sandboxes.write().await.insert(
                    container_id.clone(),
                    DockerSandbox {
                        sandbox_dir,
                        workspace_mount,
                        extra_search_skips,
                        hard_deadline: hard_deadline_from_metadata(metadata),
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
                    self.execute_local_tool(sandbox_dir, tool, input, hard_cancel_token)
                        .await
                }
                HandHandle::Docker { container_id } => {
                    self.execute_docker_tool(container_id, tool, input, hard_cancel_token)
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
                let sandbox_dir = self.create_sandbox_dir().await?;
                self.provision_local(&spec, sandbox_dir).await
            }
            SandboxTier::Container if self.docker_available => {
                let sandbox_dir = self.create_sandbox_dir().await?;
                match self.provision_docker(&spec, &sandbox_dir).await {
                    Ok(handle) => Ok(handle),
                    Err(error) => {
                        self.cleanup_failed_sandbox_dir(&sandbox_dir, "docker provisioning failed")
                            .await;
                        Err(error)
                    }
                }
            }
            SandboxTier::Container => Err(MoaError::ProviderError(
                "container sandbox requested but Docker is unavailable".to_string(),
            )),
            SandboxTier::MicroVM => Err(MoaError::Unsupported(
                "microvm sandboxes are not supported by the local hand provider".to_string(),
            )),
        }
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        self.execute_with_cancel(handle, tool, input, None).await
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
                self.install_files_at_root(&sandbox.execution_root, files)
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
                self.install_files_at_root(&sandbox.sandbox_dir, files)
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

    async fn pause(&self, handle: &HandHandle) -> Result<()> {
        match handle {
            HandHandle::Docker { container_id } => {
                let output = Command::new("docker")
                    .args(["pause", container_id])
                    .output()
                    .await?;
                if !output.status.success() {
                    return Err(MoaError::ProviderError(format!(
                        "failed to pause docker sandbox: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
                Ok(())
            }
            HandHandle::Local { .. } => Ok(()),
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to LocalHandProvider".to_string(),
            )),
        }
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

fn string_vec_from_json_array(values: &[serde_json::Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
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

#[cfg(test)]
mod tests {

    use moa_core::{
        error::MoaError, traits::HandProvider, types::hands::HandSpec,
        types::hands::SandboxProfile, types::hands::SandboxTier,
    };
    use tempfile::tempdir;

    use super::LocalHandProvider;

    fn hand_spec(tier: SandboxTier) -> HandSpec {
        crate::core::profile::test_support::hand_spec(tier, SandboxProfile::unrestricted())
    }

    #[tokio::test]
    async fn local_container_tier_fails_when_docker_is_unavailable() {
        // Pins: requested container isolation must not silently become host-local execution.
        let dir = tempdir().expect("create tempdir");
        let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
            .await
            .expect("create local hand provider");

        let error = provider
            .provision(hand_spec(SandboxTier::Container))
            .await
            .expect_err("container tier should fail when Docker is unavailable");

        assert!(
            matches!(error, MoaError::ProviderError(message) if message.contains("Docker is unavailable"))
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read sandbox root")
                .count(),
            0,
            "failed container provisioning should not leave a local fallback sandbox"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_sandbox_directory_is_owner_group_restricted() {
        // Pins: local sandbox directories must not grant world access.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("create tempdir");
        let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
            .await
            .expect("create local hand provider");

        let handle = provider
            .provision(hand_spec(SandboxTier::Local))
            .await
            .expect("provision local sandbox");
        let sandbox_dir = match &handle {
            moa_core::types::hands::HandHandle::Local { sandbox_dir } => sandbox_dir,
            other => panic!("expected local hand, got {other:?}"),
        };
        let mode = std::fs::metadata(sandbox_dir)
            .expect("read sandbox metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o770);

        provider
            .destroy(&handle)
            .await
            .expect("destroy local sandbox");
    }
}
