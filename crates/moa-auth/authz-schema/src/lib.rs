//! Authorization schema for MOA.
//!
//! This crate is the single source of truth for object types, relations, and
//! the OpenFGA model DSL. Other crates depend on this for compile-time checked
//! tuple construction.
//!
//! Schema versioning is append-only: schema changes write a new model ID, and
//! old tuples remain valid under the old model. See [`MODEL_VERSION`].

pub mod tuple;

pub use tuple::{ObjectType, Relation, TupleKey, TupleKeyWire, TupleOp, UserType};

/// The authoritative OpenFGA model DSL for schema v1.
///
/// This string is what `moa-fga-bootstrap` ships to OpenFGA through
/// `WriteAuthorizationModel`. It is checked into version control because the
/// schema is part of the security contract, not deployment configuration.
pub const SCHEMA_V1_DSL: &str = include_str!("schema_v1.fga");

/// Logical version of the schema.
///
/// Increment this on any change that adds, removes, or restructures relations.
/// Outbox idempotency keys include this version so a tuple written under v1
/// cannot be silently re-applied under v2.
pub const MODEL_VERSION: u32 = 2;
