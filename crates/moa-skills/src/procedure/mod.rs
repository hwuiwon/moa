//! Deterministic procedure execution for skill-backed procedures.
//!
//! A procedure is the optional deterministic graph a skill definition may carry
//! (`SkillDefinition::procedure`). This module owns the pure graph interpreter
//! and the durable run lifecycle that binds it to the artifact registry, while
//! the orchestrator layers Restate and authorization on top. Skills without a
//! procedure are purely agent-mediated and never reach this module.

/// Procedure runtime errors.
pub mod error;
/// Focused JSON Schema validation for procedure run input.
mod input_validation;
/// Pure procedure graph interpreter.
pub mod interpreter;
/// Durable procedure run lifecycle operations.
pub mod runtime;
