-- Tenant knowledge-base storage for linked-account ingestion and inspection.

CREATE TABLE IF NOT EXISTS moa.knowledge_connections (
    connection_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    provider_config_key TEXT NOT NULL,
    provider_connection_id TEXT NOT NULL,
    connector TEXT NOT NULL,
    credential_ref TEXT NOT NULL,
    status TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    last_synced_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (provider <> ''),
    CHECK (provider_config_key <> ''),
    CHECK (provider_connection_id <> ''),
    CHECK (credential_ref <> ''),
    CHECK (status IN ('pending', 'active', 'disabled', 'error'))
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_connections_provider_uniq
    ON moa.knowledge_connections (
        tenant_id,
        provider,
        provider_config_key,
        provider_connection_id
    );

CREATE INDEX IF NOT EXISTS knowledge_connections_tenant_status_idx
    ON moa.knowledge_connections (tenant_id, status, updated_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_sync_runs (
    sync_run_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    connection_id UUID NOT NULL REFERENCES moa.knowledge_connections(connection_uid) ON DELETE CASCADE,
    status TEXT NOT NULL,
    requested_by_identity_id UUID,
    trigger_reason TEXT,
    provider_sync_id TEXT,
    parser_provider TEXT,
    parser_job_count BIGINT NOT NULL DEFAULT 0,
    cursor_before JSONB,
    cursor_after JSONB,
    records_seen BIGINT NOT NULL DEFAULT 0,
    records_changed BIGINT NOT NULL DEFAULT 0,
    records_deleted BIGINT NOT NULL DEFAULT 0,
    records_ingested BIGINT NOT NULL DEFAULT 0,
    records_failed BIGINT NOT NULL DEFAULT 0,
    objects_parsed BIGINT NOT NULL DEFAULT 0,
    chunks_embedded BIGINT NOT NULL DEFAULT 0,
    graph_nodes_upserted BIGINT NOT NULL DEFAULT 0,
    graph_edges_upserted BIGINT NOT NULL DEFAULT 0,
    error JSONB,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN (
        'queued',
        'provider_syncing',
        'provider_synced',
        'parse_pending',
        'ingesting',
        'completed',
        'failed_retryable',
        'failed_terminal',
        'canceled'
    )),
    CHECK (parser_job_count >= 0),
    CHECK (records_seen >= 0),
    CHECK (records_changed >= 0),
    CHECK (records_deleted >= 0),
    CHECK (records_ingested >= 0),
    CHECK (records_failed >= 0),
    CHECK (objects_parsed >= 0),
    CHECK (chunks_embedded >= 0),
    CHECK (graph_nodes_upserted >= 0),
    CHECK (graph_edges_upserted >= 0)
);

CREATE INDEX IF NOT EXISTS knowledge_sync_runs_connection_started_idx
    ON moa.knowledge_sync_runs (tenant_id, connection_id, started_at DESC);

CREATE INDEX IF NOT EXISTS knowledge_sync_runs_fk_connection_idx
    ON moa.knowledge_sync_runs (connection_id, started_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_objects (
    object_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    connection_id UUID NOT NULL REFERENCES moa.knowledge_connections(connection_uid) ON DELETE CASCADE,
    object_type TEXT NOT NULL,
    external_object_id TEXT NOT NULL,
    parent_external_object_id TEXT,
    title TEXT,
    change_token TEXT,
    last_modified_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    source_uri TEXT,
    mime_type TEXT,
    status TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (object_type <> ''),
    CHECK (external_object_id <> ''),
    CHECK (status IN ('pending', 'active', 'deleted', 'error'))
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_objects_external_uniq
    ON moa.knowledge_objects (tenant_id, connection_id, external_object_id);

CREATE INDEX IF NOT EXISTS knowledge_objects_connection_updated_idx
    ON moa.knowledge_objects (tenant_id, connection_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS knowledge_objects_fk_connection_idx
    ON moa.knowledge_objects (connection_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_ingestion_steps (
    step_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    sync_run_id UUID NOT NULL REFERENCES moa.knowledge_sync_runs(sync_run_uid) ON DELETE CASCADE,
    object_id UUID REFERENCES moa.knowledge_objects(object_uid) ON DELETE CASCADE,
    stage TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    duration_ms BIGINT,
    attempt INT NOT NULL DEFAULT 0,
    counters JSONB NOT NULL DEFAULT '{}'::JSONB,
    safe_summary TEXT,
    error_code TEXT,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (stage <> ''),
    CHECK (status IN ('started', 'completed', 'failed', 'skipped')),
    CHECK (duration_ms IS NULL OR duration_ms >= 0),
    CHECK (attempt >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_ingestion_steps_attempt_uniq
    ON moa.knowledge_ingestion_steps (
        tenant_id,
        sync_run_id,
        COALESCE(object_id, '00000000-0000-0000-0000-000000000000'::UUID),
        stage,
        attempt
    );

CREATE INDEX IF NOT EXISTS knowledge_ingestion_steps_run_started_idx
    ON moa.knowledge_ingestion_steps (tenant_id, sync_run_id, started_at ASC);

CREATE INDEX IF NOT EXISTS knowledge_ingestion_steps_fk_run_idx
    ON moa.knowledge_ingestion_steps (sync_run_id, started_at ASC);

CREATE INDEX IF NOT EXISTS knowledge_ingestion_steps_object_started_idx
    ON moa.knowledge_ingestion_steps (tenant_id, object_id, started_at ASC)
    WHERE object_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS knowledge_ingestion_steps_fk_object_idx
    ON moa.knowledge_ingestion_steps (object_id, started_at ASC)
    WHERE object_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.knowledge_provider_events (
    provider_event_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    connection_id UUID REFERENCES moa.knowledge_connections(connection_uid) ON DELETE SET NULL,
    provider TEXT NOT NULL,
    provider_event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'received',
    payload JSONB NOT NULL DEFAULT '{}'::JSONB,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (provider <> ''),
    CHECK (provider_event_id <> ''),
    CHECK (event_type <> '')
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_provider_events_provider_event_uniq
    ON moa.knowledge_provider_events (tenant_id, provider, provider_event_id);

CREATE INDEX IF NOT EXISTS knowledge_provider_events_fk_connection_idx
    ON moa.knowledge_provider_events (connection_id, received_at DESC)
    WHERE connection_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.knowledge_document_versions (
    document_version_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    object_id UUID NOT NULL REFERENCES moa.knowledge_objects(object_uid) ON DELETE CASCADE,
    parser_provider TEXT NOT NULL,
    parser_job_id TEXT,
    content_hash TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (parser_provider <> ''),
    CHECK (content_hash <> '')
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_document_versions_content_uniq
    ON moa.knowledge_document_versions (tenant_id, object_id, content_hash);

CREATE INDEX IF NOT EXISTS knowledge_document_versions_object_created_idx
    ON moa.knowledge_document_versions (tenant_id, object_id, created_at DESC);

CREATE INDEX IF NOT EXISTS knowledge_document_versions_fk_object_idx
    ON moa.knowledge_document_versions (object_id, created_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_blocks (
    block_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    document_version_id UUID NOT NULL REFERENCES moa.knowledge_document_versions(document_version_uid) ON DELETE CASCADE,
    element_id TEXT NOT NULL,
    block_hash TEXT NOT NULL,
    ordinal INT NOT NULL,
    normalized_text TEXT NOT NULL,
    heading_path TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (element_id <> ''),
    CHECK (block_hash <> ''),
    CHECK (ordinal >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_blocks_ordinal_uniq
    ON moa.knowledge_blocks (tenant_id, document_version_id, ordinal);

CREATE INDEX IF NOT EXISTS knowledge_blocks_version_hash_idx
    ON moa.knowledge_blocks (tenant_id, document_version_id, block_hash);

CREATE INDEX IF NOT EXISTS knowledge_blocks_fk_version_idx
    ON moa.knowledge_blocks (document_version_id, ordinal);

CREATE TABLE IF NOT EXISTS moa.knowledge_chunks (
    chunk_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    document_version_id UUID NOT NULL REFERENCES moa.knowledge_document_versions(document_version_uid) ON DELETE CASCADE,
    graph_node_uid UUID,
    chunk_hash TEXT NOT NULL,
    block_hashes TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    heading_path TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    text TEXT NOT NULL,
    ordinal INT NOT NULL,
    token_count INT NOT NULL DEFAULT 0,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (chunk_hash <> ''),
    CHECK (ordinal >= 0),
    CHECK (token_count >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_chunks_hash_uniq
    ON moa.knowledge_chunks (tenant_id, document_version_id, chunk_hash);

CREATE INDEX IF NOT EXISTS knowledge_chunks_graph_node_idx
    ON moa.knowledge_chunks (tenant_id, graph_node_uid)
    WHERE graph_node_uid IS NOT NULL;

CREATE INDEX IF NOT EXISTS knowledge_chunks_fk_version_idx
    ON moa.knowledge_chunks (document_version_id, ordinal);

CREATE INDEX IF NOT EXISTS knowledge_chunks_graph_node_active_idx
    ON moa.knowledge_chunks (graph_node_uid, created_at DESC)
    WHERE graph_node_uid IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.knowledge_contact_groups (
    group_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    group_kind TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    display_name TEXT NOT NULL,
    source_connection_id UUID REFERENCES moa.knowledge_connections(connection_uid) ON DELETE SET NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (group_kind <> ''),
    CHECK (normalized_name <> ''),
    CHECK (display_name <> '')
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_contact_groups_name_uniq
    ON moa.knowledge_contact_groups (
        tenant_id,
        group_kind,
        normalized_name,
        COALESCE(source_connection_id, '00000000-0000-0000-0000-000000000000'::UUID)
    );

CREATE INDEX IF NOT EXISTS knowledge_contact_groups_fk_source_connection_idx
    ON moa.knowledge_contact_groups (source_connection_id, updated_at DESC)
    WHERE source_connection_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.knowledge_contact_group_memberships (
    membership_uid UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    group_id UUID NOT NULL REFERENCES moa.knowledge_contact_groups(group_uid) ON DELETE CASCADE,
    contact_id UUID NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    evidence_ids UUID[] NOT NULL DEFAULT ARRAY[]::UUID[],
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_contact_group_active_membership_uniq
    ON moa.knowledge_contact_group_memberships (tenant_id, group_id, contact_id)
    WHERE active = TRUE;

CREATE INDEX IF NOT EXISTS knowledge_contact_group_memberships_contact_idx
    ON moa.knowledge_contact_group_memberships (tenant_id, contact_id)
    WHERE active = TRUE;

CREATE INDEX IF NOT EXISTS knowledge_contact_group_memberships_fk_group_idx
    ON moa.knowledge_contact_group_memberships (group_id, contact_id);

DROP TRIGGER IF EXISTS knowledge_connections_set_tenant_columns ON moa.knowledge_connections;
CREATE TRIGGER knowledge_connections_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_connections
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_sync_runs_set_tenant_columns ON moa.knowledge_sync_runs;
CREATE TRIGGER knowledge_sync_runs_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_sync_runs
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_ingestion_steps_set_tenant_columns ON moa.knowledge_ingestion_steps;
CREATE TRIGGER knowledge_ingestion_steps_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_ingestion_steps
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_provider_events_set_tenant_columns ON moa.knowledge_provider_events;
CREATE TRIGGER knowledge_provider_events_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_provider_events
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_objects_set_tenant_columns ON moa.knowledge_objects;
CREATE TRIGGER knowledge_objects_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_objects
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_document_versions_set_tenant_columns ON moa.knowledge_document_versions;
CREATE TRIGGER knowledge_document_versions_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_document_versions
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_blocks_set_tenant_columns ON moa.knowledge_blocks;
CREATE TRIGGER knowledge_blocks_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_blocks
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_chunks_set_tenant_columns ON moa.knowledge_chunks;
CREATE TRIGGER knowledge_chunks_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_chunks
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_contact_groups_set_tenant_columns ON moa.knowledge_contact_groups;
CREATE TRIGGER knowledge_contact_groups_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_contact_groups
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

DROP TRIGGER IF EXISTS knowledge_contact_group_memberships_set_tenant_columns ON moa.knowledge_contact_group_memberships;
CREATE TRIGGER knowledge_contact_group_memberships_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_contact_group_memberships
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

SELECT moa.apply_tenant_rls('moa.knowledge_connections'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_sync_runs'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_ingestion_steps'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_provider_events'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_objects'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_document_versions'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_blocks'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_chunks'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_contact_groups'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_contact_group_memberships'::REGCLASS);

CREATE OR REPLACE FUNCTION moa.age_vertex_labels() RETURNS TEXT[]
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

CREATE OR REPLACE FUNCTION moa.age_edge_labels() RETURNS TEXT[]
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

LOAD 'age';
SET search_path = ag_catalog, "$user", public;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY ARRAY['Document', 'Chunk', 'ContactGroup']::TEXT[] LOOP
        IF to_regclass(format('%I.%I', 'moa_graph', label_name)) IS NULL THEN
            EXECUTE format('SELECT ag_catalog.create_vlabel(%L, %L)', 'moa_graph', label_name);
        END IF;
    END LOOP;

    FOREACH label_name IN ARRAY ARRAY['CONTAINS', 'MEMBER_OF']::TEXT[] LOOP
        IF to_regclass(format('%I.%I', 'moa_graph', label_name)) IS NULL THEN
            EXECUTE format('SELECT ag_catalog.create_elabel(%L, %L)', 'moa_graph', label_name);
        END IF;
    END LOOP;
END $$;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY ARRAY['Document', 'Chunk', 'ContactGroup']::TEXT[] LOOP
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE (id)',
            label_name || '_id_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"uid"''::ag_catalog.agtype])))',
            label_name || '_uid_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"storage_partition_id"''::ag_catalog.agtype])))',
            label_name || '_storage_partition_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"scope"''::ag_catalog.agtype])))',
            label_name || '_scope_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"valid_to"''::ag_catalog.agtype])))
             WHERE (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"valid_to"''::ag_catalog.agtype])) IS NULL',
            label_name || '_validto_partial_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING GIN (properties)',
            label_name || '_props_gin',
            label_name
        );
    END LOOP;
END $$;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY ARRAY['CONTAINS', 'MEMBER_OF']::TEXT[] LOOP
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE (start_id)',
            label_name || '_start_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE (end_id)',
            label_name || '_end_idx',
            label_name
        );
        EXECUTE format(
            'CREATE INDEX IF NOT EXISTS %I ON moa_graph.%I USING BTREE
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"storage_partition_id"''::ag_catalog.agtype])))',
            label_name || '_storage_partition_idx',
            label_name
        );
    END LOOP;
END $$;

GRANT USAGE ON SCHEMA ag_catalog TO moa_app, moa_promoter;
GRANT USAGE ON SCHEMA moa_graph TO moa_app, moa_promoter;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY ARRAY[
        'Document',
        'Chunk',
        'ContactGroup',
        'CONTAINS',
        'MEMBER_OF'
    ]::TEXT[] LOOP
        PERFORM moa.apply_age_three_tier_rls(format('%I.%I', 'moa_graph', label_name)::REGCLASS);
    END LOOP;
END $$;

GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA moa_graph TO moa_app, moa_promoter;

SELECT pg_catalog.set_config(
    'search_path',
    COALESCE(
        NULLIF(pg_catalog.current_setting('moa.migration_search_path', true), ''),
        '"$user", public'
    ),
    false
);
