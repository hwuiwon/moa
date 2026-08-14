//! Hermetic durable-owner and crash-barrier recovery matrix for sandbox workspaces.
//!
//! Cloud provider APIs are intentionally absent. The fixture keeps Postgres,
//! Restate, OpenFGA, Valkey, RustFS, the KMS generation, and the local sandbox
//! root alive while hard-restarting only the orchestrator child.

#![cfg(all(feature = "integration", feature = "sandbox-workspace-failpoints"))]

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use moa_core::{
    events::Event,
    types::{
        action_policy::ActionPolicyEffect,
        events_stream::EventRange,
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceOperationId},
        sandbox_workspace::{WorkspaceCapacityDimension, WorkspaceOperationKind},
        worker::state::WorkerStatus,
    },
};
use moa_hands::core::{
    sandbox_workspace::capacity::{
        CapacityQuantity, CapacityReservationRequest, PostgresWorkspaceCapacityRepository,
    },
    sandbox_workspace::operations::{
        PostgresWorkspaceOperationRepository, WorkspaceOperationIntent,
    },
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::fixtures::fresh_client_message_id;
use moa_test_support::{
    IsolatedTest, OrchestratorTestFixture, SandboxWorkspaceCrashBarrier,
    SandboxWorkspaceCrashControl,
};
use moa_wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
    tenant_purge_operation_id,
};
use moa_wire::turn::StartTurnRequest;
use serde_json::json;
use uuid::Uuid;

const FIXTURE_TENANT_UUID: Uuid = Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001);
const FIXTURE_PROVIDER_ACCOUNT_UUID: Uuid =
    Uuid::from_u128(0x3000_0000_0000_0000_0000_0000_0000_0001);
const WORKER_REQUEST: &str = "Run the sandbox workspace crash-recovery probe.";
const WORKER_TASK: &str = "execute the sandbox workspace checkpoint recovery probe";
const WORKER_COMMAND: &str = "python3 -c 'import os,sys; p=\".moa-recovery-command-ran\"; sys.exit(97) if os.path.exists(p) else (open(p,\"x\").close(),print(\"workspace-recovery-ok\"))'";
const WORKER_ACTION_PATTERN: &str = "python3 *";

#[test]
fn recovery_command_is_policy_matchable_and_fails_second_execution_offline() -> Result<()> {
    // Pins: the crash probe is one policy-matchable shell command and its
    // durable marker makes an accidental second provider execution observable.
    assert!(moa_security::parse_and_match_command(
        WORKER_COMMAND,
        WORKER_ACTION_PATTERN
    )?);

    let workspace = tempfile::tempdir().context("create recovery command workspace")?;
    let first = std::process::Command::new("/bin/sh")
        .args(["-c", WORKER_COMMAND])
        .current_dir(workspace.path())
        .output()
        .context("execute recovery command once")?;
    assert!(first.status.success());
    assert_eq!(String::from_utf8(first.stdout)?, "workspace-recovery-ok\n");

    let second = std::process::Command::new("/bin/sh")
        .args(["-c", WORKER_COMMAND])
        .current_dir(workspace.path())
        .output()
        .context("execute recovery command twice")?;
    assert_eq!(second.status.code(), Some(97));
    Ok(())
}

fn recovery_script() -> serde_json::Value {
    json!({
        "default": {
            "completion": {
                "content": "unexpected sandbox workspace recovery fallback",
                "tool_calls": []
            }
        },
        "keyed": [
            {
                "match": "workspace-recovery-ok",
                "completion": {
                    "content": "Workspace recovery worker completed.",
                    "tool_calls": []
                }
            },
            {
                "match": WORKER_TASK,
                "completion": {
                    "content": "Running the workspace recovery probe.",
                    "tool_calls": [{
                        "name": "bash",
                        "id": "sandbox-workspace-recovery-bash",
                        "input": { "cmd": WORKER_COMMAND }
                    }]
                }
            },
            {
                "match": "Spawned worker",
                "completion": {
                    "content": "Workspace recovery worker dispatched.",
                    "tool_calls": []
                }
            },
            {
                "match": "You classify one user turn into MOA's public execution decision.",
                "completion": {
                    "content": json!({
                        "label": "execute",
                        "strategy": "inline",
                        "rationale": "The request delegates one bounded sandbox probe.",
                        "confidence_bps": 10_000,
                        "missing_inputs": []
                    }).to_string(),
                    "tool_calls": []
                }
            },
            {
                "match": WORKER_REQUEST,
                "completion": {
                    "content": "Delegating the workspace recovery probe.",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "id": "spawn-sandbox-workspace-recovery",
                        "input": {
                            "task": WORKER_TASK,
                            "tool_subset": ["bash"],
                            "budget_tokens": 1_200,
                            "max_turns": 2
                        }
                    }]
                }
            }
        ]
    })
}

fn local_admission_env() -> Vec<(String, String)> {
    let account_id = FIXTURE_PROVIDER_ACCOUNT_UUID;
    vec![
        (
            "MOA_LOCAL_PROVIDER_ACCOUNT_JSON".to_string(),
            json!({
                "provider_account_id": account_id,
                "generation": 1,
                "isolation_cell": "local-fixture-a"
            })
            .to_string(),
        ),
        (
            "MOA_SANDBOX_WORKSPACE_MODE".to_string(),
            "admit".to_string(),
        ),
        (
            "MOA_SANDBOX_WORKSPACE_CANARY_JSON".to_string(),
            json!({
                "provider_account_id": account_id,
                "provider_account_generation": 1,
                "isolation_cell": "local-fixture-a",
                "tenant_allowlist": [FIXTURE_TENANT_UUID]
            })
            .to_string(),
        ),
        (
            "MOA_SANDBOX_WORKSPACE_QUOTA_ROUTES_JSON".to_string(),
            json!([{
                "tenant_id": FIXTURE_TENANT_UUID,
                "provider_account_id": account_id,
                "provider_account_generation": 1,
                "max_workspaces": 64,
                "max_active_hands": 16,
                "max_checkpoints": 256,
                "max_logical_bytes": 1_073_741_824_u64
            }])
            .to_string(),
        ),
        (
            "MOA_AUTHZ_OPENFGA_MODEL_VERSION".to_string(),
            "7".to_string(),
        ),
    ]
}

async fn allow_worker_bash(fixture: &OrchestratorTestFixture) -> Result<()> {
    let tenant_id = TenantId::from(FIXTURE_TENANT_UUID);
    fixture.grant_default_tenant_admin(tenant_id).await?;
    fixture
        .client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id,
                contact_id: None,
                tool_name: "bash".to_string(),
                pattern: WORKER_ACTION_PATTERN.to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("deterministic sandbox workspace recovery fixture".to_string()),
            },
        )
        .await
}

async fn start_worker_probe(
    test: &IsolatedTest<'_>,
    suffix: &str,
) -> Result<(moa_core::types::identifiers::SessionId, String)> {
    let session_id = test.create_session(suffix).await?;
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: WORKER_REQUEST.to_string(),
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
        .context("idle workspace recovery session must start immediately")?;
    Ok((session_id, turn_id))
}

async fn pre_barrier_diagnostic(
    fixture: &OrchestratorTestFixture,
    test: &IsolatedTest<'_>,
    session_id: moa_core::types::identifiers::SessionId,
) -> Result<serde_json::Value> {
    let typed_events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    let worker_id = typed_events.iter().find_map(|record| match &record.event {
        Event::WorkerSpawned { worker_id, .. } => Some(worker_id.clone()),
        _ => None,
    });
    let worker_status = if let Some(worker_id) = worker_id.as_ref() {
        Some(
            fixture
                .client
                .post_empty_call::<WorkerStatus>(&format!("/Worker/{worker_id}/status"))
                .await,
        )
    } else {
        None
    };
    let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
    let durable_state: serde_json::Value = sqlx::query_scalar(
        r#"
        SELECT jsonb_build_object(
            'session', COALESCE((
                SELECT to_jsonb(session_row)
                FROM public.sessions AS session_row
                WHERE session_row.id = $1
            ), 'null'::jsonb),
            'events', COALESCE((
                SELECT jsonb_agg(to_jsonb(event_row) ORDER BY event_row.sequence_num)
                FROM public.events AS event_row
                WHERE event_row.session_id = $1
            ), '[]'::jsonb),
            'action_policy_rules', COALESCE((
                SELECT jsonb_agg(to_jsonb(rule_row) ORDER BY rule_row.created_at)
                FROM public.action_policy_rules AS rule_row
                WHERE rule_row.tenant_id = $2
            ), '[]'::jsonb),
            'action_reviews', COALESCE((
                SELECT jsonb_agg(to_jsonb(review_row) ORDER BY review_row.created_at)
                FROM public.tenant_action_reviews AS review_row
                WHERE review_row.tenant_id = $2
            ), '[]'::jsonb),
            'provider_accounts', COALESCE((
                SELECT jsonb_agg(to_jsonb(account_row) ORDER BY account_row.provider_account_id)
                FROM moa.sandbox_provider_accounts AS account_row
            ), '[]'::jsonb),
            'capacity_limits', COALESCE((
                SELECT jsonb_agg(to_jsonb(limit_row) ORDER BY limit_row.tenant_id)
                FROM moa.sandbox_tenant_capacity_limits AS limit_row
                WHERE limit_row.tenant_id = $2
            ), '[]'::jsonb),
            'workspaces', COALESCE((
                SELECT jsonb_agg(to_jsonb(workspace_row) ORDER BY workspace_row.created_at)
                FROM moa.sandbox_workspaces AS workspace_row
                WHERE workspace_row.tenant_id = $2
            ), '[]'::jsonb),
            'operations', COALESCE((
                SELECT jsonb_agg(to_jsonb(operation_row) ORDER BY operation_row.created_at)
                FROM moa.sandbox_workspace_operations AS operation_row
                WHERE operation_row.tenant_id = $2
            ), '[]'::jsonb),
            'reservations', COALESCE((
                SELECT jsonb_agg(to_jsonb(reservation_row) ORDER BY reservation_row.created_at)
                FROM moa.sandbox_capacity_reservations AS reservation_row
                WHERE reservation_row.tenant_id = $2
            ), '[]'::jsonb),
            'storage_resources', COALESCE((
                SELECT jsonb_agg(to_jsonb(storage_row) ORDER BY storage_row.created_at)
                FROM moa.sandbox_storage_resources AS storage_row
                WHERE storage_row.tenant_id = $2
            ), '[]'::jsonb)
        )
        "#,
    )
    .bind(session_id)
    .bind(FIXTURE_TENANT_UUID)
    .fetch_one(&pool)
    .await?;
    pool.close().await;

    Ok(json!({
        "durable_state": durable_state,
        "worker_id": worker_id,
        "worker_status": worker_status.map(|status| format!("{status:?}")),
        "scripted_provider_requests": fixture.scripted_requests()?,
        "child_exit": fixture.unexpected_orchestrator_exit().await?,
    }))
}

fn bash_event_counts(events: &[moa_core::types::events_stream::EventRecord]) -> (usize, usize) {
    let calls = events
        .iter()
        .filter(|record| matches!(&record.event, Event::ToolCall { tool_name, .. } if tool_name == "bash"))
        .count();
    let results = events
        .iter()
        .filter(|record| matches!(&record.event, Event::ToolResult { provider_tool_use_id: Some(id), output, success, .. } if id == "sandbox-workspace-recovery-bash" && *success && output.to_text().contains("workspace-recovery-ok")))
        .count();
    (calls, results)
}

async fn wait_for_one_bash_result(
    fixture: &OrchestratorTestFixture,
    test: &IsolatedTest<'_>,
    session_id: moa_core::types::identifiers::SessionId,
    barrier: SandboxWorkspaceCrashBarrier,
) -> Result<Vec<moa_core::types::events_stream::EventRecord>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let events = test
            .client()
            .get_events(session_id, EventRange::all())
            .await?;
        if bash_event_counts(&events) == (1, 1) {
            return Ok(events);
        }
        if tokio::time::Instant::now() >= deadline {
            let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
            let durable_state: serde_json::Value = sqlx::query_scalar(
                r#"
                SELECT jsonb_build_object(
                    'workspaces', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'workspace_id', workspace_id,
                            'state', lifecycle_state,
                            'writer_epoch', writer_epoch,
                            'instance_generation', instance_generation,
                            'checkpoint_generation', current_checkpoint_generation
                        ) ORDER BY created_at)
                        FROM moa.sandbox_workspaces WHERE tenant_id = $1
                    ), '[]'::jsonb),
                    'operations', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'operation_id', operation_id,
                            'kind', operation_kind,
                            'outcome', outcome_class,
                            'claim_token', claim_token,
                            'claim_expires_at', claim_expires_at,
                            'retry_not_before', retry_not_before,
                            'reconcile_not_before', reconcile_not_before
                        ) ORDER BY created_at)
                        FROM moa.sandbox_workspace_operations WHERE tenant_id = $1
                    ), '[]'::jsonb),
                    'checkpoints', COALESCE((
                        SELECT jsonb_agg(to_jsonb(checkpoint_row) ORDER BY checkpoint_row.created_at)
                        FROM moa.sandbox_workspace_checkpoints AS checkpoint_row
                        WHERE checkpoint_row.tenant_id = $1
                    ), '[]'::jsonb),
                    'reservations', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'operation_id', operation_id,
                            'dimension', resource_dimension,
                            'state', reservation_state,
                            'expires_at', expires_at
                        ) ORDER BY created_at)
                        FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1
                    ), '[]'::jsonb)
                )
                "#,
            )
            .bind(FIXTURE_TENANT_UUID)
            .fetch_one(&pool)
            .await?;
            pool.close().await;
            let child_exit = fixture.unexpected_orchestrator_exit().await?;
            bail!(
                "workspace recovery replay after {} did not converge to one bash call/result; counts={:?}; child_exit={child_exit:?}; durable_state={durable_state}",
                barrier.as_str(),
                bash_event_counts(&events),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn seed_ambiguous_absent_operation(
    pool: &sqlx::PgPool,
    sandbox_root: &Path,
) -> Result<WorkspaceOperationId> {
    let tenant_id = TenantId::from(FIXTURE_TENANT_UUID);
    let row: (
        SandboxWorkspaceId,
        ProviderAccountId,
        i64,
        i64,
        i64,
        i64,
        Vec<Uuid>,
    ) = sqlx::query_as(
        r#"
        SELECT workspace_id, provider_account_id, provider_account_generation,
               writer_epoch, instance_generation, current_checkpoint_generation,
               ARRAY(
                   SELECT lease.provisioning_operation_id
                   FROM moa.hand_leases AS lease
                   WHERE lease.tenant_id = workspace.tenant_id
                     AND lease.workspace_id = workspace.workspace_id
               )
        FROM moa.sandbox_workspaces AS workspace
        WHERE tenant_id = $1 AND scope_kind = 'worker' AND provider = 'local'
        ORDER BY workspace.created_at
        LIMIT 1
        "#,
    )
    .bind(FIXTURE_TENANT_UUID)
    .fetch_one(pool)
    .await
    .context("load one terminal worker workspace for absence reconciliation")?;
    let operation_id = WorkspaceOperationId::new();
    let now = chrono::Utc::now();
    let request = CapacityReservationRequest {
        tenant_id,
        workspace_id: row.0,
        operation_id,
        provider_account_id: row.1,
        provider_account_generation: row.2,
        expected_writer_epoch: row.3,
        expected_instance_generation: row.4,
        quantities: vec![CapacityQuantity {
            dimension: WorkspaceCapacityDimension::Volumes,
            quantity: 1,
        }],
    };
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id: row.0,
            provider_account_id: row.1,
            provider_account_generation: row.2,
            kind: WorkspaceOperationKind::Delete,
            request_hash: format!("sha256:recovery-absence-{operation_id}"),
            expected_writer_epoch: row.3,
            expected_instance_generation: row.4,
            expected_checkpoint_generation: row.5,
            deadline_at: now - chrono::Duration::seconds(1),
            // Hold the synthetic operation away from the running reaper until
            // the exact local provider state is removed and the child restarts.
            reconcile_not_before: now + chrono::Duration::hours(1),
        })
        .await
        .context("persist exact ambiguous-delete recovery intent")?;
    let reservations = PostgresWorkspaceCapacityRepository::new(pool.clone())
        .reserve(&request)
        .await
        .context("reserve exact absent storage-operation capacity owner")?;
    assert_eq!(reservations.len(), 1);
    assert!(
        operations
            .mark_unknown(tenant_id, operation_id)
            .await
            .context("mark delete outcome ambiguous")?
    );
    for provisioning_operation_id in row.6 {
        let sandbox_dir = sandbox_root.join(format!("hand-{provisioning_operation_id}"));
        match tokio::fs::remove_dir_all(&sandbox_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "remove absent-provider fixture root {}",
                        sandbox_dir.display()
                    )
                });
            }
        }
        let trusted_dir = sandbox_root
            .join(".moa-hand-trusted")
            .join(provisioning_operation_id.to_string());
        match tokio::fs::remove_dir_all(&trusted_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "remove absent-provider trusted root {}",
                        trusted_dir.display()
                    )
                });
            }
        }
        let marker = sandbox_root
            .join(".moa-hand-intents")
            .join(format!("{provisioning_operation_id}.json"));
        match tokio::fs::remove_file(&marker).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove absent-provider intent marker {}", marker.display())
                });
            }
        }
    }
    Ok(operation_id)
}

async fn make_absent_operation_due(
    pool: &sqlx::PgPool,
    operation_id: WorkspaceOperationId,
) -> Result<()> {
    let affected = sqlx::query(
        r#"
        UPDATE moa.sandbox_workspace_operations
        SET reconcile_not_before = now(), updated_at = now()
        WHERE tenant_id = $1 AND operation_id = $2
          AND outcome_class = 'unknown' AND claim_token IS NULL
        "#,
    )
    .bind(FIXTURE_TENANT_UUID)
    .bind(operation_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        bail!("synthetic absent operation lost its pre-reconciliation fence");
    }
    Ok(())
}

async fn wait_for_absence_release(
    pool: &sqlx::PgPool,
    operation_id: WorkspaceOperationId,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let state: Option<serde_json::Value> = sqlx::query_scalar(
            r#"
            SELECT jsonb_build_object(
                'outcome', operation.outcome_class,
                'disposition', operation.confirmed_disposition,
                'claim_token', operation.claim_token,
                'claim_expires_at', operation.claim_expires_at,
                'retry_not_before', operation.retry_not_before,
                'reconcile_not_before', operation.reconcile_not_before,
                'attempts', operation.attempts,
                'absence_observation_count', operation.absence_observation_count,
                'absence_first_observed_at', operation.absence_first_observed_at,
                'absence_last_observed_at', operation.absence_last_observed_at,
                'absence_inventory_digest', operation.absence_inventory_digest,
                'reservation_state', reservation.reservation_state,
                'database_now', now()
            )
            FROM moa.sandbox_workspace_operations AS operation
            LEFT JOIN moa.sandbox_capacity_reservations AS reservation
              ON reservation.tenant_id = operation.tenant_id
             AND reservation.operation_id = operation.operation_id
            WHERE operation.operation_id = $1
            "#,
        )
        .bind(operation_id)
        .fetch_optional(pool)
        .await?;
        if state.as_ref().is_some_and(|state| {
            state.get("outcome").and_then(serde_json::Value::as_str) == Some("confirmed")
                && state.get("disposition").and_then(serde_json::Value::as_str)
                    == Some("resource_absent")
                && state
                    .get("reservation_state")
                    .and_then(serde_json::Value::as_str)
                    == Some("released")
        }) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "absence reconciliation did not atomically confirm and release operation {operation_id}; state={state:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_tenant_purge(fixture: &OrchestratorTestFixture) -> Result<()> {
    let tenant_id = TenantId::from(FIXTURE_TENANT_UUID);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let status = fixture
            .client
            .post_call::<_, TenantPurgeStatusResponse>(
                &format!("/TenantPurge/{tenant_id}/status"),
                &TenantPurgeStatusRequest { tenant_id },
            )
            .await;
        if status
            .as_ref()
            .is_ok_and(|status| status.status == TenantPurgeStatus::AnalyticsPurged)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
            let durable_state: serde_json::Value = sqlx::query_scalar(
                r#"
                SELECT jsonb_build_object(
                    'purge', COALESCE((
                        SELECT to_jsonb(purge_row)
                        FROM moa.tenant_purge_operations AS purge_row
                        WHERE purge_row.tenant_id = $1
                    ), 'null'::jsonb),
                    'workspaces', COALESCE((
                        SELECT jsonb_agg(to_jsonb(workspace_row) ORDER BY workspace_row.created_at)
                        FROM moa.sandbox_workspaces AS workspace_row
                        WHERE workspace_row.tenant_id = $1
                    ), '[]'::jsonb),
                    'operations', COALESCE((
                        SELECT jsonb_agg(to_jsonb(operation_row) ORDER BY operation_row.created_at)
                        FROM moa.sandbox_workspace_operations AS operation_row
                        WHERE operation_row.tenant_id = $1
                    ), '[]'::jsonb),
                    'checkpoints', COALESCE((
                        SELECT jsonb_agg(to_jsonb(checkpoint_row) ORDER BY checkpoint_row.created_at)
                        FROM moa.sandbox_workspace_checkpoints AS checkpoint_row
                        WHERE checkpoint_row.tenant_id = $1
                    ), '[]'::jsonb),
                    'storage_resources', COALESCE((
                        SELECT jsonb_agg(to_jsonb(storage_row) ORDER BY storage_row.created_at)
                        FROM moa.sandbox_storage_resources AS storage_row
                        WHERE storage_row.tenant_id = $1
                    ), '[]'::jsonb),
                    'database_now', now()
                )
                "#,
            )
            .bind(FIXTURE_TENANT_UUID)
            .fetch_one(&pool)
            .await?;
            pool.close().await;
            let invocation_admin = match reqwest::get(format!(
                "{}/invocations?limit=100",
                fixture.admin_url.trim_end_matches('/')
            ))
            .await
            {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    format!("{status}: {body}")
                }
                Err(error) => format!("request failed: {error}"),
            };
            let child_exit = fixture.unexpected_orchestrator_exit().await?;
            bail!(
                "tenant purge did not converge after provider-delete crash: {status:?}; child_exit={child_exit:?}; durable_state={durable_state}; restate_invocations={invocation_admin}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn provider_delete_barrier_diagnostic(
    fixture: &OrchestratorTestFixture,
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
) -> serde_json::Value {
    let purge_operation_id = tenant_purge_operation_id(tenant_id);
    let phase_trace = fixture
        .sandbox_workspace()
        .map(|workspace| workspace.purge_external_phase_trace(&purge_operation_id));
    let phase_trace = match phase_trace {
        Ok(trace) => match trace.await {
            Ok(trace) => json!({
                "entered_count": trace.iter().filter(|phase| phase.as_str() == "entered").count(),
                "trace": trace,
            }),
            Err(error) => json!({ "error": format!("{error:#}") }),
        },
        Err(error) => json!({ "error": format!("{error:#}") }),
    };

    let durable_state = match sqlx::query_scalar::<_, serde_json::Value>(
        r#"
        SELECT jsonb_build_object(
            'purge_operation', COALESCE((
                SELECT to_jsonb(purge_row)
                FROM moa.tenant_purge_operations AS purge_row
                WHERE purge_row.tenant_id = $1
            ), 'null'::jsonb),
            'purge_summary', COALESCE((
                SELECT jsonb_build_object(
                    'operation_id', purge_row.operation_id,
                    'status', purge_row.status,
                    'current_stage', purge_row.current_stage
                )
                FROM moa.tenant_purge_operations AS purge_row
                WHERE purge_row.tenant_id = $1
            ), 'null'::jsonb),
            'workspaces', COALESCE((
                SELECT jsonb_agg(to_jsonb(workspace_row) ORDER BY workspace_row.created_at)
                FROM moa.sandbox_workspaces AS workspace_row
                WHERE workspace_row.tenant_id = $1
            ), '[]'::jsonb),
            'destruction_fences', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'workspace_id', workspace_row.workspace_id,
                    'lifecycle_state', workspace_row.lifecycle_state,
                    'access_fenced_at', workspace_row.access_fenced_at
                ) ORDER BY workspace_row.created_at)
                FROM moa.sandbox_workspaces AS workspace_row
                WHERE workspace_row.tenant_id = $1
            ), '[]'::jsonb),
            'workspace_counts', (
                SELECT jsonb_build_object(
                    'total', count(*),
                    'fenced', count(*) FILTER (WHERE workspace_row.access_fenced_at IS NOT NULL)
                )
                FROM moa.sandbox_workspaces AS workspace_row
                WHERE workspace_row.tenant_id = $1
            ),
            'operations', COALESCE((
                SELECT jsonb_agg(to_jsonb(operation_row) ORDER BY operation_row.created_at)
                FROM moa.sandbox_workspace_operations AS operation_row
                WHERE operation_row.tenant_id = $1
            ), '[]'::jsonb),
            'checkpoints', COALESCE((
                SELECT jsonb_agg(to_jsonb(checkpoint_row) ORDER BY checkpoint_row.created_at)
                FROM moa.sandbox_workspace_checkpoints AS checkpoint_row
                WHERE checkpoint_row.tenant_id = $1
            ), '[]'::jsonb),
            'storage_resources', COALESCE((
                SELECT jsonb_agg(to_jsonb(storage_row) ORDER BY storage_row.created_at)
                FROM moa.sandbox_storage_resources AS storage_row
                WHERE storage_row.tenant_id = $1
            ), '[]'::jsonb),
            'hand_leases', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'session_id', hand_row.session_id,
                    'worker_id', hand_row.worker_id,
                    'provider', hand_row.provider,
                    'status', hand_row.status,
                    'generation', hand_row.generation,
                    'idle_expires_at', hand_row.idle_expires_at,
                    'hard_expires_at', hand_row.hard_expires_at,
                    'reap_attempts', hand_row.reap_attempts,
                    'reap_not_before', hand_row.reap_not_before,
                    'reap_claim_token', hand_row.reap_claim_token,
                    'reap_claim_expires_at', hand_row.reap_claim_expires_at
                ) ORDER BY hand_row.created_at)
                FROM moa.hand_leases AS hand_row
                WHERE hand_row.tenant_id = $1
            ), '[]'::jsonb),
            'active_vector_claims', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'id', vector_row.id,
                    'storage_partition_id', vector_row.storage_partition_id,
                    'claim_token', vector_row.claim_token,
                    'claim_expires_at', vector_row.claim_expires_at,
                    'processed_at', vector_row.processed_at,
                    'database_now', now()
                ) ORDER BY vector_row.id)
                FROM moa.vector_sync_outbox AS vector_row
                WHERE vector_row.storage_partition_id = $2
                  AND vector_row.processed_at IS NULL
                  AND vector_row.claim_token IS NOT NULL
            ), '[]'::jsonb),
            'counts', jsonb_build_object(
                'operations', (SELECT count(*) FROM moa.sandbox_workspace_operations WHERE tenant_id = $1),
                'checkpoints', (SELECT count(*) FROM moa.sandbox_workspace_checkpoints WHERE tenant_id = $1),
                'storage_resources', (SELECT count(*) FROM moa.sandbox_storage_resources WHERE tenant_id = $1),
                'hand_leases', (SELECT count(*) FROM moa.hand_leases WHERE tenant_id = $1)
            ),
            'database_now', now()
        )
        "#,
    )
    .bind(FIXTURE_TENANT_UUID)
    .bind(FIXTURE_TENANT_UUID.to_string())
    .fetch_one(pool)
    .await
    {
        Ok(state) => state,
        Err(error) => json!({ "error": format!("{error:#}") }),
    };

    let restate_invocations = match reqwest::get(format!(
        "{}/invocations?limit=100",
        fixture.admin_url.trim_end_matches('/')
    ))
    .await
    {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            json!({ "status": status.as_u16(), "body": body })
        }
        Err(error) => json!({ "error": format!("{error:#}") }),
    };
    let child_exit = match fixture.unexpected_orchestrator_exit().await {
        Ok(exit) => json!(exit),
        Err(error) => json!({ "error": format!("{error:#}") }),
    };

    json!({
        "purge_operation_id": purge_operation_id,
        "phase_log": phase_trace,
        "durable_state": durable_state,
        "child_exit": child_exit,
        "child_logs": "inherited by nextest and captured in the test system-out stream",
        "restate_invocations": restate_invocations,
    })
}

#[tokio::test]
#[ignore = "requires Docker for isolated Postgres, Restate, OpenFGA, Valkey, and RustFS"]
async fn recovery_matrix_sandbox_workspace_durable_owners_survive_child_crashes_service_e2e()
-> Result<()> {
    // Pins: every feature-qualified barrier can be armed on a replacement child
    // without replacing any durable owner; Postgres KMS unwrap, exact RustFS
    // bytes, and exact local-root bytes remain stable through every SIGKILL.
    // The lifecycle-driving test below crosses each production hook separately.
    let fixture = OrchestratorTestFixture::with_sandbox_workspace_fixture(
        json!({
            "default": {
                "completion": {
                    "content": "sandbox workspace recovery fixture",
                    "duration_ms": 1,
                    "input_tokens": 1,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "tool_calls": []
                }
            }
        }),
        Vec::new(),
    )
    .await
    .context("start restart-stable sandbox workspace fixture")?;
    let durable = fixture.sandbox_workspace()?;
    let expected = b"sandbox-workspace-recovery-matrix-exact-checkpoint-v1";
    let probe = durable
        .prepare_restart_probe(&fixture.postgres_url, expected)
        .await
        .context("prepare durable KMS, RustFS, and sandbox-root probe")?;

    for barrier in SandboxWorkspaceCrashBarrier::all() {
        let control = SandboxWorkspaceCrashControl::new(barrier)
            .with_context(|| format!("create isolated {} control", barrier.as_str()))?;
        let environment = control.orchestrator_env();
        assert_eq!(environment.len(), 3);
        assert_eq!(environment[0].1.as_str(), barrier.as_str());

        fixture
            .restart_orchestrator_with_env(environment)
            .await
            .with_context(|| format!("arm {} on the child", barrier.as_str()))?;
        assert_eq!(
            fixture.unexpected_orchestrator_exit().await?,
            None,
            "feature-qualified child must remain healthy before the selected lifecycle reaches {}",
            barrier.as_str()
        );
        fixture
            .hard_crash_and_restart_orchestrator()
            .await
            .with_context(|| format!("hard restart child armed for {}", barrier.as_str()))?;
        durable
            .verify_restart_probe(&fixture.postgres_url, &probe)
            .await
            .with_context(|| format!("verify all durable owners after {}", barrier.as_str()))?;
    }

    // The health checks above establish that each replacement child can reopen
    // all service dependencies. This short second observation catches a child
    // that exits immediately after service registration.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fixture.unexpected_orchestrator_exit().await?, None);
    fixture.cleanup_sandbox_workspace_namespace().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for isolated Postgres, Restate, OpenFGA, Valkey, and RustFS"]
async fn recovery_matrix_sandbox_workspace_all_six_barriers_replay_once_service_e2e() -> Result<()>
{
    // Pins: all six durable windows are observed in the real child process.
    // Restate replay converges without a duplicate tool result, workspace/head,
    // provider deletion, or capacity owner.
    let fixture = OrchestratorTestFixture::with_sandbox_workspace_fixture(
        recovery_script(),
        local_admission_env(),
    )
    .await
    .context("start admitted local workspace recovery fixture")?;
    allow_worker_bash(&fixture).await?;
    let test = fixture.isolated().await;
    let barriers = [
        SandboxWorkspaceCrashBarrier::PostReservationPreProviderCreate,
        SandboxWorkspaceCrashBarrier::PostProviderCreatePreActivation,
        SandboxWorkspaceCrashBarrier::PostCommandPreCheckpointPublication,
        SandboxWorkspaceCrashBarrier::PostCheckpointReadyPreHeadCas,
    ];

    for (ordinal, barrier) in barriers.into_iter().enumerate() {
        let control = SandboxWorkspaceCrashControl::new(barrier)?;
        fixture
            .restart_orchestrator_with_env(control.orchestrator_env())
            .await
            .with_context(|| format!("arm production barrier {}", barrier.as_str()))?;
        let (session_id, _turn_id) =
            start_worker_probe(&test, &format!("workspace-recovery-{ordinal}")).await?;
        if let Err(error) = control.wait_until_reached(Duration::from_secs(90)).await {
            let diagnostic = pre_barrier_diagnostic(&fixture, &test, session_id)
                .await
                .context("collect pre-barrier recovery diagnostic")?;
            bail!(
                "observe production barrier {}: {error:#}; diagnostic={diagnostic}",
                barrier.as_str()
            );
        }
        let before = test
            .client()
            .get_events(session_id, EventRange::all())
            .await?;
        assert_eq!(
            bash_event_counts(&before).1,
            0,
            "ToolResult cannot escape before the checkpoint/head commit at {}",
            barrier.as_str()
        );

        fixture
            .hard_crash_and_restart_orchestrator()
            .await
            .with_context(|| format!("hard restart at {}", barrier.as_str()))?;
        let events = wait_for_one_bash_result(&fixture, &test, session_id, barrier).await?;
        assert_eq!(bash_event_counts(&events), (1, 1));

        let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
        let workspace_state: (i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT count(*)::BIGINT,
                   count(*) FILTER (WHERE current_checkpoint_id IS NOT NULL)::BIGINT,
                   count(DISTINCT (scope_session_id, scope_worker_id))::BIGINT
            FROM moa.sandbox_workspaces
            WHERE tenant_id = $1 AND scope_kind = 'worker'
            "#,
        )
        .bind(FIXTURE_TENANT_UUID)
        .fetch_one(&pool)
        .await?;
        let expected = i64::try_from(ordinal + 1).context("workspace count fits i64")?;
        assert_eq!(workspace_state, (expected, expected, expected));
        let charged: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.sandbox_capacity_reservations \
             WHERE tenant_id = $1 AND reservation_state IN ('pending', 'reconciling')",
        )
        .bind(FIXTURE_TENANT_UUID)
        .fetch_one(&pool)
        .await?;
        assert_eq!(charged, 0, "terminal recovery cannot leak a capacity owner");
        pool.close().await;
    }

    let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
    let operation_id =
        seed_ambiguous_absent_operation(&pool, fixture.sandbox_workspace()?.sandbox_root()).await?;
    let absence_control = SandboxWorkspaceCrashControl::new(
        SandboxWorkspaceCrashBarrier::PostAbsenceConfirmationPreReservationRelease,
    )?;
    fixture
        .restart_execution_maintenance_owner_with_env(absence_control.orchestrator_env())
        .await?;
    make_absent_operation_due(&pool, operation_id).await?;
    if let Err(error) = absence_control
        .wait_until_reached(Duration::from_secs(120))
        .await
    {
        let durable_state: serde_json::Value = sqlx::query_scalar(
            r#"
            SELECT jsonb_build_object(
                'operation', to_jsonb(operation),
                'reservation', to_jsonb(reservation),
                'workspace', to_jsonb(workspace),
                'leases', COALESCE((
                    SELECT jsonb_agg(to_jsonb(lease) ORDER BY lease.created_at)
                    FROM moa.hand_leases AS lease
                    WHERE lease.tenant_id = operation.tenant_id
                      AND lease.workspace_id = operation.workspace_id
                ), '[]'::jsonb),
                'database_now', now()
            )
            FROM moa.sandbox_workspace_operations AS operation
            JOIN moa.sandbox_capacity_reservations AS reservation
              ON reservation.tenant_id = operation.tenant_id
             AND reservation.operation_id = operation.operation_id
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = operation.tenant_id
             AND workspace.workspace_id = operation.workspace_id
            WHERE operation.operation_id = $1
            "#,
        )
        .bind(operation_id)
        .fetch_one(&pool)
        .await?;
        let mut local_entries = Vec::new();
        let mut entries = tokio::fs::read_dir(fixture.sandbox_workspace()?.sandbox_root()).await?;
        while let Some(entry) = entries.next_entry().await? {
            local_entries.push(entry.file_name().to_string_lossy().into_owned());
        }
        local_entries.sort();
        let child_exit = fixture.unexpected_orchestrator_exit().await?;
        bail!(
            "observe post-absence-confirmation/pre-reservation-release barrier: {error:#}; durable_state={durable_state}; local_entries={local_entries:?}; child_exit={child_exit:?}"
        );
    }
    let before_absence_crash: (String, String) = sqlx::query_as(
        r#"
        SELECT operation.outcome_class, reservation.reservation_state
        FROM moa.sandbox_workspace_operations AS operation
        JOIN moa.sandbox_capacity_reservations AS reservation
          ON reservation.tenant_id = operation.tenant_id
         AND reservation.operation_id = operation.operation_id
        WHERE operation.operation_id = $1
        "#,
    )
    .bind(operation_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        before_absence_crash,
        ("unknown".to_string(), "reconciling".to_string())
    );
    fixture
        .hard_crash_and_restart_execution_maintenance_owner()
        .await?;
    wait_for_absence_release(&pool, operation_id).await?;

    let delete_control = SandboxWorkspaceCrashControl::new(
        SandboxWorkspaceCrashBarrier::PostProviderDeletePreDurableConfirmation,
    )?;
    fixture
        .restart_orchestrator_with_env(delete_control.orchestrator_env())
        .await?;
    let client = fixture.client.clone();
    let tenant_id = TenantId::from(FIXTURE_TENANT_UUID);
    let purge_call = tokio::spawn(async move {
        client
            .post_call::<_, TenantPurgeStatusResponse>(
                &format!("/TenantPurge/{tenant_id}/run"),
                &TenantPurgeRequest { tenant_id },
            )
            .await
    });
    if let Err(error) = delete_control
        .wait_until_reached(Duration::from_secs(120))
        .await
    {
        let diagnostic = provider_delete_barrier_diagnostic(&fixture, &pool, tenant_id).await;
        bail!(
            "observe post-provider-delete/pre-durable-confirmation barrier: {error:#}; diagnostic={diagnostic}"
        );
    }
    let fenced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.sandbox_workspaces \
         WHERE tenant_id = $1 AND access_fenced_at IS NOT NULL",
    )
    .bind(FIXTURE_TENANT_UUID)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        fenced, 4,
        "tenant purge must fence every workspace before external deletion"
    );
    fixture.hard_crash_and_restart_orchestrator().await?;
    purge_call.abort();
    let _ = purge_call.await;
    wait_for_tenant_purge(&fixture).await?;
    let purge_operation_id = tenant_purge_operation_id(tenant_id);
    assert_eq!(
        fixture
            .sandbox_workspace()?
            .purge_external_phase_count(&purge_operation_id)
            .await?,
        1,
        "Restate replay must reuse the journaled external-delete proof instead of re-entering provider deletion"
    );
    let remaining: (i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
          (SELECT count(*) FROM moa.sandbox_workspaces WHERE tenant_id = $1)::BIGINT,
          (SELECT count(*) FROM moa.sandbox_workspace_operations WHERE tenant_id = $1)::BIGINT,
          (SELECT count(*) FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1)::BIGINT,
          (SELECT count(*) FROM moa.sandbox_provider_inventory_findings
           WHERE provider_account_id = $2
             AND provider_account_generation = 1
             AND resolved_at IS NULL)::BIGINT
        "#,
    )
    .bind(FIXTURE_TENANT_UUID)
    .bind(FIXTURE_PROVIDER_ACCOUNT_UUID)
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining, (0, 0, 0, 0));
    pool.close().await;
    fixture.cleanup_sandbox_workspace_namespace().await?;
    Ok(())
}
