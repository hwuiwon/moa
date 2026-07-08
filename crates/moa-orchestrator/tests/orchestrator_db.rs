//! Consolidated Postgres-backed (`_db`) integration-test harness for `moa-orchestrator`.

#[path = "orchestrator_db/analytics_chaos_db.rs"]
mod analytics_chaos_db;
#[path = "orchestrator_db/analytics_export_db.rs"]
mod analytics_export_db;
#[path = "orchestrator_db/api_keys_db.rs"]
mod api_keys_db;
#[path = "orchestrator_db/authz_admin_db.rs"]
mod authz_admin_db;
#[path = "orchestrator_db/authz_challenges_db.rs"]
mod authz_challenges_db;
#[path = "orchestrator_db/contacts_db.rs"]
mod contacts_db;
#[path = "orchestrator_db/eval_run_status_db.rs"]
mod eval_run_status_db;
#[path = "orchestrator_db/lineage_postgres_db.rs"]
mod lineage_postgres_db;
#[path = "orchestrator_db/session_store_db.rs"]
mod session_store_db;
#[path = "orchestrator_db/workspace_authz_db.rs"]
mod workspace_authz_db;
