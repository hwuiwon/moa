//! Unit tests for the provider adapter.

use moa_core::{
    error::MoaError,
    traits::{HandProvider, SandboxStorageProvider},
    types::hands::{HandHandle, HandSpec, SandboxProfile, SandboxTier},
    types::identifiers::{ProviderAccountId, WorkspaceCheckpointId, WorkspaceOperationId},
    types::sandbox_workspace::{
        ProviderInventoryOwner, WorkspaceCheckpointPublishRequest, WorkspaceOperationKind,
        WorkspaceRevisionRef, WorkspaceStorageOperation,
    },
};
use tempfile::tempdir;

use super::LocalHandProvider;

fn hand_spec(tier: SandboxTier) -> HandSpec {
    crate::core::profile::test_support::hand_spec(tier, SandboxProfile::unrestricted())
}

#[tokio::test]
async fn local_container_tier_fails_when_docker_is_unavailable() {
    // Pins: requested container isolation must not silently become host-local execution.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");

    let error = provider
        .provision(hand_spec(SandboxTier::Container))
        .await
        .expect_err("container tier should fail when Docker is unavailable");

    assert!(
        matches!(error, MoaError::ProviderError(message) if message.contains("Docker is unavailable"))
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read sandbox root")
            .count(),
        0,
        "failed container provisioning should not leave a local fallback sandbox"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_sandbox_directory_is_owner_group_restricted() {
    // Pins: local sandbox directories must not grant world access.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");

    let handle = provider
        .provision(hand_spec(SandboxTier::Local))
        .await
        .expect("provision local sandbox");
    let sandbox_dir = match &handle {
        moa_core::types::hands::HandHandle::Local { sandbox_dir } => sandbox_dir,
        other => panic!("expected local hand, got {other:?}"),
    };
    let mode = std::fs::metadata(sandbox_dir)
        .expect("read sandbox metadata")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o770);

    provider
        .destroy(&handle)
        .await
        .expect("destroy local sandbox");
}

#[tokio::test]
async fn local_reuse_rejects_a_changed_workspace_binding() {
    // Pins: an operation ID cannot resolve compute attached under a stale
    // workspace writer fence, even when every other creation field matches.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");
    let spec = hand_spec(SandboxTier::Local);
    let handle = provider
        .provision(spec.clone())
        .await
        .expect("first provision should create the sandbox");
    let mut stale_spec = spec;
    stale_spec.workspace.writer_epoch += 1;

    let error = provider
        .provision(stale_spec)
        .await
        .expect_err("changed workspace binding must invalidate operation reuse");

    assert!(
        matches!(error, MoaError::ProviderError(message) if message.contains("different creation spec"))
    );
    provider
        .destroy(&handle)
        .await
        .expect("destroy local sandbox");
}

#[tokio::test]
async fn local_inventory_is_account_scoped_and_survives_durable_lease_adoption_offline() {
    // Pins: local maintenance inventory carries the exact tenant/workspace,
    // writer, instance, and provisioning-operation fences from HandSpec;
    // asking for another provider account cannot discover the resource.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");
    let spec = hand_spec(SandboxTier::Local);
    let account_id = spec.workspace.provider_account_id;
    let account_generation = spec.workspace.provider_account_generation;
    let expected_owner = ProviderInventoryOwner {
        tenant_id: spec.workspace.tenant_id,
        workspace_id: spec.workspace.workspace_id,
        provisioning_operation_id: Some(spec.provisioning_operation_id),
        writer_epoch: Some(spec.workspace.writer_epoch),
        instance_generation: Some(spec.workspace.instance_generation),
    };
    let handle = provider
        .provision(spec.clone())
        .await
        .expect("provision local sandbox");
    let lease = provider
        .lease_handle(spec.provisioning_operation_id, &handle)
        .await
        .expect("persist verified inventory identity in lease metadata");
    let adopted = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create replacement local provider");
    adopted
        .adopt_lease_handle(&lease)
        .await
        .expect("adopt durable local lease identity");

    let inventory = adopted
        .enumerate_account_storage(account_id, account_generation)
        .await
        .expect("enumerate exact local provider account");
    assert_eq!(inventory.resources.len(), 1);
    assert_eq!(inventory.resources[0].verified_owner, Some(expected_owner));
    assert!(
        adopted
            .enumerate_account_storage(ProviderAccountId::new(), account_generation)
            .await
            .expect("enumerate unrelated local provider account")
            .resources
            .is_empty()
    );

    adopted
        .destroy(&handle)
        .await
        .expect("destroy adopted local sandbox");
}

#[tokio::test]
async fn host_local_mutable_state_stays_inside_the_per_hand_directory() {
    // Pins: the host-local provider never treats a sandbox-internal mutable
    // path as a host path; checkpointable bytes remain per-hand isolated.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");
    let mut spec = hand_spec(SandboxTier::Local);
    spec.filesystem.mutable_root = std::path::PathBuf::from("/shared-workspace");
    let operation_id = spec.provisioning_operation_id;
    let handle = provider
        .provision(spec)
        .await
        .expect("provision isolated local sandbox");
    let lease = provider
        .lease_handle(operation_id, &handle)
        .await
        .expect("serialize local lease metadata");
    let sandbox_dir = match &handle {
        moa_core::types::hands::HandHandle::Local { sandbox_dir } => sandbox_dir,
        other => panic!("expected local hand, got {other:?}"),
    };

    assert_eq!(
        lease
            .provider_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("execution_root"))
            .and_then(serde_json::Value::as_str),
        sandbox_dir.to_str()
    );
    provider
        .destroy(&handle)
        .await
        .expect("destroy local sandbox");
}

#[tokio::test]
async fn local_commit_rejects_a_parent_at_generation_zero_before_storage_work() {
    // Pins: an initial checkpoint has no parent; rejecting a fabricated
    // parent must happen before resolving compute or publishing an object.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");
    let mut binding = hand_spec(SandboxTier::Local).workspace;
    binding.current_revision = None;
    let parent = WorkspaceRevisionRef {
        checkpoint_id: WorkspaceCheckpointId::new(),
        generation: 1,
        format_version: 1,
    };
    let operation = WorkspaceStorageOperation {
        operation_id: WorkspaceOperationId::new(),
        kind: WorkspaceOperationKind::Commit,
        binding,
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
        request_hash: "a".repeat(64),
    };

    let error = provider
        .publish_workspace_checkpoint(WorkspaceCheckpointPublishRequest {
            operation,
            hand: HandHandle::local(dir.path().join("missing-compute")),
            parent_revision: Some(parent),
            release_compute: false,
        })
        .await
        .expect_err("generation-zero parent must fail before compute/storage access");

    assert!(matches!(error, MoaError::ValidationError(message) if message.contains("parent")));
}
