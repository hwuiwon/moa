//! Integration tests for core scope helpers and serialization.

use moa_core::{
    types::action_policy::ActionRuleScope, types::contact::ContactId, types::identifiers::TenantId,
};
use uuid::Uuid;

#[test]
fn action_rule_scope_serialization_is_separate_from_contact_memory_scope() {
    // Pins: artifact and policy inheritance scopes are not encoded as memory scopes.
    let tenant = TenantId::from(Uuid::from_u128(3));
    let contact = ContactId(Uuid::from_u128(4));
    let cases = [
        (
            ActionRuleScope::Tenant { tenant_id: tenant },
            r#"{"tenant":{"tenant_id":"00000000-0000-0000-0000-000000000003"}}"#,
            "tenant",
            None,
        ),
        (
            ActionRuleScope::Contact {
                tenant_id: tenant,
                contact_id: contact,
            },
            r#"{"contact":{"tenant_id":"00000000-0000-0000-0000-000000000003","contact_id":"00000000-0000-0000-0000-000000000004"}}"#,
            "contact",
            Some(contact),
        ),
    ];

    for (scope, expected_json, expected_str, expected_contact) in cases {
        let json = serde_json::to_string(&scope).expect("serialize action rule scope");
        let round_trip: ActionRuleScope =
            serde_json::from_str(&json).expect("deserialize action rule scope");

        assert_eq!(json, expected_json);
        assert_eq!(round_trip, scope);
        assert_eq!(scope.as_str(), expected_str);
        assert_eq!(scope.tenant_id(), tenant);
        assert_eq!(scope.contact_id(), expected_contact);
    }
}
