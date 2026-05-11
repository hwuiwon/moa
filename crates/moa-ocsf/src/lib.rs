//! OCSF v1.3 security-event emission, signing, and persistence.
//!
//! Emission helpers are intentionally synchronous. A signing or insert failure
//! returns an error so callers can roll back the action that would otherwise be
//! missing an audit record.

pub mod classes;
pub mod emit;
pub mod enums;
pub mod jcs;
pub mod schema;
pub mod signing;

pub use emit::{
    ActorInput, EmitError, emit_agent_deactivated, emit_agent_deactivated_tx,
    emit_agent_registered, emit_agent_registered_tx, emit_api_key_created, emit_api_key_created_tx,
    emit_api_key_revoked, emit_api_key_revoked_tx, emit_approval_decided_tx, emit_authn_failure,
    emit_authn_success, emit_authz_decision, emit_delegation_granted_tx,
    emit_delegation_revoked_tx, emit_group_membership_added_tx, emit_group_membership_removed_tx,
    emit_scim_group_created_tx, emit_scim_group_deleted_tx, emit_scim_group_updated_tx,
    emit_scim_user_created_tx, emit_scim_user_deactivated_tx, emit_scim_user_deleted_tx,
    emit_scim_user_updated_tx, emit_user_created, emit_user_deactivated_tx,
};
pub use signing::{SigningError, ensure_key, rotate_key, verify};
