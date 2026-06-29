
-- Source: V000001__session_scope_helpers.sql

CREATE SCHEMA IF NOT EXISTS moa;

DO $$
BEGIN
    CREATE ROLE moa_app NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_promoter NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_owner NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_auditor NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_replicator LOGIN REPLICATION;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

GRANT moa_app TO CURRENT_USER;
GRANT moa_promoter TO CURRENT_USER;
GRANT moa_auditor TO CURRENT_USER;

CREATE OR REPLACE FUNCTION moa.compute_scope_tier(
    workspace_id TEXT,
    user_id TEXT
) RETURNS TEXT
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT CASE
        WHEN workspace_id IS NULL AND user_id IS NULL THEN 'global'
        WHEN workspace_id IS NOT NULL AND user_id IS NOT NULL THEN 'contact'
        WHEN workspace_id IS NOT NULL AND user_id IS NULL THEN 'tenant'
        ELSE NULL
    END;
$$;

CREATE OR REPLACE FUNCTION moa.current_storage_partition() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.storage_partition_id', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.current_user_id() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.user_id', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.current_scope_tier() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.scope_tier', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.drop_three_tier_policies(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('DROP POLICY IF EXISTS storage_partition_isolation ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_global ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_global_promoter ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS owner_dev_access ON %s', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_three_tier_read_policies(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);

    EXECUTE format(
        'CREATE POLICY rd_global ON %s FOR SELECT TO moa_app
         USING (scope = ''global'' AND moa.current_scope_tier() IS NOT NULL)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_tenant ON %s FOR SELECT TO moa_app
         USING (scope = ''tenant'' AND storage_partition_id = moa.current_storage_partition())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_user ON %s FOR SELECT TO moa_app
         USING (scope = ''contact''
                AND storage_partition_id = moa.current_storage_partition()
                AND user_id = moa.current_user_id())',
        target_table
    );
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_three_tier_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM moa.drop_three_tier_policies(target_table);
    PERFORM moa.apply_three_tier_read_policies(target_table);

    EXECUTE format(
        'CREATE POLICY wr_tenant ON %s FOR ALL TO moa_app
         USING (scope = ''tenant'' AND storage_partition_id = moa.current_storage_partition())
         WITH CHECK (scope = ''tenant'' AND storage_partition_id = moa.current_storage_partition())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_user ON %s FOR ALL TO moa_app
         USING (scope = ''contact''
                AND storage_partition_id = moa.current_storage_partition()
                AND user_id = moa.current_user_id())
         WITH CHECK (scope = ''contact''
                     AND storage_partition_id = moa.current_storage_partition()
                     AND user_id = moa.current_user_id())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_global_promoter ON %s FOR ALL TO moa_promoter
         USING (scope = ''global'') WITH CHECK (scope = ''global'')',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY owner_dev_access ON %s FOR ALL TO %I
         USING (true) WITH CHECK (true)',
        target_table,
        pg_get_userbyid((SELECT relowner FROM pg_class WHERE oid = target_table))
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_promoter', target_table);
END;
$$;

-- Source: V000002__session_initial.sql

CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT NOT NULL,
    platform_channel TEXT,
    model TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    parent_session_id UUID REFERENCES sessions(id),
    total_input_tokens BIGINT DEFAULT 0,
    total_output_tokens BIGINT DEFAULT 0,
    total_cost_cents BIGINT DEFAULT 0,
    event_count BIGINT DEFAULT 0,
    last_checkpoint_seq BIGINT
);

CREATE INDEX IF NOT EXISTS idx_sessions_storage_partition ON sessions(storage_partition_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_scope ON sessions(storage_partition_id, scope, user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);

CREATE TABLE IF NOT EXISTS events (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
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

CREATE INDEX IF NOT EXISTS idx_events_session_type ON events(session_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_scope ON events(storage_partition_id, scope, user_id);
CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
CREATE INDEX IF NOT EXISTS idx_events_fts ON events USING GIN(search_vector);

CREATE TABLE IF NOT EXISTS pending_signals (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    signal_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    resolved_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_pending_signals_session
    ON pending_signals(session_id, resolved_at, created_at);
CREATE INDEX IF NOT EXISTS idx_pending_signals_scope
    ON pending_signals(storage_partition_id, scope, user_id);

-- Source: V000003__session_add_session_cache_columns.sql

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS total_input_tokens_uncached BIGINT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS total_input_tokens_cache_write BIGINT DEFAULT 0,
    ADD COLUMN IF NOT EXISTS total_input_tokens_cache_read BIGINT DEFAULT 0;

-- Source: V000004__session_add_context_snapshots.sql

CREATE TABLE IF NOT EXISTS context_snapshots (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    format_version INTEGER NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_context_snapshots_last_seq
    ON context_snapshots(session_id, last_sequence_num);
CREATE INDEX IF NOT EXISTS idx_context_snapshots_scope
    ON context_snapshots(storage_partition_id, scope, user_id);

-- Source: V000005__session_generated_columns.sql

DROP MATERIALIZED VIEW IF EXISTS daily_storage_partition_metrics;
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

-- Source: V000006__session_analytic_views.sql

CREATE OR REPLACE VIEW tool_call_analytics AS
WITH tool_calls AS (
    SELECT
        s.storage_partition_id,
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
    tc.storage_partition_id,
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
    s.storage_partition_id,
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
    s.storage_partition_id,
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

-- Source: V000007__session_daily_storage_partition_metrics.sql

DROP MATERIALIZED VIEW IF EXISTS daily_storage_partition_metrics;

CREATE MATERIALIZED VIEW daily_storage_partition_metrics AS
SELECT
    storage_partition_id,
    DATE_TRUNC('day', created_at) AS day,
    COUNT(*)::BIGINT AS session_count,
    SUM(turn_count)::BIGINT AS turn_count,
    SUM(total_input_tokens)::BIGINT AS total_input_tokens,
    SUM(total_input_tokens_cache_read)::BIGINT AS total_cache_read_tokens,
    SUM(total_output_tokens)::BIGINT AS total_output_tokens,
    SUM(total_cost_cents)::BIGINT AS total_cost_cents,
    AVG(cache_hit_rate)::DOUBLE PRECISION AS avg_cache_hit_rate
FROM sessions
GROUP BY storage_partition_id, DATE_TRUNC('day', created_at);

CREATE UNIQUE INDEX idx_daily_storage_partition_metrics_partition_day
    ON daily_storage_partition_metrics(storage_partition_id, day);

-- Source: V000008__session_model_tier_analytics.sql

CREATE OR REPLACE VIEW tool_call_analytics AS
WITH tool_calls AS (
    SELECT
        s.storage_partition_id,
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
    tc.storage_partition_id,
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
    s.storage_partition_id,
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

-- Source: V000009__session_task_segments.sql

CREATE TABLE IF NOT EXISTS task_segments (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    tenant_id TEXT NOT NULL,
    segment_index INT NOT NULL,
    task_summary TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    outcome TEXT,
    assessment TEXT,
    outcome_confidence NUMERIC(4,3),
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    previous_segment_id UUID,
    UNIQUE(session_id, segment_index)
);

CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_outcome
    ON task_segments (tenant_id, outcome);
CREATE INDEX IF NOT EXISTS idx_task_segments_session
    ON task_segments (session_id, segment_index);
CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_time
    ON task_segments (tenant_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_task_segments_scope
    ON task_segments (storage_partition_id, scope, user_id);

-- Source: V000010__session_resolution_views.sql

DROP MATERIALIZED VIEW IF EXISTS skill_resolution_rates;

CREATE MATERIALIZED VIEW skill_resolution_rates AS
SELECT
    t.tenant_id,
    unnest(t.skills_activated) AS skill_name,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN t.outcome = 'resolved' THEN 1.0
             WHEN t.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS resolution_rate,
    AVG(t.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(t.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM task_segments t
WHERE t.outcome IS NOT NULL
  AND array_length(t.skills_activated, 1) IS NOT NULL
GROUP BY t.tenant_id, skill_name;

CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_resolution_rates_unique
    ON skill_resolution_rates(tenant_id, skill_name);

DROP MATERIALIZED VIEW IF EXISTS segment_baselines;

CREATE MATERIALIZED VIEW segment_baselines AS
SELECT
    tenant_id,
    COUNT(*)::BIGINT AS sample_count,
    AVG(turn_count)::DOUBLE PRECISION AS avg_turns,
    STDDEV(turn_count)::DOUBLE PRECISION AS stddev_turns,
    AVG(token_cost)::DOUBLE PRECISION AS avg_cost,
    STDDEV(token_cost)::DOUBLE PRECISION AS stddev_cost,
    AVG(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS avg_duration_secs,
    STDDEV(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS stddev_duration_secs
FROM task_segments
WHERE ended_at IS NOT NULL
GROUP BY tenant_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_segment_baselines_unique
    ON segment_baselines(tenant_id);

-- Source: V000011__session_intents_learning_log.sql

CREATE TABLE IF NOT EXISTS learning_log (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    learning_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    target_label TEXT,
    payload JSONB NOT NULL,
    confidence NUMERIC(4,3),
    source_refs UUID[] NOT NULL DEFAULT '{}',
    actor TEXT NOT NULL DEFAULT 'system',
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to TIMESTAMPTZ,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    batch_id UUID,
    version INT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_learning_log_tenant_type
    ON learning_log (tenant_id, learning_type, valid_to);
CREATE INDEX IF NOT EXISTS idx_learning_log_target
    ON learning_log (tenant_id, target_id, valid_from DESC);
CREATE INDEX IF NOT EXISTS idx_learning_log_batch
    ON learning_log (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learning_log_scope
    ON learning_log (storage_partition_id, scope, user_id);

-- Source: V000012__session_three_tier_rls.sql

SELECT moa.apply_three_tier_rls('sessions'::REGCLASS);
SELECT moa.apply_three_tier_rls('events'::REGCLASS);
SELECT moa.apply_three_tier_rls('pending_signals'::REGCLASS);
SELECT moa.apply_three_tier_rls('context_snapshots'::REGCLASS);
SELECT moa.apply_three_tier_rls('task_segments'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_log'::REGCLASS);

-- Source: V000013__session_graph_label_helpers.sql

CREATE OR REPLACE FUNCTION moa.graph_node_labels() RETURNS TEXT[]
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ARRAY[
        'Entity',
        'Concept',
        'Decision',
        'Incident',
        'Lesson',
        'Fact',
        'Source',
        'Document',
        'Chunk',
        'ContactGroup'
    ]::TEXT[];
$$;

CREATE OR REPLACE FUNCTION moa.graph_edge_labels() RETURNS TEXT[]
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ARRAY[
        'RELATES_TO',
        'DEPENDS_ON',
        'OWNED_BY',
        'SUPERSEDES',
        'CONTRADICTS',
        'DERIVED_FROM',
        'CONTAINS',
        'MENTIONED_IN',
        'MEMBER_OF',
        'CAUSED',
        'LEARNED_FROM',
        'APPLIES_TO'
    ]::TEXT[];
$$;

-- Source: V000014__session_node_index.sql

CREATE TABLE IF NOT EXISTS moa.node_index (
    uid UUID PRIMARY KEY,
    gid BIGINT,
    label TEXT NOT NULL CHECK (label = ANY(moa.graph_node_labels())),
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    name TEXT NOT NULL,
    name_tsv TSVECTOR GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(name, ''))
    ) STORED,
    pii_class TEXT NOT NULL DEFAULT 'none'
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    confidence DOUBLE PRECISION,
    reference_count BIGINT NOT NULL DEFAULT 0 CHECK (reference_count >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to TIMESTAMPTZ,
    invalidated_at TIMESTAMPTZ,
    invalidated_by UUID,
    invalidated_reason TEXT,
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    properties_summary JSONB
);

ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS reference_count BIGINT NOT NULL DEFAULT 0 CHECK (reference_count >= 0);

CREATE INDEX IF NOT EXISTS node_index_ws_scope_label
    ON moa.node_index (storage_partition_id, scope, label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS node_index_name_tsv_idx
    ON moa.node_index USING GIN (name_tsv);
CREATE INDEX IF NOT EXISTS node_index_pii_idx
    ON moa.node_index (pii_class)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS node_index_validto_partial_idx
    ON moa.node_index (valid_to)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS node_index_label_partial
    ON moa.node_index (label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS node_index_lastaccess_idx
    ON moa.node_index (last_accessed_at)
    WHERE valid_to IS NULL;

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
SELECT moa.apply_three_tier_rls('moa.node_index'::REGCLASS);

-- Source: V000014__session_edge_index.sql

CREATE TABLE IF NOT EXISTS moa.edge_index (
    uid UUID PRIMARY KEY,
    label TEXT NOT NULL CHECK (label = ANY(moa.graph_edge_labels())),
    start_uid UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    end_uid UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    tenant_id UUID,
    contact_id UUID,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    properties JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS edge_index_ws_scope_label
    ON moa.edge_index (storage_partition_id, scope, label);
CREATE INDEX IF NOT EXISTS edge_index_start_label
    ON moa.edge_index (start_uid, label);
CREATE INDEX IF NOT EXISTS edge_index_end_label
    ON moa.edge_index (end_uid, label);
CREATE INDEX IF NOT EXISTS edge_index_start_end_label
    ON moa.edge_index (start_uid, end_uid, label);
CREATE INDEX IF NOT EXISTS edge_index_ws_start
    ON moa.edge_index (storage_partition_id, start_uid);
CREATE INDEX IF NOT EXISTS edge_index_ws_end
    ON moa.edge_index (storage_partition_id, end_uid);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
SELECT moa.apply_three_tier_rls('moa.edge_index'::REGCLASS);

-- Source: V000015__session_embeddings.sql

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;

DROP TABLE IF EXISTS moa.embeddings_old CASCADE;

CREATE TABLE IF NOT EXISTS moa.embeddings (
    uid UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    label TEXT NOT NULL CHECK (label = ANY(moa.graph_node_labels())),
    pii_class TEXT NOT NULL DEFAULT 'none'
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    embedding public.halfvec(1024) NOT NULL,
    embedding_model TEXT NOT NULL,
    embedding_model_version INT NOT NULL,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
) PARTITION BY HASH (storage_partition_id);

DO $$
DECLARE
    partition_index INT;
BEGIN
    FOR partition_index IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS moa.embeddings_p%s
             PARTITION OF moa.embeddings
             FOR VALUES WITH (MODULUS 32, REMAINDER %s)',
            lpad(partition_index::TEXT, 2, '0'),
            partition_index
        );
    END LOOP;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS embeddings_storage_partition_uid_unique
    ON moa.embeddings (storage_partition_id, uid) NULLS NOT DISTINCT;
CREATE INDEX IF NOT EXISTS embeddings_embedding_hnsw_idx
    ON moa.embeddings USING hnsw (embedding public.halfvec_cosine_ops)
    WITH (m = 16, ef_construction = 64);
CREATE INDEX IF NOT EXISTS embeddings_ws_scope_label_idx
    ON moa.embeddings (storage_partition_id, scope, label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS embeddings_uid_idx
    ON moa.embeddings (uid);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
SELECT moa.apply_three_tier_rls('moa.embeddings'::REGCLASS);

-- Source: V000016__session_graph_changelog.sql

CREATE TABLE IF NOT EXISTS moa.graph_changelog (
    change_id BIGSERIAL,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    actor_id TEXT,
    actor_kind TEXT NOT NULL
        CHECK (actor_kind IN ('user', 'agent', 'system', 'promoter', 'admin')),
    op TEXT NOT NULL
        CHECK (op IN (
            'create',
            'update',
            'supersede',
            'invalidate',
            'erase'
        )),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('node', 'edge', 'contact')),
    target_label TEXT NOT NULL
        CHECK (
            target_label = ANY(moa.graph_node_labels())
            OR target_label = ANY(moa.graph_edge_labels())
        ),
    target_uid UUID NOT NULL,
    payload JSONB NOT NULL,
    redaction_marker TEXT,
    pii_class TEXT NOT NULL DEFAULT 'none'
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    audit_metadata JSONB,
    cause_change_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (change_id, created_at),
    CHECK (scope IS NOT NULL)
) PARTITION BY RANGE (created_at);

DO $$
DECLARE
    month_start DATE := (date_trunc('month', now()) - INTERVAL '12 months')::DATE;
    partition_index INT;
    partition_start DATE;
    partition_end DATE;
BEGIN
    FOR partition_index IN 0..13 LOOP
        partition_start := (month_start + (partition_index || ' months')::INTERVAL)::DATE;
        partition_end := (month_start + ((partition_index + 1) || ' months')::INTERVAL)::DATE;
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS moa.graph_changelog_%s
             PARTITION OF moa.graph_changelog
             FOR VALUES FROM (%L) TO (%L)',
            to_char(partition_start, 'YYYY_MM'),
            partition_start,
            partition_end
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS changelog_ws_idx
    ON moa.graph_changelog (storage_partition_id, created_at DESC);
CREATE INDEX IF NOT EXISTS changelog_target_uid_idx
    ON moa.graph_changelog (target_uid);
CREATE INDEX IF NOT EXISTS changelog_actor_idx
    ON moa.graph_changelog (actor_id)
    WHERE actor_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS changelog_op_idx
    ON moa.graph_changelog (op);
CREATE INDEX IF NOT EXISTS changelog_cause_idx
    ON moa.graph_changelog (cause_change_id)
    WHERE cause_change_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.storage_partition_state (
    storage_partition_id TEXT PRIMARY KEY,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    changelog_version BIGINT NOT NULL DEFAULT 0,
    vector_backend TEXT NOT NULL DEFAULT 'pgvector'
        CHECK (vector_backend IN ('pgvector', 'turbopuffer')),
    vector_backend_state TEXT NOT NULL DEFAULT 'steady'
        CHECK (vector_backend_state IN ('steady', 'migrating', 'dual_read')),
    dual_read_until TIMESTAMPTZ,
    embedding_model TEXT NOT NULL DEFAULT 'embed-v4.0',
    embedding_model_version INT NOT NULL DEFAULT 1,
    embedding_dimension INT NOT NULL DEFAULT 1024 CHECK (embedding_dimension > 0),
    reembed_state TEXT NOT NULL DEFAULT 'steady'
        CHECK (reembed_state IN ('steady', 'in_progress')),
    hipaa_tier TEXT NOT NULL DEFAULT 'standard'
        CHECK (hipaa_tier IN ('standard', 'hipaa', 'restricted')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (user_id IS NULL),
    CHECK (scope = 'tenant')
);

ALTER TABLE moa.storage_partition_state
    ADD COLUMN IF NOT EXISTS embedding_model TEXT NOT NULL DEFAULT 'embed-v4.0',
    ADD COLUMN IF NOT EXISTS embedding_model_version INT NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS embedding_dimension INT NOT NULL DEFAULT 1024 CHECK (embedding_dimension > 0),
    ADD COLUMN IF NOT EXISTS reembed_state TEXT NOT NULL DEFAULT 'steady'
        CHECK (reembed_state IN ('steady', 'in_progress'));

CREATE INDEX IF NOT EXISTS storage_partition_state_version_idx
    ON moa.storage_partition_state (storage_partition_id, changelog_version);

CREATE OR REPLACE FUNCTION moa.bump_storage_partition_state_from_changelog() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.storage_partition_id IS NULL THEN
        RETURN NEW;
    END IF;

    INSERT INTO moa.storage_partition_state (storage_partition_id, changelog_version)
    VALUES (NEW.storage_partition_id, 1)
    ON CONFLICT (storage_partition_id) DO UPDATE
        SET changelog_version = moa.storage_partition_state.changelog_version + 1,
            updated_at = now();

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS graph_changelog_bump_storage_partition_state ON moa.graph_changelog;
CREATE TRIGGER graph_changelog_bump_storage_partition_state
    AFTER INSERT ON moa.graph_changelog
    FOR EACH ROW
    EXECUTE FUNCTION moa.bump_storage_partition_state_from_changelog();

ALTER TABLE moa.graph_changelog ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.graph_changelog FORCE ROW LEVEL SECURITY;
SELECT moa.drop_three_tier_policies('moa.graph_changelog'::REGCLASS);
DROP POLICY IF EXISTS rd_auditor ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app_tenant ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app_user ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_promoter ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_promoter_global ON moa.graph_changelog;
SELECT moa.apply_three_tier_read_policies('moa.graph_changelog'::REGCLASS);

CREATE POLICY rd_auditor ON moa.graph_changelog
    FOR SELECT TO moa_auditor
    USING (true);
CREATE POLICY ins_app_tenant ON moa.graph_changelog
    FOR INSERT TO moa_app
    WITH CHECK (
        scope = 'tenant'
        AND storage_partition_id = moa.current_storage_partition()
    );
CREATE POLICY ins_app_user ON moa.graph_changelog
    FOR INSERT TO moa_app
    WITH CHECK (
        scope = 'contact'
        AND storage_partition_id = moa.current_storage_partition()
        AND user_id = moa.current_user_id()
    );
CREATE POLICY ins_promoter_global ON moa.graph_changelog
    FOR INSERT TO moa_promoter
    WITH CHECK (scope = 'global');

ALTER TABLE moa.storage_partition_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.storage_partition_state FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS ws_self_select ON moa.storage_partition_state;
DROP POLICY IF EXISTS ws_self_insert ON moa.storage_partition_state;
DROP POLICY IF EXISTS ws_self_update ON moa.storage_partition_state;
DROP POLICY IF EXISTS ws_promoter ON moa.storage_partition_state;
DROP POLICY IF EXISTS owner_dev_access ON moa.storage_partition_state;
CREATE POLICY ws_self_select ON moa.storage_partition_state
    FOR SELECT TO moa_app
    USING (storage_partition_id = moa.current_storage_partition());
CREATE POLICY ws_self_insert ON moa.storage_partition_state
    FOR INSERT TO moa_app
    WITH CHECK (storage_partition_id = moa.current_storage_partition());
CREATE POLICY ws_self_update ON moa.storage_partition_state
    FOR UPDATE TO moa_app
    USING (storage_partition_id = moa.current_storage_partition())
    WITH CHECK (storage_partition_id = moa.current_storage_partition());
CREATE POLICY ws_promoter ON moa.storage_partition_state
    FOR ALL TO moa_promoter
    USING (true)
    WITH CHECK (true);

REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM PUBLIC;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_app;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_promoter;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_auditor;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_owner;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_replicator;
GRANT SELECT, INSERT ON moa.graph_changelog TO moa_app;
GRANT SELECT, INSERT ON moa.graph_changelog TO moa_promoter;
GRANT SELECT ON moa.graph_changelog TO moa_auditor;
GRANT USAGE, SELECT ON SEQUENCE moa.graph_changelog_change_id_seq TO moa_app, moa_promoter;

GRANT SELECT, INSERT, UPDATE ON moa.storage_partition_state TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.storage_partition_state TO moa_promoter;

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter, moa_auditor, moa_replicator;
GRANT SELECT ON moa.graph_changelog TO moa_replicator;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_publication WHERE pubname = 'moa_changelog_pub'
    ) THEN
        EXECUTE 'CREATE PUBLICATION moa_changelog_pub
                 FOR TABLE moa.graph_changelog
                 WITH (publish_via_partition_root = true)';
    ELSE
        BEGIN
            EXECUTE 'ALTER PUBLICATION moa_changelog_pub ADD TABLE moa.graph_changelog';
        EXCEPTION
            WHEN duplicate_object THEN NULL;
        END;
        EXECUTE 'ALTER PUBLICATION moa_changelog_pub
                 SET (publish_via_partition_root = true)';
    END IF;
END $$;

CREATE OR REPLACE FUNCTION moa.ensure_changelog_replication_slot() RETURNS TEXT
LANGUAGE plpgsql
AS $$
BEGIN
    IF current_setting('wal_level') <> 'logical' THEN
        RAISE EXCEPTION
            'wal_level must be logical before creating moa_changelog_slot';
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_replication_slots WHERE slot_name = 'moa_changelog_slot'
    ) THEN
        PERFORM pg_create_logical_replication_slot('moa_changelog_slot', 'pgoutput');
    END IF;

    RETURN 'moa_changelog_slot';
END;
$$;

REVOKE ALL ON FUNCTION moa.ensure_changelog_replication_slot() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.ensure_changelog_replication_slot() TO moa_owner;

-- Source: V000017__session_ingest.sql

CREATE TABLE IF NOT EXISTS moa.ingest_dedup (
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    session_id UUID NOT NULL,
    turn_seq BIGINT NOT NULL,
    fact_hash BYTEA NOT NULL,
    fact_uid UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (storage_partition_id, session_id, turn_seq, fact_hash),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS ingest_dedup_fact_uid_idx
    ON moa.ingest_dedup (fact_uid);

CREATE TABLE IF NOT EXISTS moa.ingest_dlq (
    dlq_id BIGSERIAL PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    session_id UUID,
    turn_seq BIGINT,
    payload JSONB NOT NULL,
    error TEXT NOT NULL,
    retry_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    next_retry_at TIMESTAMPTZ,
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS ingest_dlq_retry_idx
    ON moa.ingest_dlq (storage_partition_id, next_retry_at, retry_count);
CREATE INDEX IF NOT EXISTS ingest_dlq_session_idx
    ON moa.ingest_dlq (storage_partition_id, session_id, turn_seq);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
GRANT USAGE, SELECT ON SEQUENCE moa.ingest_dlq_dlq_id_seq TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.ingest_dedup'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.ingest_dlq'::REGCLASS);

ALTER TABLE moa.storage_partition_state
    ADD COLUMN IF NOT EXISTS slow_path_degraded BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE moa.storage_partition_state
    ADD COLUMN IF NOT EXISTS ingest_concurrency INT NOT NULL DEFAULT 8;

ALTER TABLE moa.storage_partition_state
    DROP CONSTRAINT IF EXISTS storage_partition_state_ingest_concurrency_positive;

ALTER TABLE moa.storage_partition_state
    ADD CONSTRAINT storage_partition_state_ingest_concurrency_positive
    CHECK (ingest_concurrency > 0);

-- Source: V000020__session_pgaudit.sql

CREATE EXTENSION IF NOT EXISTS pgaudit;

DO $$
BEGIN
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.node_index IS ''READ, WRITE''';
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.embeddings IS ''READ, WRITE''';
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.graph_changelog IS ''READ, WRITE''';
EXCEPTION
    WHEN others THEN
        RAISE NOTICE
            'pgaudit SECURITY LABELs skipped: %',
            SQLERRM;
END $$;

GRANT USAGE ON SCHEMA moa TO moa_auditor;
GRANT SELECT ON moa.graph_changelog TO moa_auditor;
GRANT SELECT ON moa.node_index TO moa_auditor;
GRANT SELECT ON moa.embeddings TO moa_auditor;

CREATE OR REPLACE VIEW moa.audit_logs AS
SELECT *
FROM moa.graph_changelog
ORDER BY created_at DESC;

GRANT SELECT ON moa.audit_logs TO moa_auditor;

-- Source: V000021__session_privacy_export.sql

ALTER TABLE moa.graph_changelog
    DROP CONSTRAINT IF EXISTS graph_changelog_op_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_op_check
    CHECK (op IN (
        'create',
        'update',
        'supersede',
        'invalidate',
        'erase',
        'export'
    ));

ALTER TABLE moa.graph_changelog
    DROP CONSTRAINT IF EXISTS graph_changelog_target_kind_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_target_kind_check
    CHECK (target_kind IN ('node', 'edge', 'contact'));

ALTER TABLE moa.graph_changelog
    DROP CONSTRAINT IF EXISTS graph_changelog_target_label_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_target_label_check
    CHECK (
        target_label = 'User'
        OR target_label = ANY(moa.graph_node_labels())
        OR target_label = ANY(moa.graph_edge_labels())
    );

CREATE TABLE IF NOT EXISTS moa.audit_jti_used (
    jti TEXT PRIMARY KEY,
    op TEXT NOT NULL,
    subject_user_id TEXT NOT NULL,
    approver_id TEXT NOT NULL,
    approval_claims JSONB NOT NULL,
    used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS audit_jti_used_subject_idx
    ON moa.audit_jti_used (subject_user_id, used_at DESC);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter, moa_auditor;
GRANT SELECT, INSERT ON moa.audit_jti_used TO moa_app, moa_promoter;
GRANT SELECT ON moa.audit_jti_used TO moa_auditor;

DROP POLICY IF EXISTS rd_auditor ON moa.node_index;
CREATE POLICY rd_auditor ON moa.node_index
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.embeddings;
CREATE POLICY rd_auditor ON moa.embeddings
    FOR SELECT TO moa_auditor
    USING (true);

-- Source: V000022__session_privacy_erase.sql

ALTER TABLE moa.graph_changelog
    DROP CONSTRAINT IF EXISTS graph_changelog_op_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_op_check
    CHECK (op IN (
        'create',
        'update',
        'supersede',
        'invalidate',
        'erase',
        'export'
    ));

-- Source: V000023__session_vector_backend_turbopuffer.sql

-- M26: Turbopuffer is an opt-in vector backend.
--
-- `moa.storage_partition_state.vector_backend` was introduced in 015_graph_changelog.sql
-- with CHECK (vector_backend IN ('pgvector', 'turbopuffer')). This migration is
-- intentionally schema-neutral and documents that M26 uses the existing column.
SELECT 1;

-- Source: V000024__session_storage_partition_vector_promotion.sql

-- M27: Storage-partition vector-backend promotion state lookup.
CREATE INDEX IF NOT EXISTS storage_partition_state_dual_read_idx
    ON moa.storage_partition_state (vector_backend_state)
    WHERE vector_backend_state != 'steady';

-- Source: V000025__session_lineage.sql

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'timescaledb') THEN
        BEGIN
            CREATE EXTENSION IF NOT EXISTS timescaledb;
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'TimescaleDB extension is available but could not be created: %', SQLERRM;
        END;
    END IF;
END
$$;

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.turn_lineage (
    turn_id        UUID        NOT NULL,
    session_id     UUID        NOT NULL,
    user_id        TEXT        NOT NULL,
    storage_partition_id   TEXT        NOT NULL,
    ts             TIMESTAMPTZ NOT NULL,
    tier           SMALLINT    NOT NULL DEFAULT 1,
    record_kind    SMALLINT    NOT NULL,
    payload        JSONB       NOT NULL,
    answer_text    TEXT,
    integrity_hash BYTEA       NOT NULL,
    prev_hash      BYTEA,
    PRIMARY KEY (turn_id, record_kind, ts)
);

CREATE INDEX IF NOT EXISTS ix_lineage_session_ts
    ON analytics.turn_lineage (session_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_storage_partition_user_ts
    ON analytics.turn_lineage (storage_partition_id, user_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_zero_recall
    ON analytics.turn_lineage (ts DESC)
    WHERE record_kind = 1
      AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
      AND jsonb_array_length(payload #> '{record,top_k}') = 0;

CREATE INDEX IF NOT EXISTS ix_lineage_payload_gin
    ON analytics.turn_lineage
    USING GIN ((payload) jsonb_path_ops);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb') THEN
        PERFORM create_hypertable(
            'analytics.turn_lineage',
            'ts',
            chunk_time_interval => INTERVAL '1 day',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            ALTER TABLE analytics.turn_lineage SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'storage_partition_id',
                timescaledb.compress_orderby = 'ts DESC, turn_id'
            )
        $ddl$;

        PERFORM add_compression_policy(
            'analytics.turn_lineage',
            INTERVAL '7 days',
            if_not_exists => TRUE
        );
        PERFORM add_retention_policy(
            'analytics.turn_lineage',
            INTERVAL '30 days',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   storage_partition_id,
                   COUNT(*) AS turns,
                   COUNT(*) FILTER (
                       WHERE record_kind = 1
                         AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
                         AND jsonb_array_length(payload #> '{record,top_k}') = 0
                   ) AS zero_recall
            FROM analytics.turn_lineage
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.turn_recall_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );
    ELSE
        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   storage_partition_id,
                   COUNT(*) AS turns,
                   COUNT(*) FILTER (
                       WHERE record_kind = 1
                         AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
                         AND jsonb_array_length(payload #> '{record,top_k}') = 0
                   ) AS zero_recall
            FROM analytics.turn_lineage
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;
    END IF;
END
$$;

-- Source: V000026__session_lineage_scores.sql

CREATE TABLE IF NOT EXISTS analytics.scores (
    score_id           UUID             NOT NULL,
    ts                 TIMESTAMPTZ      NOT NULL,
    storage_partition_id       TEXT             NOT NULL,
    user_id            TEXT,
    target_kind        TEXT             NOT NULL,
    turn_id            UUID,
    session_id         UUID,
    run_id             UUID,
    item_id            UUID,
    dataset_id         UUID,
    name               TEXT             NOT NULL,
    value_type         TEXT             NOT NULL,
    value_numeric      DOUBLE PRECISION,
    value_boolean      BOOLEAN,
    value_categorical  TEXT,
    source             TEXT             NOT NULL,
    model_or_evaluator TEXT             NOT NULL,
    comment            TEXT,
    PRIMARY KEY (score_id, ts)
);

CREATE INDEX IF NOT EXISTS ix_scores_storage_partition_name_ts
    ON analytics.scores (storage_partition_id, name, ts DESC);

CREATE INDEX IF NOT EXISTS ix_scores_turn
    ON analytics.scores (turn_id)
    WHERE turn_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_scores_run
    ON analytics.scores (run_id)
    WHERE run_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS analytics.eval_datasets (
    dataset_id  UUID        PRIMARY KEY,
    name        TEXT        NOT NULL UNIQUE,
    source_path TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS analytics.eval_dataset_items (
    item_id         UUID        PRIMARY KEY,
    dataset_id      UUID        NOT NULL REFERENCES analytics.eval_datasets(dataset_id) ON DELETE CASCADE,
    storage_partition_id    TEXT        NOT NULL,
    scope           JSONB       NOT NULL,
    query           TEXT        NOT NULL,
    expected_answer TEXT,
    expected_chunk_ids UUID[]   NOT NULL DEFAULT '{}',
    metadata        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_eval_dataset_items_dataset
    ON analytics.eval_dataset_items (dataset_id, created_at ASC);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb') THEN
        PERFORM create_hypertable(
            'analytics.scores',
            'ts',
            chunk_time_interval => INTERVAL '1 day',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            ALTER TABLE analytics.scores SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'storage_partition_id, name',
                timescaledb.compress_orderby = 'ts DESC'
            )
        $ddl$;

        PERFORM add_compression_policy(
            'analytics.scores',
            INTERVAL '7 days',
            if_not_exists => TRUE
        );
        PERFORM add_retention_policy(
            'analytics.scores',
            INTERVAL '90 days',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.grounding_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   storage_partition_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.grounding_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.nli_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   storage_partition_id,
                   AVG(value_numeric) AS p50,
                   MAX(value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.nli_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );
    ELSE
        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.grounding_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   storage_partition_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.nli_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   storage_partition_id,
                   percentile_cont(0.5) WITHIN GROUP (ORDER BY value_numeric) AS p50,
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;
    END IF;
END
$$;

-- Source: V000027__session_lineage_audit.sql

CREATE TABLE IF NOT EXISTS analytics.compliance_tenants (
    storage_partition_id       TEXT PRIMARY KEY,
    enabled            BOOLEAN     NOT NULL DEFAULT TRUE,
    enabled_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_years    INT         NOT NULL DEFAULT 10,
    s3_bucket          TEXT        NOT NULL,
    kms_key_id         TEXT,
    signing_key_label  TEXT        NOT NULL,
    notes              TEXT
);

CREATE TABLE IF NOT EXISTS analytics.compliance_storage_partition_state (
    storage_partition_id          TEXT PRIMARY KEY,
    last_integrity_hash   BYTEA,
    last_ts               TIMESTAMPTZ,
    record_count          BIGINT NOT NULL DEFAULT 0,
    last_root_id          UUID
);

CREATE TABLE IF NOT EXISTS analytics.audit_roots (
    root_id            UUID PRIMARY KEY,
    storage_partition_id       TEXT        NOT NULL,
    window_start       TIMESTAMPTZ NOT NULL,
    window_end         TIMESTAMPTZ NOT NULL,
    record_count       BIGINT      NOT NULL,
    merkle_root        BYTEA       NOT NULL,
    signature          BYTEA       NOT NULL,
    signing_key_label  TEXT        NOT NULL,
    s3_object_uri      TEXT        NOT NULL,
    s3_object_etag     TEXT        NOT NULL,
    object_lock_mode   TEXT        NOT NULL,
    retain_until       TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_audit_roots_storage_partition_window
    ON analytics.audit_roots (storage_partition_id, window_end DESC);

CREATE SCHEMA IF NOT EXISTS pii_vault;

CREATE TABLE IF NOT EXISTS pii_vault.subject_keys (
    subject_pseudonym BYTEA PRIMARY KEY,
    storage_partition_id      TEXT        NOT NULL,
    hmac_key_handle   TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    erased_at         TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS pii_vault.plaintext_side (
    record_id          UUID PRIMARY KEY,
    subject_pseudonym  BYTEA       NOT NULL,
    storage_partition_id       TEXT        NOT NULL,
    field_name         TEXT        NOT NULL,
    ciphertext         BYTEA       NOT NULL,
    encryption_context JSONB       NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (subject_pseudonym) REFERENCES pii_vault.subject_keys(subject_pseudonym)
);

CREATE INDEX IF NOT EXISTS ix_plaintext_subject
    ON pii_vault.plaintext_side (subject_pseudonym);

CREATE INDEX IF NOT EXISTS ix_plaintext_storage_partition
    ON pii_vault.plaintext_side (storage_partition_id, created_at);

-- Source: V000028__session_events_append_only.sql

REVOKE UPDATE, DELETE, TRUNCATE ON TABLE events FROM moa_app;

CREATE OR REPLACE FUNCTION events_append_only_guard() RETURNS trigger AS $$
BEGIN
  IF current_setting('moa.events_maintenance', true) = 'on' THEN
    IF TG_OP = 'UPDATE' THEN
      RETURN NEW;
    END IF;
    RETURN OLD;
  END IF;

  RAISE EXCEPTION 'events table is append-only (op=%, session=%, seq=%)',
    TG_OP, OLD.session_id, OLD.sequence_num
    USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION events_append_only_guard() IS
  'Blocks UPDATE/DELETE on events unless a privileged maintenance session sets moa.events_maintenance=on.';

DROP TRIGGER IF EXISTS events_no_update ON events;
CREATE TRIGGER events_no_update
  BEFORE UPDATE ON events
  FOR EACH ROW EXECUTE FUNCTION events_append_only_guard();

DROP TRIGGER IF EXISTS events_no_delete ON events;
CREATE TRIGGER events_no_delete
  BEFORE DELETE ON events
  FOR EACH ROW EXECUTE FUNCTION events_append_only_guard();

-- Source: V000029__session_lineage_dead_letters.sql

-- Dead-letter storage for lineage writer batches that cannot be written after bounded retries.

CREATE TABLE IF NOT EXISTS analytics.lineage_dead_letters (
    dead_letter_id      UUID        PRIMARY KEY,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error               TEXT        NOT NULL,
    attempts            INTEGER     NOT NULL,
    row_count           INTEGER     NOT NULL,
    first_storage_partition_id  TEXT,
    first_session_id    UUID,
    first_turn_id       UUID,
    rows                JSONB       NOT NULL
);

CREATE INDEX IF NOT EXISTS lineage_dead_letters_created_idx
    ON analytics.lineage_dead_letters (created_at DESC);

-- Source: V000033__session_memory_digests.sql

CREATE TABLE IF NOT EXISTS moa.memory_digests (
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    content TEXT NOT NULL,
    source_fact_uids JSONB NOT NULL DEFAULT '[]'::jsonb,
    version INTEGER NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    CHECK (scope IN ('contact', 'tenant')),
    CHECK (scope IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS memory_digests_identity
    ON moa.memory_digests (storage_partition_id, scope, COALESCE(user_id, ''));

CREATE INDEX IF NOT EXISTS memory_digests_updated_at_idx
    ON moa.memory_digests (storage_partition_id, updated_at);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.memory_digests'::REGCLASS);

-- Source: V000034__session_quality_score_and_lineage.sql

ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS quality_score DOUBLE PRECISION NOT NULL DEFAULT 0.5;

CREATE TABLE IF NOT EXISTS moa.retrieval_lineage (
    storage_partition_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    session_id UUID NOT NULL,
    turn_seq BIGINT NOT NULL,
    uid UUID NOT NULL,
    rank INTEGER NOT NULL CHECK (rank > 0),
    retrieved_at TIMESTAMPTZ NOT NULL,
    CHECK (scope = 'contact')
);

CREATE INDEX IF NOT EXISTS retrieval_lineage_ws_time
    ON moa.retrieval_lineage (storage_partition_id, retrieved_at);
CREATE INDEX IF NOT EXISTS retrieval_lineage_uid_time
    ON moa.retrieval_lineage (uid, retrieved_at);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.retrieval_lineage'::REGCLASS);

-- Source: V000035__session_segment_assessment_columns.sql

DROP MATERIALIZED VIEW IF EXISTS skill_resolution_rates;
DROP MATERIALIZED VIEW IF EXISTS segment_baselines;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'outcome'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution TO outcome;
    END IF;

    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution_signal'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'assessment'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution_signal TO assessment;
    END IF;

    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution_confidence'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'outcome_confidence'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution_confidence TO outcome_confidence;
    END IF;
END $$;

DROP INDEX IF EXISTS idx_task_segments_tenant_resolution;
CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_outcome
    ON task_segments (tenant_id, outcome)
    WHERE outcome IS NOT NULL;

CREATE MATERIALIZED VIEW skill_resolution_rates AS
SELECT
    t.tenant_id,
    unnest(t.skills_activated) AS skill_name,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN t.outcome = 'resolved' THEN 1.0
             WHEN t.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS resolution_rate,
    AVG(t.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(t.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM task_segments t
WHERE t.outcome IS NOT NULL
  AND array_length(t.skills_activated, 1) IS NOT NULL
GROUP BY t.tenant_id, skill_name;

CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_resolution_rates_unique
    ON skill_resolution_rates(tenant_id, skill_name);

CREATE MATERIALIZED VIEW segment_baselines AS
SELECT
    tenant_id,
    COUNT(*)::BIGINT AS sample_count,
    AVG(turn_count)::DOUBLE PRECISION AS avg_turns,
    STDDEV(turn_count)::DOUBLE PRECISION AS stddev_turns,
    AVG(token_cost)::DOUBLE PRECISION AS avg_cost,
    STDDEV(token_cost)::DOUBLE PRECISION AS stddev_cost,
    AVG(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS avg_duration_secs,
    STDDEV(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS stddev_duration_secs
FROM task_segments
WHERE ended_at IS NOT NULL
GROUP BY tenant_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_segment_baselines_unique
    ON segment_baselines(tenant_id);

-- Source: V000036__session_experience_learning.sql

CREATE TABLE IF NOT EXISTS experience_records (
    id UUID PRIMARY KEY,
    segment_id UUID NOT NULL REFERENCES task_segments(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    tenant_id TEXT NOT NULL,
    task_summary TEXT,
    task_fingerprint TEXT NOT NULL,
    task_fingerprint_payload JSONB NOT NULL,
    task_facets JSONB NOT NULL,
    actions TEXT[] NOT NULL DEFAULT '{}',
    resources JSONB NOT NULL DEFAULT '[]'::JSONB,
    outcome TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    duration_ms BIGINT,
    assessment_policy_version TEXT NOT NULL,
    extraction_policy_version TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(segment_id, extraction_policy_version)
);

CREATE INDEX IF NOT EXISTS idx_experience_records_session
    ON experience_records (session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_experience_records_tenant_task
    ON experience_records (tenant_id, task_fingerprint, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_experience_records_scope
    ON experience_records (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS experience_attributions (
    id UUID PRIMARY KEY,
    experience_id UUID NOT NULL REFERENCES experience_records(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    subject_type TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    effect TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(experience_id, subject_type, subject_id)
);

CREATE INDEX IF NOT EXISTS idx_experience_attributions_experience
    ON experience_attributions (experience_id, subject_type);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_subject
    ON experience_attributions (tenant_id, subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_scope
    ON experience_attributions (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS learning_candidates (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    candidate_type TEXT NOT NULL,
    status TEXT NOT NULL,
    target_id TEXT,
    target_label TEXT,
    task_fingerprint TEXT,
    task_fingerprint_payload JSONB,
    task_facets JSONB,
    payload JSONB NOT NULL,
    evaluation_payload JSONB,
    source_experience_ids UUID[] NOT NULL DEFAULT '{}',
    confidence NUMERIC(4,3),
    risk_class TEXT NOT NULL,
    promotion_requirements TEXT[] NOT NULL DEFAULT '{}',
    status_reason TEXT,
    batch_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_learning_candidates_tenant_status
    ON learning_candidates (tenant_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_task
    ON learning_candidates (tenant_id, task_fingerprint);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_batch
    ON learning_candidates (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learning_candidates_scope
    ON learning_candidates (storage_partition_id, scope, user_id);

DROP MATERIALIZED VIEW IF EXISTS task_strategy_success_rates;

CREATE MATERIALIZED VIEW task_strategy_success_rates AS
SELECT
    e.tenant_id,
    e.task_fingerprint,
    a.subject_type,
    a.subject_id,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN e.outcome = 'resolved' THEN 1.0
             WHEN e.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS success_rate,
    AVG(e.confidence)::DOUBLE PRECISION AS avg_confidence,
    AVG(e.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(e.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM experience_records e
JOIN experience_attributions a ON a.experience_id = e.id
WHERE a.subject_type IN ('skill', 'tool', 'memory', 'verification', 'policy')
GROUP BY e.tenant_id, e.task_fingerprint, a.subject_type, a.subject_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_strategy_success_rates_unique
    ON task_strategy_success_rates(tenant_id, task_fingerprint, subject_type, subject_id);

SELECT moa.apply_three_tier_rls('experience_records'::REGCLASS);
SELECT moa.apply_three_tier_rls('experience_attributions'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_candidates'::REGCLASS);

-- Source: V000037__session_public_experience_learning.sql

CREATE TABLE IF NOT EXISTS experience_records (
    id UUID PRIMARY KEY,
    segment_id UUID NOT NULL REFERENCES task_segments(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    tenant_id TEXT NOT NULL,
    task_summary TEXT,
    task_fingerprint TEXT NOT NULL,
    task_fingerprint_payload JSONB NOT NULL,
    task_facets JSONB NOT NULL,
    actions TEXT[] NOT NULL DEFAULT '{}',
    resources JSONB NOT NULL DEFAULT '[]'::JSONB,
    outcome TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    duration_ms BIGINT,
    assessment_policy_version TEXT NOT NULL,
    extraction_policy_version TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(segment_id, extraction_policy_version)
);

CREATE INDEX IF NOT EXISTS idx_experience_records_session
    ON experience_records (session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_experience_records_tenant_task
    ON experience_records (tenant_id, task_fingerprint, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_experience_records_scope
    ON experience_records (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS experience_attributions (
    id UUID PRIMARY KEY,
    experience_id UUID NOT NULL REFERENCES experience_records(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    subject_type TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    effect TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(experience_id, subject_type, subject_id)
);

CREATE INDEX IF NOT EXISTS idx_experience_attributions_experience
    ON experience_attributions (experience_id, subject_type);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_subject
    ON experience_attributions (tenant_id, subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_scope
    ON experience_attributions (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS learning_candidates (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    candidate_type TEXT NOT NULL,
    status TEXT NOT NULL,
    target_id TEXT,
    target_label TEXT,
    task_fingerprint TEXT,
    task_fingerprint_payload JSONB,
    task_facets JSONB,
    payload JSONB NOT NULL,
    evaluation_payload JSONB,
    source_experience_ids UUID[] NOT NULL DEFAULT '{}',
    confidence NUMERIC(4,3),
    risk_class TEXT NOT NULL,
    promotion_requirements TEXT[] NOT NULL DEFAULT '{}',
    status_reason TEXT,
    batch_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_learning_candidates_tenant_status
    ON learning_candidates (tenant_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_task
    ON learning_candidates (tenant_id, task_fingerprint);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_batch
    ON learning_candidates (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learning_candidates_scope
    ON learning_candidates (storage_partition_id, scope, user_id);

DROP MATERIALIZED VIEW IF EXISTS task_strategy_success_rates;

CREATE MATERIALIZED VIEW task_strategy_success_rates AS
SELECT
    e.tenant_id,
    e.task_fingerprint,
    a.subject_type,
    a.subject_id,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN e.outcome = 'resolved' THEN 1.0
             WHEN e.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS success_rate,
    AVG(e.confidence)::DOUBLE PRECISION AS avg_confidence,
    AVG(e.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(e.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM experience_records e
JOIN experience_attributions a ON a.experience_id = e.id
WHERE a.subject_type IN ('skill', 'tool', 'memory', 'verification', 'policy')
GROUP BY e.tenant_id, e.task_fingerprint, a.subject_type, a.subject_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_strategy_success_rates_unique
    ON task_strategy_success_rates(tenant_id, task_fingerprint, subject_type, subject_id);

SELECT moa.apply_three_tier_rls('experience_records'::REGCLASS);
SELECT moa.apply_three_tier_rls('experience_attributions'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_candidates'::REGCLASS);

-- Source: V000038__session_agent_artifacts.sql

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS moa.artifact (
    artifact_uid UUID PRIMARY KEY,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    tags TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    latest_revision_uid UUID,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (kind IN ('agent', 'skill', 'connector', 'workflow', 'action', 'experiment_plan')),
    CHECK (name <> '')
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_active_name_uniq
    ON moa.artifact (
        coalesce(storage_partition_id, ''),
        coalesce(user_id, ''),
        kind,
        name
    )
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_scope_idx
    ON moa.artifact (storage_partition_id, scope, user_id, kind, name)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_tags_gin
    ON moa.artifact USING GIN (tags);

CREATE TABLE IF NOT EXISTS moa.artifact_revision (
    revision_uid UUID PRIMARY KEY,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    definition JSONB NOT NULL,
    canonical_hash BYTEA NOT NULL,
    source_format TEXT NOT NULL,
    source_text BYTEA NOT NULL,
    status TEXT NOT NULL,
    validation_report JSONB NOT NULL DEFAULT '{}'::JSONB,
    version INT NOT NULL,
    published_at TIMESTAMPTZ,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (source_format IN ('json', 'yaml')),
    CHECK (status IN ('draft', 'published', 'archived')),
    CHECK (version > 0)
);

ALTER TABLE moa.artifact
    DROP CONSTRAINT IF EXISTS artifact_latest_revision_fk,
    ADD CONSTRAINT artifact_latest_revision_fk
        FOREIGN KEY (latest_revision_uid)
        REFERENCES moa.artifact_revision(revision_uid)
        DEFERRABLE INITIALLY DEFERRED;

CREATE UNIQUE INDEX IF NOT EXISTS artifact_revision_version_uniq
    ON moa.artifact_revision (artifact_uid, version);

CREATE INDEX IF NOT EXISTS artifact_revision_artifact_idx
    ON moa.artifact_revision (artifact_uid, status, version DESC)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_revision_scope_idx
    ON moa.artifact_revision (storage_partition_id, scope, user_id, status)
    WHERE valid_to IS NULL;

CREATE TABLE IF NOT EXISTS moa.artifact_file (
    file_uid UUID PRIMARY KEY,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    path TEXT NOT NULL,
    content BYTEA NOT NULL,
    content_sha256 BYTEA NOT NULL,
    content_type TEXT,
    executable BOOLEAN NOT NULL DEFAULT false,
    file_size_bytes BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (path <> ''),
    CHECK (path NOT LIKE '/%'),
    CHECK (path NOT LIKE '%..%'),
    CHECK (file_size_bytes >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_file_revision_path_uniq
    ON moa.artifact_file (revision_uid, path);

CREATE INDEX IF NOT EXISTS artifact_file_artifact_idx
    ON moa.artifact_file (artifact_uid);

CREATE INDEX IF NOT EXISTS artifact_file_scope_idx
    ON moa.artifact_file (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS moa.artifact_run (
    run_uid UUID PRIMARY KEY,
    artifact_uid UUID REFERENCES moa.artifact(artifact_uid) ON DELETE SET NULL,
    revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE SET NULL,
    storage_partition_id TEXT,
    user_id TEXT,
    session_id UUID,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    workflow_ref TEXT NOT NULL,
    status TEXT NOT NULL,
    current_node_id TEXT,
    input JSONB NOT NULL DEFAULT '{}'::JSONB,
    state JSONB NOT NULL DEFAULT '{}'::JSONB,
    output JSONB,
    error TEXT,
    idempotency_key TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (status IN ('queued', 'running', 'pending_review', 'completed', 'failed', 'cancelled'))
);

CREATE INDEX IF NOT EXISTS artifact_run_scope_idx
    ON moa.artifact_run (storage_partition_id, scope, user_id, status, started_at DESC);

CREATE INDEX IF NOT EXISTS artifact_run_session_idx
    ON moa.artifact_run (session_id, started_at DESC)
    WHERE session_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS artifact_run_idempotency_uniq
    ON moa.artifact_run (
        coalesce(storage_partition_id, ''),
        coalesce(user_id, ''),
        workflow_ref,
        idempotency_key
    )
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.artifact_node_run (
    node_run_uid UUID PRIMARY KEY,
    run_uid UUID NOT NULL REFERENCES moa.artifact_run(run_uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    node_id TEXT NOT NULL,
    status TEXT NOT NULL,
    input JSONB NOT NULL DEFAULT '{}'::JSONB,
    output JSONB,
    error TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (status IN ('queued', 'running', 'pending_review', 'completed', 'failed', 'cancelled', 'skipped'))
);

CREATE INDEX IF NOT EXISTS artifact_node_run_run_idx
    ON moa.artifact_node_run (run_uid, started_at ASC);

CREATE INDEX IF NOT EXISTS artifact_node_run_scope_idx
    ON moa.artifact_node_run (storage_partition_id, scope, user_id, status);

SELECT moa.apply_three_tier_rls('moa.artifact'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_revision'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_file'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_run'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_node_run'::REGCLASS);

DROP POLICY IF EXISTS rd_auditor ON moa.artifact;
CREATE POLICY rd_auditor ON moa.artifact
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.artifact_revision;
CREATE POLICY rd_auditor ON moa.artifact_revision
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.artifact_file;
CREATE POLICY rd_auditor ON moa.artifact_file
    FOR SELECT TO moa_auditor
    USING (true);

GRANT SELECT ON moa.artifact TO moa_auditor;
GRANT SELECT ON moa.artifact_revision TO moa_auditor;
GRANT SELECT ON moa.artifact_file TO moa_auditor;

-- Source: V000039__session_behavior_experiments.sql

CREATE TABLE IF NOT EXISTS analytics.score_run (
    run_id UUID PRIMARY KEY,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    source TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS score_run_scope_source_idx
    ON analytics.score_run (storage_partition_id, scope, user_id, source, created_at DESC);

SELECT moa.apply_three_tier_rls('analytics.score_run'::REGCLASS);

CREATE TABLE IF NOT EXISTS moa.experiment_run (
    run_uid UUID PRIMARY KEY,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    name TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('agent_loop', 'workflow')),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'running', 'completed', 'failed', 'cancelled')),
    target JSONB NOT NULL,
    variant JSONB NOT NULL,
    scorecard JSONB NOT NULL DEFAULT '{}'::jsonb,
    score_run_id UUID NOT NULL REFERENCES analytics.score_run(run_id) ON DELETE RESTRICT,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    workflow_run_uid UUID REFERENCES moa.artifact_run(run_uid) ON DELETE SET NULL,
    artifact_revision_uids UUID[] NOT NULL DEFAULT '{}',
    idempotency_key TEXT,
    created_by_identity JSONB NOT NULL,
    error TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
SELECT DISTINCT score_run_id, storage_partition_id, user_id, 'experiment_run'
FROM moa.experiment_run
WHERE score_run_id IS NOT NULL
ON CONFLICT (run_id) DO NOTHING;

ALTER TABLE moa.experiment_run
    DROP CONSTRAINT IF EXISTS experiment_run_score_run_id_fkey,
    ADD CONSTRAINT experiment_run_score_run_id_fkey
        FOREIGN KEY (score_run_id)
        REFERENCES analytics.score_run(run_id)
        ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS experiment_run_scope_idx
    ON moa.experiment_run (storage_partition_id, scope, user_id, status, started_at DESC);

CREATE INDEX IF NOT EXISTS experiment_run_score_run_idx
    ON moa.experiment_run (score_run_id);

CREATE INDEX IF NOT EXISTS experiment_run_session_idx
    ON moa.experiment_run (session_id)
    WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS experiment_run_workflow_run_idx
    ON moa.experiment_run (workflow_run_uid)
    WHERE workflow_run_uid IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS experiment_run_idempotency_uniq
    ON moa.experiment_run (
        coalesce(storage_partition_id, ''),
        coalesce(user_id, ''),
        idempotency_key
    )
    WHERE idempotency_key IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.experiment_run'::REGCLASS);

CREATE TABLE IF NOT EXISTS moa.experiment_run_artifact_revision (
    run_uid UUID NOT NULL REFERENCES moa.experiment_run(run_uid) ON DELETE CASCADE,
    revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_uid, revision_uid),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS experiment_run_artifact_revision_revision_idx
    ON moa.experiment_run_artifact_revision (revision_uid);

CREATE INDEX IF NOT EXISTS experiment_run_artifact_revision_scope_idx
    ON moa.experiment_run_artifact_revision (storage_partition_id, scope, user_id, revision_uid);

SELECT moa.apply_three_tier_rls('moa.experiment_run_artifact_revision'::REGCLASS);

-- Source: V000040__session_behavior_lab_artifact_kinds.sql

ALTER TABLE moa.artifact
    DROP CONSTRAINT IF EXISTS artifact_kind_check;

DELETE FROM moa.artifact
WHERE kind IN (
    'simulation_persona',
    'simulation_profile',
    'simulation_data_bundle',
    'simulation_scenario'
);

ALTER TABLE moa.artifact
    ADD CONSTRAINT artifact_kind_check CHECK (
        kind IN (
            'skill',
            'connector',
            'workflow',
            'action',
            'agent',
            'experiment_plan'
        )
    );

-- Source: V000041__session_behavior_lab_trials.sql

CREATE TABLE IF NOT EXISTS moa.experiment_trial (
    trial_uid UUID PRIMARY KEY,
    run_uid UUID NOT NULL REFERENCES moa.experiment_run(run_uid) ON DELETE CASCADE,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    trial_key TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('accepted', 'running', 'completed', 'failed', 'cancelled')),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('agent_loop', 'workflow')),
    variant_key TEXT NOT NULL,
    plan_revision_uid UUID NOT NULL,
    persona_id TEXT,
    profile_id TEXT,
    scenario_id TEXT,
    data_bundle_ids TEXT[] NOT NULL DEFAULT '{}',
    artifact_revision_uids UUID[] NOT NULL DEFAULT '{}',
    simulator JSONB NOT NULL,
    simulator_model TEXT NOT NULL,
    target_model TEXT,
    seed TEXT,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    workflow_run_uid UUID REFERENCES moa.artifact_run(run_uid) ON DELETE SET NULL,
    score_run_id UUID NOT NULL REFERENCES analytics.score_run(run_id) ON DELETE RESTRICT,
    turn_count INT NOT NULL DEFAULT 0,
    stop_reason TEXT CHECK (
        stop_reason IS NULL OR stop_reason IN (
            'success',
            'failure',
            'max_turns',
            'budget_cap',
            'simulator_done',
            'target_terminal',
            'error',
            'cancelled'
        )
    ),
    error TEXT,
    trace_id TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS experiment_trial_scope_run_status_idx
    ON moa.experiment_trial (storage_partition_id, scope, user_id, run_uid, status, created_at DESC);

CREATE UNIQUE INDEX IF NOT EXISTS experiment_trial_run_key_uniq
    ON moa.experiment_trial (run_uid, trial_key);

CREATE INDEX IF NOT EXISTS experiment_trial_score_run_idx
    ON moa.experiment_trial (score_run_id);

CREATE INDEX IF NOT EXISTS experiment_trial_session_idx
    ON moa.experiment_trial (session_id)
    WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS experiment_trial_workflow_run_idx
    ON moa.experiment_trial (workflow_run_uid)
    WHERE workflow_run_uid IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.experiment_trial'::REGCLASS);

-- Source: V000042__session_behavior_lab_dispatched_trials.sql

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_status_check;

ALTER TABLE moa.experiment_trial
    ADD CONSTRAINT experiment_trial_status_check
    CHECK (status IN (
        'accepted',
        'dispatched',
        'running',
        'completed',
        'failed',
        'cancelled'
    ));

-- Source: V000043__session_behavior_lab_trial_plan_revision.sql

ALTER TABLE moa.experiment_trial
    ADD COLUMN IF NOT EXISTS plan_revision_uid UUID;

UPDATE moa.experiment_trial trial
SET plan_revision_uid = run.artifact_revision_uids[1]
FROM moa.experiment_run run
WHERE trial.run_uid = run.run_uid
  AND trial.plan_revision_uid IS NULL
  AND cardinality(run.artifact_revision_uids) > 0;

UPDATE moa.experiment_trial
SET plan_revision_uid = artifact_revision_uids[1]
WHERE plan_revision_uid IS NULL
  AND cardinality(artifact_revision_uids) > 0;

-- Prototype trial rows created before experiment_plan existed cannot be
-- reconstructed into a real pinned plan revision. Keep them loadable as old
-- records; runtime plan execution still rejects the nil revision.
UPDATE moa.experiment_trial
SET plan_revision_uid = '00000000-0000-0000-0000-000000000000'::UUID
WHERE plan_revision_uid IS NULL;

ALTER TABLE moa.experiment_trial
    ALTER COLUMN plan_revision_uid SET NOT NULL;

-- Source: V000044__session_behavior_lab_trial_plan_revision_index.sql

-- `plan_revision_uid` deliberately has no FK because non-plan trials can
-- use the nil sentinel during forward migration, but plan-scoped reads still
-- need an index.
CREATE INDEX IF NOT EXISTS experiment_trial_plan_revision_idx
    ON moa.experiment_trial (plan_revision_uid);

-- Source: V202606170001__index_cleanup_and_hot_paths.sql

-- Add missing FK and hot-query indexes.
CREATE INDEX IF NOT EXISTS idx_sessions_parent_session_id
    ON sessions(parent_session_id)
    WHERE parent_session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_subject_keys_storage_partition
    ON pii_vault.subject_keys(storage_partition_id);

CREATE INDEX IF NOT EXISTS ix_scores_item
    ON analytics.scores(item_id)
    WHERE item_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_scores_dataset
    ON analytics.scores(dataset_id)
    WHERE dataset_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_events_storage_partition_type_timestamp
    ON events(storage_partition_id, event_type, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_events_tool_id
    ON events(storage_partition_id, event_type, ((payload -> 'data' ->> 'tool_id')), timestamp DESC)
    WHERE payload -> 'data' ? 'tool_id';
