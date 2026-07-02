//! Skill package parsing, registry, rendering, and optional learning support.

#![recursion_limit = "256"]

pub mod artifact;
#[cfg(feature = "skill-learning")]
pub mod candidates;
#[cfg(feature = "skill-learning")]
pub mod distiller;
pub mod format;
#[cfg(feature = "skill-learning")]
pub mod improver;
pub mod lessons;
pub mod package;
/// Deterministic procedure graph execution for skill-backed procedures.
pub mod procedure;
#[cfg(feature = "skill-learning")]
pub mod proposals;
pub mod registry;
#[cfg(feature = "regression")]
pub mod regression;
pub mod render;
pub mod review;
mod util;
