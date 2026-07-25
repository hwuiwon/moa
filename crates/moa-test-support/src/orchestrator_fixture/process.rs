//! Orchestrator child-process build, spawn, health, and teardown helpers.

use super::*;
pub(super) use crate::process::TestChildGuard as ChildGuard;
pub(super) use crate::process::terminate_child;

const ORCHESTRATOR_FIXTURE_FEATURES: &str =
    "provider-overrides,integration,execution-planning-failpoints";
const ORCHESTRATOR_FIXTURE_TARGET_DIR: &str = "orchestrator-fixture-failpoints";

/// Immutable, fixture-owned copy of the selected orchestrator executable.
pub(super) struct FixtureBinarySnapshot {
    path: PathBuf,
}

impl FixtureBinarySnapshot {
    /// Returns the executable path retained for initial spawn and recovery restarts.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FixtureBinarySnapshot {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove fixture-owned orchestrator binary snapshot"
            );
        }
    }
}

/// Selects the runner-provided executable or builds one, then snapshots its bytes.
pub(super) async fn locate_orchestrator_binary(repo_root: &Path) -> Result<FixtureBinarySnapshot> {
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root.join("target"));
    if let Ok(configured) = std::env::var("MOA_ORCHESTRATOR_BIN") {
        let candidate = PathBuf::from(configured);
        let metadata = std::fs::metadata(&candidate).with_context(|| {
            format!(
                "MOA_ORCHESTRATOR_BIN points to unreadable path {}",
                candidate.display()
            )
        })?;
        if !metadata.is_file() {
            bail!(
                "MOA_ORCHESTRATOR_BIN must point to a regular file: {}",
                candidate.display()
            );
        }
        return snapshot_orchestrator_binary(&candidate, &target_dir);
    }

    let fixture_target_dir = target_dir.join(ORCHESTRATOR_FIXTURE_TARGET_DIR);
    let candidate = fixture_target_dir.join("debug").join(format!(
        "moa-orchestrator-bin{}",
        std::env::consts::EXE_SUFFIX
    ));
    let status = tokio::process::Command::new(
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()),
    )
    .current_dir(repo_root)
    .env("CARGO_TARGET_DIR", &fixture_target_dir)
    .args([
        "build",
        "-p",
        "moa-orchestrator",
        "--bin",
        "moa-orchestrator-bin",
        "--features",
        ORCHESTRATOR_FIXTURE_FEATURES,
    ])
    .status()
    .await
    .context("build moa-orchestrator-bin for test fixture")?;
    if !status.success() {
        bail!(
            "cargo build -p moa-orchestrator --bin moa-orchestrator-bin --features {ORCHESTRATOR_FIXTURE_FEATURES} failed"
        );
    }
    if candidate.exists() {
        snapshot_orchestrator_binary(&candidate, &fixture_target_dir)
    } else {
        Err(anyhow!(
            "built orchestrator binary but did not find {}",
            candidate.display()
        ))
    }
}

fn snapshot_orchestrator_binary(
    candidate: &Path,
    target_dir: &Path,
) -> Result<FixtureBinarySnapshot> {
    let debug_dir = target_dir.join("debug");
    std::fs::create_dir_all(&debug_dir).with_context(|| {
        format!(
            "create orchestrator fixture snapshot directory {}",
            debug_dir.display()
        )
    })?;
    let fixture_binary = debug_dir.join(format!(
        "moa-orchestrator-fixture-failpoints-{}-{}{}",
        std::process::id(),
        Uuid::now_v7().simple(),
        std::env::consts::EXE_SUFFIX,
    ));
    std::fs::copy(candidate, &fixture_binary).with_context(|| {
        format!(
            "copy feature-qualified fixture binary from {} to {}",
            candidate.display(),
            fixture_binary.display()
        )
    })?;
    Ok(FixtureBinarySnapshot {
        path: fixture_binary,
    })
}

pub(super) struct OrchestratorSpawnConfig<'a> {
    pub(super) binary: &'a Path,
    pub(super) port: u16,
    pub(super) health_port: u16,
    pub(super) scim_port: u16,
    pub(super) postgres_url: &'a str,
    pub(super) admin_url: &'a str,
    pub(super) ingress_url: &'a str,
    pub(super) redis_url: &'a str,
    pub(super) script_path: Option<&'a Path>,
    pub(super) journal_path: Option<&'a Path>,
    pub(super) fga_config: &'a FgaConfig,
    pub(super) extra_env: &'a [(String, String)],
    pub(super) otlp_endpoint: &'a str,
    pub(super) observability_service_name: &'a str,
}

pub(super) struct OrchestratorRestartConfig {
    pub(super) binary: PathBuf,
    pub(super) port: u16,
    pub(super) health_port: u16,
    pub(super) scim_port: u16,
    pub(super) postgres_url: String,
    pub(super) admin_url: String,
    pub(super) ingress_url: String,
    pub(super) redis_url: String,
    pub(super) script_path: PathBuf,
    pub(super) journal_path: PathBuf,
    pub(super) fga_config: FgaConfig,
    pub(super) extra_env: Vec<(String, String)>,
    pub(super) otlp_endpoint: String,
    pub(super) observability_service_name: String,
}

impl OrchestratorRestartConfig {
    pub(super) fn spawn(&self) -> Result<ChildGuard> {
        spawn_orchestrator(OrchestratorSpawnConfig {
            binary: &self.binary,
            port: self.port,
            health_port: self.health_port,
            scim_port: self.scim_port,
            postgres_url: &self.postgres_url,
            admin_url: &self.admin_url,
            ingress_url: &self.ingress_url,
            redis_url: &self.redis_url,
            script_path: Some(&self.script_path),
            journal_path: Some(&self.journal_path),
            fga_config: &self.fga_config,
            extra_env: &self.extra_env,
            otlp_endpoint: &self.otlp_endpoint,
            observability_service_name: &self.observability_service_name,
        })
    }

    pub(super) fn deployment_uri(&self) -> String {
        format!("http://host.docker.internal:{}", self.port)
    }
}

pub(super) fn spawn_orchestrator(config: OrchestratorSpawnConfig<'_>) -> Result<ChildGuard> {
    let mut command = Command::new(config.binary);
    command
        .env_remove("MOA_MCP_SERVERS_JSON")
        .env_remove("MOA_PROVIDERS_OVERRIDE")
        .env_remove("MOA_SCRIPTED_PROVIDER_REQUEST_LOG")
        .env_remove("MOA_OBSERVABILITY_ENABLED")
        .env_remove("MOA_OBSERVABILITY_SERVICE_NAME")
        .env_remove("MOA_OBSERVABILITY_OTLP_ENDPOINT")
        .env_remove("MOA_OBSERVABILITY_OTLP_PROTOCOL")
        .env_remove("MOA_OBSERVABILITY_SAMPLE_RATE")
        .env_remove("MOA_OBSERVABILITY_ENVIRONMENT")
        .env_remove("MOA_OBSERVABILITY_RELEASE")
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--health-port")
        .arg(config.health_port.to_string())
        .arg("--scim-port")
        .arg(config.scim_port.to_string())
        .env("MOA_DATABASE_URL", config.postgres_url)
        .env("MOA_RESTATE_ADMIN_URL", config.admin_url)
        .env("MOA_RESTATE_INGRESS_URL", config.ingress_url)
        .env("MOA_RUNTIME_CACHE_BACKEND", "redis")
        .env("MOA_RUNTIME_CACHE_REDIS_URL", config.redis_url)
        .env("MOA_SECURITY_PROFILE", "local")
        // The spawned orchestrator boots with the in-process ephemeral KMS; opt
        // into it explicitly so the composition-root fail-closed durability guard
        // does not reject startup (production uses a persistent postgres KMS).
        .env("MOA_KMS_ALLOW_EPHEMERAL", "true")
        .env("MOA_AUTHZ_OPENFGA_URL", &config.fga_config.url)
        .env(
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
            &config.fga_config.preshared_key,
        )
        .env("MOA_AUTHZ_OPENFGA_STORE_ID", &config.fga_config.store_id)
        .env("MOA_AUTHZ_OPENFGA_MODEL_ID", &config.fga_config.model_id)
        .env("MOA_LINEAGE_SINK", "null")
        .env("RUST_LOG", "warn");
    if let Some(script_path) = config.script_path {
        command.env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", script_path.display()),
        );
    }
    if let Some(journal_path) = config.journal_path {
        command.env("MOA_SCRIPTED_PROVIDER_REQUEST_LOG", journal_path);
    }
    for (key, value) in config.extra_env {
        command.env(key, value);
    }
    command
        .env("MOA_OBSERVABILITY_ENABLED", "true")
        .env(
            "MOA_OBSERVABILITY_SERVICE_NAME",
            config.observability_service_name,
        )
        .env("MOA_OBSERVABILITY_OTLP_ENDPOINT", config.otlp_endpoint)
        .env("MOA_OBSERVABILITY_OTLP_PROTOCOL", "http")
        .env("MOA_OBSERVABILITY_SAMPLE_RATE", "1")
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env(
            "MOA_OBSERVABILITY_RELEASE",
            config.observability_service_name,
        );
    command
        // The long-lived child must never write into an undrained pipe. Nextest already
        // captures inherited test output, including startup failures and runtime warnings.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map(ChildGuard::new)
        .with_context(|| format!("spawn orchestrator binary {}", config.binary.display()))
}

pub(super) async fn wait_for_orchestrator_health(
    health_port: u16,
    child: &mut Child,
) -> Result<()> {
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{health_port}/_health/live");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("poll orchestrator child status")? {
            let logs = read_child_logs(child);
            bail!("orchestrator exited before becoming healthy: {status}{logs}");
        }
        match client.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for orchestrator health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for orchestrator health");
            }
            Ok(response) => bail!(
                "orchestrator did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("orchestrator did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) fn read_child_logs(child: &mut Child) -> String {
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut stdout_text = String::new();
        let _ = stdout.read_to_string(&mut stdout_text);
        if !stdout_text.trim().is_empty() {
            output.push_str("\nstdout:\n");
            output.push_str(stdout_text.trim());
        }
    }
    if let Some(mut stderr) = child.stderr.take() {
        let mut stderr_text = String::new();
        let _ = stderr.read_to_string(&mut stderr_text);
        if !stderr_text.trim().is_empty() {
            output.push_str("\nstderr:\n");
            output.push_str(stderr_text.trim());
        }
    }
    output
}

pub(super) fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

pub(super) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn long_running_child() -> Child {
        Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn long-running child for guard test")
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        kill(Pid::from_raw(pid as i32), None).is_ok()
    }

    #[cfg(unix)]
    fn cleanup_mutation_orphan(pid: u32) {
        use nix::sys::signal::{Signal, kill};
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;

        let pid = Pid::from_raw(pid as i32);
        let _ = kill(pid, Signal::SIGKILL);
        let _ = waitpid(pid, None);
    }

    #[cfg(unix)]
    fn assert_terminated_or_cleanup(pid: u32) {
        let terminated = !process_exists(pid);
        if !terminated {
            cleanup_mutation_orphan(pid);
        }
        assert!(
            terminated,
            "dropping an armed child guard must terminate and reap its child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_drop_terminates_but_disarm_transfers_live_child() {
        // Pins: cancellation drops an armed guard and kills its child, while successful fixture
        // installation disarms the guard and transfers exactly one still-live child.
        let child = long_running_child();
        let guarded_pid = child.id();
        let guard = ChildGuard::new(child);
        assert!(process_exists(guarded_pid));

        drop(guard);

        assert_terminated_or_cleanup(guarded_pid);

        let child = long_running_child();
        let transferred_pid = child.id();
        let guard = ChildGuard::new(child);
        let mut transferred = guard
            .disarm()
            .expect("an armed child guard should transfer its child");
        assert!(process_exists(transferred_pid));
        assert!(
            transferred
                .try_wait()
                .expect("poll transferred child")
                .is_none()
        );
        terminate_child(transferred);
        assert!(!process_exists(transferred_pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_future_with_armed_child_guard_terminates_child() {
        // Pins: dropping a construction/restart future at any await drops its armed child guard.
        let child = long_running_child();
        let pid = child.id();
        let guard = ChildGuard::new(child);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });
        ready_rx
            .await
            .expect("guard-owning future should reach its cancellation point");

        task.abort();
        let error = task
            .await
            .expect_err("aborting guard-owning future should cancel it");
        assert!(error.is_cancelled());
        assert_terminated_or_cleanup(pid);
    }

    #[test]
    fn fixture_binary_snapshots_are_isolated_stable_and_owned() {
        // Pins: concurrent fixtures retain independent feature-qualified bytes across Cargo output
        // replacement, and each snapshot is removed only when its owning fixture drops.
        let target_dir = tempfile::tempdir().expect("create fixture target directory");
        let debug_dir = target_dir.path().join("debug");
        std::fs::create_dir(&debug_dir).expect("create fixture debug directory");
        let candidate = debug_dir.join(format!(
            "moa-orchestrator-bin{}",
            std::env::consts::EXE_SUFFIX
        ));
        std::fs::write(&candidate, b"provider-overrides,integration,failpoints")
            .expect("write feature-qualified candidate");

        let first = snapshot_orchestrator_binary(&candidate, target_dir.path())
            .expect("create first feature-qualified fixture snapshot");
        let second = snapshot_orchestrator_binary(&candidate, target_dir.path())
            .expect("create second feature-qualified fixture snapshot");
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        std::fs::write(&candidate, b"different concurrent cargo build")
            .expect("replace mutable Cargo output");

        assert_ne!(first_path, candidate);
        assert_ne!(first_path, second_path);
        assert_eq!(
            std::fs::read(&first_path).expect("read first retained fixture snapshot"),
            b"provider-overrides,integration,failpoints"
        );
        assert_eq!(
            std::fs::read(&second_path).expect("read second retained fixture snapshot"),
            b"provider-overrides,integration,failpoints"
        );

        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
    }
}
