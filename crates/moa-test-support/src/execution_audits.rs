//! Readers for normalized execution-planning audits used by integration tests.

use anyhow::{Context, Result};
use moa_core::types::execution_planning::ExecutionPlanningAuditEnvelopeV1;
use moa_core::types::identifiers::SessionId;
use serde_json::Value;
use sqlx::PgPool;

/// Loads one session's normalized route, planner-call, and compiler audits in recorded order.
pub async fn load_execution_planning_audits(
    postgres_url: &str,
    session_id: SessionId,
) -> Result<Vec<ExecutionPlanningAuditEnvelopeV1>> {
    let pool = PgPool::connect(postgres_url)
        .await
        .context("connect normalized execution-audit reader")?;
    let values: Vec<Value> = sqlx::query_scalar(
        r#"
        SELECT envelope
        FROM (
            SELECT
                accepted_at AS recorded_at,
                0 AS audit_order,
                jsonb_build_object(
                    'schema_version', 1,
                    'tenant_id', tenant_id,
                    'contact_id', contact_id,
                    'session_id', session_id,
                    'originating_sequence', originating_sequence,
                    'payload', jsonb_build_object(
                        'kind', 'route',
                        'stage', stage,
                        'decision', decision,
                        'mode', mode,
                        'reason', reason,
                        'accepted_at', accepted_at
                    )
                ) AS envelope
            FROM moa.execution_route_audit
            WHERE session_id = $1

            UNION ALL

            SELECT
                created_at AS recorded_at,
                1 AS audit_order,
                jsonb_build_object(
                    'schema_version', 1,
                    'tenant_id', tenant_id,
                    'contact_id', contact_id,
                    'session_id', session_id,
                    'originating_sequence', originating_sequence,
                    'payload', jsonb_build_object(
                        'kind', 'planner_call',
                        'call_kind', call_kind,
                        'call_ordinal', call_ordinal,
                        'run_uid', run_uid,
                        'plan_revision', plan_revision,
                        'outcome', outcome,
                        'provider_model', provider_model,
                        'prompt_version', prompt_version,
                        'candidate_hash', candidate_hash,
                        'candidate_json', candidate_json::TEXT,
                        'compiler_report', compiler_report::TEXT,
                        'duration_micros', duration_micros,
                        'created_at', created_at
                    )
                ) AS envelope
            FROM moa.execution_planner_call_audit
            WHERE session_id = $1

            UNION ALL

            SELECT
                created_at AS recorded_at,
                2 AS audit_order,
                jsonb_build_object(
                    'schema_version', 1,
                    'tenant_id', tenant_id,
                    'contact_id', contact_id,
                    'session_id', session_id,
                    'originating_sequence', originating_sequence,
                    'payload', jsonb_build_object(
                        'kind', 'compile',
                        'source', source,
                        'operation_key', operation_key,
                        'run_uid', run_uid,
                        'plan_revision', plan_revision,
                        'outcome', outcome,
                        'candidate_hash', candidate_hash,
                        'final_plan_hash', final_plan_hash,
                        'validation_report', validation_report::TEXT,
                        'duration_micros', duration_micros,
                        'created_at', created_at
                    )
                ) AS envelope
            FROM moa.execution_compile_audit
            WHERE session_id = $1
        ) AS audits
        ORDER BY recorded_at, audit_order
        "#,
    )
    .bind(session_id.0)
    .fetch_all(&pool)
    .await
    .context("load normalized execution audits")?;
    pool.close().await;
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value(value)
                .with_context(|| format!("decode normalized execution audit row {}", index + 1))
        })
        .collect()
}
