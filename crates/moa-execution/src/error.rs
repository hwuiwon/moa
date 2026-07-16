//! Error contract for execution compilation, interpretation, and persistence.

/// Error returned by fallible `moa-execution` operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Canonical JSON serialization failed.
    #[error("canonical JSON serialization failed: {0}")]
    CanonicalJson(#[from] serde_json::Error),
    /// A serialized execution hash was not exactly 64 lowercase hexadecimal characters.
    #[error("invalid execution hash: {message}")]
    InvalidHash {
        /// Human-readable hash parse failure.
        message: String,
    },
    /// JSON Schema compilation or instance validation failed.
    #[error("JSON schema error at {path}: {message}")]
    Schema {
        /// Schema or instance path associated with the failure.
        path: String,
        /// Human-readable validation failure.
        message: String,
    },
    /// Restricted execution binding resolution failed.
    #[error("execution binding error at {path}: {message}")]
    Binding {
        /// Binding path associated with the failure.
        path: String,
        /// Human-readable binding failure.
        message: String,
    },
    /// Checked integer accounting overflowed.
    #[error("execution arithmetic overflow while computing {context}")]
    ArithmeticOverflow {
        /// Accounting operation that overflowed.
        context: String,
    },
    /// A budget reservation would exceed one approved dimension.
    #[error("execution budget exceeded for {dimension}")]
    BudgetExceeded {
        /// Resource dimension that rejected the reservation.
        dimension: &'static str,
    },
    /// Actual usage exceeded a reservation or configured limit.
    #[error("execution budget overrun for {dimension}")]
    BudgetOverrun {
        /// Resource dimension that overran.
        dimension: &'static str,
    },
    /// A ledger transition supplied an invalid reservation or usage counter.
    #[error("invalid budget ledger transition: {message}")]
    InvalidBudgetLedger {
        /// Human-readable ledger invariant failure.
        message: String,
    },
    /// A logical task identity could not be framed safely.
    #[error("invalid execution task identity: {message}")]
    InvalidTaskIdentity {
        /// Human-readable identity failure.
        message: String,
    },
    /// The supplied projection cannot be interpreted under the active plan.
    #[error("invalid execution projection: {message}")]
    InvalidProjection {
        /// Human-readable projection failure.
        message: String,
    },
    /// A persistence request violated the public repository contract.
    #[error("invalid execution repository request: {message}")]
    InvalidRepositoryInput {
        /// Human-readable request invariant failure.
        message: String,
    },
    /// A persisted row did not match the execution repository schema contract.
    #[error("invalid execution repository data: {message}")]
    InvalidRepositoryData {
        /// Human-readable row decoding or invariant failure.
        message: String,
    },
    /// A database operation failed.
    #[error("execution repository storage error: {message}")]
    Storage {
        /// Database failure with operation context.
        message: String,
    },
}
