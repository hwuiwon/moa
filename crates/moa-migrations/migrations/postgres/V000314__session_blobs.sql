-- Migration-owned durable claim-check payload storage for session events.

CREATE OR REPLACE FUNCTION moa.set_session_blob_tenant_columns() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    session_tenant UUID;
BEGIN
    EXECUTE format('SELECT tenant_id FROM %I.sessions WHERE id = $1', TG_TABLE_SCHEMA)
        INTO session_tenant
        USING NEW.session_id;

    IF session_tenant IS NULL THEN
        RAISE EXCEPTION 'session_blobs row references missing session %', NEW.session_id
            USING ERRCODE = '23503';
    END IF;

    IF NEW.tenant_id IS NULL THEN
        NEW.tenant_id := session_tenant;
    ELSIF NEW.tenant_id <> session_tenant THEN
        RAISE EXCEPTION 'session_blobs tenant_id % does not match session % tenant_id %',
            NEW.tenant_id, NEW.session_id, session_tenant
            USING ERRCODE = '42501';
    END IF;

    RETURN NEW;
END;
$$;

DO $$
DECLARE
    migration_search_path TEXT := NULLIF(current_setting('moa.migration_search_path', TRUE), '');
    first_search_path_entry TEXT;
    session_schema TEXT;
    blob_table REGCLASS;
BEGIN
    first_search_path_entry := btrim(split_part(COALESCE(migration_search_path, 'public'), ',', 1));
    session_schema := NULLIF(btrim(first_search_path_entry, '"'), '');
    IF session_schema IS NULL THEN
        session_schema := 'public';
    END IF;

    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I.session_blobs (
            session_id UUID NOT NULL,
            tenant_id UUID,
            blob_id TEXT NOT NULL,
            content BYTEA NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CONSTRAINT session_blobs_pkey PRIMARY KEY (session_id, blob_id)
        )',
        session_schema
    );

    blob_table := format('%I.session_blobs', session_schema)::REGCLASS;

    EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS tenant_id UUID', blob_table);
    EXECUTE format(
        'UPDATE %s b
         SET tenant_id = s.tenant_id
         FROM %I.sessions s
         WHERE b.session_id = s.id
           AND b.tenant_id IS NULL',
        blob_table,
        session_schema
    );
    EXECUTE format('ALTER TABLE %s ALTER COLUMN tenant_id SET NOT NULL', blob_table);

    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conrelid = blob_table
          AND conname = 'session_blobs_session_id_fkey'
    ) THEN
        EXECUTE format(
            'ALTER TABLE %s
             ADD CONSTRAINT session_blobs_session_id_fkey
             FOREIGN KEY (session_id) REFERENCES %I.sessions(id) ON DELETE CASCADE',
            blob_table,
            session_schema
        );
    END IF;

    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS idx_session_blobs_tenant_session
         ON %s (tenant_id, session_id)',
        blob_table
    );
    EXECUTE format('DROP TRIGGER IF EXISTS session_blobs_set_tenant_columns ON %s', blob_table);
    EXECUTE format(
        'CREATE TRIGGER session_blobs_set_tenant_columns
         BEFORE INSERT OR UPDATE ON %s
         FOR EACH ROW
         EXECUTE FUNCTION moa.set_session_blob_tenant_columns()',
        blob_table
    );

    PERFORM moa.apply_tenant_rls(blob_table);
END $$;
