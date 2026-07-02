//! DB-backed authz tests, consolidated into one harness binary.

#[path = "authz_db/authz_poller_db.rs"]
mod authz_poller_db;
#[path = "authz_db/outbox_basic_db.rs"]
mod outbox_basic_db;
#[path = "authz_db/require_audit_db.rs"]
mod require_audit_db;
