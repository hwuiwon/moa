DROP MATERIALIZED VIEW IF EXISTS skill_resolution_rates;

CREATE MATERIALIZED VIEW skill_resolution_rates AS
SELECT
    t.tenant_id,
    t.intent_label,
    unnest(t.skills_activated) AS skill_name,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN t.resolution = 'resolved' THEN 1.0
             WHEN t.resolution = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS resolution_rate,
    AVG(t.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(t.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM task_segments t
WHERE t.intent_label IS NOT NULL
  AND t.resolution IS NOT NULL
  AND array_length(t.skills_activated, 1) IS NOT NULL
GROUP BY t.tenant_id, t.intent_label, skill_name;

CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_resolution_rates_unique
    ON skill_resolution_rates(tenant_id, intent_label, skill_name);

DROP MATERIALIZED VIEW IF EXISTS intent_transitions;

CREATE MATERIALIZED VIEW intent_transitions AS
SELECT
    curr.tenant_id,
    prev.intent_label AS from_intent,
    curr.intent_label AS to_intent,
    COUNT(*)::BIGINT AS transition_count,
    AVG(CASE WHEN prev.resolution = 'resolved' THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS from_resolution_rate
FROM task_segments curr
JOIN task_segments prev ON curr.previous_segment_id = prev.id
WHERE curr.intent_label IS NOT NULL AND prev.intent_label IS NOT NULL
GROUP BY curr.tenant_id, prev.intent_label, curr.intent_label;

CREATE UNIQUE INDEX IF NOT EXISTS idx_intent_transitions_unique
    ON intent_transitions(tenant_id, from_intent, to_intent);

DROP MATERIALIZED VIEW IF EXISTS segment_baselines;

CREATE MATERIALIZED VIEW segment_baselines AS
SELECT
    tenant_id,
    intent_label,
    COUNT(*)::BIGINT AS sample_count,
    AVG(turn_count)::DOUBLE PRECISION AS avg_turns,
    STDDEV(turn_count)::DOUBLE PRECISION AS stddev_turns,
    AVG(token_cost)::DOUBLE PRECISION AS avg_cost,
    STDDEV(token_cost)::DOUBLE PRECISION AS stddev_cost,
    AVG(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS avg_duration_secs,
    STDDEV(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS stddev_duration_secs
FROM task_segments
WHERE intent_label IS NOT NULL AND ended_at IS NOT NULL
GROUP BY tenant_id, intent_label;

CREATE UNIQUE INDEX IF NOT EXISTS idx_segment_baselines_unique
    ON segment_baselines(tenant_id, intent_label);
