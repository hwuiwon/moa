//! Graph-memory sidecar types and SQL helpers.

pub mod node;

pub use node::{
    Error, NodeIndexRow, NodeLabel, PiiClass, Result, bump_last_accessed, lookup_seed_by_name,
};
