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
    CHECK (target_kind IN ('node', 'edge', 'user'));

ALTER TABLE moa.graph_changelog
    DROP CONSTRAINT IF EXISTS graph_changelog_target_label_check;
ALTER TABLE moa.graph_changelog
    ADD CONSTRAINT graph_changelog_target_label_check
    CHECK (
        target_label = 'User'
        OR target_label = ANY(moa.age_vertex_labels())
        OR target_label = ANY(moa.age_edge_labels())
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

DROP POLICY IF EXISTS rd_auditor ON moa.skill;
CREATE POLICY rd_auditor ON moa.skill
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.skill_addendum;
CREATE POLICY rd_auditor ON moa.skill_addendum
    FOR SELECT TO moa_auditor
    USING (true);

GRANT SELECT ON moa.skill TO moa_auditor;
GRANT SELECT ON moa.skill_addendum TO moa_auditor;
