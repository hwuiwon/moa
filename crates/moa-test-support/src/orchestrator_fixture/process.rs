//! Orchestrator child-process build, spawn, health, and teardown helpers.

use super::*;
pub(super) use crate::process::TestChildGuard as ChildGuard;
pub(super) use crate::process::terminate_child;

const ORCHESTRATOR_FIXTURE_FEATURES: &str =
    "provider-overrides,integration,execution-planning-failpoints,sandbox-workspace-failpoints";
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
    pub(super) credential_port: u16,
    pub(super) postgres_url: &'a str,
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
    pub(super) credential_port: u16,
    pub(super) postgres_url: String,
    pub(super) admin_url: String,
    pub(super) ingress_url: String,
    pub(super) redis_url: String,
    pub(super) script_path: Option<PathBuf>,
    pub(super) journal_path: Option<PathBuf>,
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
            credential_port: self.credential_port,
            postgres_url: &self.postgres_url,
            ingress_url: &self.ingress_url,
            redis_url: &self.redis_url,
            script_path: self.script_path.as_deref(),
            journal_path: self.journal_path.as_deref(),
            fga_config: &self.fga_config,
            extra_env: &self.extra_env,
            otlp_endpoint: &self.otlp_endpoint,
            observability_service_name: &self.observability_service_name,
        })
    }

    pub(super) fn deployment_uri(&self) -> String {
        format!("http://host.docker.internal:{}", self.port)
    }

    /// Spawns the maintenance owner against the same durable fixture dependencies.
    pub(super) fn spawn_maintenance(&self, health_port: u16) -> Result<ChildGuard> {
        spawn_maintenance(
            OrchestratorSpawnConfig {
                binary: &self.binary,
                port: self.port,
                health_port,
                scim_port: self.scim_port,
                credential_port: self.credential_port,
                postgres_url: &self.postgres_url,
                ingress_url: &self.ingress_url,
                redis_url: &self.redis_url,
                script_path: self.script_path.as_deref(),
                journal_path: self.journal_path.as_deref(),
                fga_config: &self.fga_config,
                extra_env: &self.extra_env,
                otlp_endpoint: &self.otlp_endpoint,
                observability_service_name: &self.observability_service_name,
            },
            health_port,
        )
    }
}

/// Four distinct TCP ports used by one orchestrator child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OrchestratorPorts {
    pub(super) restate: u16,
    pub(super) health: u16,
    pub(super) scim: u16,
    pub(super) credential: u16,
}

/// Live listeners that prevent the selected orchestrator ports from being recycled.
pub(super) struct OrchestratorPortReservation {
    ports: OrchestratorPorts,
    _listeners: [std::net::TcpListener; 4],
}

impl OrchestratorPortReservation {
    /// Returns the selected ports while retaining every listener reservation.
    pub(super) fn ports(&self) -> OrchestratorPorts {
        self.ports
    }

    /// Releases the listeners and returns the ports for immediate child-process use.
    pub(super) fn release(self) -> OrchestratorPorts {
        self.ports
    }
}

/// Reserves the complete, distinct orchestrator port set on its actual wildcard bind scope.
pub(super) fn reserve_orchestrator_ports() -> Result<OrchestratorPortReservation> {
    let restate = std::net::TcpListener::bind("0.0.0.0:0")
        .context("reserve orchestrator Restate handler port")?;
    let health =
        std::net::TcpListener::bind("0.0.0.0:0").context("reserve orchestrator health port")?;
    let scim =
        std::net::TcpListener::bind("0.0.0.0:0").context("reserve orchestrator SCIM port")?;
    let credential =
        std::net::TcpListener::bind("0.0.0.0:0").context("reserve orchestrator credential port")?;
    let ports = OrchestratorPorts {
        restate: restate
            .local_addr()
            .context("read reserved orchestrator Restate handler port")?
            .port(),
        health: health
            .local_addr()
            .context("read reserved orchestrator health port")?
            .port(),
        scim: scim
            .local_addr()
            .context("read reserved orchestrator SCIM port")?
            .port(),
        credential: credential
            .local_addr()
            .context("read reserved orchestrator credential port")?
            .port(),
    };
    Ok(OrchestratorPortReservation {
        ports,
        _listeners: [restate, health, scim, credential],
    })
}

/// Abruptly kills and reaps an orchestrator child.
///
/// On Unix, [`Child::kill`] sends `SIGKILL`. Waiting here is intentional: an
/// unreaped process can leave the fixture's ports occupied and makes the next
/// spawn observe timing rather than durable recovery.
pub(super) fn hard_kill_child(mut child: Child) -> Result<()> {
    child.kill().context("send SIGKILL to orchestrator child")?;
    let status = child.wait().context("reap SIGKILLed orchestrator child")?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if status.signal() != Some(nix::libc::SIGKILL) {
            bail!("hard-killed orchestrator child exited with {status}, not SIGKILL");
        }
    }
    #[cfg(not(unix))]
    if status.success() {
        bail!("hard-killed orchestrator child unexpectedly exited successfully");
    }
    Ok(())
}

/// Spawns one normal Restate handler runtime for the fixture.
pub(super) fn spawn_orchestrator(config: OrchestratorSpawnConfig<'_>) -> Result<ChildGuard> {
    spawn_orchestrator_process(config, None)
}

/// Spawns the real singleton maintenance role on a dedicated health port.
pub(super) fn spawn_maintenance(
    config: OrchestratorSpawnConfig<'_>,
    health_port: u16,
) -> Result<ChildGuard> {
    spawn_orchestrator_process(config, Some(health_port))
}

fn spawn_orchestrator_process(
    config: OrchestratorSpawnConfig<'_>,
    maintenance_health_port: Option<u16>,
) -> Result<ChildGuard> {
    let mut command = Command::new(config.binary);
    command
        .env_remove("MOA_MCP_SERVERS_JSON")
        .env_remove("MOA_DATABASE_MAINTENANCE_URL")
        .env_remove("MOA_PROVIDERS_OVERRIDE")
        .env_remove("MOA_SCRIPTED_PROVIDER_REQUEST_LOG")
        .env_remove("MOA_OBSERVABILITY_SERVICE_NAME")
        .env_remove("MOA_OBSERVABILITY_OTLP_ENDPOINT")
        .env_remove("MOA_OBSERVABILITY_OTLP_PROTOCOL")
        .env_remove("MOA_OBSERVABILITY_SAMPLE_RATE")
        .env_remove("MOA_OBSERVABILITY_ENVIRONMENT")
        .env_remove("MOA_OBSERVABILITY_RELEASE")
        .env_remove("MOA_METRICS_EXPORTER")
        .env_remove("MOA_METRICS_PROMETHEUS_LISTEN")
        // Internal fixtures use the deterministic heuristic classifier unless
        // a test explicitly supplies a sidecar through `extra_env`. Inheriting
        // a developer or parent runner's optional endpoint makes an otherwise
        // hermetic child depend on external availability and load.
        .env_remove("MOA_PII_SERVICE_URL")
        .env_remove("OTEL_METRIC_EXPORT_INTERVAL")
        .env("MOA_DATABASE_URL", config.postgres_url)
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
        .env(
            "RUST_LOG",
            // Honor an ambient override so a failing fixture run can be
            // re-executed with routing/provider logs without editing code;
            // the quiet default keeps ordinary runs readable.
            std::env::var("MOA_FIXTURE_RUST_LOG").unwrap_or_else(|_| "warn".to_string()),
        );
    match maintenance_health_port {
        Some(health_port) => {
            command
                .arg("--health-port")
                .arg(health_port.to_string())
                .arg("maintenance");
        }
        None => {
            command
                .arg("--port")
                .arg(config.port.to_string())
                .arg("--health-port")
                .arg(config.health_port.to_string())
                .arg("--scim-port")
                .arg(config.scim_port.to_string())
                .arg("--credential-port")
                .arg(config.credential_port.to_string());
        }
    }
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
        .env(
            "MOA_OBSERVABILITY_SERVICE_NAME",
            config.observability_service_name,
        )
        // The collector BASE URL. The child derives `/v1/traces` and `/v1/metrics`
        // from it exactly as production does, which is also why the ambient
        // `MOA_METRICS_*` pair is cleared above: a developer shell carrying
        // `.env.example`'s Prometheus defaults would otherwise make every fixture
        // child try to bind the same scrape port and export no OTLP metrics at all.
        .env("MOA_OBSERVABILITY_OTLP_ENDPOINT", config.otlp_endpoint)
        .env("MOA_OBSERVABILITY_OTLP_PROTOCOL", "http")
        .env("MOA_METRICS_EXPORTER", "otlp")
        // Milliseconds. The SDK default is 60s, which is longer than any fixture
        // test's patience, so a metric assertion would time out against a working
        // exporter.
        .env("OTEL_METRIC_EXPORT_INTERVAL", "2000")
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
        .with_context(|| {
            let role = if maintenance_health_port.is_some() {
                "maintenance"
            } else {
                "runtime"
            };
            format!(
                "spawn orchestrator {role} binary {}",
                config.binary.display()
            )
        })
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

pub(super) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_ports_are_distinct_and_reserved_until_release() {
        // Pins: one fixture cannot receive a recycled port for two of its three listeners, and
        // no unrelated binder can claim those ports before the orchestrator spawn begins.
        let reservation = reserve_orchestrator_ports().expect("reserve orchestrator ports");
        let ports = reservation.ports();
        let unique = std::collections::HashSet::from([
            ports.restate,
            ports.health,
            ports.scim,
            ports.credential,
        ]);
        assert_eq!(unique.len(), 4, "all orchestrator ports must be distinct");

        for port in [ports.restate, ports.health, ports.scim, ports.credential] {
            let error = std::net::TcpListener::bind(("0.0.0.0", port))
                .expect_err("reserved fixture port must reject a competing bind");
            assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        }

        let released = reservation.release();
        for port in [
            released.restate,
            released.health,
            released.scim,
            released.credential,
        ] {
            std::net::TcpListener::bind(("0.0.0.0", port))
                .expect("released fixture port should be available to the orchestrator");
        }
    }

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
    #[test]
    fn hard_kill_child_uses_non_graceful_exit_and_reaps_process() {
        // Pins: recovery tests model process loss, not graceful shutdown, and
        // the replacement cannot race a zombie that still owns fixture state.
        let child = long_running_child();
        let pid = child.id();
        hard_kill_child(child).expect("SIGKILL and reap fixture child");
        assert!(!process_exists(pid), "hard-killed child must be reaped");
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
