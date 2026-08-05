//! Concurrent PostgreSQL contract coverage for durable execution-run persistence.

#[path = "execution_db/budget_and_materialization_db.rs"]
mod budget_and_materialization_db;
#[path = "execution_db/compensation_db.rs"]
mod compensation_db;
#[path = "execution_db/outcomes_and_replan_db.rs"]
mod outcomes_and_replan_db;
#[path = "execution_db/planning_and_audit_db.rs"]
mod planning_and_audit_db;
#[path = "execution_db/scope_and_lifecycle_db.rs"]
mod scope_and_lifecycle_db;
#[path = "execution_db/support.rs"]
mod support;
