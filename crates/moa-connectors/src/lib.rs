//! Tenant connector connection lifecycle, installed-action catalog, and runtime ports.
//!
//! Artifact definitions remain owned by `moa-artifacts`; this crate owns each
//! tenant installation, its immutable generation-pinned compiled bindings, and
//! the replay-safe invocation ledger used by connector runtimes.

/// Installed connector catalog read boundary.
pub mod catalog;
/// Connection, binding, and invocation domain contracts.
pub mod domain;
/// Crate error contract.
pub mod error;
/// Secret-free connector runtime boundary.
pub mod executor;
/// Constrained HTTP connector action runtime.
pub mod http;
/// Tenant-scoped connector persistence.
pub mod repository;
/// Atomic connector lifecycle application service.
pub mod service;

pub use error::Error;

/// Result returned by connector domain and persistence operations.
pub type Result<T> = std::result::Result<T, Error>;
