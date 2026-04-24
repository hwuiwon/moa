//! Embedded `PostgreSQL` migrations for the session store.

use moa_core::{MoaError, Result};
use sqlx::{PgPool, raw_sql};

/// Runs all embedded `PostgreSQL` migrations idempotently on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => {
            sqlx::migrate!("./migrations/postgres")
                .run(pool)
                .await
                .map_err(|error| {
                    MoaError::StorageError(format!("postgres migration failed: {error}"))
                })?;
            Ok(())
        }
    }
}

async fn migrate_in_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    let sessions = qualified_name(schema_name, "sessions");
    let events = qualified_name(schema_name, "events");
    let approval_rules = qualified_name(schema_name, "approval_rules");
    let workspaces = qualified_name(schema_name, "workspaces");
    let users = qualified_name(schema_name, "users");
    let pending_signals = qualified_name(schema_name, "pending_signals");
    let context_snapshots = qualified_name(schema_name, "context_snapshots");
    let task_segments = qualified_name(schema_name, "task_segments");
    let tool_call_analytics = qualified_name(schema_name, "tool_call_analytics");
    let tool_call_summary = qualified_name(schema_name, "tool_call_summary");
    let session_summary = qualified_name(schema_name, "session_summary");
    let session_turn_metrics = qualified_name(schema_name, "session_turn_metrics");
    let daily_workspace_metrics = qualified_name(schema_name, "daily_workspace_metrics");
    let skill_resolution_rates = qualified_name(schema_name, "skill_resolution_rates");
    let intent_transitions = qualified_name(schema_name, "intent_transitions");
    let segment_baselines = qualified_name(schema_name, "segment_baselines");
    let update_session_aggregates = qualified_name(schema_name, "update_session_aggregates");
    let idx_sessions_workspace = quote_identifier("idx_sessions_workspace");
    let idx_sessions_user = quote_identifier("idx_sessions_user");
    let idx_sessions_status = quote_identifier("idx_sessions_status");
    let idx_sessions_cache_hit_rate = quote_identifier("idx_sessions_cache_hit_rate");
    let idx_sessions_cost_cents = quote_identifier("idx_sessions_cost_cents");
    let idx_events_session_seq = quote_identifier("idx_events_session_seq");
    let idx_events_session_type = quote_identifier("idx_events_session_type");
    let idx_events_timestamp = quote_identifier("idx_events_timestamp");
    let idx_events_fts = quote_identifier("idx_events_fts");
    let idx_pending_signals_session = quote_identifier("idx_pending_signals_session");
    let idx_context_snapshots_last_seq = quote_identifier("idx_context_snapshots_last_seq");
    let idx_task_segments_tenant_intent = quote_identifier("idx_task_segments_tenant_intent");
    let idx_task_segments_session = quote_identifier("idx_task_segments_session");
    let idx_task_segments_tenant_time = quote_identifier("idx_task_segments_tenant_time");
    let idx_session_turn_metrics_session_turn =
        quote_identifier("idx_session_turn_metrics_session_turn");
    let idx_daily_workspace_metrics_workspace_day =
        quote_identifier("idx_daily_workspace_metrics_workspace_day");
    let idx_skill_resolution_rates_unique = quote_identifier("idx_skill_resolution_rates_unique");
    let idx_intent_transitions_unique = quote_identifier("idx_intent_transitions_unique");
    let idx_segment_baselines_unique = quote_identifier("idx_segment_baselines_unique");
    let trg_update_session_aggregates = quote_identifier("trg_update_session_aggregates");

    let sql = format!(
        r"
        CREATE TABLE IF NOT EXISTS {sessions} (
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
            parent_session_id UUID REFERENCES {sessions}(id),
            total_input_tokens_uncached BIGINT DEFAULT 0,
            total_input_tokens_cache_write BIGINT DEFAULT 0,
            total_input_tokens_cache_read BIGINT DEFAULT 0,
            total_input_tokens BIGINT GENERATED ALWAYS AS (
                COALESCE(total_input_tokens_uncached, 0)
                + COALESCE(total_input_tokens_cache_write, 0)
                + COALESCE(total_input_tokens_cache_read, 0)
            ) STORED,
            total_output_tokens BIGINT DEFAULT 0,
            total_cost_cents BIGINT DEFAULT 0,
            event_count BIGINT DEFAULT 0,
            turn_count BIGINT DEFAULT 0,
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

        ALTER TABLE {sessions}
            ADD COLUMN IF NOT EXISTS total_input_tokens_uncached BIGINT DEFAULT 0;
        ALTER TABLE {sessions}
            ADD COLUMN IF NOT EXISTS total_input_tokens_cache_write BIGINT DEFAULT 0;
        ALTER TABLE {sessions}
            ADD COLUMN IF NOT EXISTS total_input_tokens_cache_read BIGINT DEFAULT 0;
        ALTER TABLE {sessions}
            ADD COLUMN IF NOT EXISTS turn_count BIGINT DEFAULT 0;

        CREATE INDEX IF NOT EXISTS {idx_sessions_workspace} ON {sessions}(workspace_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS {idx_sessions_user} ON {sessions}(user_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS {idx_sessions_status} ON {sessions}(status);
        CREATE INDEX IF NOT EXISTS {idx_sessions_cache_hit_rate} ON {sessions}(cache_hit_rate);
        CREATE INDEX IF NOT EXISTS {idx_sessions_cost_cents} ON {sessions}(total_cost_cents DESC);

        CREATE TABLE IF NOT EXISTS {events} (
            id UUID PRIMARY KEY,
            session_id UUID NOT NULL REFERENCES {sessions}(id),
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

        CREATE INDEX IF NOT EXISTS {idx_events_session_seq} ON {events}(session_id, sequence_num);
        CREATE INDEX IF NOT EXISTS {idx_events_session_type} ON {events}(session_id, event_type);
        CREATE INDEX IF NOT EXISTS {idx_events_timestamp} ON {events}(timestamp);
        CREATE INDEX IF NOT EXISTS {idx_events_fts} ON {events} USING GIN(search_vector);

        CREATE TABLE IF NOT EXISTS {approval_rules} (
            id UUID PRIMARY KEY,
            workspace_id TEXT NOT NULL,
            tool TEXT NOT NULL,
            pattern TEXT NOT NULL,
            action TEXT NOT NULL,
            scope TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(workspace_id, tool, pattern)
        );

        CREATE TABLE IF NOT EXISTS {workspaces} (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_active TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            session_count BIGINT DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS {users} (
            id TEXT PRIMARY KEY,
            display_name TEXT,
            platform_links JSONB,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_active TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );

        CREATE TABLE IF NOT EXISTS {pending_signals} (
            id UUID PRIMARY KEY,
            session_id UUID NOT NULL REFERENCES {sessions}(id),
            signal_type TEXT NOT NULL,
            payload JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            resolved_at TIMESTAMPTZ
        );

        CREATE INDEX IF NOT EXISTS {idx_pending_signals_session}
            ON {pending_signals}(session_id, resolved_at, created_at);

        CREATE TABLE IF NOT EXISTS {context_snapshots} (
            session_id UUID PRIMARY KEY REFERENCES {sessions}(id) ON DELETE CASCADE,
            format_version INTEGER NOT NULL,
            last_sequence_num BIGINT NOT NULL,
            payload JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );

        CREATE INDEX IF NOT EXISTS {idx_context_snapshots_last_seq}
            ON {context_snapshots}(session_id, last_sequence_num);

        CREATE TABLE IF NOT EXISTS {task_segments} (
            id UUID PRIMARY KEY,
            session_id UUID NOT NULL REFERENCES {sessions}(id) ON DELETE CASCADE,
            tenant_id TEXT NOT NULL,
            segment_index INT NOT NULL,
            intent_label TEXT,
            intent_confidence NUMERIC(4,3),
            task_summary TEXT,
            started_at TIMESTAMPTZ NOT NULL,
            ended_at TIMESTAMPTZ,
            resolution TEXT,
            resolution_signal TEXT,
            resolution_confidence NUMERIC(4,3),
            tools_used TEXT[] NOT NULL DEFAULT '{{}}',
            skills_activated TEXT[] NOT NULL DEFAULT '{{}}',
            turn_count INT NOT NULL DEFAULT 0,
            token_cost BIGINT NOT NULL DEFAULT 0,
            previous_segment_id UUID,
            UNIQUE(session_id, segment_index)
        );

        CREATE INDEX IF NOT EXISTS {idx_task_segments_tenant_intent}
            ON {task_segments} (tenant_id, intent_label, resolution);
        CREATE INDEX IF NOT EXISTS {idx_task_segments_session}
            ON {task_segments} (session_id, segment_index);
        CREATE INDEX IF NOT EXISTS {idx_task_segments_tenant_time}
            ON {task_segments} (tenant_id, started_at DESC);

        DROP MATERIALIZED VIEW IF EXISTS {skill_resolution_rates};

        CREATE MATERIALIZED VIEW {skill_resolution_rates} AS
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
        FROM {task_segments} t
        WHERE t.intent_label IS NOT NULL
          AND t.resolution IS NOT NULL
          AND array_length(t.skills_activated, 1) IS NOT NULL
        GROUP BY t.tenant_id, t.intent_label, skill_name;

        CREATE UNIQUE INDEX IF NOT EXISTS {idx_skill_resolution_rates_unique}
            ON {skill_resolution_rates}(tenant_id, intent_label, skill_name);

        DROP MATERIALIZED VIEW IF EXISTS {intent_transitions};

        CREATE MATERIALIZED VIEW {intent_transitions} AS
        SELECT
            curr.tenant_id,
            prev.intent_label AS from_intent,
            curr.intent_label AS to_intent,
            COUNT(*)::BIGINT AS transition_count,
            AVG(CASE WHEN prev.resolution = 'resolved' THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS from_resolution_rate
        FROM {task_segments} curr
        JOIN {task_segments} prev ON curr.previous_segment_id = prev.id
        WHERE curr.intent_label IS NOT NULL AND prev.intent_label IS NOT NULL
        GROUP BY curr.tenant_id, prev.intent_label, curr.intent_label;

        CREATE UNIQUE INDEX IF NOT EXISTS {idx_intent_transitions_unique}
            ON {intent_transitions}(tenant_id, from_intent, to_intent);

        DROP MATERIALIZED VIEW IF EXISTS {segment_baselines};

        CREATE MATERIALIZED VIEW {segment_baselines} AS
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
        FROM {task_segments}
        WHERE intent_label IS NOT NULL AND ended_at IS NOT NULL
        GROUP BY tenant_id, intent_label;

        CREATE UNIQUE INDEX IF NOT EXISTS {idx_segment_baselines_unique}
            ON {segment_baselines}(tenant_id, intent_label);

        CREATE OR REPLACE FUNCTION {update_session_aggregates}() RETURNS TRIGGER AS $$
        DECLARE
            event_data JSONB := COALESCE(NEW.payload -> 'data', '{{}}'::JSONB);
        BEGIN
            UPDATE {sessions}
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

        DROP TRIGGER IF EXISTS {trg_update_session_aggregates} ON {events};

        CREATE TRIGGER {trg_update_session_aggregates}
            AFTER INSERT ON {events}
            FOR EACH ROW
            EXECUTE FUNCTION {update_session_aggregates}();

        CREATE OR REPLACE VIEW {tool_call_analytics} AS
        WITH tool_calls AS (
            SELECT
                s.workspace_id,
                s.user_id,
                e.session_id,
                e.sequence_num AS call_sequence_num,
                e.timestamp AS called_at,
                e.payload -> 'data' AS call_data
            FROM {events} e
            JOIN {sessions} s
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
            FROM {events} e
            WHERE e.session_id = tc.session_id
              AND e.event_type = 'ToolResult'
              AND (e.payload -> 'data' ->> 'tool_id') = (tc.call_data ->> 'tool_id')
            ORDER BY e.sequence_num ASC
            LIMIT 1
        ) result_event ON TRUE
        LEFT JOIN LATERAL (
            SELECT e.id, e.payload, e.timestamp
            FROM {events} e
            WHERE e.session_id = tc.session_id
              AND e.event_type = 'ToolError'
              AND (e.payload -> 'data' ->> 'tool_id') = (tc.call_data ->> 'tool_id')
            ORDER BY e.sequence_num ASC
            LIMIT 1
        ) error_event ON TRUE;

        CREATE OR REPLACE VIEW {tool_call_summary} AS
        SELECT
            tool_name,
            COUNT(*)::BIGINT AS call_count,
            AVG(duration_ms)::DOUBLE PRECISION AS avg_duration_ms,
            PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms,
            PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_ms,
            AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS success_rate
        FROM {tool_call_analytics}
        WHERE finished_at IS NOT NULL
        GROUP BY tool_name;

        CREATE OR REPLACE VIEW {session_summary} AS
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
        FROM {sessions} s
        LEFT JOIN (
            SELECT session_id, COUNT(*)::BIGINT AS tool_call_count
            FROM {events}
            WHERE event_type = 'ToolCall'
            GROUP BY session_id
        ) tool_counts
            ON tool_counts.session_id = s.id
        LEFT JOIN (
            SELECT session_id, COUNT(*)::BIGINT AS error_count
            FROM {events}
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
            FROM {events} e
            WHERE e.event_type IN ('BrainResponse', 'Checkpoint')
            GROUP BY e.session_id
        ) tier_costs
            ON tier_costs.session_id = s.id;

        DROP MATERIALIZED VIEW IF EXISTS {session_turn_metrics};

        CREATE MATERIALIZED VIEW {session_turn_metrics} AS
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
            FROM {events} e
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
            LEFT JOIN {events} tc
                ON tc.session_id = bt.session_id
               AND tc.event_type = 'ToolCall'
               AND tc.sequence_num > bt.previous_response_sequence_num
               AND tc.sequence_num < bt.response_sequence_num
            LEFT JOIN LATERAL (
                SELECT e.id, e.payload, e.timestamp
                FROM {events} e
                WHERE e.session_id = tc.session_id
                  AND e.event_type = 'ToolResult'
                  AND (e.payload -> 'data' ->> 'tool_id') = (tc.payload -> 'data' ->> 'tool_id')
                ORDER BY e.sequence_num ASC
                LIMIT 1
            ) tr ON TRUE
            LEFT JOIN LATERAL (
                SELECT e.id, e.payload, e.timestamp
                FROM {events} e
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
        JOIN {sessions} s
            ON s.id = bt.session_id
        LEFT JOIN tool_metrics tm
            ON tm.session_id = bt.session_id
           AND tm.turn_number = bt.turn_number;

        CREATE UNIQUE INDEX IF NOT EXISTS {idx_session_turn_metrics_session_turn}
            ON {session_turn_metrics}(session_id, turn_number);

        DROP MATERIALIZED VIEW IF EXISTS {daily_workspace_metrics};

        CREATE MATERIALIZED VIEW {daily_workspace_metrics} AS
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
        FROM {sessions}
        GROUP BY workspace_id, DATE_TRUNC('day', created_at);

        CREATE UNIQUE INDEX IF NOT EXISTS {idx_daily_workspace_metrics_workspace_day}
            ON {daily_workspace_metrics}(workspace_id, day);
        "
    );

    raw_sql(&sql).execute(pool).await.map_err(|error| {
        MoaError::StorageError(format!(
            "postgres schema migration failed for `{schema_name}`: {error}"
        ))
    })?;
    Ok(())
}

fn qualified_name(schema_name: &str, object_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(object_name)
    )
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
