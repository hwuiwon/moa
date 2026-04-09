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

    /// Configuration loading or validation failed.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Storage access failed.
    #[error("storage error: {0}")]
    StorageError(String),

    /// Tool execution failed.
    #[error("tool error: {0}")]
    ToolError(String),

    /// Validation failed.
    #[error("validation error: {0}")]
    ValidationError(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// Permission to perform an action was denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

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
