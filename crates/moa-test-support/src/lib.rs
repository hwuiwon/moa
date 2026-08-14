//! Internal test fixtures and helpers for MOA crates.
//!
//! This crate is `publish = false` and must only be used from `[dev-dependencies]`.
//! It intentionally centralizes test-only fixtures so pricing assertions,
//! recorded transcripts, and Postgres bootstraps do not drift across crates.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod execution_audits;

#[cfg(feature = "capability-fixture")]
pub mod fixture_capability;
#[cfg(feature = "connector-api-fixture")]
pub mod fixture_connector_api;
pub mod fixtures;
pub mod invariants;
pub mod postgres;
pub mod pricing;

#[cfg(feature = "orchestrator-fixture")]
pub mod process;

#[cfg(feature = "orchestrator-fixture")]
mod orchestrator_fixture;

#[cfg(feature = "orchestrator-fixture")]
pub use orchestrator_fixture::{
    ConversationOptions, FIXTURE_EXTERNAL_JOB_CALLBACK_TOKEN, FIXTURE_EXTERNAL_JOB_PROVIDER,
    FixtureCapabilityAttempt, FixtureCapabilityCall, FixtureCapabilityController,
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    FixtureExternalJobAfterBind, FixtureExternalJobController, FixtureExternalJobReconciliation,
    FixtureExternalJobRecovery, FixtureExternalJobStart, FixtureHandlerRevision, IsolatedTest,
    OrchestratorTestFixture, RustFsFixture, SandboxWorkspaceCrashBarrier,
    SandboxWorkspaceCrashControl, SandboxWorkspaceFixture, TestApiClient, TestSessionHandle,
    WorkspaceRestartProbe, drive_conversation, provision_workspace_maintenance_login,
};
