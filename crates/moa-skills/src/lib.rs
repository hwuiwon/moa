//! Skill package parsing, registry, rendering, and optional learning support.

#![recursion_limit = "256"]

pub mod artifact;
pub mod candidates;
pub mod distiller;
pub mod format;
pub mod improver;
pub mod lessons;
pub mod mining;
pub mod package;
/// Deterministic procedure graph execution for skill-backed procedures.
pub mod procedure;
pub mod proposals;
pub mod registry;
pub mod regression;
pub mod render;
pub mod review;
mod util;
