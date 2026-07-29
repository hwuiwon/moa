//! Shared normalization and removal of provider permission payloads.
//!
//! Google Drive (through Nango) and Merge both express per-record permissions as
//! a list of entries naming a user, a group, a domain, or "anyone", so one
//! normalizer covers both and any future connector that follows the same shape.
//!
//! Everything here fails closed. A payload with no permission list, a permission
//! MOA cannot map to a canonical principal, a truncated listing, or a missing
//! provider revision all yield an INCOMPLETE ACL — which hides the record rather
//! than sharing it. That is the deliberate cost of not knowing: a connector that
//! returns permission *identifiers* without the principals behind them (Drive's
//! `permissionIds`) tells us a record is restricted without telling us to whom,
//! and the only safe reading of that is "nobody".

use serde_json::Value;
use uuid::Uuid;

use crate::acl_key::SourceAclKey;
use crate::domain::{
    CanonicalSourcePrincipal, ProviderAclEntry, ProviderRecordAcl, SourceAclEntryKind,
    SourcePrincipalKind,
};

/// Payload keys, in priority order, that hold the permission list.
const PERMISSION_LIST_FIELDS: &[&str] = &["permissions", "permission_list", "access_control_list"];

/// Payload keys that prove a permission listing was cut short.
const TRUNCATION_MARKER_FIELDS: &[&str] = &[
    "permissionsNextPageToken",
    "permissions_next_page_token",
    "permissions_truncated",
    "hasMorePermissions",
];

/// Payload keys, in priority order, carrying the provider's ACL revision.
const REVISION_FIELDS: &[&str] = &[
    "permissionsRevision",
    "permissions_revision",
    "version",
    "etag",
    "remote_updated_at",
    "modified_at",
    "modifiedTime",
];

/// Builds the fingerprint namespace for one concrete provider connection.
///
/// Including the MOA connection identity prevents identical provider
/// principals from matching across independently linked accounts.
pub(crate) fn principal_namespace(provider: &str, connector: &str, connection_uid: Uuid) -> String {
    format!(
        "{}:{}:{}",
        provider.trim().to_ascii_lowercase(),
        connector.trim().to_ascii_lowercase(),
        connection_uid
    )
}

/// Normalizes one provider record payload into its source ACL.
///
/// `namespace` scopes the resulting principals to one provider identity domain,
/// so `alice@example.com` in Drive and `alice@example.com` in a different
/// connector are different principals unless a binding says otherwise.
///
/// Each principal is keyed with `acl_key` here. The caller then removes the raw
/// permission carriers before returning the provider record for durable
/// journaling.
pub(crate) fn record_acl_from_payload(
    namespace: &str,
    payload: &Value,
    acl_key: &SourceAclKey,
) -> ProviderRecordAcl {
    let revision = first_string(payload, REVISION_FIELDS);
    let entries = first_array(payload, PERMISSION_LIST_FIELDS);
    let truncated = TRUNCATION_MARKER_FIELDS
        .iter()
        .any(|field| payload.get(*field).is_some_and(truncation_marker_is_set));

    let mut grants = Vec::new();
    let mut complete = revision.is_some() && entries.is_some() && !truncated;
    for entry in entries.unwrap_or_default() {
        match grant_from_permission(namespace, entry, acl_key) {
            PermissionOutcome::Grant(grant) => grants.push(grant),
            // A revoked or pending permission grants nothing and is not evidence
            // of an unreadable listing.
            PermissionOutcome::Ignored => {}
            PermissionOutcome::Unmappable => complete = false,
        }
    }

    ProviderRecordAcl {
        // A record whose revision the provider did not state is already
        // incomplete; the placeholder keeps the snapshot storable as evidence
        // without ever matching a real revision.
        provider_revision: revision.unwrap_or_else(|| "unknown".to_string()),
        complete,
        entries: grants,
    }
}

/// Removes raw permission carriers after they have been fingerprinted.
///
/// Provider records are journaled durably, so the readable identities used to
/// build the ACL must not remain in the returned payload. Content and revision
/// fields are preserved.
pub(crate) fn strip_acl_principal_carriers(mut payload: Value) -> Value {
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };
    for field in [
        "permissions",
        "permission_list",
        "access_control_list",
        "permissionIds",
        "permission_ids",
    ] {
        object.remove(field);
    }
    payload
}

fn truncation_marker_is_set(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::String(value) => !value.trim().is_empty(),
        Value::Number(value) => value.as_u64().is_none_or(|value| value != 0),
        Value::Null => false,
        // An unexpected structured marker cannot prove the listing completed.
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// What one provider permission entry contributes to the normalized ACL.
enum PermissionOutcome {
    /// The entry names a principal MOA can compare against a caller.
    Grant(ProviderAclEntry),
    /// The entry grants nothing and does not make the listing incomplete.
    Ignored,
    /// The entry names something MOA cannot map, so the listing is incomplete.
    Unmappable,
}

fn grant_from_permission(
    namespace: &str,
    entry: &Value,
    acl_key: &SourceAclKey,
) -> PermissionOutcome {
    if entry
        .get("deleted")
        .or_else(|| entry.get("revoked"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return PermissionOutcome::Ignored;
    }

    let Some(kind) = principal_kind(entry) else {
        return PermissionOutcome::Unmappable;
    };
    let subject = match kind {
        SourcePrincipalKind::Anyone => String::new(),
        SourcePrincipalKind::Domain => match first_string(entry, &["domain", "domain_name"]) {
            Some(domain) => domain,
            None => return PermissionOutcome::Unmappable,
        },
        SourcePrincipalKind::User | SourcePrincipalKind::Group => match first_string(
            entry,
            &[
                "emailAddress",
                "email_address",
                "email",
                "user.email_address",
                "group.email_address",
                "remote_id",
                "id",
            ],
        ) {
            Some(subject) => subject,
            None => return PermissionOutcome::Unmappable,
        },
    };

    let Ok(principal) = CanonicalSourcePrincipal::new(namespace, kind, subject) else {
        return PermissionOutcome::Unmappable;
    };
    PermissionOutcome::Grant(ProviderAclEntry {
        entry_kind: if is_denial(entry) {
            SourceAclEntryKind::Deny
        } else {
            SourceAclEntryKind::Allow
        },
        principal_kind: principal.kind(),
        principal: acl_key.fingerprint(&principal),
    })
}

/// Returns whether a permission entry expresses an explicit refusal.
///
/// Google Drive has no deny concept, but SharePoint-style sources reached
/// through either provider do, and a deny that were silently dropped would be
/// the most dangerous possible normalization bug — so it is modelled here even
/// though the shipped connectors rarely emit it.
fn is_denial(entry: &Value) -> bool {
    if entry
        .get("deny")
        .or_else(|| entry.get("denied"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    matches!(
        first_string(entry, &["role", "access", "permission_type"])
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("none" | "deny" | "denied" | "blocked")
    )
}

fn principal_kind(entry: &Value) -> Option<SourcePrincipalKind> {
    let raw = first_string(entry, &["type", "principal_type", "grantee_type"])?;
    match raw.to_ascii_lowercase().as_str() {
        "user" | "member" => Some(SourcePrincipalKind::User),
        "group" | "team" => Some(SourcePrincipalKind::Group),
        "domain" | "company" | "organization" => Some(SourcePrincipalKind::Domain),
        "anyone" | "public" | "everyone" => Some(SourcePrincipalKind::Anyone),
        _ => None,
    }
}

fn first_array<'a>(value: &'a Value, keys: &[&str]) -> Option<Vec<&'a Value>> {
    keys.iter()
        .find_map(|key| value.get(*key)?.as_array())
        .map(|entries| entries.iter().collect())
}

/// Resolves the first present dotted key to a trimmed non-empty string.
fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let mut current = value;
        for segment in key.split('.') {
            current = current.get(segment)?;
        }
        let text = match current {
            Value::String(text) => text.trim().to_string(),
            Value::Number(number) => number.to_string(),
            _ => return None,
        };
        (!text.is_empty()).then_some(text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    /// Fixed fixture key so a fingerprint computed in an assertion matches the
    /// one normalization produced.
    fn test_key() -> SourceAclKey {
        SourceAclKey::new(1, vec![0x33; 32])
    }

    fn fingerprint(namespace: &str, kind: SourcePrincipalKind, subject: &str) -> ProviderAclEntry {
        ProviderAclEntry {
            entry_kind: SourceAclEntryKind::Allow,
            principal_kind: kind,
            principal: test_key().fingerprint(
                &CanonicalSourcePrincipal::new(namespace, kind, subject).expect("normalizes"),
            ),
        }
    }

    fn provider_acl(payload: &Value) -> ProviderRecordAcl {
        record_acl_from_payload("google_drive", payload, &test_key())
    }

    #[test]
    fn drive_permission_list_normalizes_every_principal_kind() {
        // Pins: a complete Drive permission listing maps user, group, domain, and
        // anyone entries to canonical principals, carries the file version as the
        // ACL revision, and drops revoked permissions without losing completeness.
        let acl = provider_acl(&json!({
            "version": "17",
            "permissions": [
                { "type": "user", "emailAddress": "Alice@Example.com", "role": "reader" },
                { "type": "group", "emailAddress": "sales@example.com", "role": "writer" },
                { "type": "domain", "domain": "Example.com", "role": "reader" },
                { "type": "anyone", "role": "reader" },
                { "type": "user", "emailAddress": "gone@example.com", "deleted": true }
            ]
        }));

        assert!(acl.complete);
        assert_eq!(acl.provider_revision, "17");
        assert_eq!(
            acl.entries.len(),
            4,
            "the revoked permission grants nothing"
        );
        let kinds = acl
            .entries
            .iter()
            .map(|entry| entry.principal_kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                SourcePrincipalKind::User,
                SourcePrincipalKind::Group,
                SourcePrincipalKind::Domain,
                SourcePrincipalKind::Anyone,
            ]
        );
        assert!(
            acl.entries
                .iter()
                .all(|entry| entry.entry_kind == SourceAclEntryKind::Allow)
        );
        // The stored principal is opaque, so it is compared against the keyed
        // fingerprint of the identity we expect — never against a readable
        // subject, which is the whole point of keying it.
        assert_eq!(
            acl.entries[0].principal,
            fingerprint(
                "google_drive",
                SourcePrincipalKind::User,
                "alice@example.com"
            )
            .principal
        );
        assert_eq!(
            acl.entries[2].principal,
            fingerprint("google_drive", SourcePrincipalKind::Domain, "example.com").principal
        );
        assert_eq!(
            acl.entries[3].principal,
            fingerprint("google_drive", SourcePrincipalKind::Anyone, "").principal
        );
    }

    #[test]
    fn missing_or_opaque_permissions_are_incomplete_not_public() {
        // Pins the core fail-closed rule: a record with no permission list, a
        // listing MOA cannot map, a truncated listing, or no provider revision is
        // INCOMPLETE — never a record that becomes tenant-readable.
        let no_list = provider_acl(&json!({ "version": "3", "permissionIds": ["a", "b"] }));
        assert!(!no_list.complete);
        assert!(no_list.entries.is_empty());

        let unmappable = provider_acl(&json!({
            "version": "3",
            "permissions": [{ "type": "serviceAccount", "id": "sa-1" }]
        }));
        assert!(
            !unmappable.complete,
            "a permission kind MOA cannot compare must make the listing incomplete"
        );

        let missing_subject = provider_acl(&json!({
            "version": "3",
            "permissions": [{ "type": "user", "role": "reader" }]
        }));
        assert!(!missing_subject.complete);

        let truncated = provider_acl(&json!({
            "version": "3",
            "permissionsNextPageToken": "page-2",
            "permissions": [{ "type": "user", "emailAddress": "a@example.com" }]
        }));
        assert!(!truncated.complete);
        assert_eq!(
            truncated.entries.len(),
            1,
            "the entries seen are still recorded as evidence"
        );

        let no_revision = provider_acl(&json!({
            "permissions": [{ "type": "user", "emailAddress": "a@example.com" }]
        }));
        assert!(
            !no_revision.complete,
            "without a revision, staleness could never be detected"
        );
    }

    #[test]
    fn truncation_markers_use_typed_truthiness() {
        // Pins: false, zero, null, and an empty token mean the provider
        // completed the listing; true, nonzero, and a non-empty token do not.
        for marker in [json!(false), json!(0), Value::Null, json!("")] {
            let acl = provider_acl(&json!({
                "version": "3",
                "permissions_truncated": marker,
                "permissions": [{ "type": "anyone", "role": "reader" }]
            }));
            assert!(acl.complete, "marker {marker} should not truncate");
        }
        for marker in [json!(true), json!(1), json!("page-2"), json!({})] {
            let acl = provider_acl(&json!({
                "version": "3",
                "permissions_truncated": marker,
                "permissions": [{ "type": "anyone", "role": "reader" }]
            }));
            assert!(!acl.complete, "marker {marker} must truncate");
        }
    }

    #[test]
    fn permission_carriers_are_removed_after_fingerprinting() {
        // Pins: raw provider identities never survive in the durable record
        // payload, while ordinary content and the ACL revision remain.
        let payload = json!({
            "version": "3",
            "content": "Quarterly plan",
            "permissions": [{
                "type": "user",
                "emailAddress": "alice@example.com",
                "role": "reader"
            }],
            "permissionIds": ["provider-readable-id"]
        });
        let acl = provider_acl(&payload);
        assert!(acl.complete);

        let stripped = strip_acl_principal_carriers(payload);
        let serialized = serde_json::to_string(&stripped).expect("payload serializes");
        assert!(!serialized.contains("alice@example.com"));
        assert!(!serialized.contains("provider-readable-id"));
        assert_eq!(stripped["content"], "Quarterly plan");
        assert_eq!(stripped["version"], "3");
    }

    #[test]
    fn identical_anyone_acl_does_not_cross_connection_namespaces() {
        // Pins: a public grant from linked connection A is not a principal for
        // linked connection B, even when provider and connector are identical.
        let namespace_a = principal_namespace("nango", "google-drive", Uuid::from_u128(41));
        let namespace_b = principal_namespace("nango", "google-drive", Uuid::from_u128(42));
        let payload = json!({
            "version": "3",
            "permissions": [{ "type": "anyone", "role": "reader" }]
        });
        let acl_a = record_acl_from_payload(&namespace_a, &payload, &test_key());
        let acl_b = record_acl_from_payload(&namespace_b, &payload, &test_key());
        let principal_a = acl_a.entries[0].principal.clone();
        let principal_b = acl_b.entries[0].principal.clone();

        assert_ne!(principal_a, principal_b);
        assert!(
            !BTreeSet::from([principal_a]).contains(&principal_b),
            "connection A's Anyone context must not match connection B"
        );
    }

    #[test]
    fn explicit_denials_survive_normalization() {
        // Pins: an explicit refusal is carried through as a deny grant. Dropping
        // it would turn the most restrictive statement a source can make into
        // silence, and silence admits.
        let acl = provider_acl(&json!({
            "version": "9",
            "permissions": [
                { "type": "user", "emailAddress": "a@example.com", "role": "reader" },
                { "type": "user", "emailAddress": "b@example.com", "role": "none" },
                { "type": "group", "emailAddress": "c@example.com", "deny": true }
            ]
        }));
        assert!(acl.complete);
        let denies = acl
            .entries
            .iter()
            .filter(|entry| entry.entry_kind == SourceAclEntryKind::Deny)
            .map(|entry| entry.principal.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            denies,
            vec![
                fingerprint("google_drive", SourcePrincipalKind::User, "b@example.com").principal,
                fingerprint("google_drive", SourcePrincipalKind::Group, "c@example.com").principal,
            ]
        );
    }

    #[test]
    fn merge_nested_principal_shape_normalizes() {
        // Pins: Merge nests the principal under `user`/`group` and spells the
        // type in upper case, and both still reach the same canonical principal.
        let acl = record_acl_from_payload(
            "merge",
            &json!({
                "remote_updated_at": "2026-07-01T00:00:00Z",
                "permissions": [
                    { "type": "USER", "user": { "email_address": "Dana@Example.com" } },
                    { "type": "COMPANY", "domain": "example.com" }
                ]
            }),
            &test_key(),
        );
        assert!(acl.complete);
        assert_eq!(acl.provider_revision, "2026-07-01T00:00:00Z");
        assert_eq!(
            acl.entries[0].principal,
            fingerprint("merge", SourcePrincipalKind::User, "dana@example.com").principal
        );
        assert_eq!(acl.entries[1].principal_kind, SourcePrincipalKind::Domain);
    }
}
