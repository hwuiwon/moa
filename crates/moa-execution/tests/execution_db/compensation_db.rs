//! Compensation registration, reverse-order settlement, and replay persistence contracts.

use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, ExecutionCancelPolicy, ExecutionCompensation,
};
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    tools::IdempotencyClass,
};
use moa_execution::{
    capability::{
        CapabilityPolicyContext, CapabilityRollbackContract, CapabilitySource, ExecutionCapability,
        ExecutionClass,
    },
    state::{CompensationId, ExecutionCompensationOutcome},
};

use super::support::*;

#[tokio::test]
async fn concurrent_forward_commits_register_one_unique_monotonic_sequence_each_db() -> TestResult {
    // Pins: forward effects may finish concurrently, but the run-row lock makes
    // their atomic compensation registrations unique and gap-free.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "concurrent-compensation-registration",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let tasks = ["concurrent_a", "concurrent_b", "concurrent_c"].map(|node_id| {
        compensated_task(
            run.run_uid,
            node_id,
            forward_reference.clone(),
            compensation.clone(),
        )
    });
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.to_vec())
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    }
    let (first, second, third) = tokio::join!(
        repository.record_task_outcome(scope, run.run_uid, tasks[0].task_id, 1, completed(1)),
        repository.record_task_outcome(scope, run.run_uid, tasks[1].task_id, 1, completed(1)),
        repository.record_task_outcome(scope, run.run_uid, tasks[2].task_id, 1, completed(1)),
    );
    for outcome in [first?, second?, third?] {
        assert!(matches!(outcome, TaskOutcomeWrite::Applied { .. }));
    }
    let registrations: Vec<(i64, Uuid)> = sqlx::query_as(
        "SELECT registered_sequence,forward_task_id FROM moa.execution_compensation \
         WHERE run_uid=$1 ORDER BY registered_sequence",
    )
    .bind(run.run_uid)
    .fetch_all(test_db.store().pool())
    .await?;
    let mut sequences = registrations
        .iter()
        .map(|(sequence, _)| u64::try_from(*sequence).expect("fixture sequence fits u64"))
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    assert_eq!(sequences, vec![1, 2, 3]);
    let mut owners = registrations
        .iter()
        .map(|(_, forward_task_id)| *forward_task_id)
        .collect::<Vec<_>>();
    owners.sort_unstable();
    owners.dedup();
    assert_eq!(
        owners.len(),
        3,
        "each committed effect must own one registration"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_compensation_mapping_registers_failed_without_rolling_back_forward_commit_db()
-> TestResult {
    // Pins: a committed forward effect is never erased when its durable
    // compensation mapping cannot resolve; the same transaction installs a
    // deterministic failed registration and a manual-repair fence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new = new_run(
        tenant_id,
        None,
        "invalid-compensation-registration",
        ExecutionRunStatus::Queued,
        budget(5),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    let run = create_run(&repository, scope, new).await?;
    let forward_reference = capability_reference("effects.commit");
    let missing_compensator = capability_reference("effects.missing_undo");
    let compensation = ExecutionCompensation {
        compensator: missing_compensator,
        input_mapping: token_mapping(),
    };
    let task = compensated_task(
        run.run_uid,
        "invalid-mapping",
        forward_reference,
        compensation.clone(),
    );
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let TaskOutcomeWrite::Applied {
        run: committed_run,
        task: committed_task,
        ..
    } = repository
        .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
        .await?
    else {
        panic!("forward completion must commit despite invalid compensation mapping");
    };
    assert_eq!(committed_task.status, ExecutionTaskStatus::Completed);
    assert_eq!(committed_task.output, Some(json!({"tokens": 1})));
    assert!(committed_run.manual_repair_required);
    assert_eq!(committed_run.next_compensation_sequence, 2);

    let registration: serde_json::Value = sqlx::query_scalar(
        "SELECT to_jsonb(compensation) - 'updated_at' \
         FROM moa.execution_compensation AS compensation WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    let expected_message = "invalid execution repository data: persisted compensation contract has no pinned compensator";
    assert_eq!(
        registration["compensation_id"],
        json!(CompensationId::derive(task.task_id))
    );
    assert_eq!(registration["forward_task_id"], json!(task.task_id));
    assert_eq!(registration["registered_sequence"], json!(1));
    assert_eq!(registration["forward_generation"], json!(1));
    assert_eq!(registration["compensator"], json!(compensation));
    assert_eq!(registration["mapped_input"], serde_json::Value::Null);
    assert_eq!(registration["status"], json!("failed"));
    assert_eq!(
        registration["outcome"]["result"],
        json!(ExecutionCompensationOutcome::Failed {
            message: expected_message.to_string(),
            retryable: false,
            usage: usage(0),
        })
    );
    assert_eq!(
        registration["error"],
        json!({
            "class": "mapping_input_invalid",
            "message": expected_message,
        })
    );
    assert!(!registration["started_at"].is_null());
    assert!(!registration["completed_at"].is_null());

    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
            .await?,
        TaskOutcomeWrite::Replayed { .. }
    ));
    let replayed_registration: serde_json::Value = sqlx::query_scalar(
        "SELECT to_jsonb(compensation) - 'updated_at' \
         FROM moa.execution_compensation AS compensation WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(replayed_registration, registration);
    Ok(())
}

fn compensated_catalog() -> (
    ExecutionCapabilityCatalog,
    CapabilityReference,
    ExecutionCompensation,
) {
    let mut forward = capability("effects.commit");
    let compensator = capability("effects.undo");
    let compensation = ExecutionCompensation {
        compensator: compensator.reference.clone(),
        input_mapping: token_mapping(),
    };
    forward.rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    let forward_reference = forward.reference.clone();
    let catalog = ExecutionCapabilityCatalog::build(vec![forward, compensator])
        .expect("compensated test catalog must be valid");
    (catalog, forward_reference, compensation)
}

fn capability(name: &str) -> ExecutionCapability {
    let source = CapabilitySource::BuiltInTool {
        name: name.to_string(),
    };
    ExecutionCapability {
        reference: capability_reference(name),
        contract_revision: "contract-v1".to_string(),
        description: format!("test capability {name}"),
        input_schema: json!({
            "type": "object",
            "required": ["tokens"],
            "properties": {"tokens": {"type": "integer", "minimum": 0}},
            "additionalProperties": false,
        }),
        output_schema: json!({
            "type": "object",
            "required": ["tokens"],
            "properties": {"tokens": {"type": "integer", "minimum": 0}},
            "additionalProperties": false,
        }),
        action_class: ActionClass::ExternalWrite,
        risk_level: RiskLevel::Medium,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        async_mode: moa_core::types::tools::ToolAsyncMode::SynchronousOnly,
        execution_class: ExecutionClass::External,
        requires_sandbox: false,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: estimate(1),
        rollback: None,
    }
}

fn capability_reference(name: &str) -> CapabilityReference {
    CapabilityReference {
        name: name.to_string(),
        version: "v1".to_string(),
    }
}

fn token_mapping() -> CompensationInputMapping {
    CompensationInputMapping {
        bindings: vec![CompensationInputBinding {
            target_pointer: "/tokens".to_string(),
            source: CompensationValueSource::OriginalOutput {
                pointer: "/tokens".to_string(),
            },
        }],
    }
}

fn compensated_task(
    run_uid: Uuid,
    node_id: &str,
    forward_reference: CapabilityReference,
    compensation: ExecutionCompensation,
) -> LogicalTask {
    let mut task = logical_task(run_uid, node_id, "", estimate(1));
    task.input = json!({"tokens": 1});
    task.kind = LogicalTaskKind::Capability {
        reference: forward_reference,
    };
    task.compensation = Some(compensation);
    task
}
