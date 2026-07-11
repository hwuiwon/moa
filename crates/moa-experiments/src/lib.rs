//! Domain types for MOA experiment runs and scorecard configuration.
//!
//! This crate intentionally stays below the orchestration layer. It models
//! experiment definitions and durable records without depending on Restate or
//! `moa-orchestrator`.

pub mod app;
pub mod model;
pub mod plan;
pub mod scores;
pub mod store;
