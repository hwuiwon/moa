//! Shared test-support modules for moa-orchestrator integration and e2e tests.
//!
//! Each test binary includes this module once via
//! `#[path = "support/mod.rs"] mod support;` and uses only a subset of the
//! helpers below, so unused fixtures and re-exports are expected here and
//! allowed crate-wide for the whole module tree.
#![allow(dead_code, unused_imports)]

pub mod durable_step_replay_recorder;
pub mod fake_clock;
pub mod grants;
pub mod graph_ingested_brain_responses;
pub mod restate_env;
pub mod restate_identity;
pub mod restate_ports;
pub mod restate_register;
pub mod session_fixtures;

/// Facade aggregating the helpers used to drive a live Restate runtime.
pub mod restate_runtime {
    pub use super::grants::{grant_session_participant, grant_tenant_admin, grant_tenant_operator};
    pub use super::restate_env::{RESTATE_E2E_LOCK, restate_admin_url, restate_ingress_url};
    pub use super::restate_identity::{test_user_identity, with_identity};
    pub use super::restate_ports::{
        OrchestratorPorts, deployment_endpoint_url, reserve_orchestrator_ports,
    };
    pub use super::restate_register::register_deployment;
}

/// Facade aggregating SessionStore request and metadata fixtures.
pub mod session_store_service {
    pub use super::session_fixtures::{
        append_event_request, get_events_request, init_session_vo_request,
        storage_partition_id_from_meta, test_session_meta, user_message, user_message_event,
    };
}

/// Facade exposing the graph-ingestion wait helper.
pub mod graph_ingest {
    pub use super::graph_ingested_brain_responses::wait_for_ingested_brain_responses;
}

/// Facade exposing the durable-step replay recorder.
pub mod durable_step_recorder {
    pub use super::durable_step_replay_recorder::{DurableStep, Recorder, assert_traces_identical};
}
