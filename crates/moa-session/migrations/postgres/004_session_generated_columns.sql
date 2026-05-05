DROP MATERIALIZED VIEW IF EXISTS daily_workspace_metrics;
DROP MATERIALIZED VIEW IF EXISTS session_turn_metrics;
DROP VIEW IF EXISTS session_summary;
DROP VIEW IF EXISTS tool_call_summary;
DROP VIEW IF EXISTS tool_call_analytics;

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS turn_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE sessions
    DROP COLUMN IF EXISTS total_input_tokens;

ALTER TABLE sessions
    DROP COLUMN IF EXISTS cache_hit_rate;

ALTER TABLE sessions
    ADD COLUMN total_input_tokens BIGINT GENERATED ALWAYS AS (
        COALESCE(total_input_tokens_uncached, 0)
        + COALESCE(total_input_tokens_cache_write, 0)
        + COALESCE(total_input_tokens_cache_read, 0)
    ) STORED;

ALTER TABLE sessions
    ADD COLUMN cache_hit_rate DOUBLE PRECISION GENERATED ALWAYS AS (
        CASE
            WHEN (
                COALESCE(total_input_tokens_uncached, 0)
                + COALESCE(total_input_tokens_cache_write, 0)
                + COALESCE(total_input_tokens_cache_read, 0)
            ) = 0 THEN 0.0
            ELSE COALESCE(total_input_tokens_cache_read, 0)::DOUBLE PRECISION
                / (
                    COALESCE(total_input_tokens_uncached, 0)
                    + COALESCE(total_input_tokens_cache_write, 0)
                    + COALESCE(total_input_tokens_cache_read, 0)
                )::DOUBLE PRECISION
        END
    ) STORED;

CREATE INDEX IF NOT EXISTS idx_sessions_cache_hit_rate
    ON sessions(cache_hit_rate);

CREATE INDEX IF NOT EXISTS idx_sessions_cost_cents
    ON sessions(total_cost_cents DESC);

CREATE OR REPLACE FUNCTION update_session_aggregates() RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
DECLARE
    event_data JSONB := COALESCE(NEW.payload -> 'data', '{}'::JSONB);
BEGIN
    UPDATE sessions
    SET
        event_count = event_count + 1,
        turn_count = turn_count + CASE WHEN NEW.event_type = 'BrainResponse' THEN 1 ELSE 0 END,
        total_input_tokens_uncached = total_input_tokens_uncached + CASE
            WHEN NEW.event_type = 'BrainResponse' THEN COALESCE((event_data ->> 'input_tokens_uncached')::BIGINT, 0)
            WHEN NEW.event_type = 'Checkpoint' THEN COALESCE((event_data ->> 'input_tokens')::BIGINT, 0)
            ELSE 0
        END,
        total_input_tokens_cache_write = total_input_tokens_cache_write + CASE
            WHEN NEW.event_type = 'BrainResponse' THEN COALESCE((event_data ->> 'input_tokens_cache_write')::BIGINT, 0)
            ELSE 0
        END,
        total_input_tokens_cache_read = total_input_tokens_cache_read + CASE
            WHEN NEW.event_type = 'BrainResponse' THEN COALESCE((event_data ->> 'input_tokens_cache_read')::BIGINT, 0)
            ELSE 0
        END,
        total_output_tokens = total_output_tokens + CASE
            WHEN NEW.event_type IN ('BrainResponse', 'Checkpoint') THEN COALESCE((event_data ->> 'output_tokens')::BIGINT, 0)
            ELSE 0
        END,
        total_cost_cents = total_cost_cents + CASE
            WHEN NEW.event_type IN ('BrainResponse', 'Checkpoint') THEN COALESCE((event_data ->> 'cost_cents')::BIGINT, 0)
            ELSE 0
        END,
        last_checkpoint_seq = CASE
            WHEN NEW.event_type = 'Checkpoint' THEN NEW.sequence_num
            ELSE last_checkpoint_seq
        END,
        updated_at = GREATEST(updated_at, NEW.timestamp)
    WHERE id = NEW.session_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_update_session_aggregates ON events;

CREATE TRIGGER trg_update_session_aggregates
    AFTER INSERT ON events
    FOR EACH ROW
    EXECUTE FUNCTION update_session_aggregates();

WITH event_aggregates AS (
    SELECT
        e.session_id,
        COUNT(*)::BIGINT AS event_count,
        COUNT(*) FILTER (WHERE e.event_type = 'BrainResponse')::BIGINT AS turn_count,
        COALESCE(SUM(
            CASE
                WHEN e.event_type = 'BrainResponse' THEN COALESCE((e.payload -> 'data' ->> 'input_tokens_uncached')::BIGINT, 0)
                WHEN e.event_type = 'Checkpoint' THEN COALESCE((e.payload -> 'data' ->> 'input_tokens')::BIGINT, 0)
                ELSE 0
            END
        ), 0)::BIGINT AS total_input_tokens_uncached,
        COALESCE(SUM(
            CASE
                WHEN e.event_type = 'BrainResponse' THEN COALESCE((e.payload -> 'data' ->> 'input_tokens_cache_write')::BIGINT, 0)
                ELSE 0
            END
        ), 0)::BIGINT AS total_input_tokens_cache_write,
        COALESCE(SUM(
            CASE
                WHEN e.event_type = 'BrainResponse' THEN COALESCE((e.payload -> 'data' ->> 'input_tokens_cache_read')::BIGINT, 0)
                ELSE 0
            END
        ), 0)::BIGINT AS total_input_tokens_cache_read,
        COALESCE(SUM(
            CASE
                WHEN e.event_type IN ('BrainResponse', 'Checkpoint') THEN COALESCE((e.payload -> 'data' ->> 'output_tokens')::BIGINT, 0)
                ELSE 0
            END
        ), 0)::BIGINT AS total_output_tokens,
        COALESCE(SUM(
            CASE
                WHEN e.event_type IN ('BrainResponse', 'Checkpoint') THEN COALESCE((e.payload -> 'data' ->> 'cost_cents')::BIGINT, 0)
                ELSE 0
            END
        ), 0)::BIGINT AS total_cost_cents,
        MAX(CASE WHEN e.event_type = 'Checkpoint' THEN e.sequence_num END)::BIGINT AS last_checkpoint_seq,
        MAX(e.timestamp) AS latest_event_at
    FROM events e
    GROUP BY e.session_id
)
UPDATE sessions s
SET
    event_count = COALESCE(a.event_count, 0),
    turn_count = COALESCE(a.turn_count, 0),
    total_input_tokens_uncached = COALESCE(a.total_input_tokens_uncached, 0),
    total_input_tokens_cache_write = COALESCE(a.total_input_tokens_cache_write, 0),
    total_input_tokens_cache_read = COALESCE(a.total_input_tokens_cache_read, 0),
    total_output_tokens = COALESCE(a.total_output_tokens, 0),
    total_cost_cents = COALESCE(a.total_cost_cents, 0),
    last_checkpoint_seq = a.last_checkpoint_seq,
    updated_at = COALESCE(a.latest_event_at, s.updated_at)
FROM (
    SELECT
        s.id,
        a.event_count,
        a.turn_count,
        a.total_input_tokens_uncached,
        a.total_input_tokens_cache_write,
        a.total_input_tokens_cache_read,
        a.total_output_tokens,
        a.total_cost_cents,
        a.last_checkpoint_seq,
        a.latest_event_at
    FROM sessions s
    LEFT JOIN event_aggregates a
        ON a.session_id = s.id
) a
WHERE s.id = a.id;
