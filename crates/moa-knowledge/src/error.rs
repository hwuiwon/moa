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
    /// A rechunk was asked to activate before every staged member was present.
    #[error("rechunk staging for document version {document_version_uid} is missing: {missing}")]
    RechunkStagingIncomplete {
        /// Document version whose staging set is short.
        document_version_uid: uuid::Uuid,
        /// Members that are absent.
        missing: String,
    },
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
    /// A database driver error with its diagnostic fields preserved.
    ///
    /// Concurrency failures are only diagnosable from these fields: a
    /// duplicate key surfaces as SQLSTATE `23505` plus the violated
    /// constraint and key tuple, while a deadlock surfaces as `40P01`.
    /// Collapsing them into one opaque string (the old `Repository` mapping)
    /// made full-suite-only races unattributable.
    #[error(
        "knowledge database operation failed: {message}{}",
        database_error_suffix(code, constraint, table, detail)
    )]
    Database {
        /// Five-character SQLSTATE reported by the database, when available.
        code: Option<String>,
        /// Violated constraint name, when the driver reports one.
        constraint: Option<String>,
        /// Affected table, when the driver reports one.
        table: Option<String>,
        /// Primary driver message.
        message: String,
        /// `DETAIL` line (for example the duplicate key tuple), when present.
        detail: Option<String>,
    },
    /// A model-backed semantic graph extraction call, timeout, or parse failed.
    ///
    /// The ingestion pipeline treats this as a per-chunk signal to fall back to
    /// the deterministic keyword extractor, so it never aborts a sync run.
    #[error("semantic graph model extraction failed: {0}")]
    ModelExtraction(String),
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

/// Formats the optional database diagnostics as a bracketed display suffix.
fn database_error_suffix(
    code: &Option<String>,
    constraint: &Option<String>,
    table: &Option<String>,
    detail: &Option<String>,
) -> String {
    let fields = [
        ("sqlstate", code),
        ("constraint", constraint),
        ("table", table),
        ("detail", detail),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.as_deref().map(|value| format!("{label}={value}")))
    .collect::<Vec<_>>();
    if fields.is_empty() {
        String::new()
    } else {
        format!(" [{}]", fields.join(", "))
    }
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
