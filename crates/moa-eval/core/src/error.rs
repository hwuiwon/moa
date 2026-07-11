//! Error types for loading and reporting MOA evaluation artifacts.

use std::path::PathBuf;

/// Result alias used throughout `moa-eval-core`.
pub type Result<T> = std::result::Result<T, EvalError>;

/// Errors returned by the `moa-eval-core` crate.
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
    /// Parsing a JSON document failed.
    #[error("failed to parse JSON from {path}: {source}")]
    ParseJson {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying JSON parsing error.
        source: serde_json::Error,
    },
    /// A MOA runtime component returned an error.
    #[error(transparent)]
    Moa(#[from] moa_core::error::MoaError),
    /// A Tokio task failed to join.
    #[error("task join failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// JSON serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// An owning eval layer failed while reading or writing its storage.
    #[error("eval storage operation failed: {0}")]
    Storage(String),
    /// Regex compilation failed while evaluating output expectations.
    #[error(transparent)]
    Regex(#[from] regex::Error),
    /// A config or fixture path was invalid for eval execution.
    #[error("invalid eval configuration: {0}")]
    InvalidConfig(String),
}
