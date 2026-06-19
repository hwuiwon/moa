CREATE OR REPLACE FUNCTION moa.apply_age_three_tier_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);

    EXECUTE format('DROP POLICY IF EXISTS rd_global ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_workspace ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_workspace ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_global_promoter ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS owner_dev_access ON %s', target_table);

    EXECUTE format($policy$
        CREATE POLICY rd_global ON %s FOR SELECT TO moa_app
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"global"'::ag_catalog.agtype
            AND moa.current_scope_tier() IS NOT NULL
        )
    $policy$, target_table);
    EXECUTE format($policy$
        CREATE POLICY rd_workspace ON %s FOR SELECT TO moa_app
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"workspace"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
        )
    $policy$, target_table);
    EXECUTE format($policy$
        CREATE POLICY rd_user ON %s FOR SELECT TO moa_app
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"user"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
            AND moa.age_property(properties, 'user_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_user_id() || '"')::ag_catalog.agtype
        )
    $policy$, target_table);
    EXECUTE format($policy$
        CREATE POLICY wr_workspace ON %s FOR ALL TO moa_app
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"workspace"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
        )
        WITH CHECK (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"workspace"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
        )
    $policy$, target_table);
    EXECUTE format($policy$
        CREATE POLICY wr_user ON %s FOR ALL TO moa_app
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"user"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
            AND moa.age_property(properties, 'user_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_user_id() || '"')::ag_catalog.agtype
        )
        WITH CHECK (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"user"'::ag_catalog.agtype
            AND moa.age_property(properties, 'workspace_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_workspace() || '"')::ag_catalog.agtype
            AND moa.age_property(properties, 'user_id')
                OPERATOR(ag_catalog.=) ('"' || moa.current_user_id() || '"')::ag_catalog.agtype
        )
    $policy$, target_table);
    EXECUTE format($policy$
        CREATE POLICY wr_global_promoter ON %s FOR ALL TO moa_promoter
        USING (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"global"'::ag_catalog.agtype
        )
        WITH CHECK (
            moa.age_property(properties, 'scope') OPERATOR(ag_catalog.=) '"global"'::ag_catalog.agtype
        )
    $policy$, target_table);
    EXECUTE format(
        'CREATE POLICY owner_dev_access ON %s FOR ALL TO %I
         USING (true) WITH CHECK (true)',
        target_table,
        pg_get_userbyid((SELECT relowner FROM pg_class WHERE oid = target_table))
    );

    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_app', target_table);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO moa_promoter', target_table);
END;
$$;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY (
        moa.age_vertex_labels() || moa.age_edge_labels() || moa.age_base_labels()
    ) LOOP
        PERFORM moa.apply_age_three_tier_rls(format('%I.%I', 'moa_graph', label_name)::REGCLASS);
    END LOOP;
END $$;
