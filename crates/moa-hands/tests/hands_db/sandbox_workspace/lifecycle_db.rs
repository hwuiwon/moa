//! Durable workspace lifecycle, fencing, and reconciliation against Postgres.

use std::{path::PathBuf, time::Duration};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::error::MoaError;
use moa_core::types::{
    action_policy::CallOrigin,
    contact::ContactId,
    hands::{
        BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, HandHandle, LifetimeLimit,
        MemoryLimit, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
    },
    identifiers::{
        ExecutionCompensationScopeId, ExecutionRunScopeId, ExecutionTaskScopeId,
        HandProvisioningOperationId, ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId,
        WorkspaceCheckpointId, WorkspaceOperationId,
    },
    sandbox_workspace::{
        DurabilityClass, ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt,
        ProviderStorageKind, ProviderStorageRef, SandboxWorkspaceScope, SandboxWorkspaceState,
        WorkspaceBinding, WorkspaceCheckpointPublication, WorkspaceConfirmedDisposition,
        WorkspaceOperationKind, WorkspaceOperationOutcome, WorkspacePostCommitState,
        WorkspaceRevisionRef, WorkspaceStorageOperation,
    },
};
use moa_hands::core::{
    leases::{
        HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStatus, HandLeaseStore,
        HandLeaseWorkspaceAttachment, LeaseHandle, PostgresHandLeaseStore,
    },
    sandbox_workspace::{
        capacity::{ActiveHandCapacityRequest, PostgresWorkspaceCapacityRepository},
        checkpoint::model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
        model::{
            AbsentTaskHandReleaseIntent, ActivateHydratedWorkspaceRequest,
            CompensationHandReleaseClaimIntent, CompensationHandReleaseIntent,
            CreateWorkspaceRequest, SandboxWorkspace, TaskHandReleaseIntent, WorkspaceTransition,
            WorkspaceWriterClaim,
        },
        operations::{
            AbsenceObservation, PostgresWorkspaceOperationRepository, WorkspaceOperationIntent,
        },
        repository::PostgresWorkspaceRepository,
    },
};
use moa_test_support::fixtures::pg_now;
use sqlx::postgres::PgPoolOptions;

use super::{database_url, seed_session};

async fn seed_account(pool: &sqlx::PgPool, account_id: ProviderAccountId) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'local', $2, $3, '{}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("lifecycle-{account_id}"))
    .bind(format!("org-{account_id}"))
    .execute(pool)
    .await
    .expect("seed provider account");
}

async fn seed_cancelling_compensation(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    session_id: SessionId,
    contact_id: Option<ContactId>,
) -> (
    ExecutionRunScopeId,
    ExecutionTaskScopeId,
    ExecutionCompensationScopeId,
) {
    let planning_context_uid = uuid::Uuid::now_v7();
    let run_id = ExecutionRunScopeId::new();
    let task_id = ExecutionTaskScopeId::new();
    let compensation_id = ExecutionCompensationScopeId::new();
    let plan_hash = "1".repeat(64);
    let context_hash = "2".repeat(64);
    let plan = serde_json::json!({
        "definition": {
            "cancel_policy": "retain_effects",
            "input_schema": {},
            "output_schema": {},
            "nodes": [{
                "id": "output", "requirement_ids": [], "depends_on": [], "when": null,
                "input": {}, "output_schema": {},
                "operation": {"kind": "output", "value": {}},
                "compensation": null,
                "retry": {"max_attempts": 1, "initial_backoff_ms": 1, "max_backoff_ms": 1},
                "budget": null
            }]
        },
        "plan_hash": plan_hash,
        "catalog_hash": "0".repeat(64),
        "estimate": {"cost_microusd": 0, "tokens": 0, "tool_calls": 0,
                     "retrieved_bytes": 0, "tasks": 1},
        "report": {"issues": []}
    });
    sqlx::query(
        "INSERT INTO moa.execution_planning_context (\
             planning_context_uid, tenant_id, session_id, originating_user_sequence_num,\
             originating_user_event_hash, owner_user_id, planning_context_hash, snapshot,\
             contact_id\
         ) VALUES ($1, $2, $3, 0, $4, 'hands-release-test', $4, '{}'::JSONB, $5)",
    )
    .bind(planning_context_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(&context_hash)
    .bind(contact_id)
    .execute(pool)
    .await
    .expect("seed execution planning context");
    sqlx::query(
        "INSERT INTO moa.execution_run (\
             run_uid, tenant_id, session_id, originating_user_sequence_num,\
             planning_context_uid, planning_context_hash, owner_user_id, goal_contract,\
             initial_plan, active_plan, initial_plan_hash, active_plan_hash,\
             capability_catalog, authorization_envelope, source_provenance, source_kind,\
             input, admitted_identity, status, contact_id\
         ) VALUES ($1, $2, $3, 0, $4, $5, 'hands-release-test', $6, $7, $7, $8, $8,\
                   $9, $10, $11, 'generated_plan', '{}'::JSONB, $12, 'queued', $13)",
    )
    .bind(run_id)
    .bind(tenant_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind(&context_hash)
    .bind(serde_json::json!({
        "objective": "release", "requirements": [], "deliverables": [],
        "coverage": [], "constraints": [], "completion_checks": []
    }))
    .bind(&plan)
    .bind(&plan_hash)
    .bind(serde_json::json!({"capabilities": [], "catalog_hash": "0".repeat(64)}))
    .bind(serde_json::json!({"capability_refs": [], "skill_refs": []}))
    .bind(serde_json::json!({
        "kind": "generated_plan",
        "planner": {"model": "hands-release-test", "prompt_version": "test",
                    "candidate_hash": "3".repeat(64), "compiler_report_hash": "4".repeat(64),
                    "final_plan_hash": plan_hash, "repair_attempts": 0}
    }))
    .bind(serde_json::json!({
        "identity_type": "operator", "id": run_id, "tenant_id": tenant_id,
        "api_key_id": null, "acting_on_behalf_of": null
    }))
    .bind(contact_id)
    .execute(pool)
    .await
    .expect("seed execution run");
    sqlx::query(
        "INSERT INTO moa.execution_task (\
             task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, input,\
             task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, estimate_tasks,\
             estimate_tool_calls, estimate_retrieved_bytes, contact_id\
         ) VALUES ($1, $2, $3, 'forward', 'forward', 1, 'completed', '{}',\
                   '{\"kind\":\"output\",\"value\":null}',\
                   '{\"max_attempts\":2,\"initial_backoff_ms\":1,\"max_backoff_ms\":1}',\
                   0, 0, 1, 0, 0, $4)",
    )
    .bind(task_id)
    .bind(run_id)
    .bind(tenant_id)
    .bind(contact_id)
    .execute(pool)
    .await
    .expect("seed forward task");
    sqlx::query(
        "INSERT INTO moa.execution_compensation (\
             compensation_id, run_uid, forward_task_id, tenant_id, registered_sequence,\
             forward_generation, compensator, mapped_input, status, started_at,\
             attempt_state, attempt_started_at, attempt_deadline_at, release_intent, contact_id\
         ) VALUES ($1, $2, $3, $4, 1, 1, $5, '{}', 'running', now(),\
                   'cancelling', now(), now() + interval '10 minutes', 'pause', $6)",
    )
    .bind(compensation_id)
    .bind(run_id)
    .bind(task_id)
    .bind(tenant_id)
    .bind(serde_json::json!({
        "compensator": {"name": "test.undo", "version": "contract"},
        "input_mapping": {"bindings": []}
    }))
    .bind(contact_id)
    .execute(pool)
    .await
    .expect("seed cancelling compensation");
    (run_id, task_id, compensation_id)
}

async fn seed_cancelling_task(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    run_id: ExecutionRunScopeId,
    node_id: &str,
    contact_id: Option<ContactId>,
) -> ExecutionTaskScopeId {
    let task_id = ExecutionTaskScopeId::new();
    sqlx::query(
        "INSERT INTO moa.execution_task (\
             task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, input,\
             task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, estimate_tasks,\
             estimate_tool_calls, estimate_retrieved_bytes, reserved_tasks, reserved_at,\
             started_at, attempt_state, attempt_started_at, attempt_deadline_at, contact_id\
         ) VALUES ($1, $2, $3, $4, $4, 1, 'running', '{}',\
                   '{\"kind\":\"output\",\"value\":null}',\
                   '{\"max_attempts\":2,\"initial_backoff_ms\":1,\"max_backoff_ms\":1}',\
                   0, 0, 1, 0, 0, 1, now(), now(), 'cancelling', now(),\
                   now() + interval '10 minutes', $5)",
    )
    .bind(task_id)
    .bind(run_id)
    .bind(tenant_id)
    .bind(node_id)
    .bind(contact_id)
    .execute(pool)
    .await
    .expect("seed cancelling execution task");
    task_id
}

fn lease_policy() -> HandLeasePolicy {
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        LifetimeLimit::Unbounded,
        LifetimeLimit::Unbounded,
    )
    .expect("test profile validates");
    let effective = moa_core::types::hands::resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("workspace-lifecycle-deployment", profile)
            .expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "workspace-lifecycle-capabilities-v1",
    )
    .expect("test resolution succeeds");
    HandLeasePolicy::from_effective(&effective)
}

fn binding(workspace: &SandboxWorkspace) -> WorkspaceBinding {
    assert_eq!(
        (workspace.checkpoint_generation, workspace.checkpoint_id),
        (0, None),
        "the initial hydration must restore generation zero"
    );
    WorkspaceBinding {
        tenant_id: workspace.tenant_id,
        scope: workspace.scope.clone(),
        workspace_id: workspace.workspace_id,
        provider_account_id: workspace.provider_account_id,
        provider_account_generation: u64::try_from(workspace.provider_account_generation)
            .expect("positive provider account generation"),
        durability_class: workspace.durability_class,
        writer_epoch: u64::try_from(workspace.writer_epoch).expect("nonnegative writer epoch"),
        instance_generation: u64::try_from(workspace.instance_generation)
            .expect("nonnegative instance generation"),
        current_revision: None,
    }
}

async fn activate_hydrated_workspace(
    pool: &sqlx::PgPool,
    session_id: SessionId,
    worker_id: &str,
    workspace: &SandboxWorkspace,
) -> ActiveHandCapacityRequest {
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let attachment = HandLeaseWorkspaceAttachment::new(
        workspace.workspace_id,
        workspace.writer_epoch,
        workspace.instance_generation,
        None,
    )
    .expect("claimed workspace attachment validates");
    let policy = lease_policy();
    let provisioning = leases
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id: workspace.tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment,
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim provisioning lease")
        .expect("provisioning lease exists");
    let lease_handle = LeaseHandle::new(
        provisioning.provisioning_operation_id,
        HandHandle::local(PathBuf::from(format!(
            "/tmp/moa-workspace-lifecycle-{}",
            workspace.workspace_id
        ))),
    );
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let active_capacity = ActiveHandCapacityRequest {
        tenant_id: workspace.tenant_id,
        workspace_id: workspace.workspace_id,
        provider_account_id: workspace.provider_account_id,
        provider_account_generation: workspace.provider_account_generation,
        provisioning_operation_id: provisioning.provisioning_operation_id,
        hand_lease_generation: provisioning.generation,
        expected_writer_epoch: workspace.writer_epoch,
        expected_instance_generation: workspace.instance_generation,
    };
    capacity
        .reserve_active_hand(&active_capacity)
        .await
        .expect("reserve exact active hand before provider creation");
    assert!(
        workspaces
            .activate_hydrated(ActivateHydratedWorkspaceRequest {
                binding: &binding(workspace),
                lease: &provisioning,
                handle: lease_handle,
            })
            .await
            .expect("atomically activate exact hydrated lease and workspace")
    );
    assert!(
        capacity
            .commit_active_hand(&active_capacity)
            .await
            .expect("commit exact active hand after activation")
    );
    let active_lease = leases
        .get(workspace.tenant_id, session_id, worker_id, "local")
        .await
        .expect("load activated hand lease")
        .expect("activated hand lease exists");
    assert_eq!(active_lease.status, HandLeaseStatus::Active);
    assert_eq!(active_lease.attachment, provisioning.attachment);
    active_capacity
}

fn create_request(
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
) -> CreateWorkspaceRequest {
    CreateWorkspaceRequest {
        workspace_id,
        tenant_id,
        scope: SandboxWorkspaceScope::ExecutionTask {
            run_id: ExecutionRunScopeId::new(),
            task_id: ExecutionTaskScopeId::new(),
        },
        provider: "local".to_string(),
        provider_account_id: account_id,
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        retention_deadline_at: None,
    }
}

#[tokio::test]
#[ignore = "requires Postgres for an isolated current-schema test database"]
async fn cancelling_task_without_owned_compute_gets_exact_absence_receipt_db() {
    // Pins: a sandbox-capable task denied before provisioning still obtains a
    // durable exact-attempt absence receipt, while a live lease cannot be hidden
    // behind that no-workspace path.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated current-schema Postgres");
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let contact_id = ContactId::new();
    seed_session(&pool, session_id, tenant_id).await;
    let (run_id, _, _) =
        seed_cancelling_compensation(&pool, tenant_id, session_id, Some(contact_id)).await;
    let task_id = seed_cancelling_task(
        &pool,
        tenant_id,
        run_id,
        "never-provisioned",
        Some(contact_id),
    )
    .await;
    let repository = PostgresWorkspaceRepository::new(pool.clone());
    let intent = AbsentTaskHandReleaseIntent {
        receipt_id: uuid::Uuid::now_v7(),
        tenant_id,
        contact_id: Some(contact_id),
        run_id,
        task_id,
        logical_generation: 1,
        attempt_generation: 1,
        verified_at: Utc::now(),
    };
    let receipt = repository
        .record_absent_task_execution_hand_release_receipt(intent)
        .await
        .expect("record exact no-owned-compute receipt");
    assert_eq!(
        receipt.owner,
        ExecutionHandReleaseOwner::Task {
            task_id,
            logical_generation: 1,
        }
    );
    assert_eq!(receipt.attempt_generation, 1);
    assert_eq!(
        (
            receipt.workspace_id,
            receipt.hand_provisioning_operation_id,
            receipt.checkpoint_id,
        ),
        (None, None, None),
        "database-verified absence must not fabricate provider ownership"
    );
    assert_eq!(
        repository
            .get_task_execution_hand_release_receipt(
                tenant_id,
                Some(contact_id),
                run_id,
                task_id,
                1,
                1,
            )
            .await
            .expect("replay exact absence receipt"),
        Some(receipt)
    );

    let live_task_id =
        seed_cancelling_task(&pool, tenant_id, run_id, "live-owner", Some(contact_id)).await;
    let live_scope = format!("execution:{run_id}:{live_task_id}");
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) VALUES ($1, $2, $3, 'local', 'local', NULL, 'stale', 1,\
                   gen_random_uuid(), now(), now(), now())",
    )
    .bind(session_id)
    .bind(&live_scope)
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed live exact task owner");
    assert!(
        repository
            .record_absent_task_execution_hand_release_receipt(AbsentTaskHandReleaseIntent {
                receipt_id: uuid::Uuid::now_v7(),
                tenant_id,
                contact_id: Some(contact_id),
                run_id,
                task_id: live_task_id,
                logical_generation: 1,
                attempt_generation: 1,
                verified_at: Utc::now(),
            })
            .await
            .is_err(),
        "a live exact owner must block the task absence proof"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires Postgres for an isolated current-schema test database"]
async fn checkpointed_task_destroy_records_exact_release_receipt_db() {
    // Pins: after the portable checkpoint CAS destroys an execution-task hand,
    // the final receipt CAS recognizes the available checkpoint row and makes
    // the exact task-attempt release replayable.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated current-schema Postgres");
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let contact_id = ContactId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;
    let (run_id, _, _) =
        seed_cancelling_compensation(&pool, tenant_id, session_id, Some(contact_id)).await;
    let task_id = seed_cancelling_task(
        &pool,
        tenant_id,
        run_id,
        "checkpointed-release",
        Some(contact_id),
    )
    .await;
    let worker_id = format!("execution:{run_id}:{task_id}");
    let workspace_scope = SandboxWorkspaceScope::ExecutionTask { run_id, task_id };
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: workspace_scope,
            provider: "local".to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("create task workspace");
    assert!(
        workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Creating,
                to: SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("make task workspace ready")
    );
    let restoring = workspaces
        .claim_writer(WorkspaceWriterClaim {
            tenant_id,
            workspace_id,
            expected_state: SandboxWorkspaceState::Ready,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
        })
        .await
        .expect("claim task workspace writer")
        .expect("task workspace writer claim succeeds");
    activate_hydrated_workspace(&pool, session_id, &worker_id, &restoring).await;
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load active task workspace")
        .expect("active task workspace exists");
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let active_lease = leases
        .get(tenant_id, session_id, &worker_id, "local")
        .await
        .expect("load active task lease")
        .expect("active task lease exists");
    let receipt_id = uuid::Uuid::now_v7();
    let (persisted_receipt_id, claim_token, requested_at) = workspaces
        .begin_task_execution_hand_release(TaskHandReleaseIntent {
            receipt_id,
            contact_id: Some(contact_id),
            run_id,
            task_id,
            logical_generation: 1,
            attempt_generation: 1,
            deadline_at: Utc::now() + ChronoDuration::minutes(5),
            recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
            workspace: &active,
            lease: &active_lease,
        })
        .await
        .expect("persist task release intent before provider work");
    assert_eq!(persisted_receipt_id, receipt_id);

    for (from, to) in [
        (
            SandboxWorkspaceState::Active,
            SandboxWorkspaceState::Quiescing,
        ),
        (
            SandboxWorkspaceState::Quiescing,
            SandboxWorkspaceState::Committing,
        ),
    ] {
        assert!(
            workspaces
                .transition(WorkspaceTransition {
                    tenant_id,
                    workspace_id,
                    from,
                    to,
                    writer_epoch: active.writer_epoch,
                    instance_generation: active.instance_generation,
                })
                .await
                .expect("enter task release checkpoint barrier")
        );
    }
    let operation_id = WorkspaceOperationId::new();
    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let now = Utc::now();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Commit,
            request_hash: "sha256:task-release".to_string(),
            expected_writer_epoch: active.writer_epoch,
            expected_instance_generation: active.instance_generation,
            expected_checkpoint_generation: active.checkpoint_generation,
            deadline_at: now + ChronoDuration::minutes(5),
            reconcile_not_before: now + ChronoDuration::minutes(6),
        })
        .await
        .expect("persist task release checkpoint intent");
    workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: active.writer_epoch,
            expected_instance_generation: active.instance_generation,
            expected_checkpoint_generation: active.checkpoint_generation,
        })
        .await
        .expect("create task release checkpoint row")
        .expect("task release checkpoint fences match");
    let active_binding = binding(&active);
    let storage_operation = WorkspaceStorageOperation {
        operation_id,
        kind: WorkspaceOperationKind::Commit,
        binding: active_binding.clone(),
        deadline: now + ChronoDuration::minutes(5),
        request_hash: "sha256:task-release".to_string(),
    };
    PostgresWorkspaceCapacityRepository::new(pool.clone())
        .reserve_checkpoint_publication(&storage_operation, 0)
        .await
        .expect("reserve task release checkpoint before publication");
    let publication = WorkspaceCheckpointPublication {
        revision: WorkspaceRevisionRef {
            checkpoint_id,
            generation: 1,
            format_version: 1,
        },
        storage: ProviderStorageRef {
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: format!("object://{checkpoint_id}"),
            workspace_locator: None,
        },
        manifest_digest: "sha256:task-release-manifest".to_string(),
        logical_bytes: 0,
    };
    assert!(
        workspaces
            .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                binding: &active_binding,
                operation_id,
                publication: &publication,
                post_commit_state: WorkspacePostCommitState::ComputeDestroyed,
                lease: &active_lease,
            })
            .await
            .expect("atomically publish task release checkpoint and destroy lease")
    );
    let receipt = ExecutionHandReleaseReceipt {
        receipt_id,
        tenant_id,
        run_id,
        owner: ExecutionHandReleaseOwner::Task {
            task_id,
            logical_generation: 1,
        },
        attempt_generation: 1,
        workspace_id: Some(workspace_id),
        writer_epoch: Some(u64::try_from(active.writer_epoch).expect("valid writer epoch")),
        instance_generation: Some(
            u64::try_from(active.instance_generation).expect("valid instance generation"),
        ),
        hand_provisioning_operation_id: Some(active_lease.provisioning_operation_id),
        hand_lease_generation: Some(
            u64::try_from(active_lease.generation).expect("valid hand lease generation"),
        ),
        checkpoint_id: Some(checkpoint_id),
        checkpoint_generation: Some(1),
        checkpoint_manifest_digest: Some(publication.manifest_digest.clone()),
        checkpoint_logical_bytes: Some(0),
        requested_at,
        released_at: Utc::now(),
    };
    let finalized = workspaces
        .record_task_execution_hand_release_receipt(&receipt, claim_token, Some(contact_id))
        .await
        .expect("available checkpoint must finalize the task release receipt");
    assert_eq!(finalized, receipt);
    assert_eq!(
        workspaces
            .get_task_execution_hand_release_receipt(
                tenant_id,
                Some(contact_id),
                run_id,
                task_id,
                1,
                1,
            )
            .await
            .expect("replay finalized task release receipt"),
        Some(receipt)
    );
}

#[tokio::test]
#[ignore = "requires Postgres for an isolated current-schema test database"]
async fn contact_scoped_compensation_without_compute_gets_exact_release_receipt_db() {
    // Pins: a contact-scoped compensation that never provisioned compute still crosses the
    // exact cancelling-owner CAS and persists its replayable verified-absence receipt.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated current-schema Postgres");
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_id).await;
    let (run_id, _task_id, compensation_id) =
        seed_cancelling_compensation(&pool, tenant_id, session_id, Some(contact_id)).await;
    let hand_scope = format!("execution_compensation:{run_id}:{compensation_id}");
    let repository = PostgresWorkspaceRepository::new(pool.clone());
    let receipt_id = uuid::Uuid::now_v7();
    let deadline_at = Utc::now() + ChronoDuration::minutes(1);
    let (persisted_receipt_id, claim_token, requested_at) = repository
        .begin_compensation_execution_hand_release(CompensationHandReleaseIntent {
            receipt_id,
            tenant_id,
            contact_id: Some(contact_id),
            session_id,
            run_id,
            compensation_id,
            logical_generation: 1,
            attempt_generation: 1,
            hand_scope: &hand_scope,
            lease: None,
            deadline_at,
            recovery_claim_expires_at: deadline_at,
        })
        .await
        .expect("contact-scoped compensation should persist its absence intent");
    assert_eq!(persisted_receipt_id, receipt_id);

    let receipt = ExecutionHandReleaseReceipt {
        receipt_id,
        tenant_id,
        run_id,
        owner: ExecutionHandReleaseOwner::Compensation {
            compensation_id,
            logical_generation: 1,
        },
        attempt_generation: 1,
        workspace_id: None,
        writer_epoch: None,
        instance_generation: None,
        hand_provisioning_operation_id: None,
        hand_lease_generation: None,
        checkpoint_id: None,
        checkpoint_generation: None,
        checkpoint_manifest_digest: None,
        checkpoint_logical_bytes: None,
        requested_at,
        released_at: Utc::now(),
    };
    let finalized = repository
        .record_compensation_execution_hand_release_receipt(
            &receipt,
            session_id,
            &hand_scope,
            claim_token,
            Some(contact_id),
        )
        .await
        .expect("contact-scoped compensation should finalize verified absence");
    assert_eq!(finalized, receipt);
    assert_eq!(
        repository
            .get_compensation_execution_hand_release_receipt(
                tenant_id,
                run_id,
                compensation_id,
                1,
                1,
            )
            .await
            .expect("replay contact-scoped compensation receipt"),
        Some(receipt)
    );
}

#[tokio::test]
#[ignore = "requires Postgres for an isolated current-schema test database"]
async fn compensation_release_recovers_persisted_destroyed_identity_after_deadline_db() {
    // Pins: a crash after provider teardown but before receipt finalization reuses
    // the pending receipt's exact lease identity after the provider-I/O deadline;
    // deleting that exact destroyed row remains fail-closed.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated current-schema Postgres");
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_id).await;
    let (run_id, _task_id, compensation_id) =
        seed_cancelling_compensation(&pool, tenant_id, session_id, Some(contact_id)).await;
    let hand_scope = format!("execution_compensation:{run_id}:{compensation_id}");
    let provisioning_operation_id = HandProvisioningOperationId::new();
    let generation = 7_i64;
    let lease_handle = LeaseHandle::new(
        provisioning_operation_id,
        HandHandle::local(PathBuf::from(format!("/tmp/{provisioning_operation_id}"))),
    );
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) VALUES ($1, $2, $3, 'local', 'local', $4, 'stale', $5, $6, now(), now(), now())",
    )
    .bind(session_id)
    .bind(&hand_scope)
    .bind(tenant_id)
    .bind(sqlx::types::Json(&lease_handle))
    .bind(generation)
    .bind(provisioning_operation_id)
    .execute(&pool)
    .await
    .expect("seed stale compensation lease");
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let initial_lease = leases
        .get(tenant_id, session_id, &hand_scope, "local")
        .await
        .expect("load exact compensation lease")
        .expect("seeded compensation lease exists");
    let repository = PostgresWorkspaceRepository::new(pool.clone());
    let receipt_id = uuid::Uuid::now_v7();
    let deadline_at = Utc::now() + ChronoDuration::minutes(1);
    repository
        .begin_compensation_execution_hand_release(CompensationHandReleaseIntent {
            receipt_id,
            tenant_id,
            contact_id: Some(contact_id),
            session_id,
            run_id,
            compensation_id,
            logical_generation: 1,
            attempt_generation: 1,
            hand_scope: &hand_scope,
            lease: Some(&initial_lease),
            deadline_at,
            recovery_claim_expires_at: deadline_at,
        })
        .await
        .expect("persist exact compensation release intent");
    let destroy_claim = leases
        .claim_for_destroy(tenant_id, &initial_lease, Duration::from_secs(30))
        .await
        .expect("claim exact lease destroy")
        .expect("stale lease is destroyable");
    assert!(
        leases
            .finalize_destroy(tenant_id, &initial_lease, destroy_claim)
            .await
            .expect("finalize exact lease destroy")
    );
    sqlx::query(
        "UPDATE moa.sandbox_execution_hand_release_receipts \
         SET requested_at = now() - interval '10 minutes', \
             deadline_at = now() - interval '5 minutes', \
             claim_expires_at = now() - interval '6 minutes' \
         WHERE receipt_id = $1",
    )
    .bind(receipt_id)
    .execute(&pool)
    .await
    .expect("advance pending release beyond its provider deadline");
    let claim = repository
        .claim_pending_compensation_execution_hand_release(CompensationHandReleaseClaimIntent {
            tenant_id,
            contact_id: Some(contact_id),
            run_id,
            compensation_id,
            logical_generation: 1,
            attempt_generation: 1,
            recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
        })
        .await
        .expect("renew storage-only recovery claim")
        .expect("expired pending release is reclaimable");
    assert_eq!(claim.receipt_id, receipt_id);
    assert_eq!(
        claim.hand_provisioning_operation_id,
        Some(provisioning_operation_id)
    );
    assert_eq!(claim.hand_lease_generation, Some(generation));
    let destroyed = leases
        .get_exact_generation(
            tenant_id,
            session_id,
            &hand_scope,
            provisioning_operation_id,
            generation,
        )
        .await
        .expect("load exact destroyed generation")
        .expect("destroyed generation remains durable");
    assert_eq!(destroyed.status, HandLeaseStatus::Destroyed);
    assert!(destroyed.handle.is_none());

    sqlx::query(
        "DELETE FROM moa.hand_leases WHERE tenant_id = $1 AND session_id = $2 \
         AND worker_id = $3 AND provisioning_operation_id = $4 AND generation = $5",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(&hand_scope)
    .bind(provisioning_operation_id)
    .bind(generation)
    .execute(&pool)
    .await
    .expect("simulate corrupt missing exact lease row");
    let release_receipt = ExecutionHandReleaseReceipt {
        receipt_id,
        tenant_id,
        run_id,
        owner: ExecutionHandReleaseOwner::Compensation {
            compensation_id,
            logical_generation: 1,
        },
        attempt_generation: 1,
        workspace_id: None,
        writer_epoch: None,
        instance_generation: None,
        hand_provisioning_operation_id: Some(provisioning_operation_id),
        hand_lease_generation: Some(u64::try_from(generation).expect("positive generation")),
        checkpoint_id: None,
        checkpoint_generation: None,
        checkpoint_manifest_digest: None,
        checkpoint_logical_bytes: None,
        requested_at: claim.requested_at,
        released_at: Utc::now(),
    };
    assert!(
        repository
            .record_compensation_execution_hand_release_receipt(
                &release_receipt,
                session_id,
                &hand_scope,
                claim.claim_token,
                Some(contact_id),
            )
            .await
            .is_err(),
        "missing exact destroyed lease evidence must not finalize absence"
    );
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) VALUES ($1, $2, $3, 'local', 'local', NULL, 'destroyed', $4, $5, now(), now(), now())",
    )
    .bind(session_id)
    .bind(&hand_scope)
    .bind(tenant_id)
    .bind(generation)
    .bind(provisioning_operation_id)
    .execute(&pool)
    .await
    .expect("restore exact destroyed evidence");
    let finalized = repository
        .record_compensation_execution_hand_release_receipt(
            &release_receipt,
            session_id,
            &hand_scope,
            claim.claim_token,
            Some(contact_id),
        )
        .await
        .expect("finalize recovered exact receipt");
    assert_eq!(finalized, release_receipt);
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn workspace_writer_and_reconciliation_callbacks_are_generation_fenced_db() {
    // Pins: competing writers have one CAS winner, and changed inventory cannot
    // satisfy the two-separated-empty absence proof.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("writer-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    workspaces
        .create(&create_request(tenant_id, workspace_id, account_id))
        .await
        .expect("persist workspace before provider I/O");
    assert!(
        workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Creating,
                to: SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("ready transition")
    );

    let claim = WorkspaceWriterClaim {
        tenant_id,
        workspace_id,
        expected_state: SandboxWorkspaceState::Ready,
        expected_writer_epoch: 0,
        expected_instance_generation: 0,
    };
    let (left, right) = tokio::join!(
        workspaces.claim_writer(claim),
        workspaces.claim_writer(claim)
    );
    let claims = [
        left.expect("left writer claim"),
        right.expect("right writer claim"),
    ];
    assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
    let restoring = claims
        .into_iter()
        .flatten()
        .next()
        .expect("exactly one writer owns the workspace");
    assert_eq!(restoring.state, SandboxWorkspaceState::Restoring);
    assert_eq!(
        (restoring.writer_epoch, restoring.instance_generation),
        (1, 1)
    );
    let active_capacity =
        activate_hydrated_workspace(&pool, session_id, &worker_id, &restoring).await;
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after hydrated activation")
        .expect("activated workspace exists");
    assert_eq!(active.state, SandboxWorkspaceState::Active);
    assert_eq!((active.writer_epoch, active.instance_generation), (1, 1));

    let reaper_claim = uuid::Uuid::now_v7();
    sqlx::query(
        r#"
        UPDATE moa.hand_leases
        SET status = 'reaping', reap_claim_token = $2,
            reap_claim_expires_at = now() + interval '30 seconds'
        WHERE session_id = $1
        "#,
    )
    .bind(session_id)
    .bind(reaper_claim)
    .execute(&pool)
    .await
    .expect("establish exact durable reaper ownership");
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let mut stale_capacity = active_capacity;
    stale_capacity.hand_lease_generation += 1;
    assert!(
        !capacity
            .release_active_hand_to_reaper(&stale_capacity, reaper_claim)
            .await
            .expect("stale generation is a fenced miss")
    );
    assert!(
        !capacity
            .release_active_hand_to_reaper(&active_capacity, uuid::Uuid::now_v7())
            .await
            .expect("wrong reaper token is a fenced miss")
    );
    assert!(
        capacity
            .release_active_hand_to_reaper(&active_capacity, reaper_claim)
            .await
            .expect("exact live reaper claim releases active compute capacity")
    );
    let active_capacity_state = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE hand_provisioning_operation_id = $1",
    )
    .bind(active_capacity.provisioning_operation_id)
    .fetch_one(&pool)
    .await
    .expect("load released active capacity row");
    assert_eq!(active_capacity_state, "released");

    assert!(
        !workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Active,
                to: SandboxWorkspaceState::Quiescing,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("stale transition is a fenced miss")
    );

    let now = Utc::now();
    let operation_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: "sha256:lifecycle-create".to_string(),
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
            deadline_at: now - ChronoDuration::seconds(3),
            reconcile_not_before: now - ChronoDuration::seconds(2),
        })
        .await
        .expect("intent is durable before provider observation");
    operations
        .mark_unknown(tenant_id, operation_id)
        .await
        .expect("ambiguous send enters reconciliation");
    let first_claim = operations
        .claim_reconciliation(1, Duration::from_secs(30))
        .await
        .expect("claim first observation")
        .pop()
        .expect("operation is claimable");
    assert_eq!(
        operations
            .record_inventory_observation(&first_claim, true, "digest-a", now)
            .await
            .expect("record first empty"),
        AbsenceObservation::First
    );
    operations
        .release_after_first_empty(&first_claim)
        .await
        .expect("release for separated observation");
    sqlx::query(
        "UPDATE moa.sandbox_workspace_operations SET retry_not_before = now() - interval '1 second' WHERE operation_id = $1",
    )
    .bind(operation_id)
    .execute(&pool)
    .await
    .expect("avoid wall-clock sleep in DB test");
    let second_claim = operations
        .claim_reconciliation(1, Duration::from_secs(30))
        .await
        .expect("claim changed observation")
        .pop()
        .expect("operation is claimable again");
    assert_eq!(
        operations
            .record_inventory_observation(
                &second_claim,
                true,
                "digest-b",
                now + ChronoDuration::seconds(2),
            )
            .await
            .expect("changed inventory resets proof"),
        AbsenceObservation::First
    );

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace capacity reservations");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean hand lease");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean session");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn create_operation_replay_never_resends_and_reconciliation_settles_lifecycle_db() {
    // Pins: create provider I/O is authorized by one exact not-sent CAS. A crash
    // after that CAS leaves the workspace reconciling, replay cannot win the CAS
    // again, and only claimed provider evidence returns it to ready or failed.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    seed_account(&pool, account_id).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(&create_request(tenant_id, workspace_id, account_id))
        .await
        .expect("create workspace metadata and lifetime capacity");
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    let operation_id = WorkspaceOperationId::new();
    let now = Utc::now();
    let intent = WorkspaceOperationIntent {
        operation_id,
        tenant_id,
        workspace_id,
        provider_account_id: account_id,
        provider_account_generation: 1,
        kind: WorkspaceOperationKind::Create,
        request_hash: format!("sha256:create-replay-{operation_id}"),
        expected_writer_epoch: 0,
        expected_instance_generation: 0,
        expected_checkpoint_generation: 0,
        deadline_at: now - ChronoDuration::seconds(2),
        reconcile_not_before: now - ChronoDuration::seconds(1),
    };
    let persisted = operations
        .persist_intent(&intent)
        .await
        .expect("persist create intent before provider I/O");
    assert_eq!(persisted.outcome, WorkspaceOperationOutcome::NotSent);
    assert!(
        !operations
            .confirm_disposition(
                tenant_id,
                operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
            )
            .await
            .expect("not-sent intent cannot accept a provider disposition")
    );
    assert!(
        operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("first exact provider-attempt CAS succeeds")
    );
    assert!(
        !operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("crash replay cannot resend the provider request")
    );
    let reconciling = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after provider-attempt CAS")
        .expect("workspace exists");
    assert_eq!(reconciling.state, SandboxWorkspaceState::Reconciling);

    let claimed = operations
        .claim_reconciliation(1, Duration::from_secs(30))
        .await
        .expect("claim ambiguous create")
        .into_iter()
        .find(|claim| claim.operation.operation_id == operation_id)
        .expect("this create is claimable");
    assert!(
        operations
            .confirm_present_claimed(&claimed)
            .await
            .expect("exact live claim confirms provider presence")
    );
    let recovered = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load reconciled workspace")
        .expect("workspace exists");
    assert_eq!(recovered.state, SandboxWorkspaceState::Ready);
    let confirmed = operations
        .persist_intent(&intent)
        .await
        .expect("identical replay loads the durable operation");
    assert_eq!(confirmed.outcome, WorkspaceOperationOutcome::Confirmed);
    assert_eq!(
        confirmed.confirmed_disposition,
        Some(WorkspaceConfirmedDisposition::ResourcePresent)
    );
    assert!(
        !operations
            .confirm_disposition(
                tenant_id,
                operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
            )
            .await
            .expect("confirmed replay cannot re-enter synchronous settlement")
    );
    assert!(
        !operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("confirmed replay cannot authorize provider I/O")
    );

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean capacity");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn checkpoint_metadata_is_created_before_bytes_and_remains_immutable_db() {
    // Pins: checkpoint bytes cannot exist as a published revision without an
    // earlier exact operation row, and the atomic production publication path
    // cannot rewrite verified payload metadata.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("checkpoint-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    workspaces
        .create(&create_request(tenant_id, workspace_id, account_id))
        .await
        .expect("create workspace intent");
    workspaces
        .transition(WorkspaceTransition {
            tenant_id,
            workspace_id,
            from: SandboxWorkspaceState::Creating,
            to: SandboxWorkspaceState::Ready,
            writer_epoch: 0,
            instance_generation: 0,
        })
        .await
        .expect("workspace ready");
    let restoring = workspaces
        .claim_writer(WorkspaceWriterClaim {
            tenant_id,
            workspace_id,
            expected_state: SandboxWorkspaceState::Ready,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
        })
        .await
        .expect("claim writer")
        .expect("writer claim succeeds");
    assert_eq!(restoring.state, SandboxWorkspaceState::Restoring);
    let _active_capacity =
        activate_hydrated_workspace(&pool, session_id, &worker_id, &restoring).await;
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after hydrated activation")
        .expect("activated workspace exists");
    assert_eq!(active.state, SandboxWorkspaceState::Active);
    for (from, to) in [
        (
            SandboxWorkspaceState::Active,
            SandboxWorkspaceState::Quiescing,
        ),
        (
            SandboxWorkspaceState::Quiescing,
            SandboxWorkspaceState::Committing,
        ),
    ] {
        assert!(
            workspaces
                .transition(WorkspaceTransition {
                    tenant_id,
                    workspace_id,
                    from,
                    to,
                    writer_epoch: active.writer_epoch,
                    instance_generation: active.instance_generation,
                })
                .await
                .expect("commit-barrier transition")
        );
    }

    let now = pg_now();
    let operation_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Checkpoint,
            request_hash: "sha256:checkpoint-one".to_string(),
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(10),
            reconcile_not_before: now + ChronoDuration::seconds(20),
        })
        .await
        .expect("persist checkpoint operation before byte I/O");
    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let creating = workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
        })
        .await
        .expect("create checkpoint metadata")
        .expect("exact operation fence creates row");
    assert_eq!(creating.generation, 1);
    assert_eq!(creating.state.as_str(), "creating");
    assert_eq!(
        (
            creating.object_reference,
            creating.manifest_digest,
            creating.logical_bytes,
            creating.verified_at,
        ),
        (None, None, None, None),
        "no byte-derived metadata exists before verification"
    );
    let conflicting_replay = workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id: WorkspaceCheckpointId::new(),
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
        })
        .await
        .expect_err("one operation identity cannot replay with different checkpoint fences");
    assert!(matches!(conflicting_replay, MoaError::ValidationError(_)));

    assert!(
        workspaces
            .create_checkpoint(CreateCheckpointRequest {
                checkpoint_id: WorkspaceCheckpointId::new(),
                tenant_id,
                workspace_id,
                parent_checkpoint_id: None,
                operation_id: WorkspaceOperationId::new(),
                expected_writer_epoch: 0,
                expected_instance_generation: 0,
                expected_checkpoint_generation: 0,
            })
            .await
            .expect("an unpersisted stale operation is a fenced miss")
            .is_none()
    );

    let active_lease = PostgresHandLeaseStore::new(pool.clone())
        .get(tenant_id, session_id, &worker_id, "local")
        .await
        .expect("load active hand lease")
        .expect("active hand lease exists");
    let active_binding = binding(&active);
    let publication = WorkspaceCheckpointPublication {
        revision: WorkspaceRevisionRef {
            checkpoint_id,
            generation: 1,
            format_version: 1,
        },
        storage: ProviderStorageRef {
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: "object://checkpoint-one".to_string(),
            workspace_locator: None,
        },
        manifest_digest: "sha256:manifest-one".to_string(),
        logical_bytes: 17,
    };
    let storage_operation = WorkspaceStorageOperation {
        operation_id,
        kind: WorkspaceOperationKind::Checkpoint,
        binding: active_binding.clone(),
        deadline: now + ChronoDuration::seconds(30),
        request_hash: "sha256:checkpoint-one".to_string(),
    };
    PostgresWorkspaceCapacityRepository::new(pool.clone())
        .reserve_checkpoint_publication(&storage_operation, publication.logical_bytes)
        .await
        .expect("reserve provider-independent checkpoint count and bytes before upload");
    // Pins: an upload/verification failure leaves only an indexed, bounded
    // pending reservation that maintenance can reclaim at the operation deadline.
    let pending_expiries = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
        r#"
        SELECT expires_at
        FROM moa.sandbox_capacity_reservations
        WHERE tenant_id = $1 AND operation_id = $2
          AND resource_dimension IN ('checkpoints', 'logical_bytes')
          AND reservation_state = 'pending'
        ORDER BY resource_dimension
        "#,
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_all(&pool)
    .await
    .expect("load reclaimable pre-upload reservations");
    assert_eq!(pending_expiries, vec![storage_operation.deadline; 2]);
    assert!(
        workspaces
            .publish_workspace_checkpoint(PublishCheckpointCommitRequest {
                binding: &active_binding,
                operation_id,
                publication: &publication,
                post_commit_state: WorkspacePostCommitState::AttachmentRetained,
                lease: &active_lease,
            })
            .await
            .expect("atomically publish checkpoint, workspace head, operation, and lease")
    );
    let payload_rewrite = sqlx::query(
        "UPDATE moa.sandbox_workspace_checkpoints SET manifest_digest = 'sha256:tampered' WHERE checkpoint_id = $1",
    )
    .bind(checkpoint_id)
    .execute(&pool)
    .await;
    assert!(
        payload_rewrite.is_err(),
        "verified payload metadata is immutable"
    );

    sqlx::query(
        "UPDATE moa.sandbox_workspaces SET current_checkpoint_id = NULL, current_checkpoint_generation = 0 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await
    .expect("detach checkpoint head for fixture cleanup");

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace, hand, and checkpoint capacity reservations");
    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean hand lease");
    sqlx::query("DELETE FROM moa.sandbox_workspace_checkpoints WHERE checkpoint_id = $1")
        .bind(checkpoint_id)
        .execute(&pool)
        .await
        .expect("clean checkpoint");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean session");
    pool.close().await;
}
