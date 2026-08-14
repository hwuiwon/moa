//! Concurrent PostgreSQL contract coverage for durable execution-run persistence.

#[path = "execution_db/active_run_capacity_db.rs"]
mod active_run_capacity_db;
#[path = "execution_db/amendment_projection_db.rs"]
mod amendment_projection_db;
#[path = "execution_db/budget_and_materialization_db.rs"]
mod budget_and_materialization_db;
#[path = "execution_db/compensation_attempts_db.rs"]
mod compensation_attempts_db;
#[path = "execution_db/compensation_db.rs"]
mod compensation_db;
#[path = "execution_db/completion_projection_db.rs"]
mod completion_projection_db;
#[path = "execution_db/conditional_execution_db.rs"]
mod conditional_execution_db;
#[path = "execution_db/controller_wake_recovery_db.rs"]
mod controller_wake_recovery_db;
#[path = "execution_db/execution_capacity_db.rs"]
mod execution_capacity_db;
#[path = "execution_db/incremental_scheduler_db.rs"]
mod incremental_scheduler_db;
#[path = "execution_db/long_horizon_state_db.rs"]
mod long_horizon_state_db;
#[path = "execution_db/outcomes_and_replan_db.rs"]
mod outcomes_and_replan_db;
#[path = "execution_db/planning_and_audit_db.rs"]
mod planning_and_audit_db;
#[path = "execution_db/retention_db.rs"]
mod retention_db;
#[path = "execution_db/scope_and_lifecycle_db.rs"]
mod scope_and_lifecycle_db;
#[path = "execution_db/support.rs"]
mod support;
#[path = "execution_db/trigger_outbox_db.rs"]
mod trigger_outbox_db;
#[path = "execution_db/wait_entry_deadline_db.rs"]
mod wait_entry_deadline_db;
