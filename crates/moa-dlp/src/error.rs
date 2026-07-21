//! Typed failures for request-scoped DLP tokenization and restoration.

use moa_memory_pii::PiiError;

use crate::vault::{TokenDestination, TokenSourceRole};

/// Result type returned by DLP operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Fail-closed errors raised by the DLP boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request namespace could not be initialized from operating-system entropy.
    #[error("DLP request namespace entropy is unavailable")]
    EntropyUnavailable,
    /// Cleartext input already contained the reserved token delimiters.
    #[error("input contains reserved DLP token syntax")]
    LiteralTokenSyntax,
    /// A classifier emitted an empty span.
    #[error("classifier emitted an empty span at byte {start}")]
    EmptySpan {
        /// Span start byte.
        start: usize,
    },
    /// A classifier emitted a reversed span.
    #[error("classifier emitted a reversed span {start}..{end}")]
    ReversedSpan {
        /// Span start byte.
        start: usize,
        /// Span end byte.
        end: usize,
    },
    /// A classifier emitted offsets outside the input.
    #[error("classifier span {start}..{end} exceeds input length {text_len}")]
    SpanOutOfBounds {
        /// Span start byte.
        start: usize,
        /// Span end byte.
        end: usize,
        /// Input length in bytes.
        text_len: usize,
    },
    /// A classifier emitted an offset that is not a UTF-8 character boundary.
    #[error("classifier span {start}..{end} is not on UTF-8 character boundaries")]
    NonUtf8Boundary {
        /// Span start byte.
        start: usize,
        /// Span end byte.
        end: usize,
    },
    /// Two classifier spans overlap.
    #[error("classifier spans overlap at byte {start}")]
    OverlappingSpans {
        /// Start byte of the later span.
        start: usize,
    },
    /// The classifier could not inspect an outbound field.
    #[error("DLP classification failed for field '{field}'")]
    ClassificationFailed {
        /// Structural field path, never field contents.
        field: String,
        /// Underlying classifier error.
        #[source]
        source: PiiError,
    },
    /// The classifier explicitly abstained.
    #[error("DLP classifier abstained for field '{field}'")]
    ClassifierAbstained {
        /// Structural field path, never field contents.
        field: String,
    },
    /// A sensitive verdict did not include complete replaceable spans.
    #[error("DLP classifier reported sensitive content without complete spans for field '{field}'")]
    IncompleteSensitiveSpans {
        /// Structural field path, never field contents.
        field: String,
    },
    /// Tokenizing structured object keys would merge two distinct entries.
    #[error("DLP tokenization produced an object-key collision in field '{field}'")]
    StructuredKeyCollision {
        /// Structural field path, never the key contents.
        field: String,
    },
    /// A token's source is not allowed to flow to the requested destination.
    #[error("DLP token from {role:?} field '{field}' is not allowed in {destination:?}")]
    DestinationDenied {
        /// Role that introduced the protected value.
        role: TokenSourceRole,
        /// Structural source field.
        field: String,
        /// Destination the value attempted to enter.
        destination: TokenDestination,
    },
}
