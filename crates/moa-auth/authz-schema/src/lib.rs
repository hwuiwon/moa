//! Authorization schema for MOA.
//!
//! This crate is the single source of truth for object types, relations, and
//! the OpenFGA model. Other crates depend on this for compile-time checked tuple
//! construction.
//!
//! Schema versioning writes a new model ID for every relation change. Current
//! deployments hard-break stale relation names instead of supporting old tuple
//! semantics in parallel. See [`MODEL_VERSION`].

pub mod tuple;

pub use tuple::{ObjectType, Relation, TupleKey, TupleKeyWire, TupleOp, UserType};

/// The OpenFGA JSON model written by `moa-fga-bootstrap`.
///
/// This is the reviewed deployed artifact. It is checked in because the schema
/// is part of the security contract, not deployment configuration.
pub const SCHEMA_V1_JSON: &str = include_str!("schema_v1.json");

/// Logical version of the schema.
///
/// Increment this on any change that adds, removes, or restructures relations.
/// Outbox idempotency keys include this version so a tuple written under v1
/// cannot be silently re-applied under v2.
pub const MODEL_VERSION: u32 = 7;
