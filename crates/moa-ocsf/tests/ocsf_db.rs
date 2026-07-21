//! Consolidated db-backed OCSF integration tests.

#[path = "support.rs"]
mod support;

#[path = "ocsf_db/background_audit_writer_db.rs"]
mod background_audit_writer_db;
#[path = "ocsf_db/data_access_db.rs"]
mod data_access_db;
#[path = "ocsf_db/emit_authn_success_db.rs"]
mod emit_authn_success_db;
#[path = "ocsf_db/emit_matrix_db.rs"]
mod emit_matrix_db;
#[path = "ocsf_db/sign_verify_roundtrip_db.rs"]
mod sign_verify_roundtrip_db;
