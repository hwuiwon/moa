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
pub mod registry;
#[cfg(feature = "skill-learning")]
pub mod regression;
pub mod render;
