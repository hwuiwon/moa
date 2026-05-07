//! Daemon process client helpers.

use super::*;

/// Starts the MOA daemon as a background process.
pub async fn start_daemon(config: &MoaConfig) -> Result<()> {
    if daemon_info(config).await.is_ok() {
        return Ok(());
    }

    let socket_path = daemon_socket_path(config);
    let pid_path = daemon_pid_path(config);
    let log_path = daemon_log_path(config);
    ensure_parent_dir(&socket_path).await?;
    ensure_parent_dir(&pid_path).await?;
    ensure_parent_dir(&log_path).await?;

    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .with_context(|| format!("opening daemon log at {}", log_path.display()))?
        .into_std()
        .await;
    let log_file_err = log_file
        .try_clone()
        .with_context(|| format!("cloning daemon log at {}", log_path.display()))?;
    let current_exe = std::env::current_exe().context("resolving current executable")?;

    let mut command = std::process::Command::new(current_exe);
    command
        .arg("daemon")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));
    #[cfg(unix)]
    // SAFETY: this runs in the child just before exec to detach the daemon into
    // its own session. The closure performs only async-signal-safe work.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("spawning daemon process")?;

    wait_for_daemon(config, Duration::from_secs(5)).await
}

/// Stops the MOA daemon.
pub async fn stop_daemon(config: &MoaConfig) -> Result<()> {
    let socket_path = daemon_socket_path(config);
    if request(config, &DaemonCommand::Shutdown).await.is_ok() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !fs::try_exists(&socket_path).await.unwrap_or(false) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    if let Ok(pid) = read_pid_file(config).await {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }

    if fs::try_exists(&socket_path).await.unwrap_or(false) {
        fs::remove_file(&socket_path).await.ok();
    }

    Ok(())
}

/// Returns the current daemon status snapshot.
pub async fn daemon_info(config: &MoaConfig) -> Result<DaemonInfo> {
    match request(config, &DaemonCommand::Ping).await? {
        DaemonReply::Info(info) => Ok(info),
        DaemonReply::Error(message) => bail!(message),
        other => bail!("unexpected daemon ping reply: {other:?}"),
    }
}

/// Returns the daemon log tail as plain text.
pub async fn daemon_logs(config: &MoaConfig) -> Result<String> {
    let path = daemon_log_path(config);
    if !fs::try_exists(&path).await? {
        return Ok(String::new());
    }
    let content = fs::read_to_string(&path).await?;
    let lines = content.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(200);
    Ok(lines[start..].join("\n"))
}

/// Sends one request to the daemon and returns the unary reply.
pub(crate) async fn request(config: &MoaConfig, command: &DaemonCommand) -> Result<DaemonReply> {
    let socket_path = daemon_socket_path(config);
    let mut reader = send_command(&socket_path, command).await?;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        bail!("daemon closed the connection");
    }
    serde_json::from_str(line.trim_end()).context("decoding daemon reply")
}

async fn send_command(
    socket_path: &Path,
    command: &DaemonCommand,
) -> Result<BufReader<UnixStream>> {
    let mut socket = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to daemon at {}", socket_path.display()))?;
    let payload = serde_json::to_string(command).context("serializing daemon request")?;
    socket.write_all(payload.as_bytes()).await?;
    socket.write_all(b"\n").await?;
    Ok(BufReader::new(socket))
}

pub(super) async fn write_reply(stream: &mut UnixStream, reply: &DaemonReply) -> Result<()> {
    let payload = serde_json::to_string(reply).context("serializing daemon reply")?;
    stream.write_all(payload.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

pub(super) async fn write_stream_event(
    stream: &mut UnixStream,
    event: &DaemonStreamEvent,
) -> Result<()> {
    let payload = serde_json::to_string(event).context("serializing daemon stream event")?;
    stream.write_all(payload.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

pub(crate) async fn wait_for_daemon(config: &MoaConfig, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_info(config).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for daemon to start");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
