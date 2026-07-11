#![recursion_limit = "256"]

//! Canonical artifact definitions for MOA agents, skills, connectors, actions, and experiment plans.
//!
//! The crate owns the code-addressable document model used by API imports,
//! Postgres storage, and future visual builders. Runtime crates should depend
//! on these types instead of duplicating ad hoc JSON shapes.

/// Standalone action artifact definitions.
pub mod action;
/// Tenant-configurable agent artifact definitions.
pub mod agent;
/// Canonical JSON serialization and hashing helpers.
pub mod canonical;
/// Connector artifact definitions.
pub mod connector;
/// Artifact document wrappers and metadata.
pub mod document;
/// Procedure graph definitions embedded in skills.
pub mod procedure;
/// Stable artifact reference parsing and formatting.
pub mod reference;
/// Postgres-backed artifact registry.
pub mod registry;
/// Reference resolution against published artifact revisions.
pub mod resolver;
/// Behavior-lab experiment plan and embedded simulation definitions.
pub mod simulation;
/// Skill artifact definitions.
pub mod skill;
/// Semantic validation for artifact documents.
pub mod validation;

/// Result type returned by artifact helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by artifact document parsing and canonicalization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// JSON parsing or canonical serialization failed.
    #[error("artifact json: {0}")]
    Json(#[from] serde_json::Error),
    /// YAML parsing or serialization failed.
    #[error("artifact yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// A reference string did not match a supported URI form.
    #[error("invalid artifact reference `{reference}`: {message}")]
    InvalidReference {
        /// The rejected reference string.
        reference: String,
        /// Human-readable rejection reason.
        message: String,
    },
}
