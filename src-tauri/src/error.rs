//! Serializable error type for the Tauri IPC boundary.

use serde::Serialize;
use thiserror::Error;

/// Result alias used by Tauri commands.
pub type AppResult<T> = std::result::Result<T, MoaAppError>;

/// Error payload returned from the desktop backend to the frontend.
#[derive(Debug, Error, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum MoaAppError {
    /// The frontend supplied invalid input.
    #[error("{0}")]
    InvalidInput(String),
    /// The MOA runtime returned an application error.
    #[error("{0}")]
    Runtime(String),
    /// The desktop backend failed unexpectedly.
    #[error("{0}")]
    Internal(String),
}

impl From<moa_core::MoaError> for MoaAppError {
    fn from(error: moa_core::MoaError) -> Self {
        Self::Runtime(error.to_string())
    }
}

impl From<uuid::Error> for MoaAppError {
    fn from(error: uuid::Error) -> Self {
        Self::InvalidInput(error.to_string())
    }
}
