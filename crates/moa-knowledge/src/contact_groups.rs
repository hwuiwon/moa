//! Contact-group derivation seams for tenant knowledge evidence.

use crate::domain::{ContactGroup, ContactGroupMembership, KnowledgeObject};

/// Derived contact-group update produced from tenant knowledge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContactGroupDelta {
    /// Groups to upsert.
    pub groups: Vec<ContactGroup>,
    /// Memberships to replace.
    pub memberships: Vec<ContactGroupMembership>,
}

/// Derives contact groups from a source object.
#[must_use]
pub fn derive_contact_groups_from_object(_object: &KnowledgeObject) -> ContactGroupDelta {
    ContactGroupDelta::default()
}
