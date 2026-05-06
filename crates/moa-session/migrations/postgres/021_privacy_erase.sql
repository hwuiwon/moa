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
