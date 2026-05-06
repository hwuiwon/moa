CREATE TABLE IF NOT EXISTS moa.graph_changelog (
    change_id BIGSERIAL,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
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
    target_kind TEXT NOT NULL CHECK (target_kind IN ('node', 'edge')),
    target_label TEXT NOT NULL
        CHECK (
            target_label = ANY(moa.age_vertex_labels())
            OR target_label = ANY(moa.age_edge_labels())
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
    ON moa.graph_changelog (workspace_id, created_at DESC);
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

CREATE TABLE IF NOT EXISTS moa.workspace_state (
    workspace_id TEXT PRIMARY KEY,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    changelog_version BIGINT NOT NULL DEFAULT 0,
    vector_backend TEXT NOT NULL DEFAULT 'pgvector'
        CHECK (vector_backend IN ('pgvector', 'turbopuffer')),
    vector_backend_state TEXT NOT NULL DEFAULT 'steady'
        CHECK (vector_backend_state IN ('steady', 'migrating', 'dual_read')),
    dual_read_until TIMESTAMPTZ,
    hipaa_tier TEXT NOT NULL DEFAULT 'standard'
        CHECK (hipaa_tier IN ('standard', 'hipaa', 'restricted')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (user_id IS NULL),
    CHECK (scope = 'workspace')
);

CREATE INDEX IF NOT EXISTS workspace_state_version_idx
    ON moa.workspace_state (workspace_id, changelog_version);

CREATE OR REPLACE FUNCTION moa.bump_workspace_state_from_changelog() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.workspace_id IS NULL THEN
        RETURN NEW;
    END IF;

    INSERT INTO moa.workspace_state (workspace_id, changelog_version)
    VALUES (NEW.workspace_id, 1)
    ON CONFLICT (workspace_id) DO UPDATE
        SET changelog_version = moa.workspace_state.changelog_version + 1,
            updated_at = now();

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS graph_changelog_bump_workspace_state ON moa.graph_changelog;
CREATE TRIGGER graph_changelog_bump_workspace_state
    AFTER INSERT ON moa.graph_changelog
    FOR EACH ROW
    EXECUTE FUNCTION moa.bump_workspace_state_from_changelog();

ALTER TABLE moa.graph_changelog ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.graph_changelog FORCE ROW LEVEL SECURITY;
SELECT moa.drop_three_tier_policies('moa.graph_changelog'::REGCLASS);
DROP POLICY IF EXISTS rd_auditor ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app_workspace ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_app_user ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_promoter ON moa.graph_changelog;
DROP POLICY IF EXISTS ins_promoter_global ON moa.graph_changelog;
SELECT moa.apply_three_tier_read_policies('moa.graph_changelog'::REGCLASS);

CREATE POLICY rd_auditor ON moa.graph_changelog
    FOR SELECT TO moa_auditor
    USING (true);
CREATE POLICY ins_app_workspace ON moa.graph_changelog
    FOR INSERT TO moa_app
    WITH CHECK (
        scope = 'workspace'
        AND workspace_id = moa.current_workspace()
    );
CREATE POLICY ins_app_user ON moa.graph_changelog
    FOR INSERT TO moa_app
    WITH CHECK (
        scope = 'user'
        AND workspace_id = moa.current_workspace()
        AND user_id = moa.current_user_id()
    );
CREATE POLICY ins_promoter_global ON moa.graph_changelog
    FOR INSERT TO moa_promoter
    WITH CHECK (scope = 'global');

ALTER TABLE moa.workspace_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.workspace_state FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS ws_self_select ON moa.workspace_state;
DROP POLICY IF EXISTS ws_self_insert ON moa.workspace_state;
DROP POLICY IF EXISTS ws_self_update ON moa.workspace_state;
DROP POLICY IF EXISTS ws_promoter ON moa.workspace_state;
DROP POLICY IF EXISTS owner_dev_access ON moa.workspace_state;
CREATE POLICY ws_self_select ON moa.workspace_state
    FOR SELECT TO moa_app
    USING (workspace_id = moa.current_workspace());
CREATE POLICY ws_self_insert ON moa.workspace_state
    FOR INSERT TO moa_app
    WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY ws_self_update ON moa.workspace_state
    FOR UPDATE TO moa_app
    USING (workspace_id = moa.current_workspace())
    WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY ws_promoter ON moa.workspace_state
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

GRANT SELECT, INSERT, UPDATE ON moa.workspace_state TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.workspace_state TO moa_promoter;

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
