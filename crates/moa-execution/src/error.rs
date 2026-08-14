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
    /// Two catalog entries claimed the same capability reference.
    #[error("duplicate capability reference in catalog: {reference} at version {version}")]
    DuplicateCapabilityReference {
        /// Capability reference claimed more than once.
        reference: String,
        /// Version the duplicated reference was claimed at.
        version: String,
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
    /// Fleet or tenant admission has no room for one durable execution resource.
    #[error("execution capacity saturated for {dimension}")]
    CapacitySaturated {
        /// Closed execution-capacity dimension; never includes owner labels.
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
    /// A database operation failed with its SQLx provenance intact.
    #[error("execution repository database error: {source}")]
    Database {
        /// Original SQLx failure used to determine whether replay is safe.
        #[source]
        source: sqlx::Error,
    },
    /// A shared database helper reported transient storage unavailability.
    #[error("execution repository storage unavailable: {message}")]
    StorageUnavailable {
        /// Human-readable failure context retained by the shared database boundary.
        message: String,
    },
}

impl Error {
    /// Returns whether a retry-owning boundary may safely replay this storage failure.
    #[must_use]
    pub fn is_retryable_storage(&self) -> bool {
        match self {
            Self::Database { source } => moa_db::is_retryable_sqlx_error(source),
            Self::StorageUnavailable { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_error_keeps_sqlx_retry_provenance() {
        // Pins: the execution repository preserves a concrete transient SQLx
        // failure until the Restate boundary makes the retry decision.
        let error = Error::Database {
            source: sqlx::Error::PoolClosed,
        };

        assert!(error.is_retryable_storage());
        assert!(matches!(
            error,
            Error::Database {
                source: sqlx::Error::PoolClosed
            }
        ));
    }

    #[test]
    fn repository_error_does_not_retry_terminal_storage_or_decode_failures() {
        // Pins: repository invariants and corrupt row projections remain terminal
        // even though they originate at the database boundary.
        for error in [
            Error::Storage {
                message: "missing required row".to_string(),
            },
            Error::InvalidRepositoryData {
                message: "invalid persisted status".to_string(),
            },
            Error::Database {
                source: sqlx::Error::RowNotFound,
            },
        ] {
            assert!(
                !error.is_retryable_storage(),
                "classified {error} as retryable"
            );
        }
    }
}
