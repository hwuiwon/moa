//! Contact-group derivation seams for tenant knowledge evidence.

use std::collections::BTreeSet;

use moa_core::{ContactId, ContactPointInput, ContactPointKind};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::{ContactGroup, ContactGroupMembership, KnowledgeObject};

const GROUP_KINDS: &[&str] = &[
    "account",
    "department",
    "list",
    "segment",
    "team",
    "job",
    "channel",
    "project",
];

/// Derived contact-group update produced from tenant knowledge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContactGroupDelta {
    /// Groups to upsert.
    pub groups: Vec<ContactGroup>,
    /// Memberships to replace.
    pub memberships: Vec<ContactGroupMembership>,
}

#[derive(Debug, Clone)]
struct DerivedContactGroup {
    group: ContactGroup,
    evidence: Vec<Uuid>,
    member_identity_kinds: Vec<String>,
    member_points: Vec<ContactPointInput>,
}

/// Derives contact groups from a source object without assigning memberships.
#[must_use]
pub fn derive_contact_groups_from_object(object: &KnowledgeObject) -> ContactGroupDelta {
    derive_contact_groups_from_object_with_resolved_members(object, &[])
}

/// Derives contact groups and assigns already-resolved verified contacts.
///
/// The source metadata may contain raw member contact points, but persisted
/// group and membership metadata includes only source identifiers, evidence,
/// counts, and contact-point kinds. Callers should resolve contact points
/// through the contacts repository before passing contact IDs here.
#[must_use]
pub fn derive_contact_groups_from_object_with_resolved_members(
    object: &KnowledgeObject,
    resolved_contact_ids: &[ContactId],
) -> ContactGroupDelta {
    let mut groups = Vec::new();
    let mut memberships = Vec::new();
    let resolved_contact_ids = distinct_contact_ids(resolved_contact_ids);
    for derived in derived_groups(object) {
        for contact_id in &resolved_contact_ids {
            memberships.push(ContactGroupMembership {
                group_uid: derived.group.group_uid,
                contact_id: *contact_id,
                evidence: derived.evidence.clone(),
                metadata: json!({
                    "source_provider": derived.group.metadata["source_provider"].clone(),
                    "group_kind": derived.group.metadata["group_kind"].clone(),
                    "source_group_id": derived.group.metadata["source_group_id"].clone(),
                    "member_identity_kinds": derived.member_identity_kinds.clone(),
                    "resolved_from": "verified_contact_point",
                }),
            });
        }
        groups.push(derived.group);
    }
    ContactGroupDelta {
        groups,
        memberships,
    }
}

/// Extracts ephemeral member contact points that callers can resolve safely.
///
/// The returned values come from source metadata and should not be logged or
/// stored as group metadata.
#[must_use]
pub fn contact_group_member_contact_points(object: &KnowledgeObject) -> Vec<ContactPointInput> {
    derived_groups(object)
        .into_iter()
        .flat_map(|group| group.member_points)
        .collect()
}

fn derived_groups(object: &KnowledgeObject) -> Vec<DerivedContactGroup> {
    let Some(metadata) = object.metadata.as_object() else {
        return Vec::new();
    };
    metadata
        .iter()
        .filter_map(|(source_provider, source)| {
            source
                .as_object()
                .and_then(|source| derive_group(object, source_provider, source))
        })
        .collect()
}

fn derive_group(
    object: &KnowledgeObject,
    source_provider: &str,
    source: &serde_json::Map<String, Value>,
) -> Option<DerivedContactGroup> {
    let (group_kind, group_value) = GROUP_KINDS
        .iter()
        .find_map(|kind| source.get(*kind).map(|value| (*kind, value)))?;
    let group = group_value.as_object()?;
    let source_group_id = non_empty_str(group.get("id")?)?;
    let display_name = group
        .get("name")
        .and_then(non_empty_str)
        .unwrap_or(source_group_id)
        .to_string();
    let group_key = format!(
        "{}:{}:{}",
        stable_key_part(source_provider),
        group_kind,
        stable_key_part(source_group_id)
    );
    let group_uid = deterministic_group_uid(object.tenant_id.0, &group_key);
    let member_points = member_contact_points(source);
    let member_identity_kinds = member_identity_kinds(&member_points);
    let unresolved_member_identities = member_points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            json!({
                "ordinal": index,
                "kind": point.kind.as_str(),
                "evidence_object_uid": object.object_uid,
            })
        })
        .collect::<Vec<_>>();
    let evidence = vec![object.object_uid];
    Some(DerivedContactGroup {
        group: ContactGroup {
            group_uid,
            tenant_id: object.tenant_id,
            group_key,
            display_name,
            metadata: json!({
                "source_provider": source_provider,
                "group_kind": group_kind,
                "source_group_id": source_group_id,
                "source_object_uid": object.object_uid,
                "source_connection_uid": object.connection_uid,
                "member_identity_count": member_points.len(),
                "member_identity_kinds": member_identity_kinds.clone(),
                "unresolved_member_identities": unresolved_member_identities,
            }),
        },
        evidence,
        member_identity_kinds,
        member_points,
    })
}

fn member_contact_points(source: &serde_json::Map<String, Value>) -> Vec<ContactPointInput> {
    let Some(members) = source.get("members").and_then(Value::as_array) else {
        return Vec::new();
    };
    members
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|member| {
            [
                member_contact_point(member, "email", ContactPointKind::Email),
                member_contact_point(member, "phone", ContactPointKind::Phone),
                member_contact_point(member, "external_id", ContactPointKind::ExternalId),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn member_contact_point(
    member: &serde_json::Map<String, Value>,
    field: &str,
    kind: ContactPointKind,
) -> Option<ContactPointInput> {
    let value = non_empty_str(member.get(field)?)?;
    Some(ContactPointInput {
        kind,
        value: value.to_string(),
        display_value: None,
    })
}

fn member_identity_kinds(points: &[ContactPointInput]) -> Vec<String> {
    points
        .iter()
        .map(|point| point.kind.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn distinct_contact_ids(contact_ids: &[ContactId]) -> Vec<ContactId> {
    let mut seen = BTreeSet::new();
    contact_ids
        .iter()
        .copied()
        .filter(|contact_id| seen.insert(contact_id.0))
        .collect()
}

fn non_empty_str(value: &Value) -> Option<&str> {
    let value = value.as_str()?.trim();
    (!value.is_empty()).then_some(value)
}

fn stable_key_part(value: &str) -> String {
    let mut key = String::with_capacity(value.len());
    let mut last_was_separator = false;
    for character in value.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
            key.push(character);
            last_was_separator = false;
        } else if !last_was_separator {
            key.push('-');
            last_was_separator = true;
        }
    }
    key.trim_matches('-').to_string()
}

fn deterministic_group_uid(tenant_id: Uuid, group_key: &str) -> Uuid {
    let hash =
        blake3::hash(format!("moa:knowledge-contact-group:{tenant_id}:{group_key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
