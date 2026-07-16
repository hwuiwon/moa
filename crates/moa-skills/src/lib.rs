//! Skill package parsing, registry, rendering, and optional learning support.

#![recursion_limit = "256"]

pub mod artifact;
pub mod candidates;
pub mod distiller;
pub mod embeddings;
pub mod format;
pub mod improver;
pub mod lessons;
pub mod mining;
pub mod package;
pub mod proposals;
pub mod recurrence;
pub mod registry;
pub mod regression;
pub mod render;
pub mod review;
pub mod rollback;
pub mod semantic;
mod util;
