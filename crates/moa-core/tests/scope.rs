//! Integration tests for memory scope helpers and serialization.

use moa_core::{ActionRuleScope, ContactId, MemoryScope, TenantId};
use uuid::Uuid;

#[test]
fn memory_scope_ancestors_and_serialization_cover_runtime_scope_tiers() {
    // Pins: contact memory is contact-local and does not inherit tenant memory.
    let tenant = TenantId::from(Uuid::from_u128(1));
    let contact = ContactId(Uuid::from_u128(2));
    let cases = [
        (
            MemoryScope::Tenant { tenant_id: tenant },
            vec![MemoryScope::Tenant { tenant_id: tenant }],
            Some(r#"{"kind":"tenant","tenant_id":"00000000-0000-0000-0000-000000000001"}"#),
        ),
        (
            MemoryScope::Contact {
                tenant_id: tenant,
                contact_id: contact,
            },
            vec![MemoryScope::Contact {
                tenant_id: tenant,
                contact_id: contact,
            }],
            Some(
                r#"{"kind":"contact","tenant_id":"00000000-0000-0000-0000-000000000001","contact_id":"00000000-0000-0000-0000-000000000002"}"#,
            ),
        ),
    ];

    for (scope, expected_ancestors, expected_json) in cases {
        assert_eq!(scope.ancestors(), expected_ancestors);

        let json = serde_json::to_string(&scope).expect("serialize memory scope");
        let round_trip: MemoryScope =
            serde_json::from_str(&json).expect("deserialize memory scope");
        assert_eq!(scope, round_trip);

        if let Some(expected_json) = expected_json {
            assert_eq!(json, expected_json);
        }
    }
}

#[test]
fn action_rule_scope_serialization_is_separate_from_contact_memory_scope() {
    // Pins: artifact and policy inheritance scopes are not encoded as memory scopes.
    let tenant = TenantId::from(Uuid::from_u128(3));
    let cases = [
        (
            ActionRuleScope::WorkspaceDefault,
            r#""workspace_default""#,
            "workspace_default",
        ),
        (
            ActionRuleScope::Tenant { tenant_id: tenant },
            r#"{"tenant":{"tenant_id":"00000000-0000-0000-0000-000000000003"}}"#,
            "tenant",
        ),
    ];

    for (scope, expected_json, expected_str) in cases {
        let json = serde_json::to_string(&scope).expect("serialize action rule scope");
        let round_trip: ActionRuleScope =
            serde_json::from_str(&json).expect("deserialize action rule scope");

        assert_eq!(json, expected_json);
        assert_eq!(round_trip, scope);
        assert_eq!(scope.as_str(), expected_str);
    }
}
