-- Information barriers / need-to-know retrieval for graph-memory nodes (WS4-a).
--
-- Layers a Chinese-wall / MNPI-segregation guarantee on top of the existing
-- three-tier (global/tenant/contact) RLS: desk A must never RETRIEVE desk B's
-- restricted memories even inside the same tenant. Enforced in the database as
-- defense-in-depth so a missed application-layer check cannot leak a barriered
-- row.
--
-- Model: a node may carry an optional `barrier` tag. Most nodes have NONE
-- (NULL) and stay visible under the existing tiers, unchanged. A barriered node
-- (`barrier IS NOT NULL`) is SELECT-able only when its tag is in the caller's
-- cleared-barrier set, carried in the `moa.cleared_barriers` session GUC
-- (comma-delimited, installed by moa-db's ScopedConn from RlsContext).
--
-- FAIL CLOSED: an unset/empty `moa.cleared_barriers` yields an empty clearance
-- array, so every barriered row is hidden (need-to-know default-deny). NULL
-- barrier rows are never affected.
--
-- Additive and nullable: no backfill, no rewrite of existing rows. The barrier
-- check is an ADDITIONAL AND-constraint on reads implemented as a RESTRICTIVE
-- policy. PostgreSQL combines the existing permissive tier policies
-- (rd_global/rd_tenant/rd_user) with OR and ANDs every RESTRICTIVE policy on
-- top, so a permissive-only "barrier" policy would have GRANTED extra access;
-- RESTRICTIVE is the only shape that tightens reads. The three tiers stay
-- intact and untouched.

ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS barrier TEXT;

ALTER TABLE moa.node_index
    DROP CONSTRAINT IF EXISTS node_index_barrier_valid;
ALTER TABLE moa.node_index
    ADD CONSTRAINT node_index_barrier_valid
    CHECK (
        barrier IS NULL OR (
            barrier <> ''
            AND octet_length(barrier) <= 128
            AND position(',' IN barrier) = 0
            AND barrier !~ '[[:cntrl:]]'
        )
    );

COMMENT ON COLUMN moa.node_index.barrier IS
    'Optional information-barrier / need-to-know tag. NULL (the common case) means unrestricted under the three tiers. A non-NULL tag is retrievable only when present in the caller''s moa.cleared_barriers GUC; an unset/empty clearance hides the row (fail closed).';

-- Serves the need-to-know policy''s `barrier IS NOT NULL` predicate over live
-- rows without scanning the (overwhelmingly NULL-barrier) table.
CREATE INDEX IF NOT EXISTS node_index_barrier_partial
    ON moa.node_index (barrier)
    WHERE barrier IS NOT NULL AND valid_to IS NULL;

-- Parses the comma-delimited `moa.cleared_barriers` GUC into the set of tags the
-- caller is cleared to retrieve. A missing or empty setting yields an EMPTY
-- array (not NULL), so `barrier = ANY(...)` is a definite FALSE for every
-- barriered row -- the fail-closed default. STABLE (reads a GUC, no table I/O).
CREATE OR REPLACE FUNCTION moa.current_cleared_barriers() RETURNS TEXT[]
LANGUAGE SQL STABLE
AS $$
    SELECT COALESCE(
        string_to_array(NULLIF(current_setting('moa.cleared_barriers', TRUE), ''), ','),
        ARRAY[]::TEXT[]
    );
$$;

-- RESTRICTIVE read gate: ANDed with the permissive three-tier SELECT policies.
-- NULL-barrier rows pass unconditionally (tiers alone decide); barriered rows
-- pass only when their tag is in the caller''s cleared set. TO moa_app mirrors
-- the tier read policies; moa_auditor (rd_auditor USING(true)) and moa_promoter
-- are intentionally not gated here.
DROP POLICY IF EXISTS rd_barrier_need_to_know ON moa.node_index;
CREATE POLICY rd_barrier_need_to_know ON moa.node_index
    AS RESTRICTIVE
    FOR SELECT TO moa_app
    USING (
        barrier IS NULL
        OR barrier = ANY (moa.current_cleared_barriers())
    );

-- Knowledge sources own their barrier assignment. Every sync run snapshots the
-- connection value so a concurrent source-policy update cannot retag an
-- already-running ingestion on another Kubernetes replica.
ALTER TABLE moa.knowledge_connections
    ADD COLUMN IF NOT EXISTS information_barrier TEXT;

ALTER TABLE moa.knowledge_connections
    DROP CONSTRAINT IF EXISTS knowledge_connections_information_barrier_valid;
ALTER TABLE moa.knowledge_connections
    ADD CONSTRAINT knowledge_connections_information_barrier_valid
    CHECK (
        information_barrier IS NULL OR (
            information_barrier <> ''
            AND octet_length(information_barrier) <= 128
            AND position(',' IN information_barrier) = 0
            AND information_barrier !~ '[[:cntrl:]]'
        )
    );

ALTER TABLE moa.knowledge_sync_runs
    ADD COLUMN IF NOT EXISTS information_barrier TEXT;

ALTER TABLE moa.knowledge_sync_runs
    DROP CONSTRAINT IF EXISTS knowledge_sync_runs_information_barrier_valid;
ALTER TABLE moa.knowledge_sync_runs
    ADD CONSTRAINT knowledge_sync_runs_information_barrier_valid
    CHECK (
        information_barrier IS NULL OR (
            information_barrier <> ''
            AND octet_length(information_barrier) <= 128
            AND position(',' IN information_barrier) = 0
            AND information_barrier !~ '[[:cntrl:]]'
        )
    );

-- Privacy erasure must see subject rows even when their barrier is not in the
-- caller's retrieval clearances. A dedicated NOLOGIN/NOBYPASSRLS definer gets
-- only subject-bounded policies; moa_app itself never gains BYPASSRLS.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'moa_privacy_eraser') THEN
        CREATE ROLE moa_privacy_eraser NOLOGIN NOBYPASSRLS;
    END IF;
    ALTER ROLE moa_privacy_eraser NOLOGIN NOBYPASSRLS;
    EXECUTE format('GRANT moa_privacy_eraser TO %I', current_user);
END
$$;

GRANT USAGE ON SCHEMA moa TO moa_privacy_eraser;
GRANT SELECT, DELETE ON moa.node_index, moa.edge_index, moa.embeddings,
    moa.memory_digests, moa.retrieval_lineage TO moa_privacy_eraser;
GRANT SELECT, INSERT, DELETE ON moa.graph_changelog TO moa_privacy_eraser;
GRANT SELECT, INSERT, UPDATE ON moa.storage_partition_state TO moa_privacy_eraser;
GRANT SELECT, INSERT ON moa.vector_sync_outbox TO moa_privacy_eraser;
GRANT USAGE, SELECT ON SEQUENCE moa.graph_changelog_change_id_seq,
    moa.vector_sync_outbox_sync_id_seq TO moa_privacy_eraser;

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.node_index;
CREATE POLICY privacy_eraser_subject ON moa.node_index
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND data_subject_id = moa.current_contact_id()
    )
    WITH CHECK (
        tenant_id = moa.current_tenant_id()
        AND data_subject_id = moa.current_contact_id()
    );

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.edge_index;
CREATE POLICY privacy_eraser_subject ON moa.edge_index
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND (
            contact_id = moa.current_contact_id()
            OR EXISTS (
                SELECT 1 FROM moa.node_index AS subject_node
                WHERE subject_node.tenant_id = moa.current_tenant_id()
                  AND subject_node.data_subject_id = moa.current_contact_id()
                  AND subject_node.uid IN (start_uid, end_uid)
            )
        )
    )
    WITH CHECK (false);

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.embeddings;
CREATE POLICY privacy_eraser_subject ON moa.embeddings
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND EXISTS (
            SELECT 1 FROM moa.node_index AS subject_node
            WHERE subject_node.tenant_id = moa.current_tenant_id()
              AND subject_node.data_subject_id = moa.current_contact_id()
              AND subject_node.uid = embeddings.uid
        )
    )
    WITH CHECK (false);

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.memory_digests;
CREATE POLICY privacy_eraser_subject ON moa.memory_digests
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND contact_id = moa.current_contact_id()
    )
    WITH CHECK (false);

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.retrieval_lineage;
CREATE POLICY privacy_eraser_subject ON moa.retrieval_lineage
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND contact_id = moa.current_contact_id()
    )
    WITH CHECK (false);

DROP POLICY IF EXISTS privacy_eraser_subject ON moa.graph_changelog;
CREATE POLICY privacy_eraser_subject ON moa.graph_changelog
    FOR ALL TO moa_privacy_eraser
    USING (
        tenant_id = moa.current_tenant_id()
        AND contact_id = moa.current_contact_id()
    )
    WITH CHECK (
        tenant_id = moa.current_tenant_id()
        AND contact_id = moa.current_contact_id()
        AND target_kind = 'contact'
        AND op = 'erase'
    );

DROP POLICY IF EXISTS privacy_eraser_tenant ON moa.storage_partition_state;
CREATE POLICY privacy_eraser_tenant ON moa.storage_partition_state
    FOR ALL TO moa_privacy_eraser
    USING (tenant_id = moa.current_tenant_id())
    WITH CHECK (tenant_id = moa.current_tenant_id());

DROP POLICY IF EXISTS privacy_eraser_tenant ON moa.vector_sync_outbox;
CREATE POLICY privacy_eraser_tenant ON moa.vector_sync_outbox
    FOR ALL TO moa_privacy_eraser
    USING (storage_partition_id = moa.current_storage_partition())
    WITH CHECK (storage_partition_id = moa.current_storage_partition() AND op = 'delete');

CREATE OR REPLACE FUNCTION moa.erase_memory_data_subject(
    p_tenant_id UUID,
    p_data_subject_id UUID,
    p_audit JSONB
) RETURNS JSONB
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, moa
AS $$
DECLARE
    v_node_uids UUID[] := ARRAY[]::UUID[];
    v_nodes_deleted BIGINT := 0;
    v_edges_deleted BIGINT := 0;
    v_embeddings_deleted BIGINT := 0;
BEGIN
    IF moa.current_tenant_id() IS NULL OR moa.current_contact_id() IS NULL THEN
        RAISE EXCEPTION 'privacy erasure requires tenant and contact GUCs'
            USING ERRCODE = '42501';
    END IF;
    IF p_tenant_id IS DISTINCT FROM moa.current_tenant_id()
       OR p_data_subject_id IS DISTINCT FROM moa.current_contact_id() THEN
        RAISE EXCEPTION 'privacy erasure arguments do not match scoped GUCs'
            USING ERRCODE = '42501';
    END IF;
    IF p_audit IS NULL
       OR COALESCE(p_audit->>'approver_id', '') = ''
       OR COALESCE(p_audit->>'approval_token_jti', '') = '' THEN
        RAISE EXCEPTION 'privacy erasure requires approver and approval token audit metadata'
            USING ERRCODE = '22023';
    END IF;

    SELECT COALESCE(array_agg(node.uid ORDER BY node.uid), ARRAY[]::UUID[])
      INTO v_node_uids
      FROM moa.node_index AS node
     WHERE node.tenant_id = p_tenant_id
       AND node.data_subject_id = p_data_subject_id;

    INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op)
    SELECT DISTINCT node.storage_partition_id, node.uid, 'delete'
      FROM moa.node_index AS node
     WHERE node.uid = ANY(v_node_uids)
       AND node.storage_partition_id IS NOT NULL;

    DELETE FROM moa.graph_changelog
     WHERE tenant_id = p_tenant_id
       AND contact_id = p_data_subject_id;

    DELETE FROM moa.embeddings WHERE uid = ANY(v_node_uids);
    GET DIAGNOSTICS v_embeddings_deleted = ROW_COUNT;

    DELETE FROM moa.edge_index
     WHERE start_uid = ANY(v_node_uids) OR end_uid = ANY(v_node_uids);
    GET DIAGNOSTICS v_edges_deleted = ROW_COUNT;

    DELETE FROM moa.node_index
     WHERE tenant_id = p_tenant_id AND data_subject_id = p_data_subject_id;
    GET DIAGNOSTICS v_nodes_deleted = ROW_COUNT;

    INSERT INTO moa.graph_changelog (
        storage_partition_id, user_id, actor_id, actor_kind, op,
        target_kind, target_label, target_uid, payload, redaction_marker,
        pii_class, audit_metadata
    ) VALUES (
        p_tenant_id::TEXT,
        p_data_subject_id::TEXT,
        p_audit->>'approver_id',
        'admin',
        'erase',
        'contact',
        'User',
        p_data_subject_id,
        jsonb_build_object(
            'redacted', true,
            'nodes_deleted', v_nodes_deleted,
            'edges_deleted', v_edges_deleted,
            'embeddings_deleted', v_embeddings_deleted
        ),
        'erase:' || (p_audit->>'approval_token_jti'),
        'restricted',
        p_audit
    );

    RETURN jsonb_build_object(
        'nodes_deleted', v_nodes_deleted,
        'edges_deleted', v_edges_deleted,
        'embeddings_deleted', v_embeddings_deleted
    );
END;
$$;

ALTER FUNCTION moa.erase_memory_data_subject(UUID, UUID, JSONB)
    OWNER TO moa_privacy_eraser;
REVOKE ALL ON FUNCTION moa.erase_memory_data_subject(UUID, UUID, JSONB) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.erase_memory_data_subject(UUID, UUID, JSONB) TO moa_app;
