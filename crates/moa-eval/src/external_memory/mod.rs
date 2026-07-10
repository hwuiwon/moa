//! Backend-neutral contracts for external long-context memory benchmarks.

pub mod answer;
pub mod calibration;
pub mod cost;
pub mod dataset;
pub mod formation;
pub mod harness;
pub mod longmemeval;
pub mod moa_backend;
pub mod personamem;
pub mod report;

/// Error returned by external-memory benchmark contracts and adapters.
#[derive(Debug, thiserror::Error)]
pub enum ExternalMemoryError {
    /// A versioned benchmark contract was invalid.
    #[error("invalid external-memory configuration: {0}")]
    InvalidConfig(String),
    /// A dataset case or package was invalid.
    #[error("invalid external-memory dataset: {0}")]
    InvalidDataset(String),
    /// JSON encoding or decoding failed.
    #[error("external-memory JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A filesystem operation failed.
    #[error("external-memory I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A selected backend failed.
    #[error("external-memory backend: {0}")]
    Backend(String),
}

/// Result returned by external-memory benchmark contracts and adapters.
pub type Result<T> = std::result::Result<T, ExternalMemoryError>;
