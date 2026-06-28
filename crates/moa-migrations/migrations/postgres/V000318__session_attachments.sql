-- Durable user-visible attachments linked from session messages.

CREATE OR REPLACE FUNCTION moa.set_session_attachment_tenant_columns() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    session_tenant UUID;
BEGIN
    EXECUTE format('SELECT tenant_id FROM %I.sessions WHERE id = $1', TG_TABLE_SCHEMA)
        INTO session_tenant
        USING NEW.session_id;

    IF session_tenant IS NULL THEN
        RAISE EXCEPTION 'session_attachments row references missing session %', NEW.session_id
            USING ERRCODE = '23503';
    END IF;

    IF NEW.tenant_id IS NULL THEN
        NEW.tenant_id := session_tenant;
    ELSIF NEW.tenant_id <> session_tenant THEN
        RAISE EXCEPTION 'session_attachments tenant_id % does not match session % tenant_id %',
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
    attachments_table REGCLASS;
BEGIN
    first_search_path_entry := btrim(split_part(COALESCE(migration_search_path, 'public'), ',', 1));
    session_schema := NULLIF(btrim(first_search_path_entry, '"'), '');
    IF session_schema IS NULL THEN
        session_schema := 'public';
    END IF;

    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS %I.session_attachments (
            id UUID PRIMARY KEY,
            session_id UUID NOT NULL,
            tenant_id UUID,
            contact_id UUID,
            name TEXT NOT NULL,
            mime_type TEXT NOT NULL,
            sha256 TEXT NOT NULL CHECK (sha256 ~ ''^[0-9a-f]{64}$''),
            size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
            object_key TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )',
        session_schema
    );

    attachments_table := format('%I.session_attachments', session_schema)::REGCLASS;

    EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS tenant_id UUID', attachments_table);
    EXECUTE format('ALTER TABLE %s ADD COLUMN IF NOT EXISTS object_key TEXT', attachments_table);
    EXECUTE format(
        'UPDATE %s a
         SET tenant_id = s.tenant_id
         FROM %I.sessions s
         WHERE a.session_id = s.id
           AND a.tenant_id IS NULL',
        attachments_table,
        session_schema
    );
    EXECUTE format('ALTER TABLE %s ALTER COLUMN tenant_id SET NOT NULL', attachments_table);
    EXECUTE format('ALTER TABLE %s ALTER COLUMN object_key SET NOT NULL', attachments_table);

    EXECUTE format(
        'ALTER TABLE %s DROP CONSTRAINT IF EXISTS session_attachments_session_id_fkey',
        attachments_table
    );
    EXECUTE format(
        'ALTER TABLE %s
         ADD CONSTRAINT session_attachments_session_id_fkey
         FOREIGN KEY (session_id) REFERENCES %I.sessions(id) ON DELETE RESTRICT',
        attachments_table,
        session_schema
    );

    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS idx_session_attachments_tenant_session_created
         ON %s (tenant_id, session_id, created_at, id)',
        attachments_table
    );
    EXECUTE format(
        'CREATE INDEX IF NOT EXISTS idx_session_attachments_tenant_sha256
         ON %s (tenant_id, sha256)',
        attachments_table
    );
    EXECUTE format(
        'DROP TRIGGER IF EXISTS session_attachments_set_tenant_columns ON %s',
        attachments_table
    );
    EXECUTE format(
        'CREATE TRIGGER session_attachments_set_tenant_columns
         BEFORE INSERT OR UPDATE ON %s
         FOR EACH ROW
         EXECUTE FUNCTION moa.set_session_attachment_tenant_columns()',
        attachments_table
    );

    PERFORM moa.apply_tenant_rls(attachments_table);
END $$;
