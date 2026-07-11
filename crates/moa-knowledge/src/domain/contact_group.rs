//! Knowledge-derived contact-group domain types.

use moa_core::{types::contact::ContactId, types::identifiers::TenantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Derived contact group grounded in tenant knowledge evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroup {
    /// Contact-group identifier.
    pub group_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Stable group key.
    pub group_key: String,
    /// Display name.
    pub display_name: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Contact-group membership derived from source evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroupMembership {
    /// Owning group.
    pub group_uid: Uuid,
    /// Contact in the group.
    pub contact_id: ContactId,
    /// Evidence object or chunk identifiers.
    #[serde(default)]
    pub evidence: Vec<Uuid>,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Workflow-facing contact-group target projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroupTarget {
    /// Targeted group.
    pub group: ContactGroup,
    /// Active members eligible for targeting.
    #[serde(default)]
    pub members: Vec<ContactGroupTargetMember>,
    /// Active `MEMBER_OF` graph edge projections derived from active members.
    #[serde(default)]
    pub active_graph_memberships: Vec<ContactGroupGraphMembership>,
}

impl ContactGroupTarget {
    /// Builds a target projection from current active SQL memberships.
    #[must_use]
    pub fn from_active_members(
        group: ContactGroup,
        members: Vec<ContactGroupTargetMember>,
    ) -> Self {
        let active_graph_memberships = members
            .iter()
            .map(|member| ContactGroupGraphMembership {
                edge_uid: deterministic_member_of_edge_uid(group.group_uid, member.contact_id),
                edge_label: "MEMBER_OF".to_string(),
                group_uid: group.group_uid,
                contact_id: member.contact_id,
                evidence: member.evidence.clone(),
                metadata: member.metadata.clone(),
            })
            .collect();
        Self {
            group,
            members,
            active_graph_memberships,
        }
    }
}

/// One active contact-group member available to workflow targeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroupTargetMember {
    /// Targetable contact.
    pub contact_id: ContactId,
    /// Evidence object or chunk identifiers supporting the membership.
    #[serde(default)]
    pub evidence: Vec<Uuid>,
    /// Safe membership metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Active graph membership projection derived from current SQL membership rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroupGraphMembership {
    /// Stable edge identifier if the projection is materialized into the graph.
    pub edge_uid: Uuid,
    /// Graph edge label.
    pub edge_label: String,
    /// Owning group.
    pub group_uid: Uuid,
    /// Contact that is currently a member.
    pub contact_id: ContactId,
    /// Evidence object or chunk identifiers supporting the membership.
    #[serde(default)]
    pub evidence: Vec<Uuid>,
    /// Safe edge metadata.
    #[serde(default)]
    pub metadata: Value,
}

fn deterministic_member_of_edge_uid(group_uid: Uuid, contact_id: ContactId) -> Uuid {
    let hash = blake3::hash(
        format!(
            "moa:knowledge-contact-group-member-of:{group_uid}:{}",
            contact_id.0
        )
        .as_bytes(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
