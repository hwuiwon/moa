CREATE EXTENSION IF NOT EXISTS age;
LOAD 'age';
SET search_path = ag_catalog, "$user", public;
SELECT pg_advisory_xact_lock(hashtext('moa_age_bootstrap')::BIGINT);

DO $$
BEGIN
    IF to_regnamespace('moa_graph') IS NULL THEN
        PERFORM ag_catalog.create_graph('moa_graph'::NAME);
    END IF;
END $$;

CREATE OR REPLACE FUNCTION moa.age_property(
    properties ag_catalog.agtype,
    property_key TEXT
) RETURNS ag_catalog.agtype
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ag_catalog.agtype_access_operator(
        VARIADIC ARRAY[properties, ('"' || property_key || '"')::ag_catalog.agtype]
    );
$$;

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
        'Source'
    ]::TEXT[];
$$;

CREATE OR REPLACE FUNCTION moa.age_edge_labels() RETURNS TEXT[]
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ARRAY[
        'RELATES_TO',
        'DEPENDS_ON',
        'SUPERSEDES',
        'CONTRADICTS',
        'DERIVED_FROM',
        'MENTIONED_IN',
        'CAUSED',
        'LEARNED_FROM',
        'APPLIES_TO'
    ]::TEXT[];
$$;

CREATE OR REPLACE FUNCTION moa.age_base_labels() RETURNS TEXT[]
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ARRAY['_ag_label_vertex', '_ag_label_edge']::TEXT[];
$$;

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

    EXECUTE format(
        'CREATE POLICY rd_global ON %s FOR SELECT TO moa_app
         USING (moa.age_property(properties, ''scope'') = ''"global"''::ag_catalog.agtype
                AND moa.current_scope_tier() IS NOT NULL)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_workspace ON %s FOR SELECT TO moa_app
         USING (moa.age_property(properties, ''scope'') = ''"workspace"''::ag_catalog.agtype
                AND moa.age_property(properties, ''workspace_id'')
                    = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_user ON %s FOR SELECT TO moa_app
         USING (moa.age_property(properties, ''scope'') = ''"user"''::ag_catalog.agtype
                AND moa.age_property(properties, ''workspace_id'')
                    = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype
                AND moa.age_property(properties, ''user_id'')
                    = (''"'' || moa.current_user_id() || ''"'')::ag_catalog.agtype)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_workspace ON %s FOR ALL TO moa_app
         USING (moa.age_property(properties, ''scope'') = ''"workspace"''::ag_catalog.agtype
                AND moa.age_property(properties, ''workspace_id'')
                    = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype)
         WITH CHECK (moa.age_property(properties, ''scope'') = ''"workspace"''::ag_catalog.agtype
                     AND moa.age_property(properties, ''workspace_id'')
                         = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_user ON %s FOR ALL TO moa_app
         USING (moa.age_property(properties, ''scope'') = ''"user"''::ag_catalog.agtype
                AND moa.age_property(properties, ''workspace_id'')
                    = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype
                AND moa.age_property(properties, ''user_id'')
                    = (''"'' || moa.current_user_id() || ''"'')::ag_catalog.agtype)
         WITH CHECK (moa.age_property(properties, ''scope'') = ''"user"''::ag_catalog.agtype
                     AND moa.age_property(properties, ''workspace_id'')
                         = (''"'' || moa.current_workspace() || ''"'')::ag_catalog.agtype
                     AND moa.age_property(properties, ''user_id'')
                         = (''"'' || moa.current_user_id() || ''"'')::ag_catalog.agtype)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_global_promoter ON %s FOR ALL TO moa_promoter
         USING (moa.age_property(properties, ''scope'') = ''"global"''::ag_catalog.agtype)
         WITH CHECK (moa.age_property(properties, ''scope'') = ''"global"''::ag_catalog.agtype)',
        target_table
    );
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
    FOREACH label_name IN ARRAY moa.age_vertex_labels() LOOP
        IF to_regclass(format('%I.%I', 'moa_graph', label_name)) IS NULL THEN
            EXECUTE format('SELECT ag_catalog.create_vlabel(%L, %L)', 'moa_graph', label_name);
        END IF;
    END LOOP;

    FOREACH label_name IN ARRAY moa.age_edge_labels() LOOP
        IF to_regclass(format('%I.%I', 'moa_graph', label_name)) IS NULL THEN
            EXECUTE format('SELECT ag_catalog.create_elabel(%L, %L)', 'moa_graph', label_name);
        END IF;
    END LOOP;
END $$;

DO $$
DECLARE
    label_name TEXT;
BEGIN
    FOREACH label_name IN ARRAY (moa.age_vertex_labels() || ARRAY['_ag_label_vertex']::TEXT[]) LOOP
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
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"workspace_id"''::ag_catalog.agtype])))',
            label_name || '_workspace_idx',
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
    FOREACH label_name IN ARRAY (moa.age_edge_labels() || ARRAY['_ag_label_edge']::TEXT[]) LOOP
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
             ((ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, ''"workspace_id"''::ag_catalog.agtype])))',
            label_name || '_workspace_idx',
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
    FOREACH label_name IN ARRAY (
        moa.age_vertex_labels() || moa.age_edge_labels() || moa.age_base_labels()
    ) LOOP
        PERFORM moa.apply_age_three_tier_rls(format('%I.%I', 'moa_graph', label_name)::REGCLASS);
    END LOOP;
END $$;

GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA moa_graph TO moa_app, moa_promoter;
