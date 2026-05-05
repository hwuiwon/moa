//! Integration tests for memory scope helpers and serialization.

use moa_core::{MemoryScope, UserId, WorkspaceId};

#[test]
fn ancestors_global_is_just_global() {
    assert_eq!(MemoryScope::Global.ancestors(), vec![MemoryScope::Global]);
}

#[test]
fn ancestors_workspace_includes_global() {
    let w = WorkspaceId::new("workspace");
    let s = MemoryScope::Workspace {
        workspace_id: w.clone(),
    };
    assert_eq!(s.ancestors(), vec![MemoryScope::Global, s.clone()]);
}

#[test]
fn ancestors_user_is_three_tier() {
    let w = WorkspaceId::new("workspace");
    let u = UserId::new("user");
    let s = MemoryScope::User {
        workspace_id: w,
        user_id: u,
    };
    let anc = s.ancestors();
    assert_eq!(anc.len(), 3);
    assert!(matches!(anc[0], MemoryScope::Global));
    assert_eq!(anc[2], s);
}

#[test]
fn serde_round_trip_all_three() {
    for s in [
        MemoryScope::Global,
        MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace"),
        },
        MemoryScope::User {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
        },
    ] {
        let j = serde_json::to_string(&s).expect("serialize memory scope");
        let r: MemoryScope = serde_json::from_str(&j).expect("deserialize memory scope");
        assert_eq!(s, r);
    }
}

#[test]
fn serde_global_uses_tagged_shape() {
    assert_eq!(
        serde_json::to_string(&MemoryScope::Global).expect("serialize global scope"),
        r#"{"kind":"global"}"#
    );
}
