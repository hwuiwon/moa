//! Shared normalization of provider permission payloads into a [`RecordAcl`].
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

use crate::acl_key::SourceAclKey;
use crate::domain::{
    CanonicalSourcePrincipal, ProviderAclEntry, ProviderAclProvenance, ProviderRecordAcl,
    RecordAcl, SourceAclEntryKind, SourcePrincipalKind,
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

/// Normalizes one provider record payload into its source ACL.
///
/// `namespace` scopes the resulting principals to one provider identity domain,
/// so `alice@example.com` in Drive and `alice@example.com` in a different
/// connector are different principals unless a binding says otherwise.
///
/// Each principal is keyed with `acl_key` here, inside the same call that reads
/// it. A raw provider identity therefore never outlives normalization, which is
/// what lets the resulting record be journaled durably.
pub(crate) fn record_acl_from_payload(
    namespace: &str,
    payload: &Value,
    provenance: ProviderAclProvenance,
    acl_key: &SourceAclKey,
) -> RecordAcl {
    let revision = first_string(payload, REVISION_FIELDS);
    let entries = first_array(payload, PERMISSION_LIST_FIELDS);
    let truncated = TRUNCATION_MARKER_FIELDS
        .iter()
        .any(|field| payload.get(*field).is_some_and(|value| !value.is_null()));

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

    RecordAcl::Provider(ProviderRecordAcl {
        // A record whose revision the provider did not state is already
        // incomplete; the placeholder keeps the snapshot storable as evidence
        // without ever matching a real revision.
        provider_revision: revision.unwrap_or_else(|| "unknown".to_string()),
        complete,
        provenance,
        entries: grants,
        metadata: serde_json::json!({
            "permission_count": grants_len(payload),
            "listing_truncated": truncated,
        }),
    })
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

fn grants_len(payload: &Value) -> usize {
    first_array(payload, PERMISSION_LIST_FIELDS).map_or(0, |entries| entries.len())
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
        match record_acl_from_payload(
            "google_drive",
            payload,
            ProviderAclProvenance::ProviderListing,
            &test_key(),
        ) {
            RecordAcl::Provider(acl) => acl,
            RecordAcl::UniformlyPublic => panic!("normalizer must never claim uniform public"),
        }
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
        let acl = match record_acl_from_payload(
            "merge",
            &json!({
                "remote_updated_at": "2026-07-01T00:00:00Z",
                "permissions": [
                    { "type": "USER", "user": { "email_address": "Dana@Example.com" } },
                    { "type": "COMPANY", "domain": "example.com" }
                ]
            }),
            ProviderAclProvenance::ProviderListing,
            &test_key(),
        ) {
            RecordAcl::Provider(acl) => acl,
            RecordAcl::UniformlyPublic => panic!("normalizer must never claim uniform public"),
        };
        assert!(acl.complete);
        assert_eq!(acl.provider_revision, "2026-07-01T00:00:00Z");
        assert_eq!(
            acl.entries[0].principal,
            fingerprint("merge", SourcePrincipalKind::User, "dana@example.com").principal
        );
        assert_eq!(acl.entries[1].principal_kind, SourcePrincipalKind::Domain);
    }
}
