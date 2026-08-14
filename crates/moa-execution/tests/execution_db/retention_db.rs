//! Terminal execution-detail retention contracts.

use sqlx::Row;

use super::support::*;
use moa_execution::repository::retention::{
    ExecutionRetentionClaimOutcome, ExecutionRetentionPageOutcome,
};
use moa_execution::repository::terminal::{RunTriggerDrainOutcome, RunTriggerDrainRequest};

#[tokio::test]
async fn retention_self_schedule_is_singleton_and_generation_fenced_db() -> TestResult {
    // Pins: a duplicate repair invocation cannot own an in-flight pass, and an
    // old delayed generation cannot supersede the one persisted by its owner.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let scope = ExecutionScope::ControlPlane;

    let ExecutionRetentionClaimOutcome::Claimed { generation, .. } =
        repository.claim_execution_retention(scope, None).await?
    else {
        panic!("first retention repair must claim the singleton generation");
    };
    assert_eq!(generation, 1);
    assert!(matches!(
        repository.claim_execution_retention(scope, None).await?,
        ExecutionRetentionClaimOutcome::NotDue { .. }
    ));
    let receipt = repository
        .schedule_execution_retention(scope, generation, 30, None)
        .await?;
    assert_eq!(receipt.scheduled_generation, 2);
    assert_eq!(
        repository
            .schedule_execution_retention(scope, generation, 30, None)
            .await?,
        receipt,
        "a retried journal step must replay its accepted schedule"
    );
    assert!(matches!(
        repository
            .claim_execution_retention(scope, Some(generation))
            .await?,
        ExecutionRetentionClaimOutcome::NotDue {
            scheduled_generation: Some(2),
            ..
        }
    ));
    sqlx::query(
        "UPDATE moa.execution_maintenance_checkpoint SET next_run_at = now() - interval '1 second' WHERE job_kind = 'execution_terminal_retention'",
    )
    .execute(&pool)
    .await?;
    let ExecutionRetentionClaimOutcome::Claimed { generation, .. } = repository
        .claim_execution_retention(scope, Some(receipt.scheduled_generation))
        .await?
    else {
        panic!("the exact due delayed generation must claim the next pass");
    };
    assert_eq!(generation, 2);
    assert!(
        repository
            .schedule_execution_retention(scope, 1, 30, None)
            .await
            .is_err(),
        "a claimed successor prevents an old generation from replacing its schedule"
    );
    Ok(())
}

#[tokio::test]
async fn terminal_retention_honors_legal_hold_then_archives_before_bounded_deletion_db()
-> TestResult {
    // Pins: a held terminal run remains untouched; after release, persisted keyset cursors
    // resume multi-page sources without duplicate/missing evidence across crash-style calls,
    // every immutable segment advances the rolling manifest root, and no bounded deletion starts
    // before that exact root receipt is bound to execution_run.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let tenant_scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        tenant_scope,
        new_run(
            tenant_id,
            None,
            "terminal-retention",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let _running =
        claim_running_controller(&repository, tenant_scope, &ExecutionConfig::default(), &run)
            .await?;
    let logical_tasks = ["one", "two", "three"]
        .into_iter()
        .map(|item| logical_task(run.run_uid, "retained-output", item, estimate(1)))
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(tenant_scope, run.run_uid, 1, logical_tasks.clone())
        .await?;
    for logical in logical_tasks {
        reserve_and_start(&repository, tenant_scope, run.run_uid, logical.task_id).await?;
        assert!(matches!(
            repository
                .record_task_outcome(tenant_scope, run.run_uid, logical.task_id, 1, completed(1),)
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
    }
    let predrain = repository
        .load_run(tenant_scope, run.run_uid)
        .await?
        .expect("retention fixture run remains visible");
    assert!(matches!(
        repository
            .claim_controller_wake(
                tenant_scope,
                predrain.run_uid,
                predrain.controller_generation,
                predrain.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_) | RunControllerClaimOutcome::Resumed(_)
    ));
    assert!(matches!(
        repository
            .drain_run_triggers_page(
                tenant_scope,
                &ExecutionConfig::default(),
                RunTriggerDrainRequest {
                    run_uid: predrain.run_uid,
                    controller_generation: predrain.controller_generation,
                    wake_epoch: predrain.wake_epoch,
                    page_limit: 1_000,
                    now: Utc::now(),
                },
            )
            .await?,
        RunTriggerDrainOutcome::ReadyToFinalize { .. }
    ));
    let before_finalization = repository
        .load_run(tenant_scope, run.run_uid)
        .await?
        .expect("drained retention fixture remains visible");
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Completed {
        output: json!({"retained": true}),
    };
    let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    assert!(matches!(
        repository
            .finalize_run(
                tenant_scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: before_finalization.plan_revision,
                    expected_wake_epoch: before_finalization.wake_epoch,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence: evidence,
                    terminal_reason,
                },
            )
            .await?,
        FinalizationOutcome::Finalized(_)
    ));
    sqlx::query(
        "UPDATE moa.execution_run SET completed_at = now() - interval '2 days' WHERE tenant_id = $1 AND run_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(run.run_uid)
    .execute(&pool)
    .await?;

    sqlx::query(
        "INSERT INTO moa.legal_hold (tenant_id, subject_id, reason, placed_by) VALUES ($1, NULL, 'execution retention test', 'retention-test')",
    )
    .bind(tenant_id.0)
    .execute(&pool)
    .await?;
    assert_eq!(
        repository
            .advance_execution_retention_page(ExecutionScope::ControlPlane, 1, 1)
            .await?,
        ExecutionRetentionPageOutcome::Idle
    );
    let archive_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_terminal_archive WHERE tenant_id = $1 AND run_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        archive_count, 0,
        "a legal hold must prevent even manifest creation"
    );

    sqlx::query(
        "UPDATE moa.legal_hold SET released_at = now(), released_by = 'retention-test' WHERE tenant_id = $1 AND released_at IS NULL",
    )
    .bind(tenant_id.0)
    .execute(&pool)
    .await?;
    let mut saw_segment = false;
    let mut saw_finalization = false;
    let mut saw_deletion = false;
    let mut completed = false;
    for _ in 0..256 {
        match repository
            .advance_execution_retention_page(ExecutionScope::ControlPlane, 1, 1)
            .await?
        {
            ExecutionRetentionPageOutcome::SegmentArchived { .. } => saw_segment = true,
            ExecutionRetentionPageOutcome::ArchiveFinalized { .. } => saw_finalization = true,
            ExecutionRetentionPageOutcome::DetailDeleted { .. } => saw_deletion = true,
            ExecutionRetentionPageOutcome::Complete { run_uid } => {
                assert_eq!(run_uid, run.run_uid);
                completed = true;
                break;
            }
            ExecutionRetentionPageOutcome::Idle => {
                panic!("eligible retention work became idle before completion")
            }
        }
    }
    assert!(saw_segment && saw_finalization && saw_deletion && completed);

    let run_receipt: (Option<Uuid>, Option<String>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT terminal_archive_uid, terminal_archive_hash, terminal_details_archived_at FROM moa.execution_run WHERE tenant_id = $1 AND run_uid = $2",
        )
        .bind(tenant_id.0)
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
    let archive_uid = run_receipt
        .0
        .expect("run retains the archive receipt identity");
    let run_root = run_receipt.1.expect("run retains the archive root digest");
    assert!(run_receipt.2.is_some());

    let segments = sqlx::query(
        "SELECT segment_kind, segment_sequence, record_count, payload, content_digest FROM moa.execution_terminal_archive_segment WHERE archive_uid = $1 ORDER BY segment_sequence",
    )
    .bind(archive_uid)
    .fetch_all(&pool)
    .await?;
    assert!(!segments.is_empty());
    let mut rolling: Option<String> = None;
    let mut expected_records = 0_i64;
    let mut expected_bytes = 0_i64;
    let mut expected_sequence = 1_i64;
    let mut task_segment_count = 0_u32;
    for segment in segments {
        let kind: String = segment.try_get("segment_kind")?;
        let sequence: i64 = segment.try_get("segment_sequence")?;
        let records: i64 = segment.try_get("record_count")?;
        let payload: Vec<u8> = segment.try_get("payload")?;
        let stored_digest: Vec<u8> = segment.try_get("content_digest")?;
        let computed_digest = blake3::hash(&payload);
        assert_eq!(stored_digest.as_slice(), computed_digest.as_bytes());
        assert_eq!(sequence, expected_sequence);
        expected_sequence += 1;
        expected_records += records;
        expected_bytes += i64::try_from(payload.len())?;
        if kind == "execution_task" {
            task_segment_count += 1;
        }
        let mut chain = blake3::Hasher::new();
        chain.update(b"moa.execution-terminal-archive.chain.v1\0");
        match rolling.as_deref() {
            Some(previous) => chain.update(previous.as_bytes()),
            None => chain.update(b"genesis"),
        };
        chain.update(&(kind.len() as u64).to_be_bytes());
        chain.update(kind.as_bytes());
        chain.update(&sequence.to_be_bytes());
        chain.update(&records.to_be_bytes());
        chain.update(&i64::try_from(payload.len())?.to_be_bytes());
        chain.update(&stored_digest);
        rolling = Some(chain.finalize().to_hex().to_string());
    }
    assert_eq!(rolling.as_deref(), Some(run_root.as_str()));
    assert_eq!(
        task_segment_count, 3,
        "page_size=1 must archive a three-record source across three durable keyset pages"
    );
    let manifest_progress: (i64, i64, i64, serde_json::Value, Option<String>) = sqlx::query_as(
        "SELECT source_record_count, source_logical_bytes, segment_count, source_cursor, \
                rolling_chain_digest FROM moa.execution_terminal_archive WHERE archive_uid = $1",
    )
    .bind(archive_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(manifest_progress.0, expected_records);
    assert_eq!(manifest_progress.1, expected_bytes);
    assert_eq!(manifest_progress.2, expected_sequence - 1);
    assert_eq!(manifest_progress.3["kind"], "complete");
    assert_eq!(manifest_progress.4.as_deref(), Some(run_root.as_str()));

    assert!(
        sqlx::query(
            "UPDATE moa.execution_terminal_archive_segment SET payload = payload || '\\x00'::BYTEA \
             WHERE archive_uid = $1 AND segment_sequence = 1",
        )
        .bind(archive_uid)
        .execute(&pool)
        .await
        .is_err(),
        "immutable segment evidence must reject digest tampering"
    );

    let live_task_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_task WHERE tenant_id = $1 AND run_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(live_task_count, 0, "archived task detail must be removed");
    let details_deleted_at: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
        "SELECT details_deleted_at FROM moa.execution_terminal_archive WHERE archive_uid = $1",
    )
    .bind(archive_uid)
    .fetch_one(&pool)
    .await?;
    assert!(details_deleted_at.is_some());
    Ok(())
}
