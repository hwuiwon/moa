//! Local hand provider with direct host execution and optional Docker sandboxes.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use moa_core::{
    HandHandle, HandProvider, HandSpec, HandStatus, MoaError, Result, SandboxFile, SandboxTier,
    ToolFailureClass, ToolOutput, classify_tool_error, validate_sandbox_file_path,
};
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

#[derive(Debug, Clone)]
struct LocalSandbox {
    execution_root: PathBuf,
    extra_search_skips: Vec<String>,
}

#[derive(Debug, Clone)]
struct DockerSandbox {
    sandbox_dir: PathBuf,
    workspace_mount: String,
    extra_search_skips: Vec<String>,
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
            "--network".to_string(),
            "none".to_string(),
            "--pids-limit".to_string(),
            "256".to_string(),
            "-v".to_string(),
            mount,
        ];
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
            },
        );
        Ok(HandHandle::docker(container_id))
    }

    async fn provision_local(&self, spec: &HandSpec, sandbox_dir: PathBuf) -> Result<HandHandle> {
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
        let span_name = format!("hand.execute local/{tool}");
        let hand_span = tracing::info_span!("hand_execute", otel.name = %span_name);
        hand_span.set_attribute("moa.hand.provider", "local");
        hand_span.set_attribute("moa.hand.tier", tier);

        let instrument_hand_span = hand_span.clone();
        async move {
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

            match &result {
                Ok(output) if output.is_error => {
                    hand_span.set_status(Status::error(TOOL_ERROR_OUTPUT_STATUS));
                }
                Ok(_) | Err(MoaError::Cancelled) => {}
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

#[async_trait]
impl HandProvider for LocalHandProvider {
    fn provider_name(&self) -> &str {
        "local"
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
    use std::collections::HashMap;
    use std::time::Duration;

    use moa_core::{HandProvider, HandResources, HandSpec, MoaError, SandboxTier};
    use tempfile::tempdir;

    use super::LocalHandProvider;

    fn hand_spec(tier: SandboxTier) -> HandSpec {
        HandSpec {
            sandbox_tier: tier,
            image: None,
            resources: HandResources::default(),
            env: HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        }
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
            moa_core::HandHandle::Local { sandbox_dir } => sandbox_dir,
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
