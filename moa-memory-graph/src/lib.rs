//! Graph-memory sidecar types and SQL helpers.

pub mod changelog;
pub mod node;

/// Result type returned by graph-memory helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by graph-memory SQL helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A node label stored in Postgres is not part of the supported label set.
    #[error("unknown node label `{0}`")]
    UnknownNodeLabel(String),
    /// A PII class stored in Postgres is not part of the supported class set.
    #[error("unknown PII class `{0}`")]
    UnknownPiiClass(String),
    /// A changelog record's explicit scope does not match its workspace/user shape.
    #[error("changelog scope `{actual}` does not match computed scope `{expected}`")]
    ChangelogScopeMismatch {
        /// Caller-provided scope string.
        actual: String,
        /// Scope computed from `workspace_id` and `user_id`.
        expected: &'static str,
    },
    /// A changelog record used an unsupported workspace/user shape.
    #[error("changelog user scope requires a workspace_id")]
    InvalidChangelogScope,
    /// The underlying Postgres query failed.
    #[error("graph-memory query failed: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub use changelog::{ChangelogRecord, write_and_bump};
pub use node::{NodeIndexRow, NodeLabel, PiiClass, bump_last_accessed, lookup_seed_by_name};
