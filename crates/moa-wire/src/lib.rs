//! Shared wire DTO modules for the cloud orchestrator HTTP surface.
//!
//! This crate holds the request/response data-transfer objects exchanged over
//! MOA's public HTTP edge and internal service boundaries. The types depend on
//! base domain types from [`moa_core`] but carry no runtime logic, so consumers
//! that only speak the wire format do not rebuild the core runtime crates.

pub mod admin;
pub mod agents;
pub mod analytics;
pub mod artifacts;
pub mod eval;
pub mod experiments;
pub mod knowledge;
pub mod lineage;
pub mod memory;
pub mod privacy;
pub mod session_store;
pub mod skills;
pub mod tenants;
pub mod tools;
pub mod turn;
