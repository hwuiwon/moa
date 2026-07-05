//! Integration tests for memory scope helpers and serialization.

use moa_core::{ContactId, TenantId};
use moa_memory_types::MemoryScope;
use uuid::Uuid;

#[test]
fn memory_scope_serialization_covers_runtime_scope_tiers() {
    // Pins: contact memory is contact-local and does not inherit tenant memory.
    let tenant = TenantId::from(Uuid::from_u128(1));
    let contact = ContactId(Uuid::from_u128(2));
    let cases = [
        (
            MemoryScope::Tenant { tenant_id: tenant },
            Some(r#"{"kind":"tenant","tenant_id":"00000000-0000-0000-0000-000000000001"}"#),
        ),
        (
            MemoryScope::Contact {
                tenant_id: tenant,
                contact_id: contact,
            },
            Some(
                r#"{"kind":"contact","tenant_id":"00000000-0000-0000-0000-000000000001","contact_id":"00000000-0000-0000-0000-000000000002"}"#,
            ),
        ),
    ];

    for (scope, expected_json) in cases {
        let json = serde_json::to_string(&scope).expect("serialize memory scope");
        let round_trip: MemoryScope =
            serde_json::from_str(&json).expect("deserialize memory scope");
        assert_eq!(scope, round_trip);

        if let Some(expected_json) = expected_json {
            assert_eq!(json, expected_json);
        }
    }
}
