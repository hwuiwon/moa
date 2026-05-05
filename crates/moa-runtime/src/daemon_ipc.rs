//! Daemon transport helpers for unix-socket command and stream relays.

use std::path::Path;

use moa_core::{
    DaemonCommand, DaemonReply, DaemonStreamEvent, LiveEvent, MoaError, Result, RuntimeEvent,
    SessionId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::helpers::SessionRuntimeEvent;

#[cfg(unix)]
pub(crate) type DaemonSocket = UnixStream;
#[cfg(not(unix))]
pub(crate) type DaemonSocket = ();

#[cfg(unix)]
pub(crate) type DaemonReader = BufReader<DaemonSocket>;
#[cfg(not(unix))]
pub(crate) type DaemonReader = ();

#[cfg(unix)]
pub(crate) async fn daemon_request(
    socket_path: &Path,
    command: &DaemonCommand,
) -> Result<DaemonReply> {
    let socket = daemon_connect(socket_path).await?;
    let mut reader = daemon_send_command(socket, command).await?;
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Err(MoaError::ProviderError(
            "daemon closed the control connection".to_string(),
        ));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

#[cfg(not(unix))]
pub(crate) async fn daemon_request(
    _socket_path: &Path,
    _command: &DaemonCommand,
) -> Result<DaemonReply> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
pub(crate) async fn daemon_expect_ack(socket_path: &Path, command: &DaemonCommand) -> Result<()> {
    match daemon_request(socket_path, command).await? {
        DaemonReply::Ack => Ok(()),
        DaemonReply::Error(message) => Err(MoaError::ProviderError(message)),
        other => Err(crate::helpers::unexpected_daemon_reply("ack", &other)),
    }
}

#[cfg(not(unix))]
pub(crate) async fn daemon_expect_ack(_socket_path: &Path, _command: &DaemonCommand) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
pub(crate) async fn daemon_is_available(socket_path: &Path) -> bool {
    UnixStream::connect(socket_path).await.is_ok()
}

#[cfg(not(unix))]
pub(crate) async fn daemon_is_available(_socket_path: &Path) -> bool {
    false
}

#[cfg(unix)]
pub(crate) async fn daemon_connect(socket_path: &Path) -> Result<DaemonSocket> {
    UnixStream::connect(socket_path)
        .await
        .map_err(MoaError::from)
}

#[cfg(not(unix))]
pub(crate) async fn daemon_connect(_socket_path: &Path) -> Result<DaemonSocket> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
pub(crate) async fn daemon_send_command(
    mut socket: DaemonSocket,
    command: &DaemonCommand,
) -> Result<DaemonReader> {
    let payload = serde_json::to_string(command)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    socket.write_all(payload.as_bytes()).await?;
    socket.write_all(b"\n").await?;
    Ok(BufReader::new(socket))
}

#[cfg(not(unix))]
pub(crate) async fn daemon_send_command(
    _socket: DaemonSocket,
    _command: &DaemonCommand,
) -> Result<DaemonReader> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(unix)]
pub(crate) async fn relay_daemon_runtime_events(
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    mut reader: DaemonReader,
) -> Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(event) => {
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id,
                        event: LiveEvent::Event(event),
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Gap { count, channel } => {
                if event_tx
                    .send(SessionRuntimeEvent {
                        session_id,
                        event: LiveEvent::Gap {
                            count,
                            channel,
                            since_seq: None,
                        },
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
        }
    }
}

#[cfg(unix)]
pub(crate) async fn relay_daemon_runtime_turn_events(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    mut reader: DaemonReader,
) -> Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let event: DaemonStreamEvent = serde_json::from_str(line.trim_end())
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        match event {
            DaemonStreamEvent::Ready => continue,
            DaemonStreamEvent::Runtime(event) => {
                let should_stop = matches!(event, RuntimeEvent::TurnCompleted);
                if event_tx.send(event).is_err() || should_stop {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Gap { count, .. } => {
                let message = format!(
                    "… {count} runtime events missed (subscriber was behind; live preview resumed) …"
                );
                if event_tx.send(RuntimeEvent::Notice(message)).is_err() {
                    return Ok(());
                }
            }
            DaemonStreamEvent::Error(message) => {
                return Err(MoaError::ProviderError(message));
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn relay_daemon_runtime_events(
    _session_id: SessionId,
    _event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    _reader: DaemonReader,
) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}

#[cfg(not(unix))]
pub(crate) async fn relay_daemon_runtime_turn_events(
    _event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    _reader: DaemonReader,
) -> Result<()> {
    Err(MoaError::Unsupported(
        "daemon mode requires unix-domain sockets".to_string(),
    ))
}
