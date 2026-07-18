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
                0::BIGINT AS audit_order,
                CONCAT('route:', stage, ':', decision) AS audit_identity,
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
                        'strategy', strategy,
                        'reason', reason,
                        'provenance', jsonb_build_object(
                            'source', source,
                            'classifier_outcome', classifier_outcome,
                            'provider_model', provider_model,
                            'prompt_version', prompt_version,
                            'objective_hash', objective_hash,
                            'response_hash', response_hash,
                            'confidence_bps', confidence_bps,
                            'missing_input_count', missing_input_count,
                            'usage', jsonb_build_object(
                                'input_tokens_uncached', input_tokens_uncached,
                                'input_tokens_cache_write', input_tokens_cache_write,
                                'input_tokens_cache_read', input_tokens_cache_read,
                                'output_tokens', output_tokens
                            ),
                            'cost_microusd', cost_microusd,
                            'duration_micros', duration_micros
                        ),
                        'accepted_at', accepted_at
                    )
                ) AS envelope
            FROM moa.execution_route_audit
            WHERE session_id = $1

            UNION ALL

            SELECT
                created_at AS recorded_at,
                (call_ordinal::BIGINT * 2) + 1 AS audit_order,
                CONCAT('planner:', call_kind, ':', call_ordinal) AS audit_identity,
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
                compile.created_at AS recorded_at,
                COALESCE(
                    (
                        SELECT (planner.call_ordinal::BIGINT * 2) + 2
                        FROM moa.execution_planner_call_audit AS planner
                        WHERE planner.session_id = compile.session_id
                          AND planner.originating_sequence = compile.originating_sequence
                          AND planner.run_uid IS NOT DISTINCT FROM compile.run_uid
                          AND planner.plan_revision IS NOT DISTINCT FROM compile.plan_revision
                          AND (
                              compile.operation_key = FORMAT(
                                  'session:%s:%s:generated:%s',
                                  compile.session_id,
                                  compile.originating_sequence,
                                  planner.call_ordinal
                              )
                              OR (
                                  compile.source = 'amendment'
                                  AND planner.compiler_report::JSONB = compile.validation_report::JSONB
                              )
                          )
                        LIMIT 1
                    ),
                    2::BIGINT
                ) AS audit_order,
                CONCAT('compile:', compile.operation_key) AS audit_identity,
                jsonb_build_object(
                    'schema_version', 1,
                    'tenant_id', compile.tenant_id,
                    'contact_id', compile.contact_id,
                    'session_id', compile.session_id,
                    'originating_sequence', compile.originating_sequence,
                    'payload', jsonb_build_object(
                        'kind', 'compile',
                        'source', compile.source,
                        'operation_key', compile.operation_key,
                        'run_uid', compile.run_uid,
                        'plan_revision', compile.plan_revision,
                        'outcome', compile.outcome,
                        'candidate_hash', compile.candidate_hash,
                        'final_plan_hash', compile.final_plan_hash,
                        'validation_report', compile.validation_report::TEXT,
                        'duration_micros', compile.duration_micros,
                        'created_at', compile.created_at
                    )
                ) AS envelope
            FROM moa.execution_compile_audit AS compile
            WHERE compile.session_id = $1
        ) AS audits
        ORDER BY recorded_at, audit_order, audit_identity
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
