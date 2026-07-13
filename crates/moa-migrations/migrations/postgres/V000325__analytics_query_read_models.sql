-- Pivot-style analytics read models for tenant operator dashboards.
--
-- These materialized views intentionally stay read-only and tenant-keyed. The
-- query compiler must still inject tenant predicates for every dashboard query;
-- PostgreSQL RLS helpers target ordinary tables, not materialized views.

CREATE SCHEMA IF NOT EXISTS analytics;

DROP MATERIALIZED VIEW IF EXISTS analytics.experiment_run_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.learning_candidate_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.procedure_node_run_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.procedure_run_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.task_segment_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.tool_call_fact;
DROP VIEW IF EXISTS analytics.event_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.turn_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.session_fact;

CREATE MATERIALIZED VIEW analytics.session_fact AS
SELECT
    ss.id AS session_id,
    ss.tenant_id,
    ss.contact_id,
    sac.agent_id,
    sac.agent_revision_uid,
    sac.display_name AS agent_display_name,
    COALESCE(scb.channel, s.channel) AS channel,
    ss.status,
    ss.created_at,
    ss.updated_at,
    s.completed_at,
    ss.turn_count,
    ss.event_count,
    ss.total_input_tokens,
    COALESCE(s.total_input_tokens_cache_read, 0)::BIGINT AS total_cache_read_tokens,
    ss.total_output_tokens,
    ss.total_cost_cents,
    ss.main_cost_cents,
    ss.auxiliary_cost_cents,
    ss.cache_hit_rate,
    ss.duration_seconds,
    ss.tool_call_count,
    ss.error_count
FROM session_summary ss
JOIN sessions s
    ON s.id = ss.id
LEFT JOIN session_agent_context sac
    ON sac.session_id = ss.id
LEFT JOIN session_channel_bindings scb
    ON scb.id = s.active_channel_binding_id;

CREATE UNIQUE INDEX analytics_session_fact_session_uidx
    ON analytics.session_fact(session_id);
CREATE INDEX analytics_session_fact_tenant_updated_idx
    ON analytics.session_fact(tenant_id, updated_at DESC);
CREATE INDEX analytics_session_fact_tenant_agent_idx
    ON analytics.session_fact(tenant_id, agent_id, updated_at DESC)
    WHERE agent_id IS NOT NULL;
CREATE INDEX analytics_session_fact_tenant_channel_idx
    ON analytics.session_fact(tenant_id, channel, updated_at DESC);

CREATE MATERIALIZED VIEW analytics.turn_fact AS
SELECT
    stm.session_id,
    stm.tenant_id,
    stm.contact_id,
    sac.agent_id,
    sac.agent_revision_uid,
    COALESCE(s.channel, 'chat') AS channel,
    stm.turn_number,
    stm.finished_at,
    stm.model,
    stm.pipeline_ms,
    stm.llm_ms,
    stm.llm_ttft_ms,
    stm.tool_ms,
    stm.tool_call_count,
    stm.input_tokens_uncached,
    stm.input_tokens_cache_write,
    stm.input_tokens_cache_read,
    stm.total_input_tokens,
    stm.output_tokens,
    stm.cost_cents
FROM session_turn_metrics stm
JOIN sessions s
    ON s.id = stm.session_id
LEFT JOIN session_agent_context sac
    ON sac.session_id = stm.session_id;

CREATE UNIQUE INDEX analytics_turn_fact_session_turn_uidx
    ON analytics.turn_fact(session_id, turn_number);
CREATE INDEX analytics_turn_fact_tenant_finished_idx
    ON analytics.turn_fact(tenant_id, finished_at DESC);
CREATE INDEX analytics_turn_fact_tenant_agent_idx
    ON analytics.turn_fact(tenant_id, agent_id, finished_at DESC)
    WHERE agent_id IS NOT NULL;
CREATE INDEX analytics_turn_fact_tenant_model_idx
    ON analytics.turn_fact(tenant_id, model, finished_at DESC);

CREATE MATERIALIZED VIEW analytics.tool_call_fact AS
SELECT
    s.tenant_id,
    tca.session_id,
    sac.agent_id,
    COALESCE(s.channel, 'chat') AS channel,
    tca.tool_id,
    tca.tool_name,
    tca.called_at,
    tca.finished_at,
    tca.success,
    tca.duration_ms,
    tca.model_tier,
    tca.call_sequence_num
FROM tool_call_analytics tca
JOIN sessions s
    ON s.id = tca.session_id
LEFT JOIN session_agent_context sac
    ON sac.session_id = tca.session_id;

CREATE UNIQUE INDEX analytics_tool_call_fact_session_seq_uidx
    ON analytics.tool_call_fact(session_id, call_sequence_num);
CREATE INDEX analytics_tool_call_fact_tenant_called_idx
    ON analytics.tool_call_fact(tenant_id, called_at DESC);
CREATE INDEX analytics_tool_call_fact_tenant_tool_idx
    ON analytics.tool_call_fact(tenant_id, tool_name, called_at DESC);
CREATE INDEX analytics_tool_call_fact_tenant_agent_idx
    ON analytics.tool_call_fact(tenant_id, agent_id, called_at DESC)
    WHERE agent_id IS NOT NULL;

-- `event_fact` is a plain VIEW, not a MATERIALIZED VIEW: `events` is the
-- highest-volume table in the system, so a materialized copy doubled its
-- storage and demanded a full refresh on every dashboard cycle. The base-table
-- indexes below keep the view's on-demand joins index-served, so the analytics
-- catalog can keep pointing at `analytics.event_fact` unchanged.
CREATE VIEW analytics.event_fact AS
SELECT
    e.id AS event_id,
    e.session_id,
    e.tenant_id,
    e.contact_id,
    sac.agent_id,
    COALESCE(s.channel, 'chat') AS channel,
    e.sequence_num,
    e.event_type,
    e.timestamp AS occurred_at,
    e.token_count
FROM events e
JOIN sessions s
    ON s.id = e.session_id
LEFT JOIN session_agent_context sac
    ON sac.session_id = e.session_id;

-- Serves the view's tenant + time-ordered dashboard path
-- (WHERE tenant_id = $1 ORDER BY occurred_at DESC); a backward btree scan
-- satisfies the DESC ordering. The tenant + event_type path
-- (WHERE tenant_id = $1 AND event_type = $2 ORDER BY occurred_at DESC) is
-- already served by idx_events_tenant_type_time from V000307. The old
-- materialized-only tenant + agent partial index is dropped: agent_id comes
-- from the session_agent_context join, so it cannot live on the `events` base
-- table and is resolved by that join at query time.
CREATE INDEX IF NOT EXISTS idx_events_tenant_time
    ON events(tenant_id, timestamp DESC);

CREATE MATERIALIZED VIEW analytics.task_segment_fact AS
SELECT
    ts.id AS segment_id,
    ts.session_id,
    ts.tenant_id::UUID AS tenant_id,
    sac.agent_id,
    COALESCE(s.channel, 'chat') AS channel,
    ts.segment_index,
    ts.task_summary,
    ts.started_at,
    ts.ended_at,
    ts.outcome,
    ts.outcome_confidence::DOUBLE PRECISION AS outcome_confidence,
    ts.tools_used,
    ts.skills_activated,
    ts.skills_used,
    ts.turn_count,
    ts.token_cost,
    CASE
        WHEN ts.ended_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (ts.ended_at - ts.started_at)) * 1000.0
    END AS duration_ms
FROM task_segments ts
JOIN sessions s
    ON s.id = ts.session_id
LEFT JOIN session_agent_context sac
    ON sac.session_id = ts.session_id;

CREATE UNIQUE INDEX analytics_task_segment_fact_segment_uidx
    ON analytics.task_segment_fact(segment_id);
CREATE INDEX analytics_task_segment_fact_tenant_started_idx
    ON analytics.task_segment_fact(tenant_id, started_at DESC);
CREATE INDEX analytics_task_segment_fact_tenant_outcome_idx
    ON analytics.task_segment_fact(tenant_id, outcome, started_at DESC);
CREATE INDEX analytics_task_segment_fact_tenant_agent_idx
    ON analytics.task_segment_fact(tenant_id, agent_id, started_at DESC)
    WHERE agent_id IS NOT NULL;

CREATE MATERIALIZED VIEW analytics.procedure_run_fact AS
SELECT
    ar.run_uid,
    ar.tenant_id,
    ar.session_id,
    sac.agent_id,
    ar.procedure_ref,
    ar.revision_uid,
    ar.status,
    ar.current_node_id,
    ar.started_at,
    ar.completed_at,
    CASE
        WHEN ar.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (ar.completed_at - ar.started_at)) * 1000.0
    END AS duration_ms,
    (ar.error IS NOT NULL) AS error_present
FROM moa.artifact_run ar
LEFT JOIN session_agent_context sac
    ON sac.session_id = ar.session_id;

CREATE UNIQUE INDEX analytics_procedure_run_fact_run_uidx
    ON analytics.procedure_run_fact(run_uid);
CREATE INDEX analytics_procedure_run_fact_tenant_started_idx
    ON analytics.procedure_run_fact(tenant_id, started_at DESC);
CREATE INDEX analytics_procedure_run_fact_tenant_status_idx
    ON analytics.procedure_run_fact(tenant_id, status, started_at DESC);
CREATE INDEX analytics_procedure_run_fact_tenant_agent_idx
    ON analytics.procedure_run_fact(tenant_id, agent_id, started_at DESC)
    WHERE agent_id IS NOT NULL;

CREATE MATERIALIZED VIEW analytics.procedure_node_run_fact AS
SELECT
    anr.node_run_uid,
    anr.run_uid,
    COALESCE(anr.tenant_id, ar.tenant_id) AS tenant_id,
    ar.procedure_ref,
    anr.node_id,
    anr.status,
    anr.started_at,
    anr.completed_at,
    CASE
        WHEN anr.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (anr.completed_at - anr.started_at)) * 1000.0
    END AS duration_ms,
    (anr.error IS NOT NULL) AS error_present
FROM moa.artifact_node_run anr
JOIN moa.artifact_run ar
    ON ar.run_uid = anr.run_uid;

CREATE UNIQUE INDEX analytics_procedure_node_run_fact_node_uidx
    ON analytics.procedure_node_run_fact(node_run_uid);
CREATE INDEX analytics_procedure_node_run_fact_tenant_started_idx
    ON analytics.procedure_node_run_fact(tenant_id, started_at DESC);
CREATE INDEX analytics_procedure_node_run_fact_tenant_status_idx
    ON analytics.procedure_node_run_fact(tenant_id, status, started_at DESC);
CREATE INDEX analytics_procedure_node_run_fact_tenant_procedure_idx
    ON analytics.procedure_node_run_fact(tenant_id, procedure_ref, started_at DESC);

CREATE MATERIALIZED VIEW analytics.learning_candidate_fact AS
SELECT
    lc.id,
    lc.tenant_id::UUID AS tenant_id,
    NULL::UUID AS contact_id,
    lc.candidate_type,
    lc.status,
    lc.target_id,
    lc.target_label,
    lc.task_fingerprint,
    lc.confidence::DOUBLE PRECISION AS confidence,
    lc.risk_class,
    lc.created_at,
    lc.updated_at
FROM learning_candidates lc;

CREATE UNIQUE INDEX analytics_learning_candidate_fact_id_uidx
    ON analytics.learning_candidate_fact(id);
CREATE INDEX analytics_learning_candidate_fact_tenant_updated_idx
    ON analytics.learning_candidate_fact(tenant_id, updated_at DESC);
CREATE INDEX analytics_learning_candidate_fact_tenant_status_idx
    ON analytics.learning_candidate_fact(tenant_id, status, updated_at DESC);
CREATE INDEX analytics_learning_candidate_fact_tenant_type_idx
    ON analytics.learning_candidate_fact(tenant_id, candidate_type, updated_at DESC);

CREATE MATERIALIZED VIEW analytics.experiment_run_fact AS
SELECT
    er.run_uid,
    er.tenant_id,
    er.name,
    er.status,
    er.score_run_id,
    er.created_at,
    er.updated_at,
    er.completed_at,
    CASE
        WHEN er.started_at IS NULL OR er.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (er.completed_at - er.started_at)) * 1000.0
    END AS duration_ms,
    (er.error IS NOT NULL) AS error_present
FROM moa.experiment_run er;

CREATE UNIQUE INDEX analytics_experiment_run_fact_run_uidx
    ON analytics.experiment_run_fact(run_uid);
CREATE INDEX analytics_experiment_run_fact_tenant_updated_idx
    ON analytics.experiment_run_fact(tenant_id, updated_at DESC);
CREATE INDEX analytics_experiment_run_fact_tenant_status_idx
    ON analytics.experiment_run_fact(tenant_id, status, updated_at DESC);
