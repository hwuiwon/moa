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
}
