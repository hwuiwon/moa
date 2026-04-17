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
    END AS duration_ms
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

CREATE OR REPLACE VIEW tool_call_summary AS
SELECT
    tool_name,
    COUNT(*)::BIGINT AS call_count,
    AVG(duration_ms)::DOUBLE PRECISION AS avg_duration_ms,
    PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms,
    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_ms,
    AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS success_rate
FROM tool_call_analytics
WHERE finished_at IS NOT NULL
GROUP BY tool_name;

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
    COALESCE(error_counts.error_count, 0)::BIGINT AS error_count
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
    ON error_counts.session_id = s.id;

DROP MATERIALIZED VIEW IF EXISTS session_turn_metrics;

CREATE MATERIALIZED VIEW session_turn_metrics AS
WITH brain_turns AS (
    SELECT
        e.session_id,
        e.sequence_num AS response_sequence_num,
        ROW_NUMBER() OVER (
            PARTITION BY e.session_id
            ORDER BY e.sequence_num
        )::BIGINT AS turn_number,
        LAG(e.sequence_num, 1, -1) OVER (
            PARTITION BY e.session_id
            ORDER BY e.sequence_num
        )::BIGINT AS previous_response_sequence_num,
        e.timestamp AS finished_at,
        e.payload -> 'data' AS response_data
    FROM events e
    WHERE e.event_type = 'BrainResponse'
),
tool_metrics AS (
    SELECT
        bt.session_id,
        bt.turn_number,
        COUNT(tc.id)::BIGINT AS tool_call_count,
        COALESCE(SUM(
            CASE
                WHEN tr.id IS NOT NULL THEN COALESCE(
                    (tr.payload -> 'data' ->> 'duration_ms')::DOUBLE PRECISION,
                    EXTRACT(EPOCH FROM (tr.timestamp - tc.timestamp)) * 1000.0
                )
                WHEN te.id IS NOT NULL THEN EXTRACT(EPOCH FROM (te.timestamp - tc.timestamp)) * 1000.0
                ELSE 0.0
            END
        ), 0.0)::DOUBLE PRECISION AS tool_ms
    FROM brain_turns bt
    LEFT JOIN events tc
        ON tc.session_id = bt.session_id
       AND tc.event_type = 'ToolCall'
       AND tc.sequence_num > bt.previous_response_sequence_num
       AND tc.sequence_num < bt.response_sequence_num
    LEFT JOIN LATERAL (
        SELECT e.id, e.payload, e.timestamp
        FROM events e
        WHERE e.session_id = tc.session_id
          AND e.event_type = 'ToolResult'
          AND (e.payload -> 'data' ->> 'tool_id') = (tc.payload -> 'data' ->> 'tool_id')
        ORDER BY e.sequence_num ASC
        LIMIT 1
    ) tr ON TRUE
    LEFT JOIN LATERAL (
        SELECT e.id, e.payload, e.timestamp
        FROM events e
        WHERE e.session_id = tc.session_id
          AND e.event_type = 'ToolError'
          AND (e.payload -> 'data' ->> 'tool_id') = (tc.payload -> 'data' ->> 'tool_id')
        ORDER BY e.sequence_num ASC
        LIMIT 1
    ) te ON TRUE
    GROUP BY bt.session_id, bt.turn_number
)
SELECT
    s.workspace_id,
    s.user_id,
    bt.session_id,
    bt.turn_number,
    bt.finished_at,
    bt.response_data ->> 'model' AS model,
    NULL::DOUBLE PRECISION AS pipeline_ms,
    COALESCE((bt.response_data ->> 'duration_ms')::DOUBLE PRECISION, 0.0) AS llm_ms,
    COALESCE(tm.tool_ms, 0.0) AS tool_ms,
    COALESCE(tm.tool_call_count, 0)::BIGINT AS tool_call_count,
    COALESCE((bt.response_data ->> 'input_tokens_uncached')::BIGINT, 0)::BIGINT AS input_tokens_uncached,
    COALESCE((bt.response_data ->> 'input_tokens_cache_write')::BIGINT, 0)::BIGINT AS input_tokens_cache_write,
    COALESCE((bt.response_data ->> 'input_tokens_cache_read')::BIGINT, 0)::BIGINT AS input_tokens_cache_read,
    (
        COALESCE((bt.response_data ->> 'input_tokens_uncached')::BIGINT, 0)
        + COALESCE((bt.response_data ->> 'input_tokens_cache_write')::BIGINT, 0)
        + COALESCE((bt.response_data ->> 'input_tokens_cache_read')::BIGINT, 0)
    )::BIGINT AS total_input_tokens,
    COALESCE((bt.response_data ->> 'output_tokens')::BIGINT, 0)::BIGINT AS output_tokens,
    COALESCE((bt.response_data ->> 'cost_cents')::BIGINT, 0)::BIGINT AS cost_cents
FROM brain_turns bt
JOIN sessions s
    ON s.id = bt.session_id
LEFT JOIN tool_metrics tm
    ON tm.session_id = bt.session_id
   AND tm.turn_number = bt.turn_number;

CREATE UNIQUE INDEX idx_session_turn_metrics_session_turn
    ON session_turn_metrics(session_id, turn_number);
