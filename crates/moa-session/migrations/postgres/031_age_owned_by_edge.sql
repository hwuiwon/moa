-- Add the ownership-flavored graph edge label used by entity-resolution v2.

DO $$
BEGIN
    IF to_regclass(format('%I.%I', 'moa_graph', 'OWNED_BY')) IS NULL THEN
        EXECUTE format('SELECT ag_catalog.create_elabel(%L, %L)', 'moa_graph', 'OWNED_BY');
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS OWNED_BY_start_idx ON moa_graph."OWNED_BY" USING BTREE (start_id);
CREATE INDEX IF NOT EXISTS OWNED_BY_end_idx ON moa_graph."OWNED_BY" USING BTREE (end_id);
CREATE INDEX IF NOT EXISTS OWNED_BY_workspace_idx ON moa_graph."OWNED_BY" USING BTREE
    ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"workspace_id"'::ag_catalog.agtype])));

GRANT SELECT, INSERT, UPDATE, DELETE ON moa_graph."OWNED_BY" TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa_graph."OWNED_BY" TO moa_promoter;

SELECT moa.apply_age_three_tier_rls('moa_graph."OWNED_BY"'::REGCLASS);

GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA moa_graph TO moa_app, moa_promoter;
