//! Error types for tenant knowledge ingestion.

use std::time::Duration;

/// Result type used by the tenant knowledge crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised by provider, parser, normalization, and repository operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required configuration value is missing or invalid.
    #[error("knowledge configuration error: {0}")]
    Config(String),
    /// A linked-account provider failed.
    #[error("knowledge provider `{provider}` failed: {message}")]
    Provider {
        /// Provider identifier.
        provider: String,
        /// Safe failure message.
        message: String,
    },
    /// A parser adapter failed.
    #[error("document parser `{parser}` failed: {message}")]
    Parser {
        /// Parser identifier.
        parser: String,
        /// Safe failure message.
        message: String,
    },
    /// An HTTP service returned an unsuccessful status.
    #[error("knowledge HTTP request failed with status {status}: {message}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Optional retry-after delay when supplied.
        retry_after: Option<Duration>,
        /// Safe response message.
        message: String,
    },
    /// A transport request failed before a response was received.
    #[error("knowledge HTTP transport failed: {0}")]
    Transport(String),
    /// A response body could not be decoded.
    #[error("knowledge response decode failed: {0}")]
    Decode(String),
    /// The requested parser cannot handle this input.
    #[error("unsupported document format: {0}")]
    UnsupportedFormat(String),
    /// A repository operation failed.
    #[error("knowledge repository failed: {0}")]
    Repository(String),
    /// An embedding provider returned a different number of vectors than inputs,
    /// violating the provider cardinality contract. The whole batch is rejected
    /// rather than silently zipped (which would drop or misalign chunks).
    #[error("embedding provider returned {actual} vectors for {expected} inputs")]
    EmbeddingCardinalityMismatch {
        /// Number of inputs sent to the embedding provider.
        expected: usize,
        /// Number of vectors the provider returned.
        actual: usize,
    },
}

impl Error {
    /// Creates a provider error with a safe message.
    #[must_use]
    pub fn provider(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Provider {
            provider: provider.into(),
            message: message.into(),
        }
    }

    /// Creates a parser error with a safe message.
    #[must_use]
    pub fn parser(parser: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Parser {
            parser: parser.into(),
            message: message.into(),
        }
    }
}
