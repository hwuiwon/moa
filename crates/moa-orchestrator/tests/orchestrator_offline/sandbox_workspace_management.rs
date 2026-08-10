//! Offline contract tests for public sandbox-workspace management payloads.

use moa_wire::sandbox_workspaces::{
    CreateSandboxWorkspaceRequest, RestoreSandboxWorkspaceRequest, SandboxWorkspaceIdRequest,
    SandboxWorkspaceSummary,
};
use serde_json::json;

#[test]
fn sandbox_workspace_management_rejects_tenant_and_provider_injection_offline() {
    // Pins: tenant/provider identity is derived from verified runtime state and
    // cannot be supplied through any create, lifecycle, or restore request.
    let create = json!({
        "scope": {
            "kind": "worker",
            "session_id": "11111111-1111-1111-1111-111111111111",
            "worker_id": "worker-1"
        },
        "durability_class": "portable_filesystem"
    });
    for (field, value) in [
        ("tenant_id", json!("22222222-2222-2222-2222-222222222222")),
        ("provider", json!("daytona")),
        (
            "provider_account_id",
            json!("33333333-3333-3333-3333-333333333333"),
        ),
        ("provider_account_generation", json!(9)),
        ("isolation_cell", json!("caller-selected-cell")),
    ] {
        let mut injected = create.clone();
        injected[field] = value;
        let error = serde_json::from_value::<CreateSandboxWorkspaceRequest>(injected)
            .expect_err("caller-owned routing fields must fail create deserialization");
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "create rejection must name {field}: {error}"
        );
    }

    let workspace_id = "44444444-4444-4444-4444-444444444444";
    let checkpoint_id = "55555555-5555-5555-5555-555555555555";
    for (field, value) in [
        ("tenant_id", json!("22222222-2222-2222-2222-222222222222")),
        (
            "provider_account_id",
            json!("33333333-3333-3333-3333-333333333333"),
        ),
    ] {
        let mut lifecycle = json!({ "workspace_id": workspace_id });
        lifecycle[field] = value.clone();
        let lifecycle_error = serde_json::from_value::<SandboxWorkspaceIdRequest>(lifecycle)
            .expect_err("caller routing fields must fail lifecycle deserialization");
        assert!(
            lifecycle_error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "lifecycle rejection must name {field}: {lifecycle_error}"
        );

        let mut restore = json!({
            "workspace_id": workspace_id,
            "checkpoint_id": checkpoint_id
        });
        restore[field] = value;
        let restore_error = serde_json::from_value::<RestoreSandboxWorkspaceRequest>(restore)
            .expect_err("caller routing fields must fail restore deserialization");
        assert!(
            restore_error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "restore rejection must name {field}: {restore_error}"
        );
    }
}

#[test]
fn sandbox_workspace_summary_has_no_provider_storage_fields_offline() {
    // Pins: the public workspace DTO has no deserializable provider ID, mount
    // path, object key, token, or credential field.
    let error = serde_json::from_value::<SandboxWorkspaceSummary>(json!({
        "workspace_id": "11111111-1111-1111-1111-111111111111",
        "scope": {
            "kind": "worker",
            "session_id": "22222222-2222-2222-2222-222222222222",
            "worker_id": "worker-1"
        },
        "durability_class": "portable_filesystem",
        "state": "ready",
        "writer_epoch": 0,
        "instance_generation": 0,
        "checkpoint_generation": 0,
        "access_fenced": false,
        "object_key": "tenant/private/archive.tar.zst"
    }))
    .expect_err("provider object keys must not enter public summaries");
    assert!(error.to_string().contains("unknown field `object_key`"));
}
