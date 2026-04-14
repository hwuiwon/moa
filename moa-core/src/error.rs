//! Shared error types for MOA crates.

use std::io;

use thiserror::Error;

use crate::types::{SessionId, WorkspaceId};

/// Convenience result type for MOA libraries.
pub type Result<T> = std::result::Result<T, MoaError>;

/// Common error variants shared across MOA crates.
#[derive(Debug, Error)]
pub enum MoaError {
    /// The requested session does not exist.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// The requested workspace does not exist.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(WorkspaceId),

    /// A provider returned an error.
    #[error("provider error: {0}")]
    ProviderError(String),

    /// A required environment variable is not set.
    #[error("missing environment variable: {0}")]
    MissingEnvironmentVariable(String),

    /// Configuration loading or validation failed.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Storage access failed.
    #[error("storage error: {0}")]
    StorageError(String),

    /// A referenced blob payload could not be found.
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    /// Tool execution failed.
    #[error("tool error: {0}")]
    ToolError(String),

    /// Validation failed.
    #[error("validation error: {0}")]
    ValidationError(String),

    /// An upstream provider returned something unexpected (e.g. a field
    /// shape that changed without notice) but the session can continue
    /// after we skip the offending chunk. Non-fatal — the orchestrator
    /// pauses the session instead of killing it.
    #[error("provider quirk: {0}")]
    ProviderQuirk(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// An HTTP request returned a non-success status.
    #[error("http status {status}: {message}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// The error message or response body.
        message: String,
    },

    /// The provider rate limited the request after retries.
    #[error("rate limited after {retries} retries: {message}")]
    RateLimited {
        /// Number of retry attempts performed.
        retries: usize,
        /// Provider-supplied error message when available.
        message: String,
    },

    /// An error occurred while parsing or consuming a stream.
    #[error("stream error: {0}")]
    StreamError(String),

    /// Permission to perform an action was denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The workspace has exhausted its configured daily spend budget.
    #[error("daily workspace budget exhausted: {0}")]
    BudgetExhausted(String),

    /// Operation was cancelled by the user.
    #[error("operation cancelled by user")]
    Cancelled,

    /// The requested functionality is unsupported in the current mode.
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// The user's home directory could not be resolved.
    #[error("home directory not found")]
    HomeDirectoryNotFound,

    /// An I/O error occurred.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A config crate error occurred.
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    /// A JSON serialization error occurred.
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    /// A UUID parsing error occurred.
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
}

impl MoaError {
    /// Whether this error should terminate the session (`true`) or is
    /// recoverable (the orchestrator pauses the session so the user can
    /// resume, retry, or intervene). Fatal classes are typically
    /// configuration, auth/permission, storage, and anything that
    /// indicates the process can't meaningfully continue.
    ///
    /// Recoverable classes are provider quirks, single-payload
    /// deserialization failures, exhausted rate-limit retries, and
    /// transient stream hiccups.
    pub fn is_fatal(&self) -> bool {
        match self {
            // Configuration, identity, and infrastructure — can't be
            // "retried" without user action.
            Self::ConfigError(_)
            | Self::StorageError(_)
            | Self::MissingEnvironmentVariable(_)
            | Self::HomeDirectoryNotFound
            | Self::PermissionDenied(_)
            | Self::BudgetExhausted(_)
            | Self::Unsupported(_)
            | Self::Io(_)
            | Self::Config(_) => true,
            // Single-payload/event-shaped problems — resumable.
            Self::ProviderQuirk(_)
            | Self::ValidationError(_)
            | Self::SerializationError(_)
            | Self::RateLimited { .. }
            | Self::StreamError(_)
            | Self::HttpStatus { .. }
            | Self::ProviderError(_)
            | Self::ToolError(_)
            | Self::SerdeJson(_)
            | Self::Uuid(_) => false,
            // Session/blob lookups and cancellation are neither fatal
            // in the "kill the app" sense nor recoverable within the
            // same session — treat them as fatal so the supervisor
            // doesn't leave a broken session in `Paused`.
            Self::SessionNotFound(_) | Self::WorkspaceNotFound(_) | Self::BlobNotFound(_) => true,
            Self::Cancelled => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MoaError;

    #[test]
    fn provider_quirk_is_non_fatal() {
        assert!(!MoaError::ProviderQuirk("new field".into()).is_fatal());
    }

    #[test]
    fn validation_error_is_non_fatal() {
        assert!(!MoaError::ValidationError("compatibility: ...".into()).is_fatal());
    }

    #[test]
    fn config_error_is_fatal() {
        assert!(MoaError::ConfigError("bad toml".into()).is_fatal());
    }
}
