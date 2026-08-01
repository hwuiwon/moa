//! End-to-end action-policy and tenant-admin review coverage.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::{
    events::Event,
    types::action_policy::ActionReviewDecision,
    types::action_policy::ActionReviewOwner,
    types::action_policy::ActionReviewStatus,
    types::action_policy::ExecutionTaskOrigin,
    types::action_policy::{ActionClass, ActionEnvelope, ActionPolicyEffect, RiskLevel},
    types::completion::ToolInvocation,
    types::contact::SessionActorRef,
    types::events_stream::EventRange,
    types::events_stream::EventRecord,
    types::identifiers::SessionId,
    types::identifiers::TenantId,
    types::identifiers::ToolCallId,
    types::session::SessionStatus,
    types::tools::SecuredToolOutput,
    types::tools::ToolCallRequest,
};
use moa_hands::ToolCatalogPin;
use moa_orchestrator::objects::tenant::TenantConfig;
use moa_orchestrator::services::action_policy::{PrepareActionReviewRequest, PreparedActionReview};
use moa_orchestrator::services::action_reviews::{
    ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
    ListActionReviewsRequest, RequestActionReview,
};
use moa_orchestrator::services::tool_executor::ExecutionTaskToolCallRequest;
use moa_test_support::fixtures::fresh_client_message_id;
use moa_test_support::{IsolatedTest, OrchestratorTestFixture, TestApiClient};
use moa_wire::turn::{StartTurnRequest, TurnOutcomeKind};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn action_policy_flow_covers_auto_review_decision_and_member_authz() -> Result<()> {
    // Pins: action-policy E2E scenarios share one scripted fixture process.
    let fixture = OrchestratorTestFixture::with_script(action_policy_script()).await?;
    action_policy_auto_mode_executes_shell_without_user_approval(&fixture).await?;
    admin_review_policy_records_pending_review_and_turn_continues(&fixture).await?;
    tenant_admin_clear_executes_stored_review_action(&fixture).await?;
    tenant_admin_deny_does_not_execute_stored_review_action(&fixture).await?;
    tenant_member_cannot_decide_action_review(&fixture).await?;
    decision_waits_for_durable_owner_registration(&fixture).await?;
    coordinator_review_clear_resumes_its_owner_exactly_once(&fixture).await?;
    coordinator_review_deny_resumes_without_executing_the_action(&fixture).await?;
    superseded_coordinator_review_produces_no_continuation(&fixture).await?;
    pending_worker_review_holds_its_report_until_the_clear_continues_it(&fixture).await?;
    execution_task_tool_executor_emits_zero_root_tool_events(&fixture).await?;
    claimed_execution_review_exact_replay_resumes_and_conflict_rejects(&fixture).await?;
    Ok(())
}

async fn decision_waits_for_durable_owner_registration(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: the durable review row must not become admin-actionable before the
    // Session acknowledgement is persisted, even when the review id is known.
    let test = fixture.isolated().await;
    let session_id = test.create_session("review-owner-readiness").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture.grant_default_tenant_admin(meta.tenant_id).await?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;
    let owner = ActionReviewOwner::Coordinator {
        session_id,
        turn_id: "turn-owner-readiness".to_string(),
        generation: 0,
    };
    let (review_id, _) =
        create_pending_bash_review(&test, session_id, "printf readiness", owner).await?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to fixture Postgres")?;
    sqlx::query("UPDATE tenant_action_reviews SET owner_registered_at = NULL WHERE id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .context("simulate crash before owner registration acknowledgement")?;

    let early = decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Denied,
        None,
    )
    .await;
    assert!(
        early.is_err(),
        "decision must be rejected until owner registration is durable"
    );

    sqlx::query("UPDATE tenant_action_reviews SET owner_registered_at = NOW() WHERE id = $1")
        .bind(review_id)
        .execute(&pool)
        .await
        .context("restore owner readiness so the fixture can release its registration")?;
    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Denied,
        None,
    )
    .await?;
    Ok(())
}

async fn pending_worker_review_holds_its_report_until_the_clear_continues_it(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: a worker whose own action is awaiting a tenant-admin decision is not
    // finished. Its model loop returns, but it must not deliver its terminal report to
    // the parent while the review is open — otherwise the approved action's answer is
    // never folded into the result the coordinator already consumed. Clearing the
    // review runs exactly one worker continuation and only then releases the report.
    let test = fixture.isolated().await;
    let session_id = test.create_session("worker-review-continuation").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before deciding the worker review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    run_scripted_turn(
        &test,
        session_id,
        "Run the worker-continuation bash command.",
    )
    .await?;
    let events = wait_for_events(&test, session_id, |events| {
        worker_owned_review(events).is_some()
    })
    .await
    .context("the delegated worker's bash should request an admin review")?;
    let (review_id, worker_id) =
        worker_owned_review(&events).context("expected a worker-owned action review")?;

    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::WorkerNotificationDelivered { worker_id: delivered, .. }
                if *delivered == worker_id
        )),
        "a worker holding an unresolved review must not report as terminal: {}",
        event_summary(&events)
    );

    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await?;

    let resolved = wait_for_events(&test, session_id, |events| {
        events.iter().any(|record| {
            matches!(
                &record.event,
                Event::WorkerNotificationDelivered { worker_id: delivered, .. }
                    if *delivered == worker_id
            )
        })
    })
    .await
    .context("clearing the review should release the worker's held report")?;

    let facts = continuation_facts(&resolved, review_id);
    assert_eq!(
        facts.len(),
        1,
        "exactly one worker continuation per review: {}",
        event_summary(&resolved)
    );
    let (_continuation_turn_id, receipt) = facts[0];
    assert_eq!(
        receipt.owner.worker_id().map(String::as_str),
        Some(worker_id.as_str()),
        "the receipt resumes the worker that raised the review"
    );
    assert!(
        matches!(
            &receipt.outcome,
            moa_core::types::action_policy::ActionReviewOutcome::Cleared(
                moa_core::types::action_policy::ToolTerminalFact::Result(metadata)
            ) if metadata.success
        ),
        "unexpected worker receipt outcome: {:?}",
        receipt.outcome
    );
    assert!(
        resolved.iter().any(|record| matches!(
            &record.event,
            Event::ToolResult { output, success: true, provider_tool_use_id: None, .. }
                if output.to_text().contains("worker-continuation-ok")
        )),
        "the cleared worker action should execute with no reused provider id: {}",
        event_summary(&resolved)
    );
    Ok(())
}

/// Returns the first worker-owned review id and its worker key.
fn worker_owned_review(events: &[EventRecord]) -> Option<(Uuid, String)> {
    events.iter().find_map(|record| match &record.event {
        Event::ActionReviewRequested {
            review_id,
            envelope,
            ..
        } => envelope
            .owner
            .worker_id()
            .map(|worker_id| (*review_id, worker_id.clone())),
        _ => None,
    })
}

async fn coordinator_review_clear_resumes_its_owner_exactly_once(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: an approved action that lands after the nonblocking model loop already
    // ended still reaches the user. Clearing the review resumes the exact coordinator
    // that raised it, exactly once: one continuation fact naming a NEW turn, a typed
    // receipt whose executed tool id is fresh and whose ordered terminal facts prove
    // the callback waited for both the decision and the tool's terminal event, and a
    // visible assistant answer on that continuation turn.
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("coordinator-review-continuation")
        .await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before deciding the coordinator review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (origin_turn_id, _events) =
        run_scripted_turn_with_id(&test, session_id, "Start the reviewed work.").await?;
    let side_effect =
        tempfile::NamedTempFile::new().context("create action-review concurrency probe")?;
    let command = format!(
        "printf 'coordinator-continuation-ok\\n' | tee -a '{}'",
        side_effect.path().display()
    );
    let (review_id, requested_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        &command,
        coordinator_owner_for_turn(session_id, &origin_turn_id),
    )
    .await?;

    let (first_decision, replayed_decision) = tokio::join!(
        decide_review(
            test.client(),
            meta.tenant_id,
            review_id,
            ActionReviewDecisionKind::Cleared,
            None,
        ),
        decide_review(
            test.client(),
            meta.tenant_id,
            review_id,
            ActionReviewDecisionKind::Cleared,
            None,
        )
    );
    first_decision?;
    replayed_decision?;
    assert_eq!(
        std::fs::read_to_string(side_effect.path())
            .context("read action-review concurrency probe")?
            .lines()
            .count(),
        1,
        "concurrent replay must not execute the cleared side effect twice"
    );

    let events = wait_for_events(&test, session_id, |events| {
        !continuation_facts(events, review_id).is_empty()
    })
    .await
    .context("a cleared coordinator review should request its continuation")?;
    let facts = continuation_facts(&events, review_id);
    assert_eq!(
        facts.len(),
        1,
        "exactly one continuation per review: {}",
        event_summary(&events)
    );
    let (continuation_turn_id, receipt) = facts[0];
    assert_ne!(
        continuation_turn_id, &origin_turn_id,
        "the continuation runs as a new turn, not a revival of the origin turn"
    );
    assert_eq!(receipt.review_id, review_id);
    assert_eq!(receipt.tool_name, "bash");
    let executed = receipt
        .executed_tool_call_id
        .context("a cleared receipt names the tool call that actually ran")?;
    assert_ne!(
        executed, requested_tool_id,
        "the reviewed execution is a new MOA-owned invocation with a fresh id"
    );
    assert!(
        matches!(
            &receipt.outcome,
            moa_core::types::action_policy::ActionReviewOutcome::Cleared(
                moa_core::types::action_policy::ToolTerminalFact::Result(metadata)
            ) if metadata.success
        ),
        "unexpected receipt outcome: {:?}",
        receipt.outcome
    );
    assert_eq!(
        receipt.owner,
        coordinator_owner_for_turn(session_id, &origin_turn_id),
        "the receipt carries the exact owner that raised the review"
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::ToolResult { tool_id, provider_tool_use_id: None, success: true, .. }
                if *tool_id == executed
        )),
        "the executed tool must have a durable successful result with no provider id: {}",
        event_summary(&events)
    );

    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            continuation_turn_id,
            Duration::from_secs(90),
            Duration::from_millis(250),
        )
        .await
        .context("the dispatched continuation turn should reach a terminal outcome")?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let final_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert_eq!(
        continuation_facts(&final_events, review_id).len(),
        1,
        "replayed or repeated resolution must not append a second continuation: {}",
        event_summary(&final_events)
    );
    assert!(
        final_events.iter().all(|record| !matches!(
            &record.event,
            Event::UserMessage { text, .. } if text.contains("action_review_continuation")
        )),
        "the continuation is a system directive, never a fabricated user message: {}",
        event_summary(&final_events)
    );
    Ok(())
}

async fn coordinator_review_deny_resumes_without_executing_the_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: a denial is a real answer the user is owed. It resumes the coordinator with
    // a Denied receipt built from the decision alone — no executed tool id, no tool
    // terminal event — and the gated command still never runs.
    let test = fixture.isolated().await;
    let session_id = test.create_session("coordinator-review-denied").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before denying the coordinator review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (origin_turn_id, _events) =
        run_scripted_turn_with_id(&test, session_id, "Start the denied work.").await?;
    let (review_id, _requested_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        "printf denied-continuation-should-not-run",
        coordinator_owner_for_turn(session_id, &origin_turn_id),
    )
    .await?;

    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Denied,
        Some("not approved for production"),
    )
    .await?;

    let events = wait_for_events(&test, session_id, |events| {
        !continuation_facts(events, review_id).is_empty()
    })
    .await
    .context("a denied coordinator review should still resume its owner")?;
    let facts = continuation_facts(&events, review_id);
    assert_eq!(facts.len(), 1);
    let (_turn_id, receipt) = facts[0];
    assert_eq!(receipt.executed_tool_call_id, None);
    assert_eq!(
        receipt.outcome,
        moa_core::types::action_policy::ActionReviewOutcome::Denied
    );
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { output, success: true, .. }
                if output.to_text().contains("denied-continuation-should-not-run")
        )),
        "a denied action must never execute: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn superseded_coordinator_review_produces_no_continuation(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: an unresolved review never blocks a later user message, and once that newer
    // message is admitted the older review is stale. Its approval still executes the
    // approved action and records the decision, but it must NOT preempt the newer work
    // with a continuation turn.
    let test = fixture.isolated().await;
    let session_id = test.create_session("coordinator-review-superseded").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before deciding the superseded review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (origin_turn_id, _events) =
        run_scripted_turn_with_id(&test, session_id, "Start the superseded work.").await?;
    let (review_id, _requested_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        "printf superseded-continuation-ok",
        coordinator_owner_for_turn(session_id, &origin_turn_id),
    )
    .await?;

    // A later user message advances the session generation, stranding the review.
    run_scripted_turn(&test, session_id, "Actually, do this newer thing instead.").await?;

    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await?;

    let events = wait_for_events(&test, session_id, |events| {
        events.iter().any(|record| {
            matches!(
                &record.event,
                Event::ActionReviewDecided { review_id: id, .. } if *id == review_id
            )
        })
    })
    .await
    .context("the decision itself is still recorded for a superseded review")?;
    assert!(
        continuation_facts(&events, review_id).is_empty(),
        "a superseded review must not preempt the newer user turn: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn claimed_execution_review_exact_replay_resumes_and_conflict_rejects(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: after the clear claim commits, a crash before tool completion or
    // outbox creation can replay the identical claim and finish with the same
    // tool-call id; a different claimant cannot take over that journaled work.
    let test = fixture.isolated().await;
    let session_id = test.create_session("execution-review-claim-replay").await?;
    let session = test.client().get_session(session_id).await?;
    fixture
        .grant_default_tenant_admin(session.tenant_id)
        .await
        .context("grant admin before replaying execution review")?;
    let deciding_user = match session.created_by {
        Some(SessionActorRef::Identity { id }) => id.to_string(),
        other => anyhow::bail!("fixture session has no deciding identity: {other:?}"),
    };
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to fixture Postgres")?;
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "resume the claimed execution review".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let origin = insert_execution_review_task(
        &pool,
        session.tenant_id,
        session_id,
        originating_user_sequence_num,
    )
    .await?;
    let command = format!("printf claimed-review-replay-{}", Uuid::now_v7());
    let review_id = Uuid::now_v7();
    let claimed_tool_call_id = Uuid::now_v7();
    let tool_call_id = ToolCallId::new();
    let contract_revision = activated_contract_revision(&test, "bash").await?;
    let tool_request = ToolCallRequest {
        tool_call_id,
        caller_identity: test
            .client()
            .identity()
            .context("fixture client identity")?
            .clone(),
        provider_tool_use_id: None,
        tool_name: "bash".to_string(),
        expected_tool_contract_revision: contract_revision,
        input: json!({"cmd": command.clone()}),
        active_canary: None,
        session_id,
        trusted_sandbox_manifest: None,
        worker_id: None,
        resource_budget: Default::default(),
    };
    let envelope = ActionEnvelope {
        review_id,
        tenant_id: session.tenant_id,
        requested_by: SessionActorRef::Anonymous,
        owner: ActionReviewOwner::ExecutionTask { session_id, origin },
        tool_call_id,
        tool_name: "bash".to_string(),
        normalized_input: command.clone(),
        input_summary: "claimed execution review replay".to_string(),
        risk_level: RiskLevel::High,
        action_class: ActionClass::CommandExecution,
        origin_kind: None,
        origin_id: None,
        origin_step_id: None,
        idempotency_key: None,
        created_at: moa_test_support::fixtures::pg_now(),
    };
    sqlx::query(
        r#"
        INSERT INTO tenant_action_reviews (
            id, tenant_id, storage_partition_id, session_id, tool_call_id, tool_name,
            action_class, risk_level, input_summary, normalized_input, envelope,
            preview, tool_request, requested_by, status, created_at, expires_at,
            decided_by, decided_at, execution_tool_call_id, execution_requested_at,
            owner_registered_at
        ) VALUES (
            $1, $2, $3, $4, $5, 'bash', 'command_execution', 'high',
            'claimed execution review replay', $6, $7,
            '{"fields":[],"file_diffs":[]}'::JSONB, $8, 'anonymous', 'pending',
            NOW(), NOW() + INTERVAL '1 day', 'conflicting-claimer', NOW(), $9, NOW(), NOW()
        )
        "#,
    )
    .bind(review_id)
    .bind(session.tenant_id.0)
    .bind(
        moa_core::types::identifiers::StoragePartitionId::for_tenant(session.tenant_id).to_string(),
    )
    .bind(session_id.0)
    .bind(tool_call_id.0)
    .bind(&command)
    .bind(serde_json::to_value(envelope)?)
    .bind(serde_json::to_value(tool_request)?)
    .bind(claimed_tool_call_id)
    .execute(&pool)
    .await
    .context("simulate crash after a conflicting durable claim")?;
    let conflicting = decide_review(
        test.client(),
        session.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await;
    assert!(
        conflicting.is_err(),
        "a different caller must not replay another claimant's execution"
    );

    sqlx::query("UPDATE tenant_action_reviews SET decided_by = $2 WHERE id = $1")
        .bind(review_id)
        .bind(&deciding_user)
        .execute(&pool)
        .await
        .context("fixture should restore the identical durable claimant")?;
    decide_review(
        test.client(),
        session.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await
    .context("identical claimed clear should resume execution and finalization")?;

    let (status, persisted_tool_call_id): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, execution_tool_call_id FROM tenant_action_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .context("finalized review should remain queryable")?;
    assert_eq!(status, "cleared");
    assert_eq!(persisted_tool_call_id, Some(claimed_tool_call_id));
    let resolution: serde_json::Value = sqlx::query_scalar(
        "SELECT resolution FROM moa.execution_action_review_outbox WHERE review_uid = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .context("finalization should atomically create the execution resolution outbox")?;
    assert_eq!(
        resolution.get("status").and_then(serde_json::Value::as_str),
        Some("completed")
    );
    Ok(())
}

async fn insert_execution_review_task(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    session_id: SessionId,
    originating_user_sequence_num: u64,
) -> Result<ExecutionTaskOrigin> {
    let run_uid = Uuid::new_v4();
    let task_uid = Uuid::new_v4();
    let planning_context_uid = Uuid::new_v4();
    let hash = "0".repeat(64);
    let originating_user_sequence_num = i64::try_from(originating_user_sequence_num)
        .context("execution review origin sequence exceeds PostgreSQL BIGINT")?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_planning_context (
            planning_context_uid, tenant_id, session_id,
            originating_user_sequence_num, originating_user_event_hash,
            owner_user_id, planning_context_hash, snapshot
        ) VALUES ($1, $2, $3, $4, $5, 'test-owner', $5, '{}'::JSONB)
        "#,
    )
    .bind(planning_context_uid)
    .bind(tenant_id.0)
    .bind(session_id.0)
    .bind(originating_user_sequence_num)
    .bind(&hash)
    .execute(pool)
    .await
    .context("insert execution review planning-context fixture")?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, session_id, originating_user_sequence_num,
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract,
            initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, source_kind,
            input, status, queued_at
        ) VALUES ($1, $2, $3, $4, $5, $6, 'test-owner',
                  '{}'::JSONB, '{}'::JSONB, '{}'::JSONB, $6, $6,
                  '{}'::JSONB, '{}'::JSONB, '[]'::JSONB,
                  '{"kind":"generated_plan"}'::JSONB,
                  'generated_plan',
                  '{}'::JSONB, 'queued', NOW())
        "#,
    )
    .bind(run_uid)
    .bind(tenant_id.0)
    .bind(session_id.0)
    .bind(originating_user_sequence_num)
    .bind(planning_context_uid)
    .bind(hash)
    .execute(pool)
    .await
    .context("insert execution review run fixture")?;
    sqlx::query("UPDATE moa.execution_run SET status = 'running' WHERE run_uid = $1")
        .bind(run_uid)
        .execute(pool)
        .await
        .context("start execution review run fixture")?;
    sqlx::query(
        r#"
        INSERT INTO moa.execution_task (
            task_id, run_uid, tenant_id, node_id, item_key, plan_revision,
            status, input, task_kind, retry_policy,
            estimate_cost_microusd, estimate_tokens, estimate_tasks,
            estimate_tool_calls, estimate_retrieved_bytes
        ) VALUES ($1, $2, $3, 'review', 'replay', 1, 'running', '{}'::JSONB,
                  '{}'::JSONB, '{}'::JSONB, 0, 0, 1, 0, 0)
        "#,
    )
    .bind(task_uid)
    .bind(run_uid)
    .bind(tenant_id.0)
    .execute(pool)
    .await
    .context("insert execution review task fixture")?;
    Ok(ExecutionTaskOrigin {
        run_uid,
        task_uid,
        generation: 1,
    })
}

async fn execution_task_tool_executor_emits_zero_root_tool_events(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: dynamic task execution uses the owning session context without replaying or appending root tool events.
    let test = fixture.isolated().await;
    let session_id = test.create_session("execution-task-no-root-events").await?;
    let tool_call_id = ToolCallId::new();
    let contract_revision = activated_contract_revision(&test, "bash").await?;
    let output: SecuredToolOutput = test
        .client()
        .post_call(
            "/ToolExecutor/execute_execution_task",
            &ExecutionTaskToolCallRequest {
                call: ToolCallRequest {
                    tool_call_id,
                    caller_identity: test
                        .client()
                        .identity()
                        .context("fixture client identity")?
                        .clone(),
                    provider_tool_use_id: None,
                    tool_name: "bash".to_string(),
                    expected_tool_contract_revision: contract_revision,
                    input: json!({"cmd": "printf execution-task-ok"}),
                    active_canary: None,
                    session_id,
                    trusted_sandbox_manifest: None,
                    worker_id: None,
                    resource_budget: Default::default(),
                },
                origin: Some(ExecutionTaskOrigin {
                    run_uid: Uuid::new_v4(),
                    task_uid: Uuid::new_v4(),
                    generation: 7,
                }),
            },
        )
        .await
        .context("execute isolated execution-task tool call")?;
    assert_eq!(output.safe_output.to_text(), "execution-task-ok");

    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolCall { tool_id, .. } | Event::ToolResult { tool_id, .. }
                if *tool_id == tool_call_id
        )),
        "execution-task dispatch must emit zero root ToolCall/ToolResult events: {}",
        event_summary(&events)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn scripted_provider_repeated_tool_loop_stops_before_unbounded_dispatch() -> Result<()> {
    // Pins: scripted providers that keep requesting the same tool stop in TurnExecution.
    let fixture = OrchestratorTestFixture::with_script(repeated_tool_loop_script()).await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("repeated-tool-loop").await?;

    let events = run_scripted_turn(
        &test,
        session_id,
        "run cargo test -p moa-orchestrator for repeated-loop fixture",
    )
    .await?;

    let repeated_tool_ids = repeated_loop_tool_call_provider_ids(&events);
    assert_eq!(
        repeated_tool_ids,
        vec!["repeated-loop-1", "repeated-loop-2"],
        "third identical scripted tool request should stop before ToolCall persistence: {}",
        event_summary(&events)
    );
    assert!(
        repeated_tool_ids.len() < REPEATED_LOOP_SCRIPTED_ATTEMPTS,
        "actual ToolCall count must stay below scripted attempts: {}",
        event_summary(&events)
    );
    // The sandbox-free root coordinator denies hand-routed bash outright, so no
    // repeated call ever executes; loop detection must still stop the dispatch
    // stream on identical denied calls.
    assert_eq!(
        repeated_loop_successful_results(&events),
        0,
        "coordinator bash calls should be denied, not executed: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { output, .. }
                if output.to_text().contains("repeated-loop-ok")
        )),
        "denied bash must never reach a sandbox: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::Error { message, recoverable }
                if *recoverable
                    && message == "tool loop detected: `bash` repeated 3 consecutive times with threshold 3"
        )),
        "loop stop should append a recoverable budget error: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::BrainResponse { text, .. }
                if text == "MOA stopped before running another tool because the model repeatedly requested the same `bash` call. Narrow the scope or ask MOA to continue."
        )),
        "loop stop should append the assistant stop response: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn action_policy_auto_mode_executes_shell_without_user_approval(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: high-risk command execution defaults to review, so only an
    // explicit tenant allow rule lets a delegated worker's known bash action
    // execute without approval.
    let test = fixture.isolated().await;
    let session_id = test.create_session("auto-mode").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding explicit allow rule")?;
    add_bash_allow_rule(test.client(), meta.tenant_id, "printf auto-mode-ok").await?;

    run_scripted_turn(&test, session_id, "Run the auto-mode bash command.").await?;

    let events = wait_for_events(&test, session_id, |events| {
        has_tool_result(events, Some("auto-mode-bash"), "auto-mode-ok", true)
    })
    .await
    .context("worker bash result should reach the session log")?;
    assert!(
        events
            .iter()
            .all(|record| !matches!(record.event, Event::ActionReviewRequested { .. })),
        "auto mode should not create an action review: {}",
        event_summary(&events)
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::ToolCall { tool_name, .. } if tool_name == "spawn_worker"
        )),
        "coordinator should delegate compute to a worker: {}",
        event_summary(&events)
    );
    Ok(())
}

async fn admin_review_policy_records_pending_review_and_turn_continues(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: a delegated worker's bash hitting an admin-review rule records a
    // pending tenant action review without blocking the worker or the session.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-pending").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and listing pending review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    run_scripted_turn(&test, session_id, "Run the admin-review bash command.").await?;

    let events = wait_for_events(&test, session_id, |events| {
        action_review_id(events).is_some()
    })
    .await
    .context("worker bash should request an admin review")?;
    let review_id = action_review_id(&events).context("expected ActionReviewRequested event")?;
    let pending = list_pending_reviews(test.client(), meta.tenant_id).await?;
    assert_eq!(
        pending.len(),
        1,
        "expected exactly one pending review; pending: {pending:#?}; events: {}",
        event_summary(&events)
    );
    assert_eq!(pending[0].id, review_id);
    assert_eq!(pending[0].status, ActionReviewStatus::Pending);

    assert_tool_result(
        &events,
        Some("pending-review-bash"),
        "pending tenant admin review",
        false,
        "pending-review tool result",
    );
    assert!(
        events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { output, success: true, .. }
                if output.to_text().contains("should-not-run-before-clear")
        )),
        "gated bash must not execute before the review is cleared: {}",
        event_summary(&events)
    );
    let stored = test.client().get_session(session_id).await?;
    assert!(
        matches!(
            stored.status,
            SessionStatus::Paused | SessionStatus::Completed
        ),
        "admin review should not leave the session running or blocked: {:?}",
        stored.status
    );
    Ok(())
}

async fn tenant_admin_clear_executes_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: clearing a tenant action review executes the stored request with a fresh tool id.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-clear").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and deciding review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;
    let (origin_turn_id, _) = run_scripted_turn_with_id(
        &test,
        session_id,
        "Establish the coordinator owner for the cleared review.",
    )
    .await?;

    let (review_id, stored_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        "printf clear-review-ok",
        coordinator_owner_for_turn(session_id, &origin_turn_id),
    )
    .await?;
    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert_eq!(
        action_review_id(&events),
        Some(review_id),
        "review request should append ActionReviewRequested: {}",
        event_summary(&events)
    );
    // Sessions in this fixture share the default tenant, so earlier scenarios
    // may have left their own pending reviews; assert on ours by id.
    let pending = list_pending_reviews(test.client(), meta.tenant_id).await?;
    let stored = pending
        .iter()
        .find(|review| review.id == review_id)
        .context("stored review should be listed as pending before the decision")?;
    assert_eq!(stored.status, ActionReviewStatus::Pending);

    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await?;
    let decided_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ActionReviewDecided { review_id: id, decision: ActionReviewDecision::Cleared, .. } if *id == review_id)
        ),
        "clear decision should be persisted: {}",
        event_summary(&decided_events)
    );
    // The decision path re-mints the stored request's tool id so the execution
    // can never collide with the original tool-call events.
    let fresh_tool_id = decided_events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                output,
                success: true,
                ..
            } if provider_tool_use_id.is_none()
                && *tool_id != stored_tool_id
                && output.to_text().contains("clear-review-ok") =>
            {
                Some(*tool_id)
            }
            _ => None,
        })
        .context("expected cleared action to execute with a fresh tool id")?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ToolCall { tool_id, provider_tool_use_id, tool_name, .. }
                if *tool_id == fresh_tool_id && provider_tool_use_id.is_none() && tool_name == "bash")
        ),
        "cleared action should append a fresh ToolCall before ToolResult: {}",
        event_summary(&decided_events)
    );
    Ok(())
}

async fn tenant_admin_deny_does_not_execute_stored_review_action(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: denying a tenant action review records the decision without executing the stored action.
    let test = fixture.isolated().await;
    let session_id = test.create_session("admin-review-deny").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding and denying review")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (review_id, _stored_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        "printf deny-review-should-not-run",
        unowned_coordinator(session_id),
    )
    .await?;
    decide_review(
        test.client(),
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Denied,
        Some("risk too high"),
    )
    .await?;

    let decided_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        decided_events.iter().any(
            |record| matches!(&record.event, Event::ActionReviewDecided { review_id: id, decision: ActionReviewDecision::Denied { .. }, .. } if *id == review_id)
        ),
        "deny decision should be persisted: {}",
        event_summary(&decided_events)
    );
    assert!(
        decided_events.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { provider_tool_use_id: None, output, success: true, .. }
                if output.to_text().contains("deny-review-should-not-run")
        )),
        "denied review must not execute the stored action: {}",
        event_summary(&decided_events)
    );
    Ok(())
}

async fn tenant_member_cannot_decide_action_review(
    fixture: &OrchestratorTestFixture,
) -> Result<()> {
    // Pins: non-admin tenant operators cannot list or decide action reviews.
    let test = fixture.isolated().await;
    let session_id = test.create_session("member-denied").await?;
    let meta = test.client().get_session(session_id).await?;
    initialize_tenant(test.client(), meta.tenant_id).await?;
    fixture
        .grant_default_tenant_admin(meta.tenant_id)
        .await
        .context("grant admin before adding review rule")?;
    add_bash_admin_review_rule(test.client(), meta.tenant_id).await?;

    let (review_id, _stored_tool_id) = create_pending_bash_review(
        &test,
        session_id,
        "printf member-denied-should-not-run",
        unowned_coordinator(session_id),
    )
    .await?;

    let mut member_identity = test_identity();
    member_identity.tenant_id = meta.tenant_id;
    fixture
        .grant_tenant_operator_identity(&member_identity, meta.tenant_id)
        .await
        .context("grant non-admin member before forbidden review decision")?;
    let member_client = TestApiClient::new(&fixture.ingress_url)?.with_identity(member_identity);

    let list_error = list_pending_reviews(&member_client, meta.tenant_id)
        .await
        .expect_err("tenant operator should not list pending action reviews");
    assert_authz_error(&list_error);
    let decide_error = decide_review(
        &member_client,
        meta.tenant_id,
        review_id,
        ActionReviewDecisionKind::Cleared,
        None,
    )
    .await
    .expect_err("tenant operator should not decide action reviews");
    assert_authz_error(&decide_error);

    let after_attempt = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert!(
        after_attempt
            .iter()
            .all(|record| !matches!(record.event, Event::ActionReviewDecided { .. })),
        "failed member decision must not append a decision event: {}",
        event_summary(&after_attempt)
    );
    assert!(
        after_attempt.iter().all(|record| !matches!(
            &record.event,
            Event::ToolResult { provider_tool_use_id: None, output, success: true, .. }
                if output.to_text().contains("member-denied-should-not-run")
        )),
        "failed member decision must not execute the stored action: {}",
        event_summary(&after_attempt)
    );
    Ok(())
}

/// A coordinator owner for a session that has not run a turn in this scenario.
///
/// The Session VO has no owning identity yet, so a resolution releases the review
/// without dispatching a continuation. Scenarios that assert on the continuation
/// itself run a real turn first and use [`coordinator_owner_for_turn`].
fn unowned_coordinator(session_id: SessionId) -> ActionReviewOwner {
    ActionReviewOwner::Coordinator {
        session_id,
        turn_id: "unowned-coordinator-turn".to_string(),
        generation: 1,
    }
}

/// The coordinator owner for a session whose first admitted turn is `turn_id`.
///
/// The Session advances its generation once per admitted user message, so the
/// first admitted turn always runs under generation 1.
fn coordinator_owner_for_turn(session_id: SessionId, turn_id: &str) -> ActionReviewOwner {
    ActionReviewOwner::Coordinator {
        session_id,
        turn_id: turn_id.to_string(),
        generation: 1,
    }
}

/// Returns every continuation fact recorded for one review.
fn continuation_facts(
    events: &[EventRecord],
    review_id: Uuid,
) -> Vec<(
    &String,
    &moa_core::types::action_policy::ActionReviewReceipt,
)> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ActionReviewContinuationRequested {
                review_id: id,
                turn_id,
                receipt,
            } if *id == review_id => Some((turn_id, receipt)),
            _ => None,
        })
        .collect()
}

async fn run_scripted_turn(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    message: &str,
) -> Result<Vec<EventRecord>> {
    Ok(run_scripted_turn_with_id(test, session_id, message)
        .await?
        .1)
}

/// Runs one scripted coordinator turn to completion and returns its turn id.
///
/// The turn id is what an action review raised by that turn records as its owner,
/// so scenarios that assert on the continuation need it.
async fn run_scripted_turn_with_id(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    message: &str,
) -> Result<(String, Vec<EventRecord>)> {
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: message.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                resource_budget: Default::default(),
                execution_template: None,
            },
            None,
        )
        .await?;
    let turn_id = started
        .turn_id
        .context("start_turn should start immediately in serialized E2E")?;
    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(90),
            Duration::from_millis(250),
        )
        .await?;
    assert_eq!(
        outcome.kind,
        TurnOutcomeKind::Completed,
        "scripted turn should complete: {outcome:?}"
    );
    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    Ok((turn_id, events))
}

/// Creates a pending admin review for a worker-scoped bash invocation by
/// driving the same `ActionPolicy/prepare_action_review` and
/// `ActionReviews/request` calls the turn workflows make, returning the review
/// id and the stored tool-call id that executes if the review is cleared.
async fn create_pending_bash_review(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    cmd: &str,
    owner: ActionReviewOwner,
) -> Result<(Uuid, ToolCallId)> {
    let meta = test.client().get_session(session_id).await?;
    let review_id = Uuid::new_v4();
    let tool_call_id = ToolCallId::new();
    let contract_revision = activated_contract_revision(test, "bash").await?;
    let prepared: PreparedActionReview = test
        .client()
        .post_call(
            "/ActionPolicy/prepare_action_review",
            &PrepareActionReviewRequest {
                session: meta.clone(),
                invocation: ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": cmd }),
                },
                expected_tool_contract_revision: contract_revision.clone(),
                review_id,
                tool_call_id,
                owner,
                capability_provenance: Default::default(),
                idempotency_key: None,
            },
        )
        .await
        .context("prepare action review via ActionPolicy")?;
    assert_eq!(
        prepared.effect,
        ActionPolicyEffect::AdminReview,
        "bash admin-review rule should match the invocation"
    );

    let tool_request = ToolCallRequest {
        tool_call_id,
        caller_identity: test
            .client()
            .identity()
            .context("fixture client identity")?
            .clone(),
        provider_tool_use_id: None,
        tool_name: "bash".to_string(),
        expected_tool_contract_revision: contract_revision,
        input: json!({ "cmd": cmd }),
        active_canary: None,
        session_id,
        trusted_sandbox_manifest: None,
        worker_id: Some("worker-action-policy-e2e".to_string()),
        resource_budget: Default::default(),
    };
    let summary: ActionReviewSummary = test
        .client()
        .post_call(
            "/ActionReviews/request",
            &RequestActionReview {
                envelope: prepared.envelope,
                preview: prepared.preview,
                tool_request,
            },
        )
        .await
        .context("store pending review via ActionReviews")?;
    assert_eq!(
        summary.id, review_id,
        "stored review should keep the prepared id"
    );
    Ok((review_id, tool_call_id))
}

async fn activated_contract_revision(test: &IsolatedTest<'_>, tool_name: &str) -> Result<String> {
    let catalog: ToolCatalogPin = test
        .client()
        .post_empty_call("/ToolExecutor/activated_tool_catalog")
        .await
        .context("load activated tool catalog")?;
    catalog
        .contract_revision(tool_name)
        .map(ToOwned::to_owned)
        .with_context(|| format!("{tool_name} must exist in the activated tool catalog"))
}

async fn initialize_tenant(client: &TestApiClient, tenant_id: TenantId) -> Result<()> {
    client
        .post_void(
            &format!("/Tenant/{tenant_id}/init"),
            &TenantConfig {
                id: tenant_id,
                name: format!("Action policy E2E {tenant_id}"),
                consolidation_hour_utc: 2,
            },
        )
        .await
}

async fn add_bash_admin_review_rule(client: &TestApiClient, tenant_id: TenantId) -> Result<()> {
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &TestRuleRequest {
                tenant_id,
                tool_name: "bash".to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::AdminReview,
                reason: Some("E2E admin-review rule for bash".to_string()),
            },
        )
        .await
}

async fn add_bash_allow_rule(
    client: &TestApiClient,
    tenant_id: TenantId,
    pattern: &str,
) -> Result<()> {
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &TestRuleRequest {
                tenant_id,
                tool_name: "bash".to_string(),
                pattern: pattern.to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("E2E explicit allow rule for known bash command".to_string()),
            },
        )
        .await
}

#[derive(Serialize)]
struct TestRuleRequest {
    tenant_id: TenantId,
    tool_name: String,
    pattern: String,
    effect: ActionPolicyEffect,
    reason: Option<String>,
}

async fn list_pending_reviews(
    client: &TestApiClient,
    tenant_id: TenantId,
) -> Result<Vec<ActionReviewSummary>> {
    client
        .post_call(
            "/ActionReviews/list_pending",
            &ListActionReviewsRequest { tenant_id },
        )
        .await
}

async fn decide_review(
    client: &TestApiClient,
    tenant_id: TenantId,
    review_id: Uuid,
    decision: ActionReviewDecisionKind,
    reason: Option<&str>,
) -> Result<()> {
    client
        .post_void(
            "/ActionReviews/decide",
            &DecideActionReviewRequest {
                tenant_id,
                review_id,
                decision,
                reason: reason.map(str::to_string),
            },
        )
        .await
}

/// Task strings delegated to scripted workers. These are the keyed-match
/// needles for worker turns, so they must not be substrings of the user
/// messages that key the coordinator's spawn responses (or vice versa).
const AUTO_WORKER_TASK: &str = "execute the auto-mode bash probe";
const REVIEW_WORKER_TASK: &str = "execute the review-mode bash probe";
/// Worker final texts; the coordinator's post-completion resume keys on them.
const AUTO_WORKER_DONE: &str = "Auto-mode worker finished its delegated task.";
const REVIEW_WORKER_DONE: &str = "Admin-review worker finished its delegated task.";
const CONTINUATION_WORKER_TASK: &str = "execute the worker-continuation bash probe";
const CONTINUATION_DONE: &str = "Reported the resolved action review.";

/// Fully keyed script for the delegation-based action-policy scenarios.
///
/// Every response is keyed (no FIFO) so internal model calls the loop makes
/// (query rewrite, segment assessment, …) can never consume a scenario
/// response out of order — they fall through to `default`. First match wins,
/// so later-state needles (tool outputs, worker final texts) are registered
/// before the broader needles (task text, user message) they co-occur with.
fn action_policy_script() -> serde_json::Value {
    json!({
        "default": {
            "completion": {
                "content": "OK",
                "tool_calls": []
            }
        },
        "keyed": [
            // An action-review continuation turn (root or worker) renders the typed
            // receipt as a system directive; it is keyed first because the rest of the
            // continuing owner's replayed history also matches later entries.
            // Ordering contract, most-derived context first: continuation
            // receipts, then tool-result markers, then worker task texts, then
            // user messages. Entries resolve first-match-wins against the whole
            // rendered prompt, and a later iteration's prompt still contains
            // every earlier marker — a user-message spawn key registered before
            // the "Spawned worker" marker makes every post-spawn coordinator
            // iteration re-issue the spawn, which the session's loop-prevention
            // guard then terminally refuses against the still-active child.
            {
                "match": "action_review_continuation",
                "completion": { "content": CONTINUATION_DONE }
            },
            // Coordinator iterations after a spawn (and post-completion
            // resumes, whose compiled context replays the spawn result): the
            // spawn tool output summary is the only pre-worker marker.
            {
                "match": "Spawned worker",
                "completion": { "content": "Delegated; waiting for the worker." }
            },
            // Worker follow-up iterations key on their bash tool result…
            {
                "match": "auto-mode-ok",
                "completion": { "content": AUTO_WORKER_DONE }
            },
            {
                "match": "pending tenant admin review",
                "completion": { "content": REVIEW_WORKER_DONE }
            },
            // …worker first iterations on the delegated task text…
            {
                "match": CONTINUATION_WORKER_TASK,
                "completion": {
                    "content": "Running the worker-continuation probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "input": { "cmd": "printf worker-continuation-ok" },
                        "id": "worker-continuation-bash"
                    }]
                }
            },
            {
                "match": AUTO_WORKER_TASK,
                "completion": {
                    "content": "Running the auto-mode probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "input": { "cmd": "printf auto-mode-ok" },
                        "id": "auto-mode-bash"
                    }]
                }
            },
            {
                "match": REVIEW_WORKER_TASK,
                "completion": {
                    "content": "Running the review-mode probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "input": { "cmd": "printf should-not-run-before-clear" },
                        "id": "pending-review-bash"
                    }]
                }
            },
            // …and coordinator first iterations on the user message.
            {
                "match": "Run the worker-continuation bash command.",
                "completion": {
                    "content": "Delegating to the worker-continuation worker.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "input": {
                            "task": CONTINUATION_WORKER_TASK,
                            "tool_subset": ["bash"],
                            "max_turns": 3
                        },
                        "id": "spawn-worker-continuation"
                    }]
                }
            },
            {
                "match": "Run the auto-mode bash command.",
                "completion": {
                    "content": "Delegating to the auto-mode worker.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "input": {
                            "task": AUTO_WORKER_TASK,
                            "tool_subset": ["bash"],
                            "max_turns": 3
                        },
                        "id": "spawn-auto"
                    }]
                }
            },
            {
                "match": "Run the admin-review bash command.",
                "completion": {
                    "content": "Delegating to the admin-review worker.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "input": {
                            "task": REVIEW_WORKER_TASK,
                            "tool_subset": ["bash"],
                            "max_turns": 3
                        },
                        "id": "spawn-review"
                    }]
                }
            }
        ]
    })
}

const REPEATED_LOOP_SCRIPTED_ATTEMPTS: usize = 5;
const REPEATED_LOOP_CMD: &str = "printf repeated-loop-ok";

fn repeated_tool_loop_script() -> serde_json::Value {
    let responses = (1..=REPEATED_LOOP_SCRIPTED_ATTEMPTS)
        .map(|index| bash_tool_response(&format!("repeated-loop-{index}"), REPEATED_LOOP_CMD))
        .collect::<Vec<_>>();
    json!({
        "default": {
            "completion": {
                "content": "unexpected fallback",
                "tool_calls": []
            }
        },
        "keyed": [{
            "match": "You classify one user turn into MOA's public execution decision.",
            "completion": {
                "content": json!({
                    "label": "execute",
                    "strategy": "inline",
                    "rationale": "The work fits a bounded interactive loop.",
                    "confidence_bps": 10_000,
                    "missing_inputs": []
                }).to_string(),
                "tool_calls": []
            }
        }],
        "responses": responses
    })
}

fn bash_tool_response(provider_tool_id: &str, cmd: &str) -> serde_json::Value {
    json!({
        "completion": {
            "content": "",
            "tool_calls": [{
                "name": "bash",
                "id": provider_tool_id,
                "input": { "cmd": cmd }
            }]
        }
    })
}

fn repeated_loop_tool_call_provider_ids(events: &[EventRecord]) -> Vec<&str> {
    let expected_input = json!({ "cmd": REPEATED_LOOP_CMD });
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                provider_tool_use_id,
                tool_name,
                input,
                ..
            } if tool_name == "bash" && input == &expected_input => provider_tool_use_id.as_deref(),
            _ => None,
        })
        .collect()
}

fn repeated_loop_successful_results(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ToolResult {
                    provider_tool_use_id: Some(provider_tool_use_id),
                    output,
                    success: true,
                    ..
                } if provider_tool_use_id.starts_with("repeated-loop-")
                    && output.to_text().contains("repeated-loop-ok")
            )
        })
        .count()
}

fn test_identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn action_review_id(events: &[EventRecord]) -> Option<Uuid> {
    events.iter().find_map(|record| match &record.event {
        Event::ActionReviewRequested { review_id, .. } => Some(*review_id),
        _ => None,
    })
}

/// Returns whether a matching `ToolResult` event exists in the session log.
fn has_tool_result(
    events: &[EventRecord],
    provider_tool_use_id: Option<&str>,
    expected_text: &str,
    expected_success: bool,
) -> bool {
    events.iter().any(|record| match &record.event {
        Event::ToolResult {
            provider_tool_use_id: actual,
            output,
            success,
            ..
        } => {
            actual.as_deref() == provider_tool_use_id
                && *success == expected_success
                && output.to_text().contains(expected_text)
        }
        _ => false,
    })
}

/// Polls the session event log until `predicate` holds, returning the events.
///
/// Worker turns run in their own Restate invocations, so their events land in
/// the parent session log after the coordinator turn has already completed.
async fn wait_for_events<F>(
    test: &IsolatedTest<'_>,
    session_id: SessionId,
    predicate: F,
) -> Result<Vec<EventRecord>>
where
    F: Fn(&[EventRecord]) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let events = test
            .client()
            .get_events(session_id, EventRange::all())
            .await?;
        if predicate(&events) {
            return Ok(events);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for session events: {}",
                event_summary(&events)
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn assert_tool_result(
    events: &[EventRecord],
    provider_tool_use_id: Option<&str>,
    expected_text: &str,
    expected_success: bool,
    context: &str,
) {
    assert!(
        has_tool_result(
            events,
            provider_tool_use_id,
            expected_text,
            expected_success
        ),
        "expected {context}; events: {}",
        event_summary(events)
    );
}

fn assert_authz_error(error: &anyhow::Error) {
    let message = error.to_string();
    assert!(
        message.contains("403")
            || message.contains("Forbidden")
            || message.contains("forbidden")
            || message.contains("authorization")
            || message.contains("authorized"),
        "expected authorization failure, got: {message}"
    );
}

fn event_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| match &record.event {
            Event::ToolCall {
                tool_id,
                provider_tool_use_id,
                tool_name,
                ..
            } => format!(
                "#{} ToolCall {tool_name} {tool_id} provider={provider_tool_use_id:?}",
                record.sequence_num
            ),
            Event::ToolResult {
                tool_id,
                provider_tool_use_id,
                success,
                output,
                ..
            } => format!(
                "#{} ToolResult {tool_id} provider={provider_tool_use_id:?} success={success} text={}",
                record.sequence_num,
                truncate_for_summary(&output.to_text())
            ),
            Event::ActionReviewRequested { review_id, .. } => {
                format!("#{} ActionReviewRequested {review_id}", record.sequence_num)
            }
            Event::ActionReviewDecided {
                review_id, decision, ..
            } => format!(
                "#{} ActionReviewDecided {review_id} {decision:?}",
                record.sequence_num
            ),
            Event::BrainResponse { text, .. } => {
                format!(
                    "#{} BrainResponse {}",
                    record.sequence_num,
                    truncate_for_summary(text)
                )
            }
            Event::Error {
                message,
                recoverable,
            } => format!(
                "#{} Error recoverable={recoverable} {}",
                record.sequence_num,
                truncate_for_summary(message)
            ),
            _ => format!("#{} {:?}", record.sequence_num, record.event_type),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn truncate_for_summary(value: &str) -> String {
    const LIMIT: usize = 300;
    if value.chars().count() <= LIMIT {
        return value.replace('\n', "\\n");
    }

    let truncated = value.chars().take(LIMIT).collect::<String>();
    format!("{}...", truncated.replace('\n', "\\n"))
}
