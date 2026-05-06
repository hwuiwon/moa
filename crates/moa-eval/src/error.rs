//! Error types for loading and reporting MOA evaluation artifacts.

use std::path::PathBuf;

/// Result alias used throughout `moa-eval`.
pub type Result<T> = std::result::Result<T, EvalError>;

/// Errors returned by the `moa-eval` crate.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// Reading a file or directory failed.
    #[error("failed to read {path}: {source}")]
    Io {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Parsing a TOML document failed.
    #[error("failed to parse TOML from {path}: {source}")]
    ParseToml {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML parsing error.
        source: toml::de::Error,
    },
    /// Serializing a TOML document failed.
    #[error("failed to serialize TOML: {0}")]
    SerializeToml(#[from] toml::ser::Error),
    /// A MOA runtime component returned an error.
    #[error(transparent)]
    Moa(#[from] moa_core::MoaError),
    /// A Tokio task failed to join.
    #[error("task join failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// JSON serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// SQL execution failed.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// Regex compilation failed while evaluating output expectations.
    #[error(transparent)]
    Regex(#[from] regex::Error),
    /// A config or fixture path was invalid for eval execution.
    #[error("invalid eval configuration: {0}")]
    InvalidConfig(String),
    /// A run could not complete because it was waiting on a human approval decision.
    #[error("eval run blocked on approval for tool {tool}")]
    ApprovalRequired {
        /// Tool name that required approval.
        tool: String,
    },
    /// HTTP reporting failed.
    #[cfg(feature = "langfuse")]
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
