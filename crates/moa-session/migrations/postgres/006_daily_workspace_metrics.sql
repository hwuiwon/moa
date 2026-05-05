DROP MATERIALIZED VIEW IF EXISTS daily_workspace_metrics;

CREATE MATERIALIZED VIEW daily_workspace_metrics AS
SELECT
    workspace_id,
    DATE_TRUNC('day', created_at) AS day,
    COUNT(*)::BIGINT AS session_count,
    SUM(turn_count)::BIGINT AS turn_count,
    SUM(total_input_tokens)::BIGINT AS total_input_tokens,
    SUM(total_input_tokens_cache_read)::BIGINT AS total_cache_read_tokens,
    SUM(total_output_tokens)::BIGINT AS total_output_tokens,
    SUM(total_cost_cents)::BIGINT AS total_cost_cents,
    AVG(cache_hit_rate)::DOUBLE PRECISION AS avg_cache_hit_rate
FROM sessions
GROUP BY workspace_id, DATE_TRUNC('day', created_at);

CREATE UNIQUE INDEX idx_daily_workspace_metrics_workspace_day
    ON daily_workspace_metrics(workspace_id, day);
