//! Shared helper types and utility functions for the thin chat runtime.

use std::env;
use std::path::{Path, PathBuf};

use moa_core::{
    DaemonSessionPreview, Event, EventRecord, LiveEvent, MoaError, Result, RuntimeEvent, SessionId,
    SessionSummary, UserId, WorkspaceId,
};

/// Lightweight session preview used by interactive MOA clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreview {
    /// Persisted session summary row.
    pub summary: SessionSummary,
    /// Most recent conversational message, if any.
    pub last_message: Option<String>,
}

/// Session-scoped runtime update forwarded to interactive MOA clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuntimeEvent {
    /// Session that produced this runtime event.
    pub session_id: SessionId,
    /// Runtime event or typed lag marker.
    pub event: LiveEvent<RuntimeEvent>,
}

impl From<DaemonSessionPreview> for SessionPreview {
    fn from(value: DaemonSessionPreview) -> Self {
        Self {
            summary: value.summary,
            last_message: value.last_message,
        }
    }
}

pub(crate) fn expand_local_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }

    PathBuf::from(path)
}

pub(crate) fn detect_local_workspace_root() -> Result<PathBuf> {
    let cwd = env::current_dir().map_err(|error| {
        MoaError::ProviderError(format!("failed to resolve current directory: {error}"))
    })?;
    let cwd = match cwd.canonicalize() {
        Ok(path) => path,
        Err(_) => cwd,
    };

    for candidate in cwd.ancestors() {
        if candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }

    Ok(cwd)
}

pub(crate) fn workspace_id_for_root(root: &Path) -> WorkspaceId {
    let label = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_workspace_label)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    WorkspaceId::new(label)
}

pub(crate) fn local_user_id() -> UserId {
    UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

pub(crate) fn last_session_message(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } | Event::UserMessage { text, .. } => {
            Some(text.trim().to_string())
        }
        Event::QueuedMessage { text, .. } => Some(format!("Queued: {}", text.trim())),
        _ => None,
    })
}

pub(crate) fn client_error(error: moa_orchestrator_client::Error) -> MoaError {
    match error {
        moa_orchestrator_client::Error::EndpointNotConfigured => {
            MoaError::MissingEnvironmentVariable("MOA__ORCHESTRATOR__ENDPOINT".to_string())
        }
        moa_orchestrator_client::Error::InvalidEndpoint(message) => MoaError::ConfigError(message),
        moa_orchestrator_client::Error::Network(error) => MoaError::ProviderError(format!(
            "orchestrator network error: {error}; run `make dev` for the local stack or set MOA__ORCHESTRATOR__ENDPOINT"
        )),
        moa_orchestrator_client::Error::BadStatus { status, body } => MoaError::HttpStatus {
            status: status.as_u16(),
            retry_after: None,
            message: body,
        },
        moa_orchestrator_client::Error::Decode(error) => {
            MoaError::SerializationError(error.to_string())
        }
        moa_orchestrator_client::Error::Timeout(duration) => MoaError::ProviderError(format!(
            "orchestrator operation timed out after {duration:?}"
        )),
        moa_orchestrator_client::Error::Cancelled => MoaError::Cancelled,
    }
}

fn sanitize_workspace_label(value: &str) -> String {
    let mut label = String::new();
    let mut previous_was_dash = false;

    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            previous_was_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !previous_was_dash {
            previous_was_dash = true;
            Some('-')
        } else {
            None
        };

        if let Some(ch) = normalized {
            label.push(ch);
        }
    }

    label.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::workspace_id_for_root;

    #[test]
    fn workspace_id_for_root_uses_sanitized_directory_name() {
        // Pins: local workspace labels stay stable for mixed-case directory names.
        let workspace_id = workspace_id_for_root(Path::new("/tmp/My Project!"));

        assert_eq!(workspace_id.as_str(), "my-project");
    }

    #[test]
    fn workspace_id_for_root_falls_back_when_basename_is_missing() {
        // Pins: filesystem root still maps to a valid logical workspace ID.
        let workspace_id = workspace_id_for_root(Path::new("/"));

        assert_eq!(workspace_id.as_str(), "workspace");
    }
}
