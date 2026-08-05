//! Service-level replay coverage for external execution-template admission.

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalTemplate, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionPlanTemplate, ExecutionRequirement, RetryPolicy,
};
use moa_core::events::Event;
use moa_core::types::execution_planning::PinnedExecutionTemplateRef;
use moa_execution::state::ExecutionRunStatus;
use moa_execution::wire::{
    ExecutionRunRequest, ExecutionTemplateAdmissionRequest, ExecutionTemplateAdmissionResponse,
    execution_template_admission_operation_uid,
};
use moa_test_support::{FixtureCapabilityOptions, OrchestratorTestFixture, TestApiClient};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, SERVICE_TIMEOUT, activate_skill, await_execution_terminal, raw_events,
};

const SKILL_NAME: &str = "admission-replay-output";
const OBJECTIVE: &str = "Continue exactly once after the admission reply is lost.";
const IDEMPOTENCY_KEY: &str = "task-13-admission-replay";
const FAILPOINT_BUDGET: &str = "1000000";

pub(crate) async fn run_execution_template_admission_replay() -> Result<()> {
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": {
                "content": "The replayed execution completed.",
                "tool_calls": []
            }
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("admission-replay").await?;
    let session = test.client().get_session(session_id).await?;
    let activated = activate_skill(
        &fixture,
        test.client(),
        session.tenant_id,
        SKILL_NAME,
        template_skill_source(),
        template_skill_markdown(),
    )
    .await?;
    let request = ExecutionTemplateAdmissionRequest {
        tenant_id: session.tenant_id,
        contact_id: None,
        session_id,
        template: PinnedExecutionTemplateRef {
            skill_ref: activated.skill_ref,
            revision_uid: activated.revision_uid,
        },
        objective: OBJECTIVE.to_string(),
        input: json!({"result": "continued"}),
        idempotency_key: Some(IDEMPOTENCY_KEY.to_string()),
    };
    let operation_uid =
        execution_template_admission_operation_uid(session.tenant_id, IDEMPOTENCY_KEY)?;
    let pool = PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect admission replay verification pool")?;

    fixture
        .restart_orchestrator_with_env(vec![(
            "MOA_FAILPOINT_EVENT_APPEND_POST_COMMIT".to_string(),
            FAILPOINT_BUDGET.to_string(),
        )])
        .await
        .context("restart orchestrator with post-commit event failpoint")?;

    let path = format!("/Session/{session_id}/admit_execution_template");
    let interrupted_call = spawn_admission(test.client(), path.clone(), request.clone());
    await_committed_unacknowledged_objective(&pool, session_id, operation_uid).await?;
    interrupted_call.abort();
    let interrupted = interrupted_call
        .await
        .expect_err("aborting the unacknowledged admission request should cancel its waiter");
    assert!(
        interrupted.is_cancelled(),
        "the admission request waiter should be cancelled before reply delivery: {interrupted}"
    );

    fixture
        .restart_orchestrator()
        .await
        .context("restart orchestrator without the one-shot failpoint")?;

    let applied: ExecutionTemplateAdmissionResponse =
        tokio::time::timeout(SERVICE_TIMEOUT, test.client().post_call(&path, &request))
            .await
            .context("admission retry did not return after orchestrator restart")??;
    let replayed: ExecutionTemplateAdmissionResponse = test
        .client()
        .post_call(&path, &request)
        .await
        .context("completed admission should replay its typed response")?;
    assert_eq!(
        replayed, applied,
        "wire-equivalent replay must return the first session, origin, and run"
    );
    assert_eq!(applied.session_id, session_id);

    let terminal = await_execution_terminal(
        test.client(),
        &ExecutionRunRequest {
            tenant_id: session.tenant_id,
            contact_id: None,
            session_id,
            run_uid: applied.execution_run_uid,
        },
    )
    .await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Completed);
    assert_eq!(terminal.output, Some(json!({"result": "continued"})));
    assert_eq!(terminal.run.total_tasks, 1);
    assert_eq!(terminal.run.completed_tasks, 1);

    assert_exact_persisted_admission(
        &pool,
        session_id,
        session.tenant_id,
        operation_uid,
        &applied,
    )
    .await?;
    assert_exact_session_events(test.client(), &pool, session_id, &applied).await?;
    Ok(())
}

fn spawn_admission(
    client: &TestApiClient,
    path: String,
    request: ExecutionTemplateAdmissionRequest,
) -> tokio::task::JoinHandle<Result<ExecutionTemplateAdmissionResponse>> {
    let client = client.clone();
    tokio::spawn(async move { client.post_call(&path, &request).await })
}

async fn await_committed_unacknowledged_objective(
    pool: &PgPool,
    session_id: moa_core::types::identifiers::SessionId,
    operation_uid: uuid::Uuid,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + SERVICE_TIMEOUT;
    loop {
        let objective_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events \
             WHERE session_id = $1 AND event_type = 'UserMessage' \
               AND payload -> 'data' ->> 'text' = $2",
        )
        .bind(session_id.0)
        .bind(OBJECTIVE)
        .fetch_one(pool)
        .await
        .context("count committed admission objective events")?;
        let admission: Option<(Option<i64>, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT originating_user_sequence_num, execution_run_uid \
             FROM moa.execution_template_admission WHERE operation_uid = $1",
        )
        .bind(operation_uid)
        .fetch_optional(pool)
        .await
        .context("load interrupted admission row")?;
        let run_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_run WHERE session_id = $1")
                .bind(session_id.0)
                .fetch_one(pool)
                .await
                .context("count runs before admission acknowledgement")?;
        let started_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events \
             WHERE session_id = $1 AND event_type = 'ExecutionRunStarted'",
        )
        .bind(session_id.0)
        .fetch_one(pool)
        .await
        .context("count started events before admission acknowledgement")?;

        if objective_events > 1 || run_count > 0 || started_events > 0 {
            bail!(
                "interrupted admission crossed or duplicated the crash boundary: \
                 objective_events={objective_events}, run_count={run_count}, \
                 started_events={started_events}, admission={admission:?}"
            );
        }
        if objective_events == 1 && admission == Some((None, None)) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "post-commit failpoint did not expose the promised crash window: \
                 objective_events={objective_events}, run_count={run_count}, \
                 started_events={started_events}, admission={admission:?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn assert_exact_persisted_admission(
    pool: &PgPool,
    session_id: moa_core::types::identifiers::SessionId,
    tenant_id: moa_core::types::identifiers::TenantId,
    operation_uid: uuid::Uuid,
    response: &ExecutionTemplateAdmissionResponse,
) -> Result<()> {
    let admissions: Vec<(uuid::Uuid, Option<i64>, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT operation_uid, originating_user_sequence_num, execution_run_uid \
         FROM moa.execution_template_admission \
         WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant_id.0)
    .bind(IDEMPOTENCY_KEY)
    .fetch_all(pool)
    .await
    .context("load completed admission replay rows")?;
    assert_eq!(
        admissions,
        vec![(
            operation_uid,
            Some(i64::try_from(response.originating_user_sequence_num)?),
            Some(response.execution_run_uid),
        )],
        "one caller key must bind to one complete admission row"
    );

    let run_uids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT run_uid FROM moa.execution_run \
         WHERE tenant_id = $1 AND session_id = $2 \
           AND originating_user_sequence_num = $3 ORDER BY run_uid",
    )
    .bind(tenant_id.0)
    .bind(session_id.0)
    .bind(i64::try_from(response.originating_user_sequence_num)?)
    .fetch_all(pool)
    .await
    .context("load runs for replayed admission origin")?;
    assert_eq!(
        run_uids,
        vec![response.execution_run_uid],
        "admission replay must create one durable execution run"
    );

    let task_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_task WHERE run_uid = $1")
            .bind(response.execution_run_uid)
            .fetch_one(pool)
            .await
            .context("count replayed admission execution tasks")?;
    assert_eq!(
        task_count, 1,
        "one output node must be materialized and dispatched exactly once"
    );
    Ok(())
}

async fn assert_exact_session_events(
    client: &TestApiClient,
    pool: &PgPool,
    session_id: moa_core::types::identifiers::SessionId,
    response: &ExecutionTemplateAdmissionResponse,
) -> Result<()> {
    let events = raw_events(client, session_id).await?;
    let objective_events = events
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::UserMessage { text, attachments }
                    if text == OBJECTIVE && attachments.is_empty()
            )
        })
        .count();
    assert_eq!(
        objective_events, 1,
        "commit-before-reply replay must not duplicate the objective event"
    );

    let started = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ExecutionRunStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "admission replay must publish one Session execution-start event"
    );
    assert_eq!(started[0].run_uid, response.execution_run_uid);
    assert_eq!(
        started[0].originating_user_sequence_num,
        response.originating_user_sequence_num
    );

    let routes: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT decision, strategy, source FROM moa.execution_route_audit \
         WHERE session_id = $1 ORDER BY accepted_at",
    )
    .bind(session_id.0)
    .fetch_all(pool)
    .await
    .context("load admission replay route audits")?;
    assert_eq!(
        routes,
        vec![(
            "execute".to_string(),
            Some("durable".to_string()),
            "selected_execution_template".to_string(),
        )],
        "pinned-template admission must persist one selected-template route"
    );
    let compile_sources: Vec<String> = sqlx::query_scalar(
        "SELECT source FROM moa.execution_compile_audit \
         WHERE session_id = $1 ORDER BY created_at",
    )
    .bind(session_id.0)
    .fetch_all(pool)
    .await
    .context("load admission replay compile audits")?;
    assert_eq!(
        compile_sources,
        vec!["skill_template"],
        "pinned-template admission must persist one skill-template compile"
    );
    Ok(())
}

fn template_skill_source() -> String {
    let io_schema = template_io_schema();
    let template = ExecutionPlanTemplate {
        goal: ExecutionGoalTemplate {
            requirements: vec![ExecutionRequirement {
                id: "continued_result".to_string(),
                description: "return the exact structured continuation result".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "continued_output_schema".to_string(),
                description: "continued output satisfies the declared schema".to_string(),
                requirement_ids: vec!["continued_result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 2,
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
            input_schema: io_schema.clone(),
            output_schema: io_schema.clone(),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["continued_result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: io_schema.clone(),
                operation: ExecutionOperation::Output {
                    value: json!({"result": {"$ref": "$.input.result"}}),
                },
                compensation: None,
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
    };
    format!(
        "api_version: moa.artifact/v1\nkind: skill\nmetadata:\n  name: {SKILL_NAME}\n  description: Deterministic admission replay template.\nstatus: draft\ndefinition:\n  type: skill\n  spec:\n    instructions:\n      path: SKILL.md\n    inputs: {}\n    outputs: {}\n    execution_plan: {}\n",
        serde_json::to_string(&io_schema).expect("serialize admission replay input schema"),
        serde_json::to_string(&io_schema).expect("serialize admission replay output schema"),
        serde_json::to_string(&template).expect("serialize admission replay execution template"),
    )
}

fn template_io_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["result"],
        "properties": {"result": {"type": "string"}}
    })
}

fn template_skill_markdown() -> &'static str {
    r#"---
name: admission-replay-output
description: Deterministic admission replay template.
---

# Admission Replay Output

Return the exact structured input after replaying the durable admission.
"#
}
