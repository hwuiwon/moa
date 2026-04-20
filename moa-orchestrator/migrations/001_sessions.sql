CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT NOT NULL,
    platform_channel TEXT,
    model TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    parent_session_id UUID REFERENCES sessions(id),
    total_input_tokens_uncached BIGINT NOT NULL DEFAULT 0,
    total_input_tokens_cache_write BIGINT NOT NULL DEFAULT 0,
    total_input_tokens_cache_read BIGINT NOT NULL DEFAULT 0,
    total_input_tokens BIGINT GENERATED ALWAYS AS (
        COALESCE(total_input_tokens_uncached, 0)
        + COALESCE(total_input_tokens_cache_write, 0)
        + COALESCE(total_input_tokens_cache_read, 0)
    ) STORED,
    total_output_tokens BIGINT NOT NULL DEFAULT 0,
    total_cost_cents BIGINT NOT NULL DEFAULT 0,
    event_count BIGINT NOT NULL DEFAULT 0,
    turn_count BIGINT NOT NULL DEFAULT 0,
    cache_hit_rate DOUBLE PRECISION GENERATED ALWAYS AS (
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
    ) STORED,
    last_checkpoint_seq BIGINT
);

CREATE INDEX IF NOT EXISTS idx_sessions_workspace ON sessions(workspace_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);
CREATE INDEX IF NOT EXISTS idx_sessions_cache_hit_rate ON sessions(cache_hit_rate);
CREATE INDEX IF NOT EXISTS idx_sessions_cost_cents ON sessions(total_cost_cents DESC);

CREATE TABLE IF NOT EXISTS events (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    sequence_num BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    brain_id UUID,
    hand_id TEXT,
    token_count INTEGER,
    search_vector TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(event_type, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(payload::text, '')), 'B')
    ) STORED,
    UNIQUE(session_id, sequence_num)
);

CREATE INDEX IF NOT EXISTS idx_events_session_seq ON events(session_id, sequence_num);
CREATE INDEX IF NOT EXISTS idx_events_session_type ON events(session_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
CREATE INDEX IF NOT EXISTS idx_events_fts ON events USING GIN(search_vector);

CREATE TABLE IF NOT EXISTS pending_signals (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    signal_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_pending_signals_session
    ON pending_signals(session_id, resolved_at, created_at);

CREATE TABLE IF NOT EXISTS context_snapshots (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    format_version INTEGER NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_context_snapshots_last_seq
    ON context_snapshots(session_id, last_sequence_num);

CREATE OR REPLACE FUNCTION update_session_aggregates() RETURNS TRIGGER AS $$
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
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_update_session_aggregates ON events;

CREATE TRIGGER trg_update_session_aggregates
    AFTER INSERT ON events
    FOR EACH ROW
    EXECUTE FUNCTION update_session_aggregates();
