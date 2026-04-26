CREATE TABLE IF NOT EXISTS moa.node_index (
    uid UUID PRIMARY KEY,
    gid BIGINT,
    label TEXT NOT NULL CHECK (label = ANY(moa.age_vertex_labels())),
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    name TEXT NOT NULL,
    name_tsv TSVECTOR GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(name, ''))
    ) STORED,
    pii_class TEXT NOT NULL DEFAULT 'none'
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    confidence DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to TIMESTAMPTZ,
    invalidated_at TIMESTAMPTZ,
    invalidated_by UUID,
    invalidated_reason TEXT,
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    properties_summary JSONB
);

CREATE INDEX IF NOT EXISTS node_index_ws_scope_label
    ON moa.node_index (workspace_id, scope, label)
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
    ON moa.node_index (last_accessed_at);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
SELECT moa.apply_three_tier_rls('moa.node_index'::REGCLASS);
