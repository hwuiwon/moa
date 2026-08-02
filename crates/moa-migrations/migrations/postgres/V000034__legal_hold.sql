-- Linearizable legal holds and durable destruction fences.
--
-- Every hold/destruction path takes the same transaction advisory locks: the
-- tenant key first, followed by subject keys in UUID order.  A destruction
-- fence is committed before the first destructive stage.  It is intentionally
-- retained as a minimal tombstone so a later hold can never misrepresent
-- already-destroyed data as preserved.

CREATE TABLE moa.legal_hold (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    subject_id   UUID,
    reason       TEXT        NOT NULL,
    placed_by    TEXT        NOT NULL,
    placed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    released_at  TIMESTAMPTZ,
    released_by  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT legal_hold_release_consistent CHECK (
        (released_at IS NULL AND released_by IS NULL)
        OR (released_at IS NOT NULL AND released_by IS NOT NULL)
    )
);

COMMENT ON TABLE moa.legal_hold IS
    'Litigation/finance holds. Active rows preserve one subject or an entire tenant; released rows remain as redacted audit tombstones after tenant purge.';

CREATE UNIQUE INDEX legal_hold_one_active
    ON moa.legal_hold (tenant_id, subject_id)
    NULLS NOT DISTINCT
    WHERE released_at IS NULL;

CREATE TABLE moa.destruction_operation_fence (
    tenant_id      UUID        NOT NULL,
    subject_id     UUID,
    operation_id   TEXT        NOT NULL,
    operation_kind TEXT        NOT NULL,
    status         TEXT        NOT NULL DEFAULT 'in_progress'
        CHECK (status IN ('in_progress', 'committed')),
    started_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    committed_at   TIMESTAMPTZ,
    CONSTRAINT destruction_fence_operation_nonempty CHECK (operation_id <> ''),
    CONSTRAINT destruction_fence_kind_nonempty CHECK (operation_kind <> ''),
    CONSTRAINT destruction_fence_status_consistent CHECK (
        (status = 'in_progress' AND committed_at IS NULL)
        OR (status = 'committed' AND committed_at IS NOT NULL)
    )
);

COMMENT ON TABLE moa.destruction_operation_fence IS
    'Minimal permanent fence committed before destructive privacy work. NULL subject_id covers the whole tenant.';

CREATE UNIQUE INDEX destruction_fence_scope
    ON moa.destruction_operation_fence (tenant_id, subject_id)
    NULLS NOT DISTINCT;
CREATE INDEX destruction_fence_operation
    ON moa.destruction_operation_fence (tenant_id, operation_id);
CREATE INDEX destruction_fence_in_progress_tenant
    ON moa.destruction_operation_fence (tenant_id)
    WHERE status = 'in_progress';

-- Once destructive work is admitted, no replica may recreate graph data in the
-- protected tenant/subject while resumable external and relational stages run.
CREATE FUNCTION moa.reject_graph_write_during_destruction() RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, moa
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence AS fence
        WHERE fence.tenant_id = NEW.tenant_id
          AND (fence.subject_id IS NULL OR fence.subject_id = NEW.data_subject_id)
    ) THEN
        RAISE EXCEPTION 'graph write refused: destruction is fenced for tenant or subject'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER node_index_destruction_fence
BEFORE INSERT OR UPDATE ON moa.node_index
FOR EACH ROW EXECUTE FUNCTION moa.reject_graph_write_during_destruction();

ALTER TABLE moa.legal_hold ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.legal_hold FORCE ROW LEVEL SECURITY;
ALTER TABLE moa.destruction_operation_fence ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.destruction_operation_fence FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.legal_hold FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = moa.current_tenant_id()
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = moa.current_tenant_id()
    );

CREATE POLICY tenant_isolation ON moa.destruction_operation_fence FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = moa.current_tenant_id()
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = moa.current_tenant_id()
    );

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.legal_hold TO moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.destruction_operation_fence TO moa_app;
