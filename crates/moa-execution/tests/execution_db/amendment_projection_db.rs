//! Bounded persisted amendment-evidence contracts.

use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_execution::repository::{
    amendment::{AmendmentProjectionOutcome, AmendmentProjectionRequest},
    ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest},
};

use super::support::*;

fn output_node() -> ExecutionNode {
    ExecutionNode {
        id: "output".to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        budget: None,
    }
}

#[tokio::test]
async fn amendment_projection_counts_twenty_five_hundred_prior_failures_in_one_bounded_call_db()
-> TestResult {
    // Pins: amendment validation sees one exact WaitingReplan origin and one indexed scalar count;
    // it never reloads or pages across the full 2,501-task history.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "bounded-amendment-2501",
        ExecutionRunStatus::Queued,
        budget(5_000),
    );
    candidate.plan.definition.nodes = vec![output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    let config = ExecutionConfig::default();

    let mut all_tasks = (0_u64..2_501)
        .map(|index| logical_task(run.run_uid, "output", &format!("{index:04}"), estimate(1)))
        .collect::<Vec<_>>();
    let replan_task = logical_task(run.run_uid, "output", "replan", estimate(1));
    let replan_task_id = replan_task.task_id;
    all_tasks.push(replan_task);
    let mut cursor = 0_u64;
    for (page_index, page) in all_tasks.chunks(1_000).enumerate() {
        let ReadyMaterializationOutcome::Applied { next_cursor, .. } = repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    node_id: "output".to_string(),
                    expected_cursor: cursor,
                    reduce_cursor: None,
                    source_exhausted: page_index == 2,
                    terminal_output: None,
                    tasks: page.to_vec(),
                },
            )
            .await?
        else {
            panic!("fresh amendment setup page must apply");
        };
        cursor = next_cursor;
    }

    for (status, attempt_state) in [("dispatching", "dispatching"), ("running", "running")] {
        sqlx::query(
            "UPDATE moa.execution_task SET status=$2,attempt_state=$3,updated_at=NOW() \
             WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .bind(status)
        .bind(attempt_state)
        .execute(&pool)
        .await?;
    }
    let repeated_failure = ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(0),
        result: ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Terminal,
            message: "source unavailable".to_string(),
        },
    };
    let repeated_fingerprint = failure_fingerprint(&FailureFingerprintInput {
        class: ExecutionFailureClass::Terminal,
        node_id: "output".to_string(),
        capability_ref: None,
        message: "source unavailable".to_string(),
    })?;
    sqlx::query(
        "UPDATE moa.execution_task SET status='failed',attempt_state='terminal',current_outcome=$2, \
             failure_fingerprint=$3,completed_at=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND task_id<>$4",
    )
    .bind(run.run_uid)
    .bind(serde_json::to_value(repeated_failure)?)
    .bind(repeated_fingerprint.to_string())
    .bind(replan_task_id.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE moa.execution_run SET status='running',updated_at=NOW() WHERE run_uid=$1")
        .bind(run.run_uid)
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE moa.execution_task SET status='waiting_replan',attempt_state='waiting', \
             current_outcome=$2,failure_fingerprint=$3,waiting_since=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND task_id=$4",
    )
    .bind(run.run_uid)
    .bind(serde_json::to_value(needs_replan(1))?)
    .bind(repeated_fingerprint.to_string())
    .bind(replan_task_id.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='waiting',ready_task_count=0, \
             waiting_task_count=1,terminal_task_count=total_task_count-1,failed_task_count=total_task_count-1, \
             updated_at=NOW() WHERE run_uid=$1 AND node_id='output'",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status='waiting_replan',ready_task_count=0,active_task_count=0, \
             waiting_task_count=1,waiting_replan_task_count=1,waiting_reasons_truncated=TRUE, \
             waiting_since=NOW(),updated_at=NOW() \
         WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;

    let request = AmendmentProjectionRequest {
        run_uid: run.run_uid,
        session_id: run.session_id,
        expected_plan_revision: run.plan_revision,
    };
    let AmendmentProjectionOutcome::Ready(snapshot) = repository
        .load_amendment_projection_for_session(scope, &config, request)
        .await?
    else {
        panic!("current bounded amendment projection must be ready in one call");
    };
    assert_eq!(snapshot.projection.replan_tasks.len(), 1);
    assert_eq!(snapshot.projection.replan_tasks[0].task_id, replan_task_id);
    assert_eq!(snapshot.projection.node_statuses.len(), 1);
    assert!(snapshot.projection.started_node_ids.contains("output"));
    assert_eq!(
        snapshot
            .prior_failure_fingerprint_counts
            .get(&repeated_fingerprint),
        Some(&2_501)
    );
    Ok(())
}
