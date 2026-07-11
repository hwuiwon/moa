//! Internal test fixtures and helpers for MOA crates.
//!
//! This crate is `publish = false` and must only be used from `[dev-dependencies]`.
//! It intentionally centralizes test-only fixtures so pricing assertions,
//! recorded transcripts, and Postgres bootstraps do not drift across crates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod fixtures;
pub mod invariants;
pub mod postgres;
pub mod pricing;

#[cfg(feature = "orchestrator-fixture")]
mod orchestrator_fixture;

#[cfg(feature = "orchestrator-fixture")]
pub use orchestrator_fixture::{
    ConversationOptions, IsolatedTest, OrchestratorTestFixture, TestApiClient, TestSessionHandle,
    drive_conversation,
};
