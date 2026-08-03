//! Clean-apply and idempotency coverage for the central refinery migration runner.
//!
//! The lane remains one integration-test binary while focused child modules
//! preserve the migration protocol, catalog, and subsystem scenarios.

#[path = "run_idempotency_db/connectors.rs"]
mod connectors;
#[path = "run_idempotency_db/execution_and_security_catalog.rs"]
mod execution_and_security_catalog;
#[path = "run_idempotency_db/hand_leases.rs"]
mod hand_leases;
#[path = "run_idempotency_db/knowledge.rs"]
mod knowledge;
#[path = "run_idempotency_db/learning_and_lineage.rs"]
mod learning_and_lineage;
#[path = "run_idempotency_db/protocol.rs"]
mod protocol;
#[path = "run_idempotency_db/support.rs"]
mod support;
#[path = "run_idempotency_db/tenant_purge.rs"]
mod tenant_purge;
