//! Integration tests for memory scope helpers and serialization.

use moa_core::{MemoryScope, UserId, WorkspaceId};

#[test]
fn memory_scope_ancestors_and_serialization_cover_all_scope_tiers() {
    let workspace = WorkspaceId::new("workspace");
    let user = UserId::new("user");
    let cases = [
        (
            MemoryScope::Global,
            vec![MemoryScope::Global],
            Some(r#"{"kind":"global"}"#),
        ),
        (
            MemoryScope::Workspace {
                workspace_id: workspace.clone(),
            },
            vec![
                MemoryScope::Global,
                MemoryScope::Workspace {
                    workspace_id: workspace.clone(),
                },
            ],
            None,
        ),
        (
            MemoryScope::User {
                workspace_id: workspace.clone(),
                user_id: user.clone(),
            },
            vec![
                MemoryScope::Global,
                MemoryScope::Workspace {
                    workspace_id: workspace.clone(),
                },
                MemoryScope::User {
                    workspace_id: workspace,
                    user_id: user,
                },
            ],
            None,
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
