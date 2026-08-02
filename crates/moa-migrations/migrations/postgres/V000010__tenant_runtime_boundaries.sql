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
END;
$$;

CREATE OR REPLACE FUNCTION moa.runtime_boundary_column_predicate(
    target_table REGCLASS,
    column_name NAME,
    boundary_kind TEXT
) RETURNS TEXT
LANGUAGE plpgsql
AS $$
DECLARE
    column_type REGTYPE;
    boundary_function TEXT;
BEGIN
    SELECT attribute.atttypid::REGTYPE
    INTO column_type
    FROM pg_catalog.pg_attribute attribute
    WHERE attribute.attrelid = target_table
      AND attribute.attname = column_name
      AND attribute.attnum > 0
      AND NOT attribute.attisdropped;

    IF column_type IS NULL THEN
        RAISE EXCEPTION 'runtime boundary column %.% does not exist', target_table, column_name
            USING ERRCODE = '42703';
    END IF;

    boundary_function := CASE boundary_kind
        WHEN 'tenant' THEN 'moa.current_tenant_id()'
        WHEN 'contact' THEN 'moa.current_contact_id()'
        ELSE NULL
    END;
    IF boundary_function IS NULL THEN
        RAISE EXCEPTION 'unsupported runtime boundary kind %', boundary_kind
            USING ERRCODE = '22023';
    END IF;

    IF column_type = 'uuid'::REGTYPE THEN
        RETURN format('%I = %s', column_name, boundary_function);
    ELSIF column_type IN ('text'::REGTYPE, 'character varying'::REGTYPE, 'character'::REGTYPE) THEN
        RETURN format('%I = %s::TEXT', column_name, boundary_function);
    END IF;

    RAISE EXCEPTION 'unsupported runtime boundary type % for %.%', column_type, target_table, column_name
        USING ERRCODE = '42804';
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_tenant_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    tenant_predicate TEXT;
BEGIN
    tenant_predicate := moa.runtime_boundary_column_predicate(target_table, 'tenant_id', 'tenant');
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY tenant_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR %s
         )
         WITH CHECK (
             moa.current_control_plane()
             OR %s
         )',
        target_table,
        tenant_predicate,
        tenant_predicate
    );
    IF target_table = 'events'::REGCLASS THEN
        EXECUTE format('GRANT SELECT, INSERT ON %s TO moa_app', target_table);
    ELSE
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_contact_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    tenant_predicate TEXT;
    contact_predicate TEXT;
BEGIN
    tenant_predicate := moa.runtime_boundary_column_predicate(target_table, 'tenant_id', 'tenant');
    contact_predicate := moa.runtime_boundary_column_predicate(target_table, 'contact_id', 'contact');
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY contact_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR (
                 %s
                 AND (
                     (moa.current_contact_id() IS NULL AND contact_id IS NULL)
                     OR %s
                 )
             )
         )
         WITH CHECK (
             moa.current_control_plane()
             OR (
                 %s
                 AND (
                     (moa.current_contact_id() IS NULL AND contact_id IS NULL)
                     OR %s
                 )
             )
         )',
        target_table,
        tenant_predicate,
        contact_predicate,
        tenant_predicate,
        contact_predicate
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_memory_contact_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    tenant_predicate TEXT;
    contact_predicate TEXT;
BEGIN
    tenant_predicate := moa.runtime_boundary_column_predicate(target_table, 'tenant_id', 'tenant');
    contact_predicate := moa.runtime_boundary_column_predicate(target_table, 'contact_id', 'contact');
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY contact_isolation ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR (
                 %s
                 AND (
                     contact_id IS NULL
                     OR %s
                 )
             )
         )
         WITH CHECK (
             moa.current_control_plane()
             OR (
                 %s
                 AND (
                     (moa.current_contact_id() IS NULL AND contact_id IS NULL)
                     OR %s
                 )
             )
         )',
        target_table,
        tenant_predicate,
        contact_predicate,
        tenant_predicate,
        contact_predicate
    );
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_global_tenant_override_rls(target_table REGCLASS)
RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    tenant_predicate TEXT;
BEGIN
    tenant_predicate := moa.runtime_boundary_column_predicate(target_table, 'tenant_id', 'tenant');
    PERFORM moa.drop_runtime_boundary_policies(target_table);
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);
    EXECUTE format(
        'CREATE POLICY global_tenant_override ON %s FOR ALL TO moa_app
         USING (
             moa.current_control_plane()
             OR %s
             OR (
                 tenant_id IS NULL
                 AND moa.current_tenant_id() IS NOT NULL
                 AND COALESCE(scope, '''') = ''global''
                 AND user_id IS NULL
             )
         )
         WITH CHECK (
             moa.current_control_plane()
             OR %s
         )',
        target_table,
        tenant_predicate,
        tenant_predicate
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
            'cannot derive tenant_id from %.storage_partition_id value %; expected a UUID',
            table_name,
            value
            USING ERRCODE = 'P0001';
    END IF;
    RETURN value::UUID;
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
        IF TG_TABLE_NAME IN ('events', 'context_snapshots') THEN
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
            IF TG_OP = 'UPDATE' THEN
                NEW.tenant_id := COALESCE(OLD.tenant_id, node_tenant, moa.current_tenant_id());
            ELSE
                NEW.tenant_id := COALESCE(node_tenant, moa.current_tenant_id());
            END IF;
        END IF;
        IF NEW.contact_id IS NULL THEN
            IF TG_OP = 'UPDATE' THEN
                NEW.contact_id := OLD.contact_id;
            ELSE
                NEW.contact_id := COALESCE(node_contact, moa.current_contact_id());
            END IF;
        END IF;
    ELSIF TG_TABLE_SCHEMA = 'moa' AND TG_TABLE_NAME = 'edge_index' THEN
        SELECT tenant_id, contact_id
        INTO node_tenant, node_contact
        FROM moa.node_index
        WHERE uid = NEW.start_uid;

        IF NEW.tenant_id IS NULL THEN
            IF TG_OP = 'UPDATE' THEN
                NEW.tenant_id := COALESCE(OLD.tenant_id, node_tenant, moa.current_tenant_id());
            ELSE
                NEW.tenant_id := COALESCE(node_tenant, moa.current_tenant_id());
            END IF;
        END IF;
        IF NEW.contact_id IS NULL THEN
            IF TG_OP = 'UPDATE' THEN
                NEW.contact_id := OLD.contact_id;
            ELSE
                NEW.contact_id := COALESCE(node_contact, moa.current_contact_id());
            END IF;
        END IF;
    ELSE
        IF NEW.tenant_id IS NULL THEN
            IF TG_OP = 'UPDATE' THEN
                NEW.tenant_id := COALESCE(OLD.tenant_id, moa.current_tenant_id(), CASE
                    WHEN NEW.storage_partition_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                        THEN NEW.storage_partition_id::UUID
                    WHEN current_schema() <> 'public'
                        THEN gen_random_uuid()
                    ELSE moa.storage_partition_text_to_tenant_uuid(NEW.storage_partition_id, TG_TABLE_NAME)
                END);
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
        IF NEW.contact_id IS NULL THEN
            IF TG_OP = 'UPDATE' THEN
                NEW.contact_id := OLD.contact_id;
            ELSE
                NEW.contact_id := moa.current_contact_id();
            END IF;
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

CREATE INDEX IF NOT EXISTS idx_sessions_tenant_updated ON sessions(tenant_id, updated_at DESC);
CREATE TRIGGER sessions_set_tenant_columns
    BEFORE INSERT OR UPDATE ON sessions
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

CREATE INDEX IF NOT EXISTS idx_events_contact ON events(tenant_id, contact_id, timestamp)
    WHERE contact_id IS NOT NULL;
-- Serves tenant_cost_since: WHERE tenant_id = $1 AND event_type = $2 AND timestamp >= $3.
CREATE INDEX IF NOT EXISTS idx_events_tenant_type_time ON events(tenant_id, event_type, timestamp);
-- No BEFORE INSERT trigger on the hot `events` table: every writer
-- (moa_session::store::session_store) binds tenant_id/contact_id explicitly from
-- the locked session row, so the per-row `set_runtime_tenant_columns` trigger was
-- pure overhead on the append path. The shared function stays for other tables.
CREATE INDEX IF NOT EXISTS idx_context_snapshots_tenant ON context_snapshots(tenant_id, session_id);
CREATE TRIGGER context_snapshots_set_tenant_columns
    BEFORE INSERT OR UPDATE ON context_snapshots
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

CREATE INDEX IF NOT EXISTS session_agent_context_tenant_idx
    ON session_agent_context(tenant_id, agent_revision_uid);
CREATE TRIGGER session_agent_context_set_tenant_columns
    BEFORE INSERT OR UPDATE ON session_agent_context
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

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

CREATE TRIGGER edge_index_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.edge_index
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS edge_index_tenant_label_idx
    ON moa.edge_index(tenant_id, label);
CREATE INDEX IF NOT EXISTS edge_index_contact_label_idx
    ON moa.edge_index(tenant_id, contact_id, label)
    WHERE contact_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS edge_index_tenant_start_idx
    ON moa.edge_index(tenant_id, start_uid);
CREATE INDEX IF NOT EXISTS edge_index_tenant_end_idx
    ON moa.edge_index(tenant_id, end_uid);

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

CREATE TRIGGER memory_digests_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.memory_digests
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS memory_digests_tenant_contact_idx
    ON moa.memory_digests(tenant_id, contact_id, updated_at);

CREATE TRIGGER retrieval_lineage_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.retrieval_lineage
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS retrieval_lineage_tenant_contact_idx
    ON moa.retrieval_lineage(tenant_id, contact_id, retrieved_at);

CREATE TRIGGER graph_changelog_set_memory_runtime_columns
    BEFORE INSERT OR UPDATE ON moa.graph_changelog
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_memory_runtime_columns();
CREATE INDEX IF NOT EXISTS graph_changelog_tenant_created_idx
    ON moa.graph_changelog(tenant_id, created_at DESC);

CREATE TRIGGER storage_partition_state_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.storage_partition_state
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS storage_partition_state_tenant_idx ON moa.storage_partition_state(tenant_id);

CREATE TRIGGER moa_ingest_dedup_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.ingest_dedup
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS moa_ingest_dedup_tenant_rls_idx
    ON moa.ingest_dedup (tenant_id);

CREATE TRIGGER moa_ingest_dlq_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.ingest_dlq
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS moa_ingest_dlq_tenant_rls_idx
    ON moa.ingest_dlq (tenant_id);

CREATE TRIGGER moa_agent_installation_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.agent_installation
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS moa_agent_installation_tenant_rls_idx
    ON moa.agent_installation (tenant_id);

CREATE TRIGGER moa_agent_deployment_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.agent_deployment
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();
CREATE INDEX IF NOT EXISTS moa_agent_deployment_tenant_rls_idx
    ON moa.agent_deployment (tenant_id);

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
        'moa.experiment_run',
        'moa.experiment_run_artifact_revision',
        'moa.experiment_trial',
        'analytics.score_run',
        'analytics.scores',
        'analytics.turn_lineage'
    ] LOOP
        IF to_regclass(table_name) IS NOT NULL THEN
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
SELECT moa.apply_memory_contact_rls('moa.node_index'::REGCLASS);
SELECT moa.apply_memory_contact_rls('moa.edge_index'::REGCLASS);
SELECT moa.apply_memory_contact_rls('moa.embeddings'::REGCLASS);
SELECT moa.apply_memory_contact_rls('moa.memory_digests'::REGCLASS);
SELECT moa.apply_memory_contact_rls('moa.retrieval_lineage'::REGCLASS);
SELECT moa.apply_memory_contact_rls('moa.graph_changelog'::REGCLASS);
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_app;
GRANT SELECT, INSERT ON moa.graph_changelog TO moa_app;
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
SELECT moa.apply_tenant_rls('moa.experiment_run'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.experiment_run_artifact_revision'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.experiment_trial'::REGCLASS);
SELECT moa.apply_tenant_rls('analytics.score_run'::REGCLASS);

-- These tables retain the promoter's baseline DML privileges. Their final RLS
-- policies still decide which rows, if any, that role can access.
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE
    sessions,
    events,
    context_snapshots,
    task_segments,
    learning_log,
    moa.node_index,
    moa.edge_index,
    moa.embeddings,
    moa.ingest_dedup,
    moa.ingest_dlq,
    moa.memory_digests,
    moa.retrieval_lineage,
    experience_records,
    experience_attributions,
    learning_candidates,
    moa.artifact,
    moa.artifact_revision,
    moa.artifact_file,
    analytics.score_run,
    moa.experiment_run,
    moa.experiment_run_artifact_revision,
    moa.experiment_trial,
    action_policy_rules,
    tenant_action_reviews,
    moa.agent_installation,
    moa.agent_deployment
TO moa_promoter;

DO $$
BEGIN
    IF to_regclass('analytics.scores') IS NOT NULL THEN
        PERFORM moa.apply_tenant_rls('analytics.scores'::REGCLASS);
    END IF;
    IF to_regclass('analytics.turn_lineage') IS NOT NULL THEN
        PERFORM moa.apply_tenant_rls('analytics.turn_lineage'::REGCLASS);
    END IF;
END $$;
