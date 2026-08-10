//! Offline contract tests for durable sandbox-workspace types.

use moa_core::MoaError;
use moa_core::types::{
    hands::HandHandle,
    identifiers::{
        ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
        SessionId, TenantId, WorkspaceCheckpointId, WorkspaceOperationId,
    },
    sandbox_workspace::*,
};

fn fixture_workspace_binding() -> WorkspaceBinding {
    WorkspaceBinding {
        tenant_id: TenantId(uuid::Uuid::from_u128(1)),
        scope: SandboxWorkspaceScope::Worker {
            session_id: SessionId(uuid::Uuid::from_u128(2)),
            worker_id: "worker-a".to_string(),
        },
        workspace_id: SandboxWorkspaceId(uuid::Uuid::from_u128(3)),
        provider_account_id: ProviderAccountId(uuid::Uuid::from_u128(4)),
        provider_account_generation: 2,
        durability_class: DurabilityClass::PortableFilesystem,
        writer_epoch: 7,
        instance_generation: 11,
        current_revision: Some(WorkspaceRevisionRef {
            checkpoint_id: WorkspaceCheckpointId(uuid::Uuid::from_u128(5)),
            generation: 13,
            format_version: 1,
        }),
    }
}

fn fixture_workspace_operation(binding: WorkspaceBinding) -> WorkspaceStorageOperation {
    WorkspaceStorageOperation {
        operation_id: WorkspaceOperationId(uuid::Uuid::from_u128(8)),
        kind: WorkspaceOperationKind::Create,
        binding,
        deadline: chrono::Utc::now(),
        request_hash: "a".repeat(64),
    }
}

#[test]
fn workspace_reconcile_request_round_trips_exact_resource_fences() {
    // Pins: an ambiguous provider callback retains its exact operation kind,
    // workspace binding, compute handle, storage reference, and request hash.
    let binding = fixture_workspace_binding();
    let hand = HandHandle::daytona(
        "sandbox-1",
        binding.provider_account_id,
        binding.provider_account_generation,
    );
    let storage = ProviderStorageRef {
        provider_account_id: binding.provider_account_id,
        provider_account_generation: binding.provider_account_generation,
        kind: ProviderStorageKind::MutableFilesystem,
        resource_id: "volume-1".to_string(),
        workspace_locator: Some("w/opaque".to_string()),
    };
    let request = WorkspaceReconcileRequest::new(
        fixture_workspace_operation(binding.clone()),
        Some(hand.clone()),
        Some(storage.clone()),
    )
    .expect("matching exact-resource request should validate");
    let decoded: WorkspaceReconcileRequest =
        serde_json::from_value(serde_json::to_value(&request).expect("request should serialize"))
            .expect("request should deserialize");

    assert_eq!(decoded, request);
    assert_eq!(decoded.operation().kind, WorkspaceOperationKind::Create);
    assert_eq!(decoded.operation().binding, binding);
    assert_eq!(decoded.hand(), Some(&hand));
    assert_eq!(decoded.storage(), Some(&storage));
}

#[test]
fn workspace_reconcile_request_rejects_account_mismatches() {
    // Pins: provider inventory is never selected by a handle or storage
    // reference from a different account or account generation.
    let binding = fixture_workspace_binding();
    let wrong_hand = HandHandle::daytona(
        "sandbox-1",
        ProviderAccountId(uuid::Uuid::from_u128(99)),
        binding.provider_account_generation,
    );
    let error = WorkspaceReconcileRequest::new(
        fixture_workspace_operation(binding.clone()),
        Some(wrong_hand),
        None,
    )
    .expect_err("wrong-account hand must fail closed");
    assert!(matches!(error, MoaError::ValidationError(_)));

    let wrong_storage = ProviderStorageRef {
        provider_account_id: binding.provider_account_id,
        provider_account_generation: binding.provider_account_generation + 1,
        kind: ProviderStorageKind::MutableFilesystem,
        resource_id: "volume-1".to_string(),
        workspace_locator: None,
    };
    let error = WorkspaceReconcileRequest::new(
        fixture_workspace_operation(binding),
        None,
        Some(wrong_storage),
    )
    .expect_err("wrong-generation storage must fail closed");
    assert!(matches!(error, MoaError::ValidationError(_)));
}

#[test]
fn workspace_contract_variants_round_trip_without_losing_fences() {
    // Pins: persisted workspace ownership, lifecycle, and operation values
    // retain exact typed identities and generations across JSON boundaries.
    let binding = fixture_workspace_binding();
    let encoded = serde_json::to_value(&binding).expect("workspace binding should serialize");
    let decoded: WorkspaceBinding =
        serde_json::from_value(encoded).expect("workspace binding should deserialize");
    assert_eq!(decoded, binding);

    let execution_scope = SandboxWorkspaceScope::ExecutionTask {
        run_id: ExecutionRunScopeId(uuid::Uuid::from_u128(6)),
        task_id: ExecutionTaskScopeId(uuid::Uuid::from_u128(7)),
    };
    let encoded =
        serde_json::to_value(&execution_scope).expect("execution task scope should serialize");
    assert_eq!(
        serde_json::from_value::<SandboxWorkspaceScope>(encoded)
            .expect("execution task scope should deserialize"),
        execution_scope
    );

    let states = [
        SandboxWorkspaceState::Creating,
        SandboxWorkspaceState::Ready,
        SandboxWorkspaceState::Active,
        SandboxWorkspaceState::Quiescing,
        SandboxWorkspaceState::Committing,
        SandboxWorkspaceState::Restoring,
        SandboxWorkspaceState::Reconciling,
        SandboxWorkspaceState::Failed,
        SandboxWorkspaceState::Deleting,
        SandboxWorkspaceState::Deleted,
    ];
    let outcomes = [
        WorkspaceOperationOutcome::NotSent,
        WorkspaceOperationOutcome::Unknown,
        WorkspaceOperationOutcome::Confirmed,
    ];
    let dispositions = [
        WorkspaceConfirmedDisposition::ResourcePresent,
        WorkspaceConfirmedDisposition::ResourceAbsent,
    ];
    let checkpoint_states = [
        WorkspaceCheckpointState::Creating,
        WorkspaceCheckpointState::Available,
        WorkspaceCheckpointState::Deleting,
        WorkspaceCheckpointState::Deleted,
        WorkspaceCheckpointState::Failed,
    ];
    let operation_kinds = [
        WorkspaceOperationKind::Create,
        WorkspaceOperationKind::Attach,
        WorkspaceOperationKind::Commit,
        WorkspaceOperationKind::Checkpoint,
        WorkspaceOperationKind::Restore,
        WorkspaceOperationKind::Delete,
    ];
    let storage_kinds = [
        ProviderStorageKind::MutableFilesystem,
        ProviderStorageKind::PortableCheckpoint,
    ];
    let effects = [WorkspaceEffect::ReadOnly, WorkspaceEffect::MayWrite];
    assert_eq!(
        serde_json::from_value::<Vec<SandboxWorkspaceState>>(
            serde_json::to_value(states).expect("workspace states should serialize")
        )
        .expect("workspace states should deserialize"),
        states
    );
    assert_eq!(
        serde_json::from_value::<Vec<WorkspaceOperationOutcome>>(
            serde_json::to_value(outcomes).expect("operation outcomes should serialize")
        )
        .expect("operation outcomes should deserialize"),
        outcomes
    );
    assert_eq!(
        serde_json::from_value::<Vec<WorkspaceConfirmedDisposition>>(
            serde_json::to_value(dispositions).expect("confirmed dispositions should serialize")
        )
        .expect("confirmed dispositions should deserialize"),
        dispositions
    );
    assert_eq!(
        serde_json::from_value::<Vec<WorkspaceCheckpointState>>(
            serde_json::to_value(checkpoint_states).expect("checkpoint states should serialize")
        )
        .expect("checkpoint states should deserialize"),
        checkpoint_states
    );
    assert_eq!(
        serde_json::from_value::<Vec<WorkspaceOperationKind>>(
            serde_json::to_value(operation_kinds).expect("operation kinds should serialize")
        )
        .expect("operation kinds should deserialize"),
        operation_kinds
    );
    assert_eq!(
        serde_json::from_value::<Vec<ProviderStorageKind>>(
            serde_json::to_value(storage_kinds).expect("storage kinds should serialize")
        )
        .expect("storage kinds should deserialize"),
        storage_kinds
    );
    assert_eq!(
        serde_json::from_value::<Vec<WorkspaceEffect>>(
            serde_json::to_value(effects).expect("workspace effects should serialize")
        )
        .expect("workspace effects should deserialize"),
        effects
    );
}

#[test]
fn workspace_persisted_contracts_reject_unknown_fields() {
    // Pins: a newer or misspelled generation fence cannot be silently
    // ignored by an older workspace binary during replay or recovery.
    let mut encoded =
        serde_json::to_value(fixture_workspace_binding()).expect("binding should serialize");
    encoded
        .as_object_mut()
        .expect("binding should be a JSON object")
        .insert("writer_generation".to_string(), serde_json::json!(99));

    let error = serde_json::from_value::<WorkspaceBinding>(encoded)
        .expect_err("unknown workspace fields must fail closed");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn workspace_persisted_label_parsers_are_exhaustive_and_fail_closed() {
    // Pins: repository row decoding uses one stable label per state and
    // refuses database values no binary version understands.
    for (state, label) in [
        (SandboxWorkspaceState::Creating, "creating"),
        (SandboxWorkspaceState::Ready, "ready"),
        (SandboxWorkspaceState::Active, "active"),
        (SandboxWorkspaceState::Quiescing, "quiescing"),
        (SandboxWorkspaceState::Committing, "committing"),
        (SandboxWorkspaceState::Restoring, "restoring"),
        (SandboxWorkspaceState::Reconciling, "reconciling"),
        (SandboxWorkspaceState::Failed, "failed"),
        (SandboxWorkspaceState::Deleting, "deleting"),
        (SandboxWorkspaceState::Deleted, "deleted"),
    ] {
        assert_eq!(state.as_str(), label);
        assert_eq!(
            SandboxWorkspaceState::from_label(label).expect("known state should parse"),
            state
        );
    }
    for (outcome, label) in [
        (WorkspaceOperationOutcome::NotSent, "not_sent"),
        (WorkspaceOperationOutcome::Unknown, "unknown"),
        (WorkspaceOperationOutcome::Confirmed, "confirmed"),
    ] {
        assert_eq!(outcome.as_str(), label);
        assert_eq!(
            WorkspaceOperationOutcome::from_label(label)
                .expect("known operation outcome should parse"),
            outcome
        );
    }
    for (disposition, label) in [
        (
            WorkspaceConfirmedDisposition::ResourcePresent,
            "resource_present",
        ),
        (
            WorkspaceConfirmedDisposition::ResourceAbsent,
            "resource_absent",
        ),
    ] {
        assert_eq!(disposition.as_str(), label);
        assert_eq!(
            WorkspaceConfirmedDisposition::from_label(label)
                .expect("known confirmed disposition should parse"),
            disposition
        );
    }
    assert!(SandboxWorkspaceState::from_label("running").is_err());
    assert!(WorkspaceOperationOutcome::from_label("success").is_err());
    assert!(WorkspaceConfirmedDisposition::from_label("missing").is_err());
    assert!(WorkspaceEffect::from_label("writes_maybe").is_err());
}
