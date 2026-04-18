CREATE OR REPLACE VIEW tool_call_analytics AS
WITH tool_calls AS (
    SELECT
        s.workspace_id,
        s.user_id,
        e.session_id,
        e.sequence_num AS call_sequence_num,
        e.timestamp AS called_at,
        e.payload -> 'data' AS call_data
    FROM events e
    JOIN sessions s
        ON s.id = e.session_id
    WHERE e.event_type = 'ToolCall'
)
SELECT
    tc.workspace_id,
    tc.user_id,
    tc.session_id,
    tc.call_sequence_num,
    tc.called_at,
    tc.call_data ->> 'tool_name' AS tool_name,
    (tc.call_data ->> 'tool_id')::UUID AS tool_id,
    COALESCE(result_event.timestamp, error_event.timestamp) AS finished_at,
    CASE
        WHEN result_event.id IS NOT NULL THEN COALESCE((result_event.payload -> 'data' ->> 'success')::BOOLEAN, FALSE)
        WHEN error_event.id IS NOT NULL THEN FALSE
        ELSE FALSE
    END AS success,
    CASE
        WHEN result_event.id IS NOT NULL THEN COALESCE(
            (result_event.payload -> 'data' ->> 'duration_ms')::DOUBLE PRECISION,
            EXTRACT(EPOCH FROM (result_event.timestamp - tc.called_at)) * 1000.0
        )
        WHEN error_event.id IS NOT NULL THEN EXTRACT(EPOCH FROM (error_event.timestamp - tc.called_at)) * 1000.0
        ELSE NULL
    END AS duration_ms,
    'main'::TEXT AS model_tier
FROM tool_calls tc
LEFT JOIN LATERAL (
    SELECT e.id, e.payload, e.timestamp
    FROM events e
    WHERE e.session_id = tc.session_id
      AND e.event_type = 'ToolResult'
      AND (e.payload -> 'data' ->> 'tool_id') = (tc.call_data ->> 'tool_id')
    ORDER BY e.sequence_num ASC
    LIMIT 1
) result_event ON TRUE
LEFT JOIN LATERAL (
    SELECT e.id, e.payload, e.timestamp
    FROM events e
    WHERE e.session_id = tc.session_id
      AND e.event_type = 'ToolError'
      AND (e.payload -> 'data' ->> 'tool_id') = (tc.call_data ->> 'tool_id')
    ORDER BY e.sequence_num ASC
    LIMIT 1
) error_event ON TRUE;

CREATE OR REPLACE VIEW session_summary AS
SELECT
    s.id,
    s.workspace_id,
    s.user_id,
    s.status,
    s.turn_count,
    s.event_count,
    s.total_input_tokens,
    s.total_output_tokens,
    s.total_cost_cents,
    s.cache_hit_rate,
    s.created_at,
    s.updated_at,
    EXTRACT(EPOCH FROM (s.updated_at - s.created_at))::DOUBLE PRECISION AS duration_seconds,
    COALESCE(tool_counts.tool_call_count, 0)::BIGINT AS tool_call_count,
    COALESCE(error_counts.error_count, 0)::BIGINT AS error_count,
    COALESCE(tier_costs.main_cost_cents, 0)::BIGINT AS main_cost_cents,
    COALESCE(tier_costs.auxiliary_cost_cents, 0)::BIGINT AS auxiliary_cost_cents
FROM sessions s
LEFT JOIN (
    SELECT session_id, COUNT(*)::BIGINT AS tool_call_count
    FROM events
    WHERE event_type = 'ToolCall'
    GROUP BY session_id
) tool_counts
    ON tool_counts.session_id = s.id
LEFT JOIN (
    SELECT session_id, COUNT(*)::BIGINT AS error_count
    FROM events
    WHERE event_type = 'Error'
    GROUP BY session_id
) error_counts
    ON error_counts.session_id = s.id
LEFT JOIN (
    SELECT
        e.session_id,
        SUM(
            CASE
                WHEN COALESCE(
                    e.payload -> 'data' ->> 'model_tier',
                    CASE
                        WHEN e.event_type = 'Checkpoint' THEN 'auxiliary'
                        ELSE 'main'
                    END
                ) = 'main' THEN COALESCE((e.payload -> 'data' ->> 'cost_cents')::BIGINT, 0)
                ELSE 0
            END
        )::BIGINT AS main_cost_cents,
        SUM(
            CASE
                WHEN COALESCE(
                    e.payload -> 'data' ->> 'model_tier',
                    CASE
                        WHEN e.event_type = 'Checkpoint' THEN 'auxiliary'
                        ELSE 'main'
                    END
                ) = 'auxiliary' THEN COALESCE((e.payload -> 'data' ->> 'cost_cents')::BIGINT, 0)
                ELSE 0
            END
        )::BIGINT AS auxiliary_cost_cents
    FROM events e
    WHERE e.event_type IN ('BrainResponse', 'Checkpoint')
    GROUP BY e.session_id
) tier_costs
    ON tier_costs.session_id = s.id;
