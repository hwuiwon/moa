//! Integration tests for core scope helpers and serialization.

use moa_core::{ActionRuleScope, TenantId};
use uuid::Uuid;

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
