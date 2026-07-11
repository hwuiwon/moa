//! Consolidated PostgreSQL-backed session-store integration lane.

#[path = "session_db/audit_smoke_db.rs"]
mod audit_smoke_db;
#[path = "session_db/events_append_only_db.rs"]
mod events_append_only_db;
#[path = "session_db/events_concurrent_monotonicity_db.rs"]
mod events_concurrent_monotonicity_db;
#[path = "session_db/events_partitioning_db.rs"]
mod events_partitioning_db;
#[path = "session_db/postgres_store_db.rs"]
mod postgres_store_db;
#[path = "session_db/session_blobs_db.rs"]
mod session_blobs_db;
#[path = "session_db/tenant_rls_db.rs"]
mod tenant_rls_db;

#[path = "shared/mod.rs"]
mod shared;
