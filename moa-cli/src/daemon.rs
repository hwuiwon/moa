//! Local daemon server and client helpers for persistent background MOA operation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use moa_core::{
    DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent, EventRange,
    MoaConfig, Platform, SessionFilter, SessionStatus, SessionStore,
};
use moa_session::TursoSessionStore;
use moa_tui::runner::ChatRuntime;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

/// Shared daemon server state.
#[derive(Clone)]
struct DaemonState {
    runtime: Arc<Mutex<ChatRuntime>>,
    session_store: Arc<TursoSessionStore>,
    info: Arc<DaemonInfo>,
}

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

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening daemon log at {}", log_path.display()))?;
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

/// Runs the daemon server in the foreground.
pub async fn run_daemon_server(config: MoaConfig) -> Result<()> {
    let socket_path = daemon_socket_path(&config);
    let pid_path = daemon_pid_path(&config);
    ensure_parent_dir(&socket_path).await?;
    ensure_parent_dir(&pid_path).await?;

    if fs::try_exists(&socket_path).await.unwrap_or(false) {
        fs::remove_file(&socket_path).await.ok();
    }

    let runtime = ChatRuntime::from_local_config(config.clone(), Platform::Tui).await?;
    let session_store = Arc::new(TursoSessionStore::from_config(&config).await?);
    let listener = UnixListener::bind(&socket_path)?;
    let info = Arc::new(DaemonInfo {
        pid: std::process::id(),
        socket_path: socket_path.display().to_string(),
        log_path: daemon_log_path(&config).display().to_string(),
        started_at: Utc::now(),
        session_count: 0,
        active_session_count: 0,
    });
    let state = DaemonState {
        runtime: Arc::new(Mutex::new(runtime)),
        session_store,
        info,
    };

    fs::write(&pid_path, format!("{}\n", std::process::id())).await?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let signal_task = spawn_signal_listener(shutdown_tx.clone());
    let health_task = spawn_health_server(&config, shutdown_tx.subscribe()).await?;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let state = state.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(state, shutdown_tx, stream).await {
                        tracing::error!(error = %error, "daemon request failed");
                    }
                });
            }
        }
    }

    let shutdown_grace = graceful_shutdown_timeout(&config);
    wait_for_active_turns(state.session_store.as_ref(), shutdown_grace).await?;
    signal_task.abort();
    if let Some(task) = health_task {
        task.abort();
        let _ = task.await;
    }
    fs::remove_file(&socket_path).await.ok();
    fs::remove_file(&pid_path).await.ok();
    Ok(())
}

/// Sends one request to the daemon and returns the unary reply.
pub async fn request(config: &MoaConfig, command: &DaemonCommand) -> Result<DaemonReply> {
    let socket_path = daemon_socket_path(config);
    let mut reader = send_command(&socket_path, command).await?;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        bail!("daemon closed the connection");
    }
    serde_json::from_str(line.trim_end()).context("decoding daemon reply")
}

async fn handle_connection(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    stream: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }

    let command: DaemonCommand = serde_json::from_str(line.trim_end())?;
    match command {
        DaemonCommand::ObserveSession { session_id } => {
            write_stream_event(reader.get_mut(), &DaemonStreamEvent::Ready).await?;
            let runtime = { state.runtime.lock().await.clone() };
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                if let Err(error) = runtime.observe_session(session_id, event_tx).await {
                    tracing::error!(error = %error, "daemon observation relay failed");
                }
            });
            while let Some(envelope) = event_rx.recv().await {
                write_stream_event(
                    reader.get_mut(),
                    &DaemonStreamEvent::Runtime(envelope.event),
                )
                .await?;
            }
            Ok(())
        }
        other => {
            let reply = handle_unary_command(state, shutdown_tx, other).await;
            write_reply(reader.get_mut(), &reply).await
        }
    }
}

async fn handle_unary_command(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    command: DaemonCommand,
) -> DaemonReply {
    match handle_unary_command_inner(state, shutdown_tx, command).await {
        Ok(reply) => reply,
        Err(error) => DaemonReply::Error(error.to_string()),
    }
}

async fn handle_unary_command_inner(
    state: DaemonState,
    shutdown_tx: watch::Sender<bool>,
    command: DaemonCommand,
) -> Result<DaemonReply> {
    match command {
        DaemonCommand::Ping => {
            let sessions = state
                .session_store
                .list_sessions(SessionFilter::default())
                .await?;
            let active_session_count = sessions
                .iter()
                .filter(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Created
                            | SessionStatus::Running
                            | SessionStatus::WaitingApproval
                    )
                })
                .count();
            let mut info = (*state.info).clone();
            info.session_count = sessions.len();
            info.active_session_count = active_session_count;
            Ok(DaemonReply::Info(info))
        }
        DaemonCommand::Shutdown => {
            let _ = shutdown_tx.send(true);
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::CreateSession => {
            let runtime = state.runtime.lock().await;
            Ok(DaemonReply::SessionId(runtime.create_session().await?))
        }
        DaemonCommand::SetWorkspace { workspace_id } => {
            let mut runtime = state.runtime.lock().await;
            runtime.set_workspace(workspace_id).await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::SetModel { model } => {
            let mut runtime = state.runtime.lock().await;
            runtime.set_model(model).await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::ListSessions { filter } => Ok(DaemonReply::Sessions(
            state.session_store.list_sessions(filter).await?,
        )),
        DaemonCommand::ListSessionPreviews => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::SessionPreviews(
                runtime
                    .list_session_previews()
                    .await?
                    .into_iter()
                    .map(|preview| DaemonSessionPreview {
                        summary: preview.summary,
                        last_message: preview.last_message,
                    })
                    .collect(),
            ))
        }
        DaemonCommand::GetSession { session_id } => Ok(DaemonReply::Session(
            state.session_store.get_session(session_id).await?,
        )),
        DaemonCommand::GetSessionEvents { session_id } => Ok(DaemonReply::SessionEvents(
            state
                .session_store
                .get_events(session_id, EventRange::all())
                .await?,
        )),
        DaemonCommand::RecentMemoryEntries { limit } => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::MemoryEntries(
                runtime.recent_memory_entries(limit).await?,
            ))
        }
        DaemonCommand::SearchMemory { query, limit } => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::MemorySearchResults(
                runtime.search_memory(&query, limit).await?,
            ))
        }
        DaemonCommand::ReadMemoryPage { path } => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::MemoryPage(
                runtime.read_memory_page(&path).await?,
            ))
        }
        DaemonCommand::MemoryIndex => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::MemoryIndex(runtime.memory_index().await?))
        }
        DaemonCommand::ToolNames => {
            let runtime = state.runtime.lock().await.clone();
            Ok(DaemonReply::ToolNames(runtime.tool_names_async().await?))
        }
        DaemonCommand::QueueMessage { session_id, prompt } => {
            let runtime = state.runtime.lock().await.clone();
            runtime.queue_message(session_id, prompt).await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::SoftCancel { session_id } => {
            let runtime = state.runtime.lock().await.clone();
            runtime.soft_cancel_session(session_id).await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::HardCancel { session_id } => {
            let runtime = state.runtime.lock().await.clone();
            runtime.hard_cancel_session(session_id).await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::RespondToApproval {
            session_id,
            request_id,
            decision,
        } => {
            let runtime = state.runtime.lock().await.clone();
            runtime
                .respond_to_session_approval(session_id, request_id, decision)
                .await?;
            Ok(DaemonReply::Ack)
        }
        DaemonCommand::ObserveSession { .. } => bail!("observe is handled separately"),
    }
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

async fn write_reply(stream: &mut UnixStream, reply: &DaemonReply) -> Result<()> {
    let payload = serde_json::to_string(reply).context("serializing daemon reply")?;
    stream.write_all(payload.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn write_stream_event(stream: &mut UnixStream, event: &DaemonStreamEvent) -> Result<()> {
    let payload = serde_json::to_string(event).context("serializing daemon stream event")?;
    stream.write_all(payload.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn wait_for_daemon(config: &MoaConfig, timeout: Duration) -> Result<()> {
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

fn spawn_signal_listener(shutdown_tx: watch::Sender<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        match wait_for_process_signal().await {
            Ok(signal_name) => {
                tracing::warn!(signal = signal_name, "daemon received shutdown signal");
                let _ = shutdown_tx.send(true);
            }
            Err(error) => {
                tracing::error!(error = %error, "daemon signal listener failed");
            }
        }
    })
}

#[cfg(unix)]
async fn wait_for_process_signal() -> Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok("SIGINT"),
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn wait_for_process_signal() -> Result<&'static str> {
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl+C")?;
    Ok("SIGINT")
}

async fn spawn_health_server(
    config: &MoaConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<Option<JoinHandle<Result<()>>>> {
    if !config.cloud.enabled {
        return Ok(None);
    }

    let fly = config.cloud.flyio.as_ref();
    let bind_host = fly
        .map(|config| config.health_bind.as_str())
        .unwrap_or("0.0.0.0");
    let port = fly.map(|config| config.internal_port).unwrap_or(8080);
    let listener = TcpListener::bind((bind_host, port))
        .await
        .with_context(|| format!("binding cloud health listener on {bind_host}:{port}"))?;

    Ok(Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                accept = listener.accept() => {
                    let (stream, _) = accept?;
                    tokio::spawn(async move {
                        if let Err(error) = handle_health_connection(stream).await {
                            tracing::warn!(error = %error, "health request failed");
                        }
                    });
                }
            }
        }

        Ok(())
    })))
}

async fn handle_health_connection(mut stream: TcpStream) -> Result<()> {
    let mut buffer = [0_u8; 1024];
    let bytes = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (status_line, body) = if path == "/health" {
        ("HTTP/1.1 200 OK", "{\"status\":\"ok\"}\n")
    } else {
        ("HTTP/1.1 404 Not Found", "{\"status\":\"not_found\"}\n")
    };
    let response = format!(
        "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn graceful_shutdown_timeout(config: &MoaConfig) -> Duration {
    let seconds = config
        .cloud
        .flyio
        .as_ref()
        .map(|fly| fly.graceful_shutdown_timeout_secs)
        .unwrap_or(30)
        .max(1);
    Duration::from_secs(seconds)
}

async fn wait_for_active_turns(session_store: &TursoSessionStore, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let active = session_store
            .list_sessions(SessionFilter::default())
            .await?
            .into_iter()
            .any(|session| {
                matches!(
                    session.status,
                    SessionStatus::Running | SessionStatus::WaitingApproval
                )
            });
        if !active {
            return Ok(());
        }
        if Instant::now() >= deadline {
            tracing::warn!("graceful shutdown timeout elapsed while active sessions remain");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn read_pid_file(config: &MoaConfig) -> Result<u32> {
    let content = fs::read_to_string(daemon_pid_path(config)).await?;
    content
        .trim()
        .parse::<u32>()
        .context("parsing daemon pid file")
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    Ok(())
}

fn daemon_socket_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.socket_path)
}

fn daemon_pid_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.pid_file)
}

fn daemon_log_path(config: &MoaConfig) -> PathBuf {
    expand_path(&config.daemon.log_file)
}

fn expand_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener as StdTcpListener;
    use std::time::Duration;

    use anyhow::Result;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::{daemon_info, request, run_daemon_server, stop_daemon, wait_for_daemon};
    use moa_core::{DaemonCommand, DaemonReply, MoaConfig, SessionId};

    fn test_config() -> Option<MoaConfig> {
        let dir = tempdir().ok()?;
        let base = dir.keep();
        let mut config = MoaConfig::default();
        config.local.session_db = base.join("sessions.db").display().to_string();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();
        config.daemon.socket_path = base.join("daemon.sock").display().to_string();
        config.daemon.pid_file = base.join("daemon.pid").display().to_string();
        config.daemon.log_file = base.join("daemon.log").display().to_string();
        config.daemon.auto_connect = false;

        if std::env::var(&config.providers.openai.api_key_env).is_ok() {
            return Some(config);
        }
        if std::env::var(&config.providers.anthropic.api_key_env).is_ok() {
            config.general.default_provider = "anthropic".to_string();
            config.general.default_model = "claude-sonnet-4-6".to_string();
            return Some(config);
        }
        if std::env::var(&config.providers.openrouter.api_key_env).is_ok() {
            config.general.default_provider = "openrouter".to_string();
            config.general.default_model = "openai/gpt-5.4".to_string();
            return Some(config);
        }

        None
    }

    fn random_port() -> u16 {
        StdTcpListener::bind(("127.0.0.1", 0))
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port()
    }

    #[tokio::test]
    async fn daemon_ping_create_and_shutdown_roundtrip() -> Result<()> {
        let Some(config) = test_config() else {
            return Ok(());
        };
        let server = tokio::spawn(run_daemon_server(config.clone()));
        wait_for_daemon(&config, std::time::Duration::from_secs(5)).await?;

        let info = daemon_info(&config).await?;
        assert!(info.pid > 0);

        let session_id = match request(&config, &DaemonCommand::CreateSession).await? {
            DaemonReply::SessionId(session_id) => session_id,
            other => panic!("unexpected create-session reply: {other:?}"),
        };
        assert_ne!(session_id, SessionId::default());

        stop_daemon(&config).await?;
        server.await.expect("daemon task join")?;
        Ok(())
    }

    #[tokio::test]
    async fn daemon_lists_session_previews() -> Result<()> {
        let Some(config) = test_config() else {
            return Ok(());
        };
        let server = tokio::spawn(run_daemon_server(config.clone()));
        wait_for_daemon(&config, std::time::Duration::from_secs(5)).await?;

        let _ = request(&config, &DaemonCommand::CreateSession).await?;
        let previews = match request(&config, &DaemonCommand::ListSessionPreviews).await? {
            DaemonReply::SessionPreviews(previews) => previews,
            other => panic!("unexpected preview reply: {other:?}"),
        };
        assert!(!previews.is_empty());

        stop_daemon(&config).await?;
        server.await.expect("daemon task join")?;
        Ok(())
    }

    #[tokio::test]
    async fn daemon_health_endpoint_responds_when_cloud_enabled() -> Result<()> {
        let Some(mut config) = test_config() else {
            return Ok(());
        };
        config.cloud.enabled = true;
        config.cloud.hands = None;
        if let Some(fly) = config.cloud.flyio.as_mut() {
            fly.health_bind = "127.0.0.1".to_string();
            fly.internal_port = random_port();
            fly.graceful_shutdown_timeout_secs = 1;
        }

        let port = config
            .cloud
            .flyio
            .as_ref()
            .expect("fly config")
            .internal_port;
        let server = tokio::spawn(run_daemon_server(config.clone()));
        wait_for_daemon(&config, Duration::from_secs(5)).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await?;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

        stop_daemon(&config).await?;
        server.await.expect("daemon task join")?;
        Ok(())
    }
}
