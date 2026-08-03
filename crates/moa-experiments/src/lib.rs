//! Domain types for MOA experiment runs and scorecard configuration.
//!
//! This crate intentionally stays below the orchestration layer. It models
//! experiment definitions and durable records without depending on Restate or
//! `moa-orchestrator`.

pub mod app;
pub mod eligibility;
pub mod evaluator;
pub mod evidence;
pub mod model;
pub mod plan;
pub mod score_store;
pub mod scores;
pub mod simulator_policy;
pub mod store;
