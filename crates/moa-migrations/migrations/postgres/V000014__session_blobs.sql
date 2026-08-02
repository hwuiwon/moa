-- Migration-owned durable claim-check payload storage for session events.

CREATE OR REPLACE FUNCTION moa.set_session_blob_tenant_columns() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    session_tenant UUID;
BEGIN
    SELECT tenant_id
    INTO session_tenant
    FROM public.sessions
    WHERE id = NEW.session_id;

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

CREATE TABLE IF NOT EXISTS public.session_blobs (
    session_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    blob_id TEXT NOT NULL,
    content BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT session_blobs_pkey PRIMARY KEY (session_id, blob_id),
    CONSTRAINT session_blobs_session_tenant_fkey
        FOREIGN KEY (session_id, tenant_id)
        REFERENCES public.sessions(id, tenant_id)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_blobs_tenant_session
    ON public.session_blobs (tenant_id, session_id);

CREATE TRIGGER session_blobs_set_tenant_columns
    BEFORE INSERT ON public.session_blobs
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_session_blob_tenant_columns();

SELECT moa.apply_tenant_rls('public.session_blobs'::REGCLASS);
