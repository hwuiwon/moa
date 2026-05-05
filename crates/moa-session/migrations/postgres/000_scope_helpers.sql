CREATE SCHEMA IF NOT EXISTS moa;

DO $$
BEGIN
    CREATE ROLE moa_app NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_promoter NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_owner NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_auditor NOLOGIN;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE moa_replicator LOGIN REPLICATION;
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

GRANT moa_app TO CURRENT_USER;
GRANT moa_promoter TO CURRENT_USER;
GRANT moa_auditor TO CURRENT_USER;

CREATE OR REPLACE FUNCTION moa.compute_scope_tier(
    workspace_id TEXT,
    user_id TEXT
) RETURNS TEXT
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT CASE
        WHEN workspace_id IS NULL AND user_id IS NULL THEN 'global'
        WHEN workspace_id IS NOT NULL AND user_id IS NOT NULL THEN 'user'
        WHEN workspace_id IS NOT NULL AND user_id IS NULL THEN 'workspace'
        ELSE NULL
    END;
$$;

CREATE OR REPLACE FUNCTION moa.current_workspace() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.workspace_id', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.current_user_id() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.user_id', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.current_scope_tier() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.scope_tier', TRUE), '');
$$;

CREATE OR REPLACE FUNCTION moa.drop_three_tier_policies(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('DROP POLICY IF EXISTS workspace_isolation ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_global ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_workspace ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS rd_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_workspace ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_user ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS wr_global_promoter ON %s', target_table);
    EXECUTE format('DROP POLICY IF EXISTS owner_dev_access ON %s', target_table);
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_three_tier_read_policies(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target_table);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target_table);

    EXECUTE format(
        'CREATE POLICY rd_global ON %s FOR SELECT TO moa_app
         USING (scope = ''global'' AND moa.current_scope_tier() IS NOT NULL)',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_workspace ON %s FOR SELECT TO moa_app
         USING (scope = ''workspace'' AND workspace_id = moa.current_workspace())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY rd_user ON %s FOR SELECT TO moa_app
         USING (scope = ''user''
                AND workspace_id = moa.current_workspace()
                AND user_id = moa.current_user_id())',
        target_table
    );
END;
$$;

CREATE OR REPLACE FUNCTION moa.apply_three_tier_rls(target_table REGCLASS) RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM moa.drop_three_tier_policies(target_table);
    PERFORM moa.apply_three_tier_read_policies(target_table);

    EXECUTE format(
        'CREATE POLICY wr_workspace ON %s FOR ALL TO moa_app
         USING (scope = ''workspace'' AND workspace_id = moa.current_workspace())
         WITH CHECK (scope = ''workspace'' AND workspace_id = moa.current_workspace())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_user ON %s FOR ALL TO moa_app
         USING (scope = ''user''
                AND workspace_id = moa.current_workspace()
                AND user_id = moa.current_user_id())
         WITH CHECK (scope = ''user''
                     AND workspace_id = moa.current_workspace()
                     AND user_id = moa.current_user_id())',
        target_table
    );
    EXECUTE format(
        'CREATE POLICY wr_global_promoter ON %s FOR ALL TO moa_promoter
         USING (scope = ''global'') WITH CHECK (scope = ''global'')',
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
