//! Session vo provider e2e coverage.

#[path = "integration/session_vo_e2e.rs"]
mod session_vo_e2e;
mod support {
    pub mod grant_session_participant;
    pub mod grant_tenant_operator;
    pub mod graph_ingested_brain_responses;
    pub mod restate_admin_url;
    pub mod restate_identity;
    pub mod restate_ingress_url;
    pub mod restate_lock;
    pub mod restate_ports;
    pub mod restate_register;
    pub mod session_get_events;
    pub mod session_init_vo;
    pub mod session_meta_fixture;
    pub mod session_storage_partition;
    pub mod session_user_message;

    pub mod graph_ingest {
        pub use super::graph_ingested_brain_responses::wait_for_ingested_brain_responses;
    }

    pub mod restate_runtime {
        pub use super::grant_session_participant::grant_session_participant;
        pub use super::grant_tenant_operator::grant_tenant_operator;
        pub use super::restate_admin_url::restate_admin_url;
        pub use super::restate_identity::{test_user_identity, with_identity};
        pub use super::restate_ingress_url::restate_ingress_url;
        pub use super::restate_lock::RESTATE_E2E_LOCK;
        pub use super::restate_ports::{
            OrchestratorPorts, deployment_endpoint_url, reserve_orchestrator_ports,
        };
        pub use super::restate_register::register_deployment;
    }

    pub mod session_store_service {
        pub use super::session_get_events::get_events_request;
        pub use super::session_init_vo::init_session_vo_request;
        pub use super::session_meta_fixture::test_session_meta;
        pub use super::session_storage_partition::storage_partition_id_from_meta;
        pub use super::session_user_message::user_message;
    }
}
