//! Orchestrator child-process build, spawn, health, and teardown helpers.

use super::*;

pub(super) async fn locate_orchestrator_binary(repo_root: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("MOA_ORCHESTRATOR_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "MOA_ORCHESTRATOR_BIN points to missing file {}",
            path.display()
        );
    }

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root.join("target"));
    let candidate = target_dir.join("debug").join(format!(
        "moa-orchestrator-bin{}",
        std::env::consts::EXE_SUFFIX
    ));
    let status = tokio::process::Command::new(
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()),
    )
    .current_dir(repo_root)
    .args([
        "build",
        "-p",
        "moa-orchestrator",
        "--bin",
        "moa-orchestrator-bin",
        "--features",
        "provider-overrides",
    ])
    .status()
    .await
    .context("build moa-orchestrator-bin for test fixture")?;
    if !status.success() {
        bail!(
            "cargo build -p moa-orchestrator --bin moa-orchestrator-bin --features provider-overrides failed"
        );
    }
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(anyhow!(
            "built orchestrator binary but did not find {}",
            candidate.display()
        ))
    }
}

pub(super) struct OrchestratorSpawnConfig<'a> {
    pub(super) binary: &'a Path,
    pub(super) port: u16,
    pub(super) health_port: u16,
    pub(super) scim_port: u16,
    pub(super) postgres_url: &'a str,
    pub(super) admin_url: &'a str,
    pub(super) ingress_url: &'a str,
    pub(super) script_path: &'a Path,
    pub(super) fga_config: &'a FgaConfig,
    pub(super) extra_env: &'a [(String, String)],
}

pub(super) fn spawn_orchestrator(config: OrchestratorSpawnConfig<'_>) -> Result<Child> {
    let mut command = Command::new(config.binary);
    command
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--health-port")
        .arg(config.health_port.to_string())
        .arg("--scim-port")
        .arg(config.scim_port.to_string())
        .env("MOA_DATABASE_URL", config.postgres_url)
        .env("MOA_RESTATE_ADMIN_URL", config.admin_url)
        .env("MOA_RESTATE_INGRESS_URL", config.ingress_url)
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", config.script_path.display()),
        )
        .env("MOA_AUTHZ_OPENFGA_URL", &config.fga_config.url)
        .env(
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
            &config.fga_config.preshared_key,
        )
        .env("MOA_AUTHZ_OPENFGA_STORE_ID", &config.fga_config.store_id)
        .env("MOA_AUTHZ_OPENFGA_MODEL_ID", &config.fga_config.model_id)
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env("MOA_LINEAGE_SINK", "null")
        .env("RUST_LOG", "warn");
    for (key, value) in config.extra_env {
        command.env(key, value);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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

fn read_child_logs(child: &mut Child) -> String {
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

pub(super) fn terminate_child(mut child: Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
