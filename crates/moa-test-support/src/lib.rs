//! Internal test fixtures and helpers for MOA crates.
//!
//! This crate is `publish = false` and must only be used from `[dev-dependencies]`.
//! It intentionally centralizes test-only fixtures so pricing assertions,
//! recorded transcripts, and Postgres bootstraps do not drift across crates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod postgres;
pub mod pricing;
pub mod transcript;

mod orchestrator_fixture;

pub use orchestrator_fixture::{IsolatedTest, OrchestratorTestFixture, SerializedTest};
