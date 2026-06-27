//! Consolidate service e2e coverage.

#[path = "integration/consolidate_e2e.rs"]
mod consolidate_e2e;
mod support {
    pub mod restate_admin_url;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;

    pub mod restate_runtime {
        pub use super::restate_admin_url::restate_admin_url;
        pub use super::restate_ingress_url::restate_ingress_url;
        pub use super::restate_lock::RESTATE_E2E_LOCK;
        pub use super::restate_ports::{
            OrchestratorPorts, deployment_endpoint_url, reserve_orchestrator_ports,
        };
        pub use super::restate_register::register_deployment;
    }
}
