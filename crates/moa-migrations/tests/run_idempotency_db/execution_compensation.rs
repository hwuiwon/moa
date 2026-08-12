//! Durable execution-compensation migration contracts.

use serde_json::{Value, json};

use super::support::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn execution_compensation_schema_and_transitions_are_strict_db() {
    // Pins: a fresh migration installs the durable compensation state machine,
    // its action-review ownership hard break, and its pending-terminal contract.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create execution-compensation migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        let first = run_reporting_applied_serialized(database.target_url()).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let seeded = seed_execution_run(&target).await?;

        let compensation_schema: bool = sqlx::query_scalar(
            "SELECT to_regclass('moa.execution_compensation') IS NOT NULL \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_compensation' \
                              AND column_name = 'started_at') \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_compensation' \
                              AND column_name = 'completed_at') \
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_trigger \
                            WHERE tgrelid = 'moa.execution_compensation'::REGCLASS \
                              AND tgname = 'execution_compensation_update_guard')",
        )
        .fetch_one(&target)
        .await?;
        let action_review_owner_schema: bool = sqlx::query_scalar(
            "SELECT \
                EXISTS (SELECT 1 FROM information_schema.columns \
                        WHERE table_schema = 'moa' \
                          AND table_name = 'execution_action_review_outbox' \
                          AND column_name = 'operation_id') \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_action_review_outbox' \
                              AND column_name = 'owner_kind') \
                AND NOT EXISTS (SELECT 1 FROM information_schema.columns \
                                WHERE table_schema = 'moa' \
                                  AND table_name = 'execution_action_review_outbox' \
                                  AND column_name = 'task_id')",
        )
        .fetch_one(&target)
        .await?;
        let version_neutral_shapes: (bool, bool, bool) = sqlx::query_as(
            "SELECT moa.execution_plan_definition_is_valid(initial_plan -> 'definition'), \
                    moa.execution_plan_definition_is_valid( \
                        (initial_plan -> 'definition') \
                            || '{\"schema_version\":2}'::JSONB \
                    ), \
                    NOT capability_catalog ? 'schema_version' \
             FROM moa.execution_run WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .fetch_one(&target)
        .await?;
        let compensation_reason: Option<String> = sqlx::query_scalar(
            "SELECT moa.execution_terminal_reason_for( \
                'failed', $1::JSONB, 'generated_plan' \
             )",
        )
        .bind(json!({
            "kind": "compensation_failure",
            "original_status": "cancelled",
            "original_reason": "cancelled",
            "original_cause": {"kind": "cancellation"},
            "compensation_id": uuid::Uuid::new_v4(),
            "outcome": {
                "kind": "unknown_outcome",
                "message": "upstream committed before disconnect",
                "usage": usage(1)
            }
        }))
        .fetch_one(&target)
        .await?;

        exercise_compensation_transitions(&target, seeded.run_uid, seeded.tenant_id).await?;
        assert!(
            accepts_unconfirmed_pending_terminal(&target, seeded.run_uid).await?,
            "an unconfirmed run must accept a validated pending terminal intent"
        );
        target.close().await;
        let second = run_reporting_applied_serialized(database.target_url()).await?;

        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            compensation_schema,
            action_review_owner_schema,
            version_neutral_shapes,
            compensation_reason,
        ))
    }
    .await;

    let (
        first,
        second,
        compensation_schema,
        action_review_owner_schema,
        version_neutral_shapes,
        compensation_reason,
    ) = database
        .finish(outcome)
        .await
        .expect("fresh execution-compensation migration should remain strict and replayable");
    assert_eq!(first, expected_migration_labels());
    assert!(
        second.is_empty(),
        "exact migration replay must apply no SQL"
    );
    assert!(compensation_schema);
    assert!(action_review_owner_schema);
    assert_eq!(version_neutral_shapes, (true, false, true));
    assert_eq!(compensation_reason.as_deref(), Some("compensation_failed"));
}

struct SeededExecutionRun {
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
}

async fn seed_execution_run(target: &PgPool) -> TestResult<SeededExecutionRun> {
    let tenant_id = uuid::Uuid::new_v4();
    let session_id = uuid::Uuid::new_v4();
    let planning_context_uid = uuid::Uuid::new_v4();
    let run_uid = uuid::Uuid::new_v4();
    let plan_hash = "1".repeat(64);
    let plan = json!({
        "definition": {
            "cancel_policy": "retain_effects",
            "input_schema": {},
            "output_schema": {},
            "input_wait_policy": {
                "expiry": {"kind": "after", "delay_seconds": 1},
                "on_expiry": {"kind": "fail_run"}
            },
            "nodes": [{
                "id": "output",
                "requirement_ids": [],
                "depends_on": [],
                "when": null,
                "input": {},
                "output_schema": {},
                "operation": {"kind": "output", "value": {}},
                "compensation": null,
                "retry": {
                    "max_attempts": 1,
                    "initial_backoff_ms": 1,
                    "max_backoff_ms": 1
                },
                "budget": null
            }]
        },
        "plan_hash": plan_hash,
        "catalog_hash": "0".repeat(64),
        "estimate": {
            "cost_microusd": 0,
            "tokens": 0,
            "tool_calls": 0,
            "retrieved_bytes": 0,
            "tasks": 1
        },
        "report": {"issues": []}
    });
    sqlx::query(
        "INSERT INTO moa.execution_planning_context ( \
            planning_context_uid, tenant_id, session_id, \
            originating_user_sequence_num, originating_user_event_hash, \
            owner_user_id, planning_context_hash, snapshot \
         ) VALUES ($1, $2, $3, 0, $4, 'migration-test', $4, '{}'::JSONB)",
    )
    .bind(planning_context_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind("2".repeat(64))
    .execute(target)
    .await?;
    sqlx::query(
        "INSERT INTO moa.execution_run ( \
            run_uid, tenant_id, session_id, originating_user_sequence_num, \
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract, \
            initial_plan, active_plan, initial_plan_hash, active_plan_hash, \
            capability_catalog, authorization_envelope, source_provenance, source_kind, \
            input, admitted_identity, status \
         ) VALUES ( \
            $1, $2, $3, 0, $4, $5, 'migration-test', $6, $7, $7, $8, $8, \
            $9, $10, $11, 'generated_plan', '{}'::JSONB, $12, 'awaiting_confirmation' \
         )",
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind("2".repeat(64))
    .bind(json!({
        "objective": "migration",
        "requirements": [],
        "deliverables": [],
        "coverage": [],
        "constraints": [],
        "completion_checks": []
    }))
    .bind(&plan)
    .bind(&plan_hash)
    .bind(json!({
        "capabilities": [],
        "catalog_hash": "0".repeat(64)
    }))
    .bind(json!({"capability_refs": [], "skill_refs": []}))
    .bind(json!({
        "kind": "generated_plan",
        "planner": {
            "model": "migration-test",
            "prompt_version": "planner",
            "candidate_hash": "3".repeat(64),
            "compiler_report_hash": "4".repeat(64),
            "final_plan_hash": plan_hash,
            "repair_attempts": 0
        }
    }))
    .bind(json!({
        "identity_type": "operator",
        "id": run_uid,
        "tenant_id": tenant_id,
        "api_key_id": null,
        "acting_on_behalf_of": null
    }))
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'queued', confirmed_plan_hash = initial_plan_hash, \
             confirmed_at = NOW(), queued_at = NOW() \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(target)
    .await?;
    Ok(SeededExecutionRun { run_uid, tenant_id })
}

async fn accepts_unconfirmed_pending_terminal(
    target: &PgPool,
    run_uid: uuid::Uuid,
) -> TestResult<bool> {
    let mut tx = target.begin().await?;
    sqlx::raw_sql("ALTER TABLE moa.execution_run DISABLE TRIGGER execution_run_update_guard;")
        .execute(&mut *tx)
        .await?;
    let updated = sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'awaiting_confirmation', queued_at = NULL, \
             confirmed_plan_hash = NULL, confirmed_at = NULL, \
             terminal_reason = NULL, terminal_cause = NULL, \
             terminal_satisfied_requirement_count = NULL, \
             terminal_requirement_count = NULL, \
             pending_terminal_status = 'cancelled', \
             pending_terminal_reason = 'cancelled', \
             pending_terminal_cause = '{ \
                 \"terminal_evidence\": { \
                     \"cause\": {\"kind\":\"cancellation\"}, \
                     \"satisfied_requirement_count\": 0, \
                     \"requirement_count\": 0 \
                 }, \
                 \"completion_check_results\": [], \
                 \"terminal_gaps\": [] \
             }'::JSONB, \
             pending_terminal_output = NULL, \
             cancellation_reason = 'migration cancellation fixture' \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::raw_sql("ALTER TABLE moa.execution_run ENABLE TRIGGER execution_run_update_guard;")
        .execute(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(updated == 1)
}

async fn exercise_compensation_transitions(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
) -> TestResult {
    let forward_task_id = uuid::Uuid::new_v4();
    let compensation_id = uuid::Uuid::new_v4();
    insert_compensation_fixture(
        target,
        run_uid,
        tenant_id,
        forward_task_id,
        compensation_id,
        1,
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'running', updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .execute(target)
    .await?;
    let retry_outcome = json!({
        "result": {
            "kind": "failed",
            "message": "temporary rollback failure",
            "retryable": true,
            "usage": usage(1)
        },
        "review_audit": []
    });
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'pending', attempt = 2, generation = 2, \
             outcome = $2, error = $3, updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .bind(json!({"class":"retryable", "message":"temporary rollback failure"}))
    .execute(target)
    .await?;
    let retry: (String, i64, i64, bool) = sqlx::query_as(
        "SELECT status, attempt, generation, \
                outcome = $2::JSONB AND started_at IS NOT NULL AND completed_at IS NULL \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .fetch_one(target)
    .await?;
    assert_eq!(retry, ("pending".to_string(), 2, 2, true));
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'running', updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .execute(target)
    .await?;
    let reclaim_kept_evidence: bool = sqlx::query_scalar(
        "SELECT outcome = $2::JSONB AND started_at IS NOT NULL AND completed_at IS NULL \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .fetch_one(target)
    .await?;
    assert!(reclaim_kept_evidence);
    let completed_outcome = json!({
        "result": {
            "kind": "completed",
            "output": {"undone": true},
            "usage": usage(2)
        },
        "review_audit": [review_audit_entry(2, true)]
    });
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'completed', outcome = $2, error = NULL, updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&completed_outcome)
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET outcome = jsonb_set( \
                 outcome, '{review_audit}', \
                 (outcome -> 'review_audit') || jsonb_build_array($2::JSONB) \
             ), \
             updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(review_audit_entry(2, false))
    .execute(target)
    .await?;
    let terminal_audit_count: i32 = sqlx::query_scalar(
        "SELECT jsonb_array_length(outcome -> 'review_audit') \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .fetch_one(target)
    .await?;
    assert_eq!(terminal_audit_count, 2);

    let budget_task_id = uuid::Uuid::new_v4();
    let budget_compensation_id = uuid::Uuid::new_v4();
    insert_compensation_fixture(
        target,
        run_uid,
        tenant_id,
        budget_task_id,
        budget_compensation_id,
        2,
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'failed', outcome = $2, error = $3, \
             completed_at = NOW(), updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(budget_compensation_id)
    .bind(json!({
        "result": {
            "kind": "failed",
            "message": "approved execution budget cannot reserve compensation",
            "retryable": false,
            "usage": usage(0)
        },
        "review_audit": []
    }))
    .bind(json!({
        "class": "budget_exceeded",
        "message": "approved execution budget cannot reserve compensation"
    }))
    .execute(target)
    .await?;
    let budget_timestamps_are_atomic: bool = sqlx::query_scalar(
        "SELECT started_at IS NOT NULL AND started_at = completed_at \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(budget_compensation_id)
    .fetch_one(target)
    .await?;
    assert!(budget_timestamps_are_atomic);

    let mapping_task_id = uuid::Uuid::new_v4();
    let mapping_compensation_id = uuid::Uuid::new_v4();
    insert_forward_task_fixture(target, run_uid, tenant_id, mapping_task_id, 3).await?;
    sqlx::query(
        "INSERT INTO moa.execution_compensation ( \
            compensation_id, run_uid, forward_task_id, tenant_id, \
            registered_sequence, forward_generation, compensator, mapped_input, \
            status, outcome, error, started_at, completed_at \
         ) VALUES ( \
            $1, $2, $3, $4, 3, 1, $5, 'null'::JSONB, 'failed', $6, $7, \
            statement_timestamp(), statement_timestamp() \
         )",
    )
    .bind(mapping_compensation_id)
    .bind(run_uid)
    .bind(mapping_task_id)
    .bind(tenant_id)
    .bind(compensator_fixture())
    .bind(json!({
        "result": {
            "kind": "failed",
            "message": "compensation input mapping failed",
            "retryable": false,
            "usage": usage(0)
        },
        "review_audit": []
    }))
    .bind(json!({
        "class": "mapping_input_invalid",
        "message": "compensation input mapping failed"
    }))
    .execute(target)
    .await?;
    let mapping_failure_is_terminal: bool = sqlx::query_scalar(
        "SELECT status = 'failed' AND mapped_input = 'null'::JSONB \
                AND started_at IS NOT NULL AND started_at = completed_at \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(mapping_compensation_id)
    .fetch_one(target)
    .await?;
    assert!(mapping_failure_is_terminal);
    assert!(exercise_revoked_action_review_status(target, tenant_id).await?);
    Ok(())
}

async fn exercise_revoked_action_review_status(
    target: &PgPool,
    tenant_id: uuid::Uuid,
) -> TestResult<bool> {
    let review_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO public.tenant_action_reviews ( \
            id, tenant_id, storage_partition_id, tool_call_id, tool_name, \
            action_class, risk_level, input_summary, normalized_input, envelope, \
            preview, tool_request, requested_by \
         ) VALUES ( \
            $1, $2, $3, $4, 'migration.review', 'write', 'high', \
            'migration review', '{}', '{}', '{}', '{}', 'migration-test' \
         )",
    )
    .bind(review_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(uuid::Uuid::new_v4())
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE public.tenant_action_reviews \
         SET status = 'revoked', decided_at = NOW(), \
             deny_reason = 'execution terminal fence revoked pending review' \
         WHERE id = $1",
    )
    .bind(review_id)
    .execute(target)
    .await?;
    let late_clear =
        sqlx::query("UPDATE public.tenant_action_reviews SET status = 'cleared' WHERE id = $1")
            .bind(review_id)
            .execute(target)
            .await
            .expect_err("revoked action review must reject a late clear");
    let status: String =
        sqlx::query_scalar("SELECT status FROM public.tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(target)
            .await?;
    Ok(status == "revoked"
        && late_clear
            .to_string()
            .contains("invalid tenant action review"))
}

async fn insert_compensation_fixture(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
    forward_task_id: uuid::Uuid,
    compensation_id: uuid::Uuid,
    sequence: i64,
) -> TestResult {
    insert_forward_task_fixture(target, run_uid, tenant_id, forward_task_id, sequence).await?;
    sqlx::query(
        "INSERT INTO moa.execution_compensation ( \
            compensation_id, run_uid, forward_task_id, tenant_id, \
            registered_sequence, forward_generation, compensator, mapped_input \
         ) VALUES ($1, $2, $3, $4, $5, 1, $6, '{}'::JSONB)",
    )
    .bind(compensation_id)
    .bind(run_uid)
    .bind(forward_task_id)
    .bind(tenant_id)
    .bind(sequence)
    .bind(compensator_fixture())
    .execute(target)
    .await?;
    Ok(())
}

async fn insert_forward_task_fixture(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
    forward_task_id: uuid::Uuid,
    sequence: i64,
) -> TestResult {
    sqlx::query(
        "INSERT INTO moa.execution_task ( \
            task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, \
            input, task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, \
            estimate_tasks, estimate_tool_calls, estimate_retrieved_bytes \
         ) VALUES ( \
            $1, $2, $3, $4, $4, 1, 'completed', '{}', \
            '{\"kind\":\"output\",\"value\":null}', \
            '{\"max_attempts\":2,\"initial_backoff_ms\":1,\"max_backoff_ms\":1}', \
            0, 0, 1, 0, 0 \
         )",
    )
    .bind(forward_task_id)
    .bind(run_uid)
    .bind(tenant_id)
    .bind(format!("compensation-{sequence}"))
    .execute(target)
    .await?;
    Ok(())
}

fn compensator_fixture() -> Value {
    json!({
        "compensator": {"name": "test.undo", "version": "contract"},
        "input_mapping": {"bindings": []}
    })
}

fn usage(tool_calls: u64) -> Value {
    json!({
        "cost_microusd": 0,
        "tokens": 0,
        "tool_calls": tool_calls,
        "retrieved_bytes": 0
    })
}

fn review_audit_entry(generation: u64, accepted: bool) -> Value {
    json!({
        "review_uid": uuid::Uuid::new_v4(),
        "generation": generation,
        "accepted": accepted,
        "resolution": {"kind": "approved"},
        "recorded_at": "2026-08-04T00:00:00Z"
    })
}
