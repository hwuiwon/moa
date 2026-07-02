//! Consolidated Postgres-backed (`_db`) integration-test harness for `moa-orchestrator`.

#[path = "orchestrator_db/authz_challenges_db.rs"]
mod authz_challenges_db;
#[path = "orchestrator_db/eval_run_status_db.rs"]
mod eval_run_status_db;
#[path = "orchestrator_db/lineage_postgres_db.rs"]
mod lineage_postgres_db;
#[path = "orchestrator_db/session_store_db.rs"]
mod session_store_db;
