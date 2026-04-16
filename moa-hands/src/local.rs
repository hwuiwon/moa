//! Local hand provider with direct host execution and optional Docker sandboxes.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use moa_core::{
    HandHandle, HandProvider, HandSpec, HandStatus, MoaError, Result, SandboxTier, ToolOutput,
};
use opentelemetry::trace::Status;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::tools::{
    bash, file_grep, file_outline, file_read, file_search, file_write, str_replace,
};

const DEFAULT_DOCKER_IMAGE: &str = "alpine:3.20";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
const DOCKER_TMPFS_OPTIONS: &str = "rw,nosuid,nodev,size=64m";
const DEFAULT_DOCKER_WORKSPACE: &str = "/workspace";

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
        let work_dir = work_dir.as_ref().to_path_buf();
        fs::create_dir_all(&work_dir).await?;

        Ok(Self {
            work_dir: Arc::new(work_dir),
            docker_available: detect_docker().await,
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
    pub fn with_command_timeout(mut self, command_timeout: Duration) -> Self {
        self.command_timeout = command_timeout;
        self
    }

    async fn create_sandbox_dir(&self) -> Result<PathBuf> {
        let sandbox_dir = self.work_dir.join(format!("sandbox-{}", Uuid::now_v7()));
        fs::create_dir_all(&sandbox_dir).await?;
        #[cfg(unix)]
        fs::set_permissions(&sandbox_dir, std::fs::Permissions::from_mode(0o777)).await?;
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
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--rm".to_string(),
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
        if let Ok(profile) = std::env::var("MOA_DOCKER_SECCOMP_PROFILE") {
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
        match tool {
            "bash" => {
                bash::execute_local(
                    &sandbox.execution_root,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "file_grep" => {
                file_grep::execute(&sandbox.execution_root, input, &sandbox.extra_search_skips)
                    .await
            }
            "file_outline" => file_outline::execute(&sandbox.execution_root, input).await,
            "file_read" => file_read::execute(&sandbox.execution_root, input).await,
            "str_replace" => str_replace::execute(&sandbox.execution_root, input).await,
            "file_write" => file_write::execute(&sandbox.execution_root, input).await,
            "file_search" => {
                file_search::execute(&sandbox.execution_root, input, &sandbox.extra_search_skips)
                    .await
            }
            other => Err(MoaError::ToolError(format!(
                "unsupported local hand tool: {other}"
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

        if tool == "bash" {
            return bash::execute_docker(
                container_id,
                &sandbox.workspace_mount,
                input,
                self.command_timeout,
                hard_cancel_token,
            )
            .await;
        }

        match tool {
            "file_grep" => {
                file_grep::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    &sandbox.extra_search_skips,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "file_outline" => {
                file_outline::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "file_read" => {
                file_read::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "str_replace" => {
                str_replace::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "file_write" => {
                file_write::execute_docker(
                    container_id,
                    &sandbox.workspace_mount,
                    input,
                    self.command_timeout,
                    hard_cancel_token,
                )
                .await
            }
            "file_search" => {
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
            other => Err(MoaError::ToolError(format!(
                "tool {other} not supported in Docker mode"
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
                    hand_span.set_status(Status::error(output.to_text()));
                }
                Ok(_) | Err(MoaError::Cancelled) => {}
                Err(error) => {
                    hand_span.set_status(Status::error(error.to_string()));
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
        let sandbox_dir = self.create_sandbox_dir().await?;
        match spec.sandbox_tier {
            SandboxTier::None | SandboxTier::Local => {
                self.provision_local(&spec, sandbox_dir).await
            }
            SandboxTier::Container if self.docker_available => {
                match self.provision_docker(&spec, &sandbox_dir).await {
                    Ok(handle) => Ok(handle),
                    Err(error) => {
                        tracing::warn!(%error, "docker sandbox provisioning failed, falling back to local execution");
                        self.provision_local(&spec, sandbox_dir).await
                    }
                }
            }
            SandboxTier::Container => {
                tracing::warn!("docker not available, falling back to local sandbox");
                self.provision_local(&spec, sandbox_dir).await
            }
            SandboxTier::MicroVM => Err(MoaError::Unsupported(
                "microvm sandboxes are not supported by the local hand provider".to_string(),
            )),
        }
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        self.execute_with_cancel(handle, tool, input, None).await
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
            HandHandle::Docker { container_id } => {
                if !self
                    .docker_sandboxes
                    .read()
                    .await
                    .contains_key(container_id)
                {
                    return Ok(HandStatus::Destroyed);
                }
                docker_status(container_id).await
            }
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

async fn detect_docker() -> bool {
    let started_at = Instant::now();
    let result = Command::new("docker").args(["info"]).output().await;
    let available = result
        .map(|output| output.status.success())
        .unwrap_or(false);
    tracing::debug!(
        docker_available = available,
        elapsed_ms = started_at.elapsed().as_millis(),
        "checked docker availability for local hand provider"
    );
    available
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
