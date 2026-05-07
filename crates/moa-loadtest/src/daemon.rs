//! Daemon socket protocol helpers for load tests.

use crate::*;

pub(crate) async fn daemon_request(
    socket_path: &Path,
    command: &DaemonCommand,
) -> Result<DaemonReply> {
    let mut reader = daemon_open_stream(socket_path, command).await?;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(MoaError::ProviderError(
            "daemon closed the control connection".to_string(),
        ));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

pub(crate) async fn daemon_expect_ack(socket_path: &Path, command: &DaemonCommand) -> Result<()> {
    match daemon_request(socket_path, command).await? {
        DaemonReply::Ack => Ok(()),
        DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
        other => Err(unexpected_daemon_reply("ack", &other)),
    }
}

pub(crate) async fn daemon_open_stream(
    socket_path: &Path,
    command: &DaemonCommand,
) -> Result<BufReader<UnixStream>> {
    #[cfg(not(unix))]
    {
        let _ = socket_path;
        let _ = command;
        return Err(MoaError::Unsupported(
            "daemon mode requires unix-domain sockets".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        let mut socket = UnixStream::connect(socket_path).await?;
        let payload = serde_json::to_string(command)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        socket.write_all(payload.as_bytes()).await?;
        socket.write_all(b"\n").await?;
        Ok(BufReader::new(socket))
    }
}

pub(crate) async fn daemon_recv_runtime_event(
    reader: &mut BufReader<UnixStream>,
) -> Result<RuntimeEvent> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(MoaError::ProviderError(
                "daemon runtime stream closed unexpectedly".to_string(),
            ));
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(runtime) => return Ok(runtime),
            DaemonStreamEvent::Gap { count, channel } => {
                return Ok(RuntimeEvent::Notice(format!(
                    "missed {count} daemon runtime events on {}",
                    channel.as_str()
                )));
            }
            DaemonStreamEvent::Error(message) => return Err(MoaError::ProviderError(message)),
        }
    }
}

pub(crate) fn unexpected_daemon_reply(expected: &str, reply: &DaemonReply) -> MoaError {
    MoaError::ProviderError(format!(
        "expected daemon reply `{expected}`, received {reply:?}"
    ))
}

pub(crate) fn map_broadcast_error(error: broadcast::error::RecvError) -> MoaError {
    match error {
        broadcast::error::RecvError::Closed => {
            MoaError::ProviderError("runtime stream closed".to_string())
        }
        broadcast::error::RecvError::Lagged(skipped) => {
            MoaError::ProviderError(format!("runtime stream lagged by {skipped} events"))
        }
    }
}
