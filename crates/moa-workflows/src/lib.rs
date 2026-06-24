//! Workflow runtime logic for artifact-backed workflow definitions.
//!
//! This crate owns runtime lifecycle behavior for workflow artifacts. The
//! orchestrator binds that behavior to Restate and authorization, while
//! `moa-artifacts` remains the canonical artifact model and storage layer.

/// Workflow runtime errors.
pub mod error;
/// Pure workflow graph interpreter.
pub mod interpreter;
/// Durable workflow run lifecycle operations.
pub mod runtime;
