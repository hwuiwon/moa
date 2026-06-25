-- Tenant-first runtime boundaries for shared-schema Postgres deployments.

CREATE OR REPLACE FUNCTION moa.current_tenant_id() RETURNS UUID
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID;
$$;

CREATE OR REPLACE FUNCTION moa.current_contact_id() RETURNS UUID
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.contact_id', TRUE), '')::UUID;
$$;

CREATE OR REPLACE FUNCTION moa.current_control_plane() RETURNS BOOLEAN
LANGUAGE SQL STABLE
AS $$
    SELECT lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
        IN ('1', 'true', 't', 'yes', 'on');
$$;

CREATE OR REPLACE FUNCTION moa.drop_runtime_boundary_policies(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS contact_isolation ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS global_tenant_override ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_global ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_global_promoter ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS owner_dev_access ON %s', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_tenant_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY tenant_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
         )
         WITH CHECK (
             moa.current_control_plane()
             OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
         )',
        target_table
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_contact_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY contact_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR (
                 tenant_id::TEXT = moa.current_tenant_id()::TEXT
                 AND (
                     (moa.current_contact_id() IS NULL AND contact_id IS NULL)
                     OR contact_id::TEXT = moa.current_contact_id()::TEXT
                 )
             )
         )
         WITH CHECK (
             moa.current_control_plane()
             OR (
                 tenant_id::TEXT = moa.current_tenant_id()::TEXT
                 AND (
                     (moa.current_contact_id() IS NULL AND contact_id IS NULL)
                     OR contact_id::TEXT = moa.current_contact_id()::TEXT
                 )
             )
         )',
        target_table
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_global_tenant_override_rls(target_table REGCLASS)
RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY global_tenant_override ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
             OR (
                 tenant_id IS NULL
                 AND moa.current_tenant_id() IS NOT NULL
                 AND COALESCE(scope, '''') = ''global''
                 AND user_id IS NULL
             )
         )
         WITH CHECK (
             moa.current_control_plane()
             OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
         )',
        target_table
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_age_tenant_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);

    EXECUTE format('DROP POLICY IF EXISTS rd_global ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_tenant ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_global_promoter ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS owner_dev_access ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %s', target_table);

    EXECUTE format(
        'CREATE POLICY tenant_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR moa.age_property(properties, ''storage_partition_id'')::TEXT
                = moa.current_tenant_id()::TEXT
         )
         WITH CHECK (
             moa.current_control_plane()
             OR moa.age_property(properties, ''storage_partition_id'')::TEXT
                = moa.current_tenant_id()::TEXT
         )',
        target_table
    );

    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.storage_partition_text_to_tenant_uuid(value TEXT, table_name TEXT)
RETURNS UUID
LANGUAGE plpgsql IMMUTABLE
AS $$
BEGIN
    IF value IS NULL OR value !~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' THEN
        RAISE EXCEPTION
            'cannot infer tenant_id for %.storage_partition_id value %, manual tenant migration required',
            table_name,
            value
            USING ERRCODE = 'P0001';
    END IF;
    RETURN value::UUID;
END;
$$;

CREATE OR REPLACE FUNCTION moa.raise_ambiguous_tenant_rows_if_public(
    target_table REGCLASS,
    table_name TEXT
) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    ambiguous_value TEXT;
BEGIN
    IF current_schema() <> 'public' THEN
        RETURN;
    END IF;

    EXECUTE format(
        'SELECT storage_partition_id FROM %s
         WHERE tenant_id IS NULL
           AND storage_partition_id IS NOT NULL
           AND storage_partition_id !~* %L
         LIMIT 1',
        target_table,
        '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    )
    INTO ambiguous_value;

    IF ambiguous_value IS NOT NULL THEN
        RAISE EXCEPTION
            'cannot infer tenant_id for %.storage_partition_id value %, manual tenant migration required',
            table_name,
            ambiguous_value
            USING ERRCODE = 'P0001';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION moa.backfill_required_tenant_id(
    target_table REGCLASS,
    table_name TEXT
) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    remaining_nulls BIGINT;
BEGIN
    PERFORM moa.raise_ambiguous_tenant_rows_if_public(target_table, table_name);

    EXECUTE format(
        'UPDATE %s
         SET tenant_id = moa.storage_partition_text_to_tenant_uuid(storage_partition_id, %L)
         WHERE tenant_id IS NULL
           AND storage_partition_id IS NOT NULL
           AND storage_partition_id ~* %L',
        target_table,
        table_name,
        '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    );

    IF current_schema() <> 'public' THEN
        EXECUTE format(
            'UPDATE %s
             SET tenant_id = md5(%L || '':'' || COALESCE(storage_partition_id, '''') || '':'' || ctid::TEXT)::UUID
             WHERE tenant_id IS NULL',
            target_table,
            table_name
        );
    END IF;

    EXECUTE format('SELECT COUNT(*) FROM %s WHERE tenant_id IS NULL', target_table)
    INTO remaining_nulls;
    IF remaining_nulls > 0 THEN
        RAISE EXCEPTION
            'cannot infer tenant_id for %, % rows remain without tenant_id; manual tenant migration required',
            table_name,
            remaining_nulls
            USING ERRCODE = 'P0001';
    END IF;

    SET CONSTRAINTS ALL IMMEDIATE;
    EXECUTE format('ALTER TABLE %s ALTER COLUMN tenant_id SET NOT NULL', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_global_tenant_constraint(
    target_table REGCLASS,
    constraint_name TEXT
) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT IF EXISTS %I', target_table, constraint_name);
    EXECUTE format(
        'ALTER TABLE %s ADD CONSTRAINT %I CHECK (
             tenant_id IS NOT NULL
             OR (
                 tenant_id IS NULL
                 AND user_id IS NULL
                 AND COALESCE(scope, '''') = ''global''
             )
         )',
        target_table,
        constraint_name
    );
END;
$$;

CREATE OR REPLACE FUNCTION moa.set_runtime_tenant_columns() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    session_tenant UUID;
    session_contact UUID;
BEGIN
    IF NEW.tenant_id IS NULL THEN
        IF TG_TABLE_NAME IN ('events', 'pending_signals', 'context_snapshots') THEN
            EXECUTE format('SELECT tenant_id, contact_id FROM %I.sessions WHERE id = $1', TG_TABLE_SCHEMA)
                INTO session_tenant, session_contact
                USING NEW.session_id;
            NEW.tenant_id := session_tenant;
            IF TG_TABLE_NAME = 'events' THEN
                IF NEW.contact_id IS NULL THEN
                    NEW.contact_id := session_contact;
                END IF;
            END IF;
        ELSE
            NEW.tenant_id := COALESCE(moa.current_tenant_id(), CASE
                WHEN NEW.storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                    THEN NEW.storage_partition_id::UUID
                WHEN current_schema() <> 'public'
                    THEN gen_random_uuid()
                ELSE moa.storage_partition_text_to_tenant_uuid(NEW.storage_partition_id, TG_TABLE_NAME)
            END);
        END IF;
    END IF;

    IF moa.current_tenant_id() IS NOT NULL
       AND NEW.storage_partition_id IS NOT NULL
       AND NEW.storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       AND NEW.storage_partition_id::UUID <> moa.current_tenant_id() THEN
        RAISE EXCEPTION 'storage_partition_id % does not match current tenant %', NEW.storage_partition_id, moa.current_tenant_id()
            USING ERRCODE = '42501';
    END IF;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION moa.set_memory_runtime_columns() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    node_tenant UUID;
    node_contact UUID;
BEGIN
    IF TG_TABLE_SCHEMA = 'moa' AND TG_TABLE_NAME = 'embeddings' THEN
        SELECT tenant_id, contact_id
        INTO node_tenant, node_contact
        FROM moa.node_index
        WHERE uid = NEW.uid;

        IF NEW.tenant_id IS NULL THEN
            NEW.tenant_id := COALESCE(node_tenant, moa.current_tenant_id());
        END IF;
        IF NEW.contact_id IS NULL THEN
            NEW.contact_id := COALESCE(node_contact, moa.current_contact_id());
        END IF;
    ELSE
        IF NEW.tenant_id IS NULL THEN
            NEW.tenant_id := COALESCE(moa.current_tenant_id(), CASE
                WHEN NEW.storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                    THEN NEW.storage_partition_id::UUID
                WHEN current_schema() <> 'public'
                    THEN gen_random_uuid()
                ELSE moa.storage_partition_text_to_tenant_uuid(NEW.storage_partition_id, TG_TABLE_NAME)
            END);
        END IF;
        IF NEW.contact_id IS NULL THEN
            NEW.contact_id := moa.current_contact_id();
        END IF;
    END IF;

    IF moa.current_tenant_id() IS NOT NULL
       AND NEW.storage_partition_id IS NOT NULL
       AND NEW.storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       AND NEW.storage_partition_id::UUID <> moa.current_tenant_id() THEN
        RAISE EXCEPTION 'storage_partition_id % does not match current tenant %', NEW.storage_partition_id, moa.current_tenant_id()
            USING ERRCODE = '42501';
    END IF;

    RETURN NEW;
END;
$$;

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS tenant_id UUID;
UPDATE sessions
SET tenant_id = moa.storage_partition_text_to_tenant_uuid(storage_partition_id, 'sessions')
WHERE tenant_id IS NULL;
ALTER TABLE sessions ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_tenant_updated ON sessions(tenant_id, updated_at DESC);
DROP TRIGGER IF EXISTS sessions_set_tenant_columns ON sessions;
CREATE TRIGGER sessions_set_tenant_columns
    BEFORE INSERT OR UPDATE ON sessions
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

ALTER TABLE events
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
UPDATE events e
SET tenant_id = s.tenant_id,
    contact_id = COALESCE(e.contact_id, s.contact_id)
FROM sessions s
WHERE e.session_id = s.id
  AND (e.tenant_id IS NULL OR e.contact_id IS NULL);
ALTER TABLE events ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_events_tenant_session ON events(tenant_id, session_id, sequence_num);
CREATE INDEX IF NOT EXISTS idx_events_contact ON events(tenant_id, contact_id, timestamp)
    WHERE contact_id IS NOT NULL;
DROP TRIGGER IF EXISTS events_set_tenant_columns ON events;
CREATE TRIGGER events_set_tenant_columns
    BEFORE INSERT OR UPDATE ON events
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP VIEW IF EXISTS session_summary;
CREATE VIEW session_summary AS
SELECT
    s.id,
    s.tenant_id,
    s.storage_partition_id,
    s.contact_id,
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
    s.tenant_id,
    s.storage_partition_id,
    s.contact_id,
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

ALTER TABLE pending_signals ADD COLUMN IF NOT EXISTS tenant_id UUID;
UPDATE pending_signals p
SET tenant_id = s.tenant_id
FROM sessions s
WHERE p.session_id = s.id
  AND p.tenant_id IS NULL;
ALTER TABLE pending_signals ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_pending_signals_tenant ON pending_signals(tenant_id, resolved_at, created_at);
DROP TRIGGER IF EXISTS pending_signals_set_tenant_columns ON pending_signals;
CREATE TRIGGER pending_signals_set_tenant_columns
    BEFORE INSERT OR UPDATE ON pending_signals
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

ALTER TABLE context_snapshots ADD COLUMN IF NOT EXISTS tenant_id UUID;
UPDATE context_snapshots c
SET tenant_id = s.tenant_id
FROM sessions s
WHERE c.session_id = s.id
  AND c.tenant_id IS NULL;
ALTER TABLE context_snapshots ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_context_snapshots_tenant ON context_snapshots(tenant_id, session_id);
DROP TRIGGER IF EXISTS context_snapshots_set_tenant_columns ON context_snapshots;
CREATE TRIGGER context_snapshots_set_tenant_columns
    BEFORE INSERT OR UPDATE ON context_snapshots
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

ALTER TABLE session_agent_context ADD COLUMN IF NOT EXISTS tenant_id UUID;
UPDATE session_agent_context
SET tenant_id = moa.storage_partition_text_to_tenant_uuid(storage_partition_id, 'session_agent_context')
WHERE tenant_id IS NULL;
ALTER TABLE session_agent_context ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS session_agent_context_tenant_idx
    ON session_agent_context(tenant_id, agent_revision_uid);
DROP TRIGGER IF EXISTS session_agent_context_set_tenant_columns ON session_agent_context;
CREATE TRIGGER session_agent_context_set_tenant_columns
    BEFORE INSERT OR UPDATE ON session_agent_context
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

ALTER TABLE contacts ADD COLUMN IF NOT EXISTS contact_id UUID;
UPDATE contacts SET contact_id = id WHERE contact_id IS NULL;
ALTER TABLE contacts ALTER COLUMN contact_id SET NOT NULL;

ALTER TABLE moa.graph_changelog DROP CONSTRAINT IF EXISTS graph_changelog_actor_kind_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_actor_kind_check
        CHECK (actor_kind IN ('user', 'contact', 'agent', 'system', 'promoter', 'admin'));

ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
SELECT moa.backfill_required_tenant_id('moa.node_index'::REGCLASS, 'moa.node_index');
DROP TRIGGER IF EXISTS node_index_set_memory_runtime_columns ON moa.node_index;
CREATE TRIGGER node_index_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.node_index
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS node_index_tenant_label_idx
    ON moa.node_index(tenant_id, label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS node_index_contact_label_idx
    ON moa.node_index(tenant_id, contact_id, label)
    WHERE valid_to IS NULL AND contact_id IS NOT NULL;

ALTER TABLE moa.embeddings
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
UPDATE moa.embeddings e
SET tenant_id = n.tenant_id,
    contact_id = n.contact_id
FROM moa.node_index n
WHERE e.uid = n.uid
  AND n.tenant_id IS NOT NULL
  AND (e.tenant_id IS NULL OR e.contact_id IS NULL);
SELECT moa.backfill_required_tenant_id('moa.embeddings'::REGCLASS, 'moa.embeddings');
DROP TRIGGER IF EXISTS embeddings_set_memory_runtime_columns ON moa.embeddings;
CREATE TRIGGER embeddings_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.embeddings
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS embeddings_tenant_label_idx
    ON moa.embeddings(tenant_id, label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS embeddings_contact_label_idx
    ON moa.embeddings(tenant_id, contact_id, label)
    WHERE valid_to IS NULL AND contact_id IS NOT NULL;

ALTER TABLE moa.memory_digests
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
SELECT moa.backfill_required_tenant_id('moa.memory_digests'::REGCLASS, 'moa.memory_digests');
DROP TRIGGER IF EXISTS memory_digests_set_memory_runtime_columns ON moa.memory_digests;
CREATE TRIGGER memory_digests_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.memory_digests
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS memory_digests_tenant_contact_idx
    ON moa.memory_digests(tenant_id, contact_id, updated_at);

ALTER TABLE moa.retrieval_lineage
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
SELECT moa.backfill_required_tenant_id('moa.retrieval_lineage'::REGCLASS, 'moa.retrieval_lineage');
DROP TRIGGER IF EXISTS retrieval_lineage_set_memory_runtime_columns ON moa.retrieval_lineage;
CREATE TRIGGER retrieval_lineage_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.retrieval_lineage
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS retrieval_lineage_tenant_contact_idx
    ON moa.retrieval_lineage(tenant_id, contact_id, retrieved_at);

ALTER TABLE moa.graph_changelog
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS contact_id UUID;
SELECT moa.backfill_required_tenant_id('moa.graph_changelog'::REGCLASS, 'moa.graph_changelog');
DROP TRIGGER IF EXISTS graph_changelog_set_memory_runtime_columns ON moa.graph_changelog;
CREATE TRIGGER graph_changelog_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.graph_changelog
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS graph_changelog_tenant_created_idx
    ON moa.graph_changelog(tenant_id, created_at DESC);

ALTER TABLE moa.storage_partition_state ADD COLUMN IF NOT EXISTS tenant_id UUID;
SELECT moa.backfill_required_tenant_id('moa.storage_partition_state'::REGCLASS, 'moa.storage_partition_state');
DROP TRIGGER IF EXISTS storage_partition_state_set_tenant_columns ON moa.storage_partition_state;
CREATE TRIGGER storage_partition_state_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.storage_partition_state
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS storage_partition_state_tenant_idx ON moa.storage_partition_state(tenant_id);

DO $$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'moa.ingest_dedup',
        'moa.ingest_dlq',
        'moa.agent_installation',
        'moa.agent_deployment'
    ] LOOP
        IF to_regclass(table_name) IS NOT NULL THEN
            EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS tenant_id UUID', table_name::REGCLASS);
            PERFORM moa.backfill_required_tenant_id(table_name::REGCLASS, table_name);
            EXECUTE format('DROP TRIGGER IF EXISTS %I ON %s', replace(table_name, '.', '_') || '_set_tenant_columns', table_name::REGCLASS);
            EXECUTE format(
                'CREATE TRIGGER %I
                 BEFORE INSERT OR UPDATE ON %s
                 FOR EACH ROW
                 EXECUTE FUNCTION moa.set_runtime_tenant_columns()',
                replace(table_name, '.', '_') || '_set_tenant_columns',
                table_name::REGCLASS
            );
            EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %s (tenant_id)', replace(table_name, '.', '_') || '_tenant_rls_idx', table_name::REGCLASS);
        END IF;
    END LOOP;
END $$;

ALTER TABLE action_policy_rules ADD COLUMN IF NOT EXISTS tenant_id UUID;
UPDATE action_policy_rules
SET tenant_id = moa.storage_partition_text_to_tenant_uuid(storage_partition_id, 'action_policy_rules')
WHERE tenant_id IS NULL
  AND storage_partition_id <> 'global'
  AND storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$';
DO $$
DECLARE
    ambiguous_value TEXT;
BEGIN
    SELECT storage_partition_id
    INTO ambiguous_value
    FROM action_policy_rules
    WHERE tenant_id IS NULL
      AND storage_partition_id <> 'global'
      AND storage_partition_id !~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    LIMIT 1;

    IF ambiguous_value IS NOT NULL AND current_schema() = 'public' THEN
        RAISE EXCEPTION
            'cannot infer tenant_id for action_policy_rules.storage_partition_id value %, manual tenant migration required',
            ambiguous_value
            USING ERRCODE = 'P0001';
    END IF;

    IF current_schema() <> 'public' THEN
        UPDATE action_policy_rules
        SET tenant_id = md5('action_policy_rules:' || storage_partition_id || ':' || id::TEXT)::UUID
        WHERE tenant_id IS NULL
          AND storage_partition_id <> 'global';
    END IF;
END $$;
ALTER TABLE action_policy_rules
    DROP CONSTRAINT IF EXISTS action_policy_rules_global_partition_check;
ALTER TABLE action_policy_rules
    DROP CONSTRAINT IF EXISTS action_policy_rules_scope_check;
ALTER TABLE action_policy_rules
    ADD CONSTRAINT action_policy_rules_scope_check
        CHECK (scope = 'tenant');
ALTER TABLE action_policy_rules
    ADD CONSTRAINT action_policy_rules_tenant_storage_partition_check
        CHECK (storage_partition_id <> 'global');
ALTER TABLE action_policy_rules ALTER COLUMN tenant_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS action_policy_rules_tenant_rls_idx
    ON action_policy_rules(tenant_id, tool, created_at);

ALTER TABLE tenant_action_reviews ADD COLUMN IF NOT EXISTS tenant_id UUID;
SELECT moa.backfill_required_tenant_id('tenant_action_reviews'::REGCLASS, 'tenant_action_reviews');
CREATE INDEX IF NOT EXISTS tenant_action_reviews_tenant_rls_idx
    ON tenant_action_reviews(tenant_id, created_at DESC);

DO $$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'task_segments',
        'learning_log',
        'experience_records',
        'experience_attributions',
        'learning_candidates'
    ] LOOP
        IF to_regclass(table_name) IS NOT NULL THEN
            EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %s (tenant_id)', table_name || '_tenant_rls_idx', table_name::REGCLASS);
        END IF;
    END LOOP;
END $$;

DO $$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'moa.artifact',
        'moa.artifact_revision',
        'moa.artifact_file'
    ] LOOP
        IF to_regclass(table_name) IS NOT NULL THEN
            EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS tenant_id UUID', table_name::REGCLASS);
            PERFORM moa.raise_ambiguous_tenant_rows_if_public(table_name::REGCLASS, table_name);
            EXECUTE format(
                'UPDATE %s
                 SET tenant_id = moa.storage_partition_text_to_tenant_uuid(storage_partition_id, %L)
                 WHERE tenant_id IS NULL
                   AND storage_partition_id IS NOT NULL
                   AND storage_partition_id ~* %L',
                table_name::REGCLASS,
                table_name,
                '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
            );
            IF current_schema() <> 'public' THEN
                EXECUTE format(
                    'UPDATE %s
                     SET tenant_id = md5(%L || '':'' || storage_partition_id || '':'' || ctid::TEXT)::UUID
                     WHERE tenant_id IS NULL
                       AND storage_partition_id IS NOT NULL',
                    table_name::REGCLASS,
                    table_name
                );
            END IF;
            SET CONSTRAINTS ALL IMMEDIATE;
            PERFORM moa.apply_global_tenant_constraint(
                table_name::REGCLASS,
                replace(table_name, '.', '_') || '_tenant_or_global_check'
            );
            EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %s (tenant_id)', replace(table_name, '.', '_') || '_tenant_rls_idx', table_name::REGCLASS);
        END IF;
    END LOOP;
END $$;

DO $$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'moa.artifact_run',
        'moa.artifact_node_run',
        'moa.experiment_run',
        'moa.experiment_run_artifact_revision',
        'moa.experiment_trial',
        'analytics.score_run',
        'analytics.scores',
        'analytics.turn_lineage'
    ] LOOP
        IF to_regclass(table_name) IS NOT NULL THEN
            EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS tenant_id UUID', table_name::REGCLASS);
            PERFORM moa.backfill_required_tenant_id(table_name::REGCLASS, table_name);
            EXECUTE format('DROP TRIGGER IF EXISTS %I ON %s', replace(table_name, '.', '_') || '_set_tenant_columns', table_name::REGCLASS);
            EXECUTE format(
                'CREATE TRIGGER %I
                 BEFORE INSERT OR UPDATE ON %s
                 FOR EACH ROW
                 EXECUTE FUNCTION moa.set_runtime_tenant_columns()',
                replace(table_name, '.', '_') || '_set_tenant_columns',
                table_name::REGCLASS
            );
            EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON %s (tenant_id)', replace(table_name, '.', '_') || '_tenant_rls_idx', table_name::REGCLASS);
        END IF;
    END LOOP;
END $$;

SELECT moa.apply_tenant_rls('sessions'::REGCLASS);
SELECT moa.apply_tenant_rls('events'::REGCLASS);
SELECT moa.apply_tenant_rls('pending_signals'::REGCLASS);
SELECT moa.apply_tenant_rls('context_snapshots'::REGCLASS);
SELECT moa.apply_tenant_rls('session_agent_context'::REGCLASS);
SELECT moa.apply_tenant_rls('task_segments'::REGCLASS);
SELECT moa.apply_tenant_rls('learning_log'::REGCLASS);
SELECT moa.apply_tenant_rls('experience_records'::REGCLASS);
SELECT moa.apply_tenant_rls('experience_attributions'::REGCLASS);
SELECT moa.apply_tenant_rls('learning_candidates'::REGCLASS);
SELECT moa.apply_tenant_rls('contacts'::REGCLASS);
SELECT moa.apply_contact_rls('contact_points'::REGCLASS);
SELECT moa.apply_contact_rls('contact_token_grants'::REGCLASS);
SELECT moa.apply_contact_rls('contact_verification_challenges'::REGCLASS);
SELECT moa.apply_contact_rls('contact_channel_accounts'::REGCLASS);
SELECT moa.apply_contact_rls('session_channel_bindings'::REGCLASS);
SELECT moa.apply_contact_rls('moa.node_index'::REGCLASS);
SELECT moa.apply_contact_rls('moa.embeddings'::REGCLASS);
SELECT moa.apply_contact_rls('moa.memory_digests'::REGCLASS);
SELECT moa.apply_contact_rls('moa.retrieval_lineage'::REGCLASS);
SELECT moa.apply_contact_rls('moa.graph_changelog'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.storage_partition_state'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.ingest_dedup'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.ingest_dlq'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.agent_installation'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.agent_deployment'::REGCLASS);
SELECT moa.apply_tenant_rls('action_policy_rules'::REGCLASS);
SELECT moa.apply_tenant_rls('tenant_action_reviews'::REGCLASS);
SELECT moa.apply_global_tenant_override_rls('moa.artifact'::REGCLASS);
SELECT moa.apply_global_tenant_override_rls('moa.artifact_revision'::REGCLASS);
SELECT moa.apply_global_tenant_override_rls('moa.artifact_file'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.artifact_run'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.artifact_node_run'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.experiment_run'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.experiment_run_artifact_revision'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.experiment_trial'::REGCLASS);
SELECT moa.apply_tenant_rls('analytics.score_run'::REGCLASS);

DO $$
BEGIN
    IF to_regclass('analytics.scores') IS NOT NULL THEN
        PERFORM moa.apply_tenant_rls('analytics.scores'::REGCLASS);
    END IF;
    IF to_regclass('analytics.turn_lineage') IS NOT NULL THEN
        PERFORM moa.apply_tenant_rls('analytics.turn_lineage'::REGCLASS);
    END IF;
END $$;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    IF to_regnamespace('moa_graph') IS NULL THEN
        RETURN;
    END IF;

    FOREACH label_name IN ARRAY (
        moa.age_vertex_labels() || moa.age_edge_labels() || moa.age_base_labels()
    ) LOOP
        IF to_regclass(format('%I.%I', 'moa_graph', label_name)) IS NOT NULL THEN
            PERFORM moa.apply_age_tenant_rls(format('%I.%I', 'moa_graph', label_name)::REGCLASS);
        END IF;
    END LOOP;
END $$;
