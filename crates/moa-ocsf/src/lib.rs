//! OCSF v1.3 security-event emission, signing, and persistence.
//!
//! Two emission modes exist. The `emit_*` helpers are synchronous and fail
//! closed: a signing or insert failure returns an error so callers can roll back
//! the action that would otherwise be missing an audit record. The `spawn_*`
//! helpers hand the event to a background batch writer ([`init_background_audit`])
//! that never blocks or fails the caller; they are used on hot request paths
//! (authentication, authorization denials) where an audit write must not gate
//! the response. Transaction-scoped `emit_*_tx` helpers stay synchronous because
//! they are part of a durable state mutation.

mod audit_sink;
pub mod classes;
pub mod emit;
pub mod enums;
pub mod jcs;
pub mod signing;

pub use audit_sink::{dropped_audit_count, init_background_audit};
pub use emit::{
    ActorInput, EmitError, emit_agent_deactivated_tx, emit_agent_registered_tx,
    emit_api_key_created_tx, emit_api_key_revoked_tx, emit_approval_decided_tx, emit_authn_failure,
    emit_authn_success, emit_authz_decision, emit_delegation_granted_tx,
    emit_delegation_revoked_tx, emit_group_membership_added_tx, emit_group_membership_removed_tx,
    emit_scim_group_created_tx, emit_scim_group_deleted_tx, emit_scim_group_updated_tx,
    emit_scim_user_created_tx, emit_scim_user_deleted_tx, emit_scim_user_updated_tx,
    emit_user_deactivated_tx, spawn_authn_failure, spawn_authn_success, spawn_authz_decision,
};
pub use signing::{SigningError, ensure_key, rotate_key, verify};
